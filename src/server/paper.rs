//! The paper routes (r3.s4.w4, spec §5): promote, stop, stop-all, the reads,
//! the on-demand shadow check and the session SSE stream.
//!
//! Every route mounts through the `mount_*` helpers with [`Scope::App`], so an
//! `agent` token is a 403 `scope_refused` on all of them (the least-privilege
//! control). The command routes go through the runtime's control handle
//! ([`PaperControl`]); with no runtime they answer 503 `runtime_unavailable`
//! and never hang. `promoted_by` and every stop label come from the caller's
//! [`AuthenticatedLabel`] in the request extensions — never from a body.
//!
//! The reads and the MCP tools share `paper_read`, so a route and a tool that
//! ask the same question answer from the same code.
//!
//! The SSE stream (`GET /api/v1/paper/sessions/{id}/events`) replays the log
//! from `events()` with `id:` = the event's `seq`, honours `Last-Event-ID: n`
//! (only `seq > n`), then polls the repository every
//! [`SESSION_EVENT_POLL_MS`]. Each poll re-checks the presented token and ends
//! the stream with an `error` frame `token_refused` once it is revoked (the
//! revocable-token control).

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as WireEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::auth::{AuthenticatedLabel, bearer_token, hash_token};
use super::routes::MountExt;
use super::{ServerState, ops};
use crate::adapters::broker::BinanceAdapter;
use crate::adapters::clock::SystemClock;
use crate::adapters::db::{
    SqliteBacktestRunRepo, SqliteClientTokenRepo, SqlitePaperSessionRepo, SqliteStrategyRepo,
};
use crate::adapters::store::CandleStore;
use crate::application::paper::{OverrideRequest, PaperPromotionError, promote};
use crate::application::paper_control::{
    PaperControl, PaperControlError, ShadowCheckReply, StopReply,
};
use crate::application::paper_read::{
    PaperReadError, list_summaries, session_summary, session_trades,
};
use crate::domain::PaperSessionRepository;
use crate::domain::paper::event::StopActor;
use crate::domain::paper::session::{NonEmptyLabel, NonEmptyReason, PaperSessionId};
use crate::domain::strategy::VersionId;
use crate::domain::{ExchangeAdapter as _, Pair, PromotionRefused, Timeframe};

/// How often the session SSE stream polls the repository for new events
/// (spec §5: "default every 2 s", the item's call).
const SESSION_EVENT_POLL_MS: u64 = 2_000;

/// The SSE keep-alive interval — shorter than the poll, so an idle stream
/// still shows liveness between polls.
const SESSION_KEEP_ALIVE: Duration = Duration::from_secs(10);

/// Mount every paper route. All of them are [`Scope::App`] (the helpers
/// enforce it).
pub(crate) fn mount(router: axum::Router, state: &Arc<ServerState>) -> axum::Router {
    router
        .mount_post(state, "/api/v1/paper/promote", |state, req| {
            Box::pin(promote_route(state, req))
        })
        .mount_post(state, "/api/v1/paper/stop-all", |state, req| {
            Box::pin(stop_all_route(state, req))
        })
        .mount_post_path(
            state,
            "/api/v1/paper/sessions/{id}/stop",
            |state, id, req| Box::pin(stop_route(state, id, req)),
        )
        .mount_post_path(
            state,
            "/api/v1/paper/sessions/{id}/shadow-check",
            |state, id, req| Box::pin(shadow_check_route(state, id, req)),
        )
        .mount_get(state, "/api/v1/paper/sessions", |state| {
            Box::pin(list_route(state))
        })
        .mount_get_path(state, "/api/v1/paper/sessions/{id}", |state, id| {
            Box::pin(session_route(state, id))
        })
        .mount_get_path(state, "/api/v1/paper/sessions/{id}/trades", |state, id| {
            Box::pin(trades_route(state, id))
        })
        .mount_get_path_request(
            state,
            "/api/v1/paper/sessions/{id}/events",
            |state, id, req| Box::pin(events_route(state, id, req)),
        )
}

// ---------------------------------------------------------------------------
// The command routes
// ---------------------------------------------------------------------------

