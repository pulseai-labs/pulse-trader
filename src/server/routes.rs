//! The command routes (r3.s3.w2, D4): the five `ops/` operation spawns, the
//! operation status and SSE stream routes, and the plain REST command routes —
//! one per non-operation bus command, same kebab path as the `BUS_COMMANDS`
//! entry.
//!
//! Every route is mounted through [`super::mount_scoped`] with [`Scope::App`],
//! so each carries the full outer stack w1 built (version outermost, request
//! log, auth innermost) and an `agent` token is a 403 `scope_refused` with an
//! audit row on every one of them. A core's `BusError` is returned as HTTP 422
//! with the `BusError` JSON as the body — the same serde shape the Tauri bus
//! emits today, so the w5 proxy forwards without translation.
//!
//! Bodies are the commands' own DTOs (their camelCase serde shapes); a body
//! that does not deserialize is a 422 `validation` `BusError` naming the parse
//! failure. The AC-1 plain routes (compose-cancel, get-backtest-run) land with
//! the operations that need them; the remaining plain routes are AC-2's.

use std::future::Future;
use std::sync::Arc;

use axum::extract::{Path, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::Value;

use super::ServerState;
use super::auth::Scope;
use super::ops;
use super::paper::{api_error, internal, label_of};
use crate::tauri::certification::certification_records_core;
use crate::tauri::power_review::power_reviews_core;
use crate::tauri::streak_card::streak_card_core;
use crate::tauri::trader_writer;
use crate::tauri::walk_forward::{get_backtest_run_core, get_walk_forward_run_core};

// ---------------------------------------------------------------------------
// The mount
// ---------------------------------------------------------------------------

/// Mount every command route onto `router`. Static segments beat `{op_id}` in
/// matchit, and the five static `ops/...` POSTs coexist with the two `{op_id}`
/// GETs because their methods differ.
pub(crate) fn mount_all(router: axum::Router, state: &Arc<ServerState>) -> axum::Router {
    let router = router
        // The five operation spawns (202 before any work).
        .mount_post(state, "/api/v1/ops/compose-strategy", |state, req| {
            Box::pin(ops::op_compose_strategy(state, req))
        })
        .mount_post(state, "/api/v1/ops/coach-turn", |state, req| {
            Box::pin(ops::op_coach_turn(state, req))
        })
        .mount_post(state, "/api/v1/ops/run-backtest-version", |state, req| {
            Box::pin(ops::op_run_backtest(state, req))
        })
        .mount_post(
            state,
            "/api/v1/ops/run-walk-forward-version",
            |state, req| Box::pin(ops::op_run_walk_forward(state, req)),
        )
        .mount_post(state, "/api/v1/ops/start-demo-stream", |state, req| {
            Box::pin(ops::op_start_demo_stream(state, req))
        })
        // Operation status + SSE stream.
        .mount_get_path(state, "/api/v1/ops/{op_id}", |state, op_id| {
            let response = ops::op_status(&state, &op_id);
            Box::pin(std::future::ready(response))
        })
        .mount_get_path_request(state, "/api/v1/ops/{op_id}/events", |state, op_id, req| {
            let response = ops::op_events(&state, &op_id, &req);
            Box::pin(std::future::ready(response))
        })
        // The plain routes — one per non-operation `BUS_COMMANDS` entry, in the
        // list's order: the three GET reads, then the six POST commands.
        .mount_get(state, "/api/v1/shell-info", |state| {
            Box::pin(plain_shell_info(state))
        })
        .mount_get(state, "/api/v1/credential-status", |_| {
            Box::pin(std::future::ready(plain_credential_status()))
        })
        .mount_get(state, "/api/v1/library-overview", |state| {
            Box::pin(plain_library_overview(state))
        })
        .mount_post_json(state, "/api/v1/compose-cancel", |state, body| {
            let response = plain_compose_cancel(&state, &body);
            Box::pin(std::future::ready(response))
        })
        .mount_post_json(state, "/api/v1/coach-decide", |state, request| {
            Box::pin(plain_coach_decide(state, request))
        })
        .mount_post_json(state, "/api/v1/compare-child-run", |state, request| {
            Box::pin(plain_compare_child_run(state, request))
        })
        .mount_post_json(state, "/api/v1/get-walk-forward-run", |state, request| {
            Box::pin(plain_get_walk_forward_run(state, request))
        })
        .mount_post_json(state, "/api/v1/get-backtest-run", |state, request| {
            Box::pin(plain_get_backtest_run(state, request))
        })
        // r4.s1.w5 (spec A4/C5): the app's full certification-record read.
        .mount_post_json(state, "/api/v1/certification-records", |state, request| {
            Box::pin(plain_certification_records(state, request))
        })
        // r5.s5.w3 (PDR-0040, ADR-0032): the Library's power-review read — count,
        // pass chance, bar, allowed; no outcome.
        .mount_post_json(state, "/api/v1/power-reviews", |state, request| {
            Box::pin(plain_power_reviews(state, request))
        })
        // r5.s4.w1 (PDR-0035 Q4): the streak and drawdown card's read.
        .mount_post_json(state, "/api/v1/streak-card", |state, request| {
            Box::pin(plain_streak_card(state, request))
        })
        // r5.s2.w1 (PDR-0036): the Chart tab's read — a window of candles and,
        // for a run, its trades and its rule's overlays.
        .mount_post_json(state, "/api/v1/chart", |state, request| {
            Box::pin(plain_chart_data(state, request))
        })
        // r5.s3.w1 (PDR-0037, ADR-0031): the example-trade marks — write one,
        // list them, withdraw one. All three are app-scope: the trader's own
        // surface, named by its token's label. Agents reach the SAME use case
        // through `/mcp` under an agent token, never through here.
        .mount_post(state, "/api/v1/marks", |state, req| {
            Box::pin(plain_write_mark(state, req))
        })
        .mount_post(state, "/api/v1/marks/list", |state, req| {
            Box::pin(plain_list_marks(state, req))
        })
        .mount_post(state, "/api/v1/marks/withdraw", |state, req| {
            Box::pin(plain_withdraw_mark(state, req))
        })
        .mount_post_json(
            state,
            "/api/v1/bus-selftest-failure",
            |_state, _body: Value| Box::pin(std::future::ready(plain_bus_selftest_failure())),
        );
    // r3.s4.w4: the paper routes (promote, stop, stop-all, the reads, the
    // shadow check and the session SSE stream) — all `Scope::App`.
    super::paper::mount(router, state)
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

/// The boxed-response future every mount deals in. A generic projection (a
/// closure whose output is an associated type) cannot satisfy axum's `Handler`
/// bound — the compiler cannot normalize the projection at the call site — so
/// the mount boundary boxes the future once and `Handler` sees a concrete
/// `Pin<Box<dyn Future + Send>>`.
type BoxedFut = std::pin::Pin<Box<dyn Future<Output = Response> + Send>>;

/// One mount, one closure shape. Each helper adapts a handler that takes what
/// it actually needs (the state plus its extractors) into a state-capturing
/// method router, then hands it to `mount_scoped` so the outer stack rides on
/// every route identically. `pub(crate)` because `server::paper` mounts the
/// paper routes through the same helpers.
pub(crate) trait MountExt: Sized {
    fn mount_get<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>) -> BoxedFut + Clone + Send + Sync + 'static;

    fn mount_post<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>, Request) -> BoxedFut + Clone + Send + Sync + 'static;

    fn mount_post_json<T, F>(
        self,
        state: &Arc<ServerState>,
        path: &'static str,
        handler: F,
    ) -> Self
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(Arc<ServerState>, T) -> BoxedFut + Clone + Send + Sync + 'static;

    fn mount_get_path<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>, String) -> BoxedFut + Clone + Send + Sync + 'static;

    fn mount_get_path_request<F>(
        self,
        state: &Arc<ServerState>,
        path: &'static str,
        handler: F,
    ) -> Self
    where
        F: Fn(Arc<ServerState>, String, Request) -> BoxedFut + Clone + Send + Sync + 'static;

    fn mount_post_path<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>, String, Request) -> BoxedFut + Clone + Send + Sync + 'static;
}