/// The promote body (spec §5). Deliberately NOT `deny_unknown_fields`: a
/// `promoted_by` in the body is ignored, never honoured — the label comes from
/// the token alone (AC-2 (i)).
#[derive(Debug, Deserialize)]
struct PromoteBody {
    /// The strategy version to promote.
    version_id: String,
    /// The typed override, when the caller supplies one.
    #[serde(rename = "override")]
    promotion_override: Option<OverrideBody>,
}

/// The override half of the promote body. `reason` stays a raw string so an
/// empty one is the typed `empty_reason` refusal, not a body-parse failure.
#[derive(Debug, Deserialize)]
struct OverrideBody {
    /// Why the human overrode the gate.
    reason: String,
    /// The pair the session trades.
    pair: String,
    /// The session's primary timeframe.
    primary_timeframe: Timeframe,
    /// The session's higher timeframe, when named.
    #[serde(default)]
    htf_timeframe: Option<Timeframe>,
    /// Whether the session consumes the fixed daily series.
    #[serde(default)]
    uses_d1: bool,
}

/// `POST /api/v1/paper/promote` — the certified or overridden promotion, then
/// an immediate `Attach` so the session's lead-in lands at once.
async fn promote_route(state: Arc<ServerState>, req: Request) -> Response {
    let Some(label) = label_of(&req) else {
        return internal("the authenticated label is missing from the request");
    };
    let body: PromoteBody = match ops::parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let promotion_override = match body.promotion_override {
        None => None,
        Some(override_body) => match override_request(override_body) {
            Ok(request) => Some(request),
            Err(response) => return response,
        },
    };
    let Some(promoted_by) = NonEmptyLabel::try_new(&label.0).ok() else {
        return internal("the token's label is empty; the audit trail cannot name it");
    };
    // With no runtime the session would be persisted unattached, and a 503
    // after a successful write would invite a duplicate retry — so the refusal
    // comes first and nothing is written (spec §1).
    let Some(control) = control_of(&state) else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "runtime_unavailable",
            "no paper runtime is running; refusing to promote a session that could not attach",
        );
    };
    let paper = paper_repo(&state);
    let strategies = SqliteStrategyRepo::new(state.db.pool().clone());
    let runs = SqliteBacktestRunRepo::new(state.db.pool().clone());
    let version_id = VersionId::new(body.version_id);
    match promote(
        &strategies,
        &runs,
        &runs,
        &paper,
        &SystemClock,
        &version_id,
        promotion_override.as_ref(),
        promoted_by,
    )
    .await
    {
        Ok(session) => {
            // The attach is best-effort: the row is real either way, and a
            // runtime that cannot start the session is reported, not fatal.
            if let Err(error) = control.attach(session.id.clone()).await {
                state.log.write(format!(
                    "pulse serve: paper promote: session {} deferred attach: {error}",
                    session.id
                ));
            }
            match session_summary(&paper, &runs, &session.id).await {
                Ok(Some(summary)) => (StatusCode::CREATED, axum::Json(summary)).into_response(),
                Ok(None) => internal("the promoted session could not be read back"),
                Err(error) => read_error(&error),
            }
        }
        Err(PaperPromotionError::Refused(refused)) => promotion_refusal(&refused),
        Err(PaperPromotionError::UnknownVersion(id)) => api_error(
            StatusCode::NOT_FOUND,
            "unknown_version",
            format!("no such strategy version {}", id.as_str()),
        ),
        Err(PaperPromotionError::Data(error)) => {
            internal(&format!("paper promotion store failure: {error}"))
        }
        Err(error @ PaperPromotionError::InvalidShape(_)) => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation",
            error.to_string(),
        ),
    }
}

/// `POST /api/v1/paper/sessions/{id}/stop` — one session, the caller's label
/// as the actor.
async fn stop_route(state: Arc<ServerState>, id: String, req: Request) -> Response {
    let Some(label) = label_of(&req) else {
        return internal("the authenticated label is missing from the request");
    };
    let Some(actor) = stop_actor(&label) else {
        return internal("the token's label is empty; the audit trail cannot name it");
    };
    let Some(control) = control_of(&state) else {
        return runtime_unavailable();
    };
    let session_id = PaperSessionId::new(id.clone());
    match control.stop(session_id, actor).await {
        Ok(StopReply::Stopped) => (
            StatusCode::OK,
            axum::Json(json!({
                "session_id": id,
                "stopped_without_shadow": false,
            })),
        )
            .into_response(),
        Ok(StopReply::StoppedWithoutShadow) => (
            StatusCode::OK,
            axum::Json(json!({
                "session_id": id,
                "stopped_without_shadow": true,
            })),
        )
            .into_response(),
        Ok(StopReply::AlreadyStopped) => api_error(
            StatusCode::CONFLICT,
            "session_stopped",
            format!("session {id} is already stopped"),
        ),
        Ok(StopReply::Unknown) => api_error(
            StatusCode::NOT_FOUND,
            "unknown_session",
            format!("no paper session with id {id}"),
        ),
        Err(error) => control_error(&error),
    }
}

/// `POST /api/v1/paper/stop-all` — the kill switch (A7): every running session,
/// with the caller's label as the sweep's issuer.
async fn stop_all_route(state: Arc<ServerState>, req: Request) -> Response {
    let Some(label) = label_of(&req) else {
        return internal("the authenticated label is missing from the request");
    };
    let Some(issuer) = NonEmptyLabel::try_new(&label.0).ok() else {
        return internal("the token's label is empty; the audit trail cannot name it");
    };
    let Some(control) = control_of(&state) else {
        return runtime_unavailable();
    };
    match control.stop_all(issuer).await {
        Ok(reply) => {
            let stopped: Vec<&str> = reply.stopped.iter().map(PaperSessionId::as_str).collect();
            let failures: Vec<Value> = reply
                .failures
                .iter()
                .map(|failure| {
                    json!({
                        "id": failure.session_id.as_str(),
                        "code": failure.error.code(),
                    })
                })
                .collect();
            (
                StatusCode::OK,
                axum::Json(json!({ "stopped": stopped, "failures": failures })),
            )
                .into_response()
        }
        Err(error) => control_error(&error),
    }
}

/// `POST /api/v1/paper/sessions/{id}/shadow-check` — the on-demand check.
async fn shadow_check_route(state: Arc<ServerState>, id: String, _req: Request) -> Response {
    let Some(control) = control_of(&state) else {
        return runtime_unavailable();
    };
    let session_id = PaperSessionId::new(id.clone());
    match control.shadow_check(session_id).await {
        Ok(ShadowCheckReply::Checked(result)) => {
            (StatusCode::OK, axum::Json(result)).into_response()
        }
        Ok(ShadowCheckReply::Stopped) => api_error(
            StatusCode::CONFLICT,
            "session_stopped",
            format!("session {id} is stopped; there is nothing to shadow-check"),
        ),
        Ok(ShadowCheckReply::Unknown) => api_error(
            StatusCode::NOT_FOUND,
            "unknown_session",
            format!("no paper session with id {id}"),
        ),
        // A runtime IS present; the session simply is not attached to it.
        Ok(ShadowCheckReply::NotAttached) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "session_not_attached",
            format!("session {id} is not attached to the running paper runtime"),
        ),
        Err(error) => control_error(&error),
    }
}

// ---------------------------------------------------------------------------
// The read routes
// ---------------------------------------------------------------------------

/// `GET /api/v1/paper/sessions` — every session's summary, catalog order.
async fn list_route(state: Arc<ServerState>) -> Response {
    let paper = paper_repo(&state);
    let runs = SqliteBacktestRunRepo::new(state.db.pool().clone());
    match list_summaries(&paper, &runs).await {
        Ok(summaries) => (StatusCode::OK, axum::Json(summaries)).into_response(),
        Err(error) => read_error(&error),
    }
}

/// `GET /api/v1/paper/sessions/{id}` — one summary.
async fn session_route(state: Arc<ServerState>, id: String) -> Response {
    let paper = paper_repo(&state);
    let runs = SqliteBacktestRunRepo::new(state.db.pool().clone());
    let session_id = PaperSessionId::new(id.clone());
    match session_summary(&paper, &runs, &session_id).await {
        Ok(Some(summary)) => (StatusCode::OK, axum::Json(summary)).into_response(),
        Ok(None) => unknown_session(&id),
        Err(error) => read_error(&error),
    }
}