use serde::de::DeserializeOwned;

impl MountExt for axum::Router {
    fn mount_get<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>) -> BoxedFut + Clone + Send + Sync + 'static,
    {
        let st = state.clone();
        let method = get(move || handler(st.clone()));
        super::mount_scoped(self, state, Scope::App, path, method)
    }

    fn mount_post<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>, Request) -> BoxedFut + Clone + Send + Sync + 'static,
    {
        let st = state.clone();
        let method = post(move |req: Request| handler(st.clone(), req));
        super::mount_scoped(self, state, Scope::App, path, method)
    }

    fn mount_post_json<T, F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        T: DeserializeOwned + Send + 'static,
        F: Fn(Arc<ServerState>, T) -> BoxedFut + Clone + Send + Sync + 'static,
    {
        self.mount_post(state, path, move |st: Arc<ServerState>, req: Request| {
            let handler = handler.clone();
            Box::pin(async move {
                match ops::parse_body::<T>(req).await {
                    Ok(body) => handler(st, body).await,
                    Err(response) => response,
                }
            })
        })
    }

    fn mount_get_path<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>, String) -> BoxedFut + Clone + Send + Sync + 'static,
    {
        let st = state.clone();
        let method = get(move |Path(op_id): Path<String>| handler(st.clone(), op_id));
        super::mount_scoped(self, state, Scope::App, path, method)
    }

    fn mount_get_path_request<F>(
        self,
        state: &Arc<ServerState>,
        path: &'static str,
        handler: F,
    ) -> Self
    where
        F: Fn(Arc<ServerState>, String, Request) -> BoxedFut + Clone + Send + Sync + 'static,
    {
        let st = state.clone();
        let method =
            get(move |Path(op_id): Path<String>, req: Request| handler(st.clone(), op_id, req));
        super::mount_scoped(self, state, Scope::App, path, method)
    }

    fn mount_post_path<F>(self, state: &Arc<ServerState>, path: &'static str, handler: F) -> Self
    where
        F: Fn(Arc<ServerState>, String, Request) -> BoxedFut + Clone + Send + Sync + 'static,
    {
        let st = state.clone();
        let method = post(move |Path(id): Path<String>, req: Request| handler(st.clone(), id, req));
        super::mount_scoped(self, state, Scope::App, path, method)
    }
}