/// `GET /api/v1/paper/sessions/{id}/trades` — the closed trades with R and the
/// open position.
async fn trades_route(state: Arc<ServerState>, id: String) -> Response {
    let paper = paper_repo(&state);
    let session_id = PaperSessionId::new(id.clone());
    match session_trades(&paper, &session_id).await {
        Ok(Some(trades)) => (StatusCode::OK, axum::Json(trades)).into_response(),
        Ok(None) => unknown_session(&id),
        Err(error) => read_error(&error),
    }
}

/// `GET /api/v1/paper/sessions/{id}/events` — the SSE stream.
async fn events_route(state: Arc<ServerState>, id: String, req: Request) -> Response {
    let after = match parse_last_event_id(req.headers()) {
        Ok(after) => after,
        Err(response) => return response,
    };
    // The stream re-checks this hash on every poll (the revocable-token
    // control): the middleware validated the token at connect time, and a
    // revocation must end the stream, not just the next request.
    let token_hash = match bearer_token(req.headers()) {
        Ok(token) => hash_token(&token),
        Err(()) => {
            return api_error(
                StatusCode::UNAUTHORIZED,
                "token_missing",
                "an Authorization header with a Bearer token is required",
            );
        }
    };
    let paper = paper_repo(&state);
    let session_id = PaperSessionId::new(id.clone());
    match paper.get_session(&session_id).await {
        Ok(Some(_)) => {}
        Ok(None) => return unknown_session(&id),
        Err(error) => return read_error(&PaperReadError::Data(error)),
    }
    let (frames, receiver) = mpsc::unbounded_channel();
    tokio::spawn(session_event_pump(
        state, session_id, token_hash, after, frames,
    ));
    Sse::new(ReceiverStream { receiver })
        .keep_alive(KeepAlive::new().interval(SESSION_KEEP_ALIVE))
        .into_response()
}

/// The `Last-Event-ID` header: absent replays everything; a non-negative
/// integer replays `seq > n` only; anything else is 400 `validation`.
#[allow(clippy::result_large_err)] // `Response` is the error carrier by design (the `ops::parse_body` precedent)
fn parse_last_event_id(headers: &HeaderMap) -> Result<Option<i64>, Response> {
    match headers.get("last-event-id") {
        None => Ok(None),
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|raw| raw.trim().parse::<i64>().ok())
            .filter(|sequence| *sequence >= 0)
        {
            Some(sequence) => Ok(Some(sequence)),
            None => Err(api_error(
                StatusCode::BAD_REQUEST,
                "validation",
                "Last-Event-ID must be a non-negative integer sequence",
            )),
        },
    }
}

/// The per-connection event pump: replay `seq > after`, then poll the
/// repository for new rows, re-checking the token each pass.
async fn session_event_pump(
    state: Arc<ServerState>,
    session_id: PaperSessionId,
    token_hash: String,
    after: Option<i64>,
    frames: mpsc::UnboundedSender<WireEvent>,
) {
    let mut last = after.unwrap_or(i64::MIN);
    loop {
        if frames.is_closed() {
            return;
        }
        let tokens = SqliteClientTokenRepo::new(state.db.pool().clone());
        match tokens.find_by_hash(&token_hash).await {
            Ok(Some(row)) if row.revoked_at.is_none() => {}
            Ok(_) => {
                let _ = frames.send(
                    WireEvent::default().event("error").data(
                        json!({
                            "code": "token_refused",
                            "message": "the presented token has been revoked or is no longer recognised",
                        })
                        .to_string(),
                    ),
                );
                return;
            }
            // A store hiccup is not a revocation: keep the stream alive.
            Err(error) => state.log.write(format!(
                "pulse serve: paper events: token re-check failed for session {session_id}: {error}"
            )),
        }
        let paper = paper_repo(&state);
        match paper.events(&session_id).await {
            Ok(events) => {
                for event in events {
                    if event.seq() > last {
                        last = event.seq();
                        let frame = WireEvent::default()
                            .event("paper")
                            .id(event.seq().to_string())
                            .data(
                                serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned()),
                            );
                        if frames.send(frame).is_err() {
                            return;
                        }
                    }
                }
            }
            Err(error) => state.log.write(format!(
                "pulse serve: paper events: read failed for session {session_id}: {error}"
            )),
        }
        tokio::time::sleep(Duration::from_millis(SESSION_EVENT_POLL_MS)).await;
    }
}

/// The SSE stream half: the pump's frames, in order.
struct ReceiverStream {
    receiver: mpsc::UnboundedReceiver<WireEvent>,
}

impl Stream for ReceiverStream {
    type Item = Result<WireEvent, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(frame)) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

// ---------------------------------------------------------------------------
// Shared plumbing
// ---------------------------------------------------------------------------

/// The session repository over the server's own pool and data dir.
fn paper_repo(state: &Arc<ServerState>) -> SqlitePaperSessionRepo<SystemClock> {
    SqlitePaperSessionRepo::new(
        state.db.pool().clone(),
        CandleStore::with_base_dir(state.data_dir.clone()),
    )
}

/// The installed control handle, when a runtime can still receive commands.
fn control_of(state: &Arc<ServerState>) -> Option<PaperControl> {
    state
        .paper_control
        .clone()
        .filter(PaperControl::is_available)
}

/// The caller's token label, as the auth middleware put it in the extensions.
fn label_of(req: &Request) -> Option<AuthenticatedLabel> {
    req.extensions().get::<AuthenticatedLabel>().cloned()
}

/// The stop actor a token label names.
fn stop_actor(label: &AuthenticatedLabel) -> Option<StopActor> {
    NonEmptyLabel::try_new(&label.0)
        .ok()
        .map(|label| StopActor::Token { label })
}

/// A `{code, message}` error body.
fn api_error(status: StatusCode, code: &'static str, message: impl Into<String>) -> Response {
    (
        status,
        axum::Json(json!({ "code": code, "message": message.into() })),
    )
        .into_response()
}

/// The 500 every "cannot happen" seam answers with, named not guessed.
fn internal(message: &str) -> Response {
    api_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
}

/// The 503 the control routes answer when no runtime is installed.
fn runtime_unavailable() -> Response {
    api_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "runtime_unavailable",
        "no paper runtime is running on this server",
    )
}

/// Map a control-handle failure onto the API's error surface.
fn control_error(error: &PaperControlError) -> Response {
    match error {
        PaperControlError::RuntimeUnavailable | PaperControlError::Timeout => runtime_unavailable(),
        PaperControlError::Runtime(error) => {
            internal(&format!("the paper runtime refused: {error}"))
        }
    }
}

/// Map a read-model failure onto the API's error surface.
fn read_error(error: &PaperReadError) -> Response {
    internal(&format!("paper read failed: {error}"))
}

/// The 404 both single-session reads answer.
fn unknown_session(id: &str) -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "unknown_session",
        format!("no paper session with id {id}"),
    )
}

/// The gate's typed refusals as 422 bodies (spec §5).
fn promotion_refusal(refused: &PromotionRefused) -> Response {
    match refused {
        PromotionRefused::Uncertified => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "uncertified",
            refused.to_string(),
        ),
        PromotionRefused::CertificationUnreadable => api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "certification_unreadable",
            refused.to_string(),
        ),
        PromotionRefused::CertifiedUnderOtherEngine {
            certified_under,
            current,
        } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            axum::Json(json!({
                "code": "certified_under_other_engine",
                "message": refused.to_string(),
                "certified_under": certified_under.as_str(),
                "current": current.as_str(),
            })),
        )
            .into_response(),
    }
}

/// Validate the override body into the use case's request; an empty reason is
/// the typed `empty_reason` refusal, and a bad pair or timeframe is
/// `validation`.
#[allow(clippy::result_large_err)] // `Response` is the error carrier by design (the `ops::parse_body` precedent)
fn override_request(body: OverrideBody) -> Result<OverrideRequest, Response> {
    let reason = NonEmptyReason::try_new(&body.reason).map_err(|_| {
        api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_reason",
            "the override reason must not be empty or whitespace-only",
        )
    })?;
    let pair = Pair::parse(body.pair).map_err(|error| {
        api_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation",
            format!("invalid pair: {error}"),
        )
    })?;
    // The runtime attaches only a pair the exchange adapter knows (its symbol
    // filters); any other would persist a session that can never attach.
    BinanceAdapter::new()
        .symbol_filters(&pair)
        .map_err(|error| {
            api_error(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation",
                format!("unsupported pair: {error}"),
            )
        })?;
    Ok(OverrideRequest {
        reason,
        pair,
        primary_timeframe: body.primary_timeframe,
        htf_timeframe: body.htf_timeframe,
        uses_d1: body.uses_d1,
    })
}