// ---------------------------------------------------------------------------
// The plain routes — thin adapters over the cores, exactly as the Tauri
// wrappers are. A core's `BusError` is the 422 body; a success DTO is the 200
// body with its own serde shape.
// ---------------------------------------------------------------------------

/// `POST /api/v1/compose-cancel` — `{"runId": "..."}` → the bool
/// `cancel_compose_run` answers: true when a live run was under that id.
fn plain_compose_cancel(state: &Arc<ServerState>, body: &ComposeCancelBody) -> Response {
    let cancelled = state.desktop.cancel_compose_run(&body.run_id);
    (StatusCode::OK, axum::Json(cancelled)).into_response()
}

/// The compose-cancel body: the command's `run_id` argument, camelCase on the
/// wire like every other DTO (`runId`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ComposeCancelBody {
    run_id: String,
}

/// `GET /api/v1/shell-info` — the `ShellInfo` the `shell_info` command returns.
async fn plain_shell_info(state: Arc<ServerState>) -> Response {
    match crate::tauri::commands::shell_info_core(&state.desktop).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `GET /api/v1/credential-status` — the server's own status, one of the five
/// wire values, computed the same way the command computes it and never
/// carrying a key.
fn plain_credential_status() -> Response {
    let status = crate::adapters::secrets::llm_credential_status();
    (StatusCode::OK, axum::Json(status)).into_response()
}

/// `GET /api/v1/library-overview` — the persisted-library read.
async fn plain_library_overview(state: Arc<ServerState>) -> Response {
    match crate::tauri::commands::library_overview_core(&state.desktop).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/coach-decide` — modify/reject/accept one recorded proposal;
/// no credential and no provider (an accept re-runs persisted inputs).
async fn plain_coach_decide(
    state: Arc<ServerState>,
    request: crate::tauri::CoachDecisionRequestDto,
) -> Response {
    match crate::tauri::coach::coach_decide_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/compare-child-run` — one child run beside its parent's latest.
async fn plain_compare_child_run(
    state: Arc<ServerState>,
    request: crate::tauri::CompareChildRunRequest,
) -> Response {
    match crate::tauri::commands::compare_child_run_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/get-walk-forward-run` — one persisted walk-forward run.
async fn plain_get_walk_forward_run(
    state: Arc<ServerState>,
    request: crate::tauri::GetWalkForwardRunRequest,
) -> Response {
    match get_walk_forward_run_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/certification-records` — a version's certification records in
/// full, holdout numbers included (r4.s1.w5, spec A4/C5), straight through the
/// core. Mounted `Scope::App`: an `agent` token is a 403 `scope_refused` with
/// an audit row, exactly like every other route on this table.
async fn plain_certification_records(
    state: Arc<ServerState>,
    request: crate::tauri::CertificationRecordsRequest,
) -> Response {
    match certification_records_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/power-reviews` — the latest power review per pair under the open
/// freeze, straight through the core (r5.s5.w3, PDR-0040, ADR-0032). Mounted
/// `Scope::App`: an `agent` token is a 403 `scope_refused` with an audit row,
/// exactly like every other route on this table.
async fn plain_power_reviews(
    state: Arc<ServerState>,
    request: crate::tauri::PowerReviewsRequest,
) -> Response {
    match power_reviews_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/streak-card` — one version's streak and drawdown card,
/// straight through the core (r5.s4.w1, PDR-0035 Q4). Mounted `Scope::App`: an
/// `agent` token is a 403 `scope_refused` with an audit row, exactly like every
/// other route on this table.
async fn plain_streak_card(
    state: Arc<ServerState>,
    request: crate::tauri::StreakCardRequest,
) -> Response {
    match streak_card_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/chart` — the Chart tab's read: a window of candles (capped, cut
/// at an open freeze's holdout start) and, for a named run, its trades and its
/// rule's overlays, straight through the core (r5.s2.w1, PDR-0036). Mounted
/// `Scope::App`: an `agent` token is a 403 `scope_refused` with an audit row,
/// exactly like every other route on this table.
async fn plain_chart_data(
    state: Arc<ServerState>,
    request: crate::tauri::ChartDataRequest,
) -> Response {
    match crate::tauri::chart::chart_data_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/get-backtest-run` — the persisted-run read, straight through
/// the core.
async fn plain_get_backtest_run(
    state: Arc<ServerState>,
    request: crate::tauri::GetBacktestRunRequest,
) -> Response {
    match get_backtest_run_core(&state.desktop, request).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(err) => ops::bus_422(&err),
    }
}

/// `POST /api/v1/marks` — mark one candle as an example trade, as the TRADER.
///
/// The writer is the authenticated token's LABEL, taken off the request
/// extensions and never off the body (ADR-0026, G5): a caller cannot say who it
/// is. The body cannot carry a writer at all — `WriteMarkRequest` has no such
/// member, and the type is deliberately not `deny_unknown_fields`, so an extra
/// `writer` key is ignored rather than honoured.
async fn plain_write_mark(state: Arc<ServerState>, req: Request) -> Response {
    let Some(label) = label_of(&req) else {
        return internal("the authenticated label is missing from the request");
    };
    let body: crate::tauri::WriteMarkRequest = match ops::parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match crate::tauri::marks::write_mark_core(&state.desktop, trader_writer(&label.0), body).await
    {
        Ok(dto) => (StatusCode::CREATED, axum::Json(dto)).into_response(),
        Err(error) => mark_error(&error),
    }
}

/// `POST /api/v1/marks/list` — the example trades that still stand.
async fn plain_list_marks(state: Arc<ServerState>, req: Request) -> Response {
    let body: crate::tauri::ListMarksRequest = match ops::parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match crate::tauri::marks::list_marks_core(&state.desktop, body).await {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(error) => mark_error(&error),
    }
}

/// `POST /api/v1/marks/withdraw` — take one example trade back, as the TRADER.
async fn plain_withdraw_mark(state: Arc<ServerState>, req: Request) -> Response {
    let Some(label) = label_of(&req) else {
        return internal("the authenticated label is missing from the request");
    };
    let body: crate::tauri::WithdrawMarkRequest = match ops::parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    match crate::tauri::marks::withdraw_mark_core(&state.desktop, trader_writer(&label.0), body)
        .await
    {
        Ok(dto) => (StatusCode::OK, axum::Json(dto)).into_response(),
        Err(error) => mark_error(&error),
    }
}

/// The mark use case's typed refusal, as the route's own status.
///
/// `MarkError::code` is the ONE spelling of each refusal — `holdout`,
/// `candle_not_found`, `not_found`, `already_withdrawn`, `not_writer`,
/// `validation` — and it is the same string the MCP tools put in their error
/// bodies (`client::route_error` folds the four correctable ones onto
/// `Validation`, keeping this message verbatim). A store failure answers `data`,
/// as every other route's does.
fn mark_error(error: &crate::domain::MarkError) -> Response {
    api_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        error.code(),
        error.to_string(),
    )
}

/// `POST /api/v1/bus-selftest-failure` — ALWAYS a 422: the same real domain
/// error the command maps through the real `From` impl (not a synthetic
/// `BusError`), so the proxy exercises the mapping the frontend depends on.
fn plain_bus_selftest_failure() -> Response {
    let err: crate::tauri::error::BusError =
        crate::domain::DataError::Parse("deliberate bus self-test failure (r1.s1.w1)".to_owned())
            .into();
    ops::bus_422(&err)
}
