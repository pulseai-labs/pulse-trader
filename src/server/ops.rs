//! Server-owned operations (r3.s3.w2, D9 as amended by R2): the operation
//! registry, the per-operation SSE buffer, the buffer-backed
//! [`EventSink`](crate::tauri::events::EventSink), the sweep, and the five
//! `ops/` spawns.
//!
//! The contract this module implements is the spec's, in its own words:
//!
//! - A POST to an `ops/...` route answers **202 `{op_id}` before any work
//!   runs**; the operation is owned by the server, not by any connection. A
//!   pre-flight refusal may answer 422 instead — this item's recorded reading
//!   refuses single-flight duplicates as the operation's TERMINAL ERROR, after
//!   the 202, so a busy answer never exists at POST time (the approved plan
//!   reading; no release-then-retake latch race).
//! - Every event is kept in an **in-memory per-operation buffer until the
//!   operation is terminal, plus 10 minutes**; after that the progress events
//!   are dropped. The **terminal event is kept for 24 hours**. The sweep
//!   interval and both retention windows are injectable
//!   ([`SweepConfig`]); tests use milliseconds.
//! - `GET /api/v1/ops/{op_id}` returns the status envelope or **404
//!   `op_unknown`** for an id the server never issued or has expired.
//! - `GET /api/v1/ops/{op_id}/events` streams **`event: bus`** frames for a
//!   streaming command's `BusEvent`s (**`id:` = the `BusEvent` `seq`**), one
//!   **`event: started`** at seq 0 for a non-streaming command, and one
//!   terminal frame — **`event: result`** (the command's success DTO) or
//!   **`event: error`** (the `BusError`) — at the next seq after the last. A
//!   `Last-Event-ID: <n>` header replays only events with `seq > n`, then
//!   continues live; a client connecting after the terminal gets the buffered
//!   events after its Last-Event-ID and the terminal, then the stream closes.
//!
//! A send into the buffer **never fails because a client left**
//! ([`OpEventSink`]) — the cores' send-error-means-cancelled rule stays
//! reachable only through compose-cancel, which is the spec's recorded
//! control: a disconnect mid-run must NOT cancel an operation.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::sse::Event as WireEvent;
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use serde::Serialize;
use serde_json::{Value, json};

use super::ServerState;
use crate::adapters::llm::coach_transport::{coach_config, coach_provider};
use crate::adapters::llm::openai_compat::OpenAiCompatProvider;
use crate::adapters::secrets::resolve_llm_api_key;
use crate::agent::config::{
    load_coach_prompt_from, load_composer_prompt, load_llm_transport, load_price_table,
    prompt_override_dir,
};
use crate::cli::compose::{ComposeWiring, compose_config};
use crate::domain::Redactor;
use crate::tauri::coach::{CoachTurnDeps, coach_turn_core};
use crate::tauri::commands::{
    ComposeDeps, compose_strategy_core, demo_stream_core, run_backtest_version_core,
};
use crate::tauri::error::{BusError, BusErrorCode};
use crate::tauri::events::{BusEvent, EventSink, RunId};
use crate::tauri::walk_forward::run_walk_forward_version_core;
use crate::tauri::{
    CoachTurnRequestDto, ComposeResult, DesktopState, StreamOutcome, WalkForwardRunRequest,
};

// ---------------------------------------------------------------------------
// Frames, phases, records
// ---------------------------------------------------------------------------

/// One SSE frame as buffered and delivered: its `seq` (the wire `id:`), its
/// event name and its JSON data. `pub(crate)` because it travels through
/// `Subscription` (which `routes` consumes); nothing here is public surface.
#[derive(Debug, Clone)]
pub(crate) struct SseRecord {
    seq: u32,
    name: &'static str,
    data: String,
}

impl SseRecord {
    fn to_wire(&self) -> WireEvent {
        WireEvent::default()
            .event(self.name)
            .id(self.seq.to_string())
            .data(self.data.clone())
    }
}

/// Where an operation is on its way to its terminal frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Running,
    Done,
    Failed,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }
}

/// Everything the server knows about one operation.
struct OpRecord {
    command: &'static str,
    phase: Phase,
    /// The success DTO's JSON — present exactly when `phase == Done`.
    result: Option<Value>,
    /// The `BusError` — present exactly when `phase == Failed`.
    error: Option<BusError>,
    /// The buffered non-terminal frames, in seq order. Dropped by the sweep one
    /// progress-window after the terminal.
    events: Vec<SseRecord>,
    /// The single terminal frame, kept a full terminal-window.
    terminal: Option<SseRecord>,
    terminal_at: Option<Instant>,
    /// Live SSE connections. An unbounded channel: frames already accumulate in
    /// the buffer under the same retention bound, so a slow reader cannot grow
    /// memory beyond what the buffer itself may hold, and a dropped reader is
    /// just a dead sender.
    subscribers: Vec<tokio::sync::mpsc::UnboundedSender<SseRecord>>,
}

/// Retention knobs. The defaults are the spec's; tests inject milliseconds.
#[derive(Debug, Clone, Copy)]
pub struct SweepConfig {
    /// How often the sweep task looks for expired records.
    pub interval: Duration,
    /// Terminal + this: the progress events are dropped (the terminal stays).
    pub progress_window: Duration,
    /// Terminal + this: the whole record is removed (`op_unknown` afterwards).
    pub terminal_window: Duration,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(60),
            progress_window: Duration::from_secs(600),
            terminal_window: Duration::from_hours(24),
        }
    }
}

struct RegistryInner {
    ops: std::sync::Mutex<HashMap<String, OpRecord>>,
    sweep: SweepConfig,
}

/// Lock the map, adopting a poisoned guard. A panic IN a task must not turn
/// into a permanently poisoned registry for every later request — the same
/// reasoning `DesktopState::held_operations` documents for its latch map
/// (`r1.s4.w3`), and the same established `PoisonError::into_inner` remedy.
fn lock_ops(
    ops: &std::sync::Mutex<HashMap<String, OpRecord>>,
) -> std::sync::MutexGuard<'_, HashMap<String, OpRecord>> {
    ops.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The operation registry: issuance, buffers, subscribers and the sweep. One
/// per server (it lives on `ServerState`), which is what makes single-flight
/// and cancel-by-id work across HTTP connections.
#[derive(Clone)]
pub struct OpRegistry {
    inner: Arc<RegistryInner>,
}

/// The subscription view `op_events` streams from.
pub struct Subscription {
    /// Frames with `seq > after`, in order; ends with the terminal frame when
    /// one already exists.
    pub(crate) backlog: Vec<SseRecord>,
    /// The live channel — `None` when the operation is already terminal (the
    /// stream ends after the backlog).
    pub(crate) live: Option<tokio::sync::mpsc::UnboundedReceiver<SseRecord>>,
}

/// The only subscription failure: no record under that id (never issued, or
/// swept after its terminal window).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownOp;

impl OpRegistry {
    /// A fresh registry, with its sweep task running. The task holds only a
    /// `Weak` to the map, so it dies with the registry — no leaked ticker per
    /// test server.
    #[must_use]
    pub fn new(sweep: SweepConfig) -> Self {
        let inner = Arc::new(RegistryInner {
            ops: std::sync::Mutex::new(HashMap::new()),
            sweep,
        });
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(sweep.interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let Some(live) = weak.upgrade() else { break };
                sweep_once(&live);
            }
        });
        Self { inner }
    }

    /// Record a freshly issued operation as `running`. Called BEFORE the 202
    /// goes out, so the id is already status-answerable and (for compose)
    /// cancellable.
    pub(crate) fn issue(&self, command: &'static str, op_id: &str) {
        lock_ops(&self.inner.ops).insert(
            op_id.to_owned(),
            OpRecord {
                command,
                phase: Phase::Running,
                result: None,
                error: None,
                events: Vec::new(),
                terminal: None,
                terminal_at: None,
                subscribers: Vec::new(),
            },
        );
    }

    /// Buffer one non-terminal frame and fan it out to the live subscribers.
    pub(crate) fn push_event(&self, op_id: &str, record: &SseRecord) {
        let mut ops = lock_ops(&self.inner.ops);
        let Some(record_state) = ops.get_mut(op_id) else {
            return;
        };
        // No frame arrives after the terminal in any code path; guard anyway so
        // a future bug cannot resurrect a swept buffer.
        if record_state.terminal.is_some() {
            return;
        }
        record_state.events.push(record.clone());
        fan_out(record_state, record);
    }

    /// Land the terminal frame, flip the phase and close every live stream.
    /// The terminal's `seq` is the next after the last buffered event (0 when
    /// the operation emitted nothing).
    pub(crate) fn finish(&self, op_id: &str, outcome: Result<Value, BusError>) {
        let mut ops = lock_ops(&self.inner.ops);
        let Some(record_state) = ops.get_mut(op_id) else {
            return;
        };
        let seq = record_state.events.last().map_or(0, |last| last.seq + 1);
        let (phase, result, error, name) = match outcome {
            Ok(dto) => (Phase::Done, Some(dto), None, "result"),
            Err(err) => (Phase::Failed, None, Some(err), "error"),
        };
        // A terminal's data is the success DTO or the BusError; serialization
        // of either cannot fail (no maps with non-string keys), but a server
        // task must never panic on a frame, so the fallback is `{}`.
        let data = match (&result, &error) {
            (Some(dto), _) => serde_json::to_string(dto).unwrap_or_else(|_| "{}".to_owned()),
            (None, Some(err)) => serde_json::to_string(err).unwrap_or_else(|_| "{}".to_owned()),
            (None, None) => "{}".to_owned(),
        };
        let terminal = SseRecord { seq, name, data };
        record_state.phase = phase;
        record_state.result = result;
        record_state.error = error;
        record_state.terminal = Some(terminal.clone());
        record_state.terminal_at = Some(Instant::now());
        fan_out(record_state, &terminal);
        // Dropping the senders ends every live stream AFTER the terminal frame
        // — the receiver drains, sees `None`, and the response completes.
        record_state.subscribers.clear();
    }

    /// Snapshot a subscription: the backlog after `after`, plus the live channel
    /// when the operation is still running. One lock hold — no terminal can
    /// slip between the snapshot and the subscription.
    pub(crate) fn subscribe(
        &self,
        op_id: &str,
        after: Option<u32>,
    ) -> Result<Subscription, UnknownOp> {
        let mut ops = lock_ops(&self.inner.ops);
        let Some(record_state) = ops.get_mut(op_id) else {
            return Err(UnknownOp);
        };
        let unseen = |seq: u32| after.is_none_or(|n| seq > n);
        let mut backlog: Vec<SseRecord> = record_state
            .events
            .iter()
            .filter(|event| unseen(event.seq))
            .cloned()
            .collect();
        if let Some(terminal) = &record_state.terminal {
            if unseen(terminal.seq) {
                backlog.push(terminal.clone());
            }
            return Ok(Subscription {
                backlog,
                live: None,
            });
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        record_state.subscribers.push(tx);
        Ok(Subscription {
            backlog,
            live: Some(rx),
        })
    }

    /// The status envelope for `GET /ops/{id}`, or `None` (`op_unknown`).
    pub(crate) fn status(&self, op_id: &str) -> Option<Value> {
        let ops = lock_ops(&self.inner.ops);
        let record_state = ops.get(op_id)?;
        Some(json!({
            "op_id": op_id,
            "command": record_state.command,
            "state": record_state.phase.as_str(),
            "result": record_state.result,
            "error": record_state.error,
        }))
    }
}

/// Send one frame to every live subscriber, dropping the dead ones.
fn fan_out(record_state: &mut OpRecord, record: &SseRecord) {
    record_state
        .subscribers
        .retain(|tx| tx.send(record.clone()).is_ok());
}

/// One sweep pass: drop progress events a progress-window past the terminal,
/// drop whole records a terminal-window past it. `std::sync::Mutex` is never
/// held across an `.await` — the tick awaits OUTSIDE the lock.
fn sweep_once(inner: &RegistryInner) {
    let mut ops = lock_ops(&inner.ops);
    let expired: Vec<String> = ops
        .iter()
        .filter_map(|(id, record_state)| {
            let elapsed = record_state.terminal_at?.elapsed();
            if elapsed >= inner.sweep.terminal_window {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();
    for id in expired {
        ops.remove(&id);
    }
    for record_state in ops.values_mut() {
        let Some(terminal_at) = record_state.terminal_at else {
            continue;
        };
        if terminal_at.elapsed() >= inner.sweep.progress_window {
            record_state.events.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// The buffer-backed sink
// ---------------------------------------------------------------------------

/// The [`EventSink`] a spawned operation drives. Every `BusEvent` the core
/// emits lands in the operation's buffer and fans out to the live SSE
/// subscribers; the send NEVER fails because a client left, so the core's
/// send-error-means-cancelled rule is reachable only through compose-cancel —
/// the disconnect-does-not-cancel control the spec pins.
pub struct OpEventSink {
    registry: OpRegistry,
    op_id: String,
}

impl EventSink for OpEventSink {
    fn send_event(&self, event: BusEvent) -> Result<(), BusError> {
        let data = serde_json::to_string(&event).map_err(|e| {
            BusError::new(BusErrorCode::Internal, format!("serialize bus event: {e}"))
        })?;
        self.registry.push_event(
            &self.op_id,
            &SseRecord {
                seq: event.seq,
                name: "bus",
                data,
            },
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The SSE stream and its handlers
// ---------------------------------------------------------------------------

/// The per-connection stream: backlog frames first, then live frames until the
/// terminal (the registry drops the senders at `finish`, so the channel closes
/// right after it), then end-of-stream. The backlog is drained before the
/// receiver is polled, so ordering is exact with no lock held across an await.
struct OpEventStream {
    backlog: std::vec::IntoIter<SseRecord>,
    live: Option<tokio::sync::mpsc::UnboundedReceiver<SseRecord>>,
}

impl Stream for OpEventStream {
    type Item = Result<WireEvent, std::convert::Infallible>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if let Some(record) = self.backlog.next() {
            return std::task::Poll::Ready(Some(Ok(record.to_wire())));
        }
        let Some(rx) = self.live.as_mut() else {
            return std::task::Poll::Ready(None);
        };
        match rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(record)) => {
                std::task::Poll::Ready(Some(Ok(record.to_wire())))
            }
            // The registry dropped the senders at the terminal: done.
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// A 202 with the issued id — the ONLY success shape an `ops/` POST returns.
fn accepted(op_id: &str) -> Response {
    (StatusCode::ACCEPTED, axum::Json(json!({ "op_id": op_id }))).into_response()
}

/// Parse one JSON body out of the request. A body that cannot be read or does
/// not deserialize into the command's DTO is a 422 `validation` `BusError` —
/// never a 500, and never echoed as a log line (the request-log layer logs
/// method/path/status only).
#[allow(clippy::result_large_err)] // `Response` is the error carrier by design
pub(crate) async fn parse_body<T: serde::de::DeserializeOwned>(
    req: Request,
) -> Result<T, Response> {
    const MAX_BODY_BYTES: usize = 1024 * 1024;
    let bytes = axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|e| {
            bus_422(&BusError::new(
                BusErrorCode::Validation,
                format!("unreadable request body: {e}"),
            ))
        })?;
    serde_json::from_slice(&bytes).map_err(|e| {
        bus_422(&BusError::new(
            BusErrorCode::Validation,
            format!("invalid request body: {e}"),
        ))
    })
}

/// A core's `BusError` as HTTP: 422 with the `BusError` JSON as the body — the
/// same serde shape the Tauri bus emits today.
pub(crate) fn bus_422(err: &BusError) -> Response {
    (StatusCode::UNPROCESSABLE_ENTITY, axum::Json(err)).into_response()
}

/// The compose-operation factory — the spec's "narrowest test seam"
/// (coordinator-approved reading): `LlmProvider` is not object-safe (its
/// `chat` returns `impl Future`), so the seam cannot be a provider slot; it is
/// the whole compose-run body. The DEFAULT is the Tauri wrapper's exact body
/// executed inside the spawned task — config overlays, then
/// `resolve_llm_api_key()` exactly as the wrappers call it, the key consumed
/// by its only two consumers (redactor + provider constructor) and dropped
/// with the frame. Tests swap the runner to script the provider; no new
/// credential lookup exists anywhere (w3's territory).
pub type ComposeRunner = Arc<
    dyn Fn(ComposeRunCtx) -> Pin<Box<dyn Future<Output = Result<ComposeResult, BusError>> + Send>>
        + Send
        + Sync,
>;

/// Everything one compose run needs that is not the wiring itself: handed to
/// the runner inside the spawned task.
pub struct ComposeRunCtx {
    /// The server's desktop state — the ONE pool, the ONE latch map.
    pub desktop: Arc<DesktopState>,
    /// The run id; also the operation id (a compose's `op_id` IS its `RunId`).
    pub run_id: RunId,
    /// The natural-language target from the request body.
    pub nl_target: String,
    /// The buffer-backed sink the core streams its `BusEvent`s through.
    pub sink: Arc<dyn EventSink + Send + Sync>,
    /// The cancellation latch from `register_compose_run`.
    pub cancelled: Arc<AtomicBool>,
}

/// The production runner: the Tauri wrapper's body, unchanged, inside the
/// spawned task.
#[must_use]
pub fn default_compose_runner() -> ComposeRunner {
    Arc::new(|ctx: ComposeRunCtx| {
        let fut: Pin<Box<dyn Future<Output = Result<ComposeResult, BusError>> + Send>> =
            Box::pin(async move {
                // Config-driven overlays, loaded exactly as the CLI live arm
                // loads them (ADR-0014).
                let transport = load_llm_transport()
                    .map_err(|e| BusError::internal(format!("load llm transport: {e}")))?;
                let prices = load_price_table()
                    .map_err(|e| BusError::internal(format!("load price table: {e}")))?;
                let prompt = load_composer_prompt()
                    .map_err(|e| BusError::internal(format!("load composer prompt: {e}")))?;

                // The credential resolves inside the ring — w2's seam, the
                // wrapper's exact call. The value never appears in an argument,
                // a return value, an event, or a log line.
                let key = resolve_llm_api_key().map_err(BusError::from)?;
                let key_source = key.source();
                let redactor = Redactor::from_config(vec![key.expose().to_owned()]);
                let provider = match transport.base_url {
                    Some(base_url) => {
                        OpenAiCompatProvider::with_base_url(key.expose().to_owned(), base_url)
                    }
                    None => OpenAiCompatProvider::new(key.expose().to_owned()),
                };

                let deps = ComposeDeps {
                    wiring: ComposeWiring {
                        provider,
                        llm_repo: ctx.desktop.llm_call_repo(),
                        redactor,
                        prices,
                        clock: crate::adapters::clock::SystemClock,
                        prompt,
                        key_source: Some(key_source),
                        config: compose_config(transport.model.as_deref()),
                    },
                    strategy_repo: ctx.desktop.strategy_repo(),
                };
                compose_strategy_core(
                    &ctx.run_id,
                    deps,
                    &ctx.nl_target,
                    ctx.sink.as_ref(),
                    ctx.cancelled,
                )
                .await
            });
        fut
    })
}

// ---------------------------------------------------------------------------
// The five op spawns
// ---------------------------------------------------------------------------

/// `POST /api/v1/ops/compose-strategy` — `{"nlTarget": "..."}`.
///
/// Registers the compose latch BEFORE the 202 (the wrapper registers before
/// the first event; over HTTP the ack comes first, so the registration comes
/// before it), then spawns the runner. `finish_compose_run` runs on EVERY exit
/// path of the spawned task.
pub(crate) async fn op_compose_strategy(state: Arc<ServerState>, req: Request) -> Response {
    let body: Value = match parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(nl_target) = body["nl_target"].as_str().map(str::to_owned) else {
        return bus_422(&BusError::new(
            BusErrorCode::Validation,
            "compose-strategy requires {\"nl_target\": \"...\"}".to_owned(),
        ));
    };
    let run_id = RunId::new();
    let op_id = run_id.as_str().to_owned();
    let cancelled = state.desktop.register_compose_run(&run_id);
    state.ops.issue("compose-strategy", &op_id);

    let sink: Arc<dyn EventSink + Send + Sync> = Arc::new(OpEventSink {
        registry: state.ops.clone(),
        op_id: op_id.clone(),
    });
    let runner = state.compose_runner.clone();
    let desktop = state.desktop.clone();
    let registry = state.ops.clone();
    let task_op_id = op_id.clone();
    tokio::spawn(async move {
        let ctx = ComposeRunCtx {
            desktop: desktop.clone(),
            run_id: run_id.clone(),
            nl_target,
            sink,
            cancelled,
        };
        let outcome = runner(ctx).await;
        registry.finish(&task_op_id, terminal_value(&outcome));
        // Every exit path — success, cancellation and error alike.
        desktop.finish_compose_run(&run_id);
    });
    accepted(&op_id)
}

/// `POST /api/v1/ops/coach-turn` — a `CoachTurnRequestDto` body. The Tauri
/// wrapper's exact body, inside the spawned task; the credential resolves
/// there and is consumed by its only two consumers.
pub(crate) async fn op_coach_turn(state: Arc<ServerState>, req: Request) -> Response {
    let request: CoachTurnRequestDto = match parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let op_id = uuid::Uuid::new_v4().to_string();
    state.ops.issue("coach-turn", &op_id);

    let desktop = state.desktop.clone();
    let registry = state.ops.clone();
    let task_op_id = op_id.clone();
    tokio::spawn(async move {
        // The wrapper's body, line for line.
        let outcome = (|| async {
            let transport = load_llm_transport()
                .map_err(|e| BusError::internal(format!("load llm transport: {e}")))?;
            let prices = load_price_table()
                .map_err(|e| BusError::internal(format!("load price table: {e}")))?;
            // The operator's overlay is honoured here for the same reason
            // `pulse coach` honours it.
            let prompt = load_coach_prompt_from(prompt_override_dir().as_deref())
                .map_err(|e| BusError::internal(format!("load coach prompt: {e}")))?;
            let key = resolve_llm_api_key().map_err(BusError::from)?;
            let key_source = key.source();
            let redactor = Redactor::from_config(vec![key.expose().to_owned()]);
            // The SHARED coach transport (#165 review R6).
            let provider = coach_provider(key.expose(), transport.base_url.as_deref());

            let deps = CoachTurnDeps {
                provider,
                prices,
                redactor,
                key_source: Some(key_source),
                config: coach_config(transport.model.as_deref()),
                prompt: prompt.text,
                prompt_version: Some(prompt.version),
                turn_timeout: None,
                max_dsl_bytes: None,
            };
            coach_turn_core(&desktop, deps, request).await
        })()
        .await;
        registry.finish(&task_op_id, terminal_value(&outcome));
    });
    accepted(&op_id)
}

/// `POST /api/v1/ops/run-backtest-version` — a `BacktestRunRequest` body. The
/// server-wide single-flight latch is taken INSIDE the core, so a concurrent
/// duplicate is refused as its terminal error (the recorded reading).
pub(crate) async fn op_run_backtest(state: Arc<ServerState>, req: Request) -> Response {
    let request: crate::tauri::BacktestRunRequest = match parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    spawn_plain_op(&state, "run-backtest-version", |desktop| async move {
        run_backtest_version_core(&desktop, request).await
    })
}

/// `POST /api/v1/ops/run-walk-forward-version` — a `WalkForwardRunRequest`.
pub(crate) async fn op_run_walk_forward(state: Arc<ServerState>, req: Request) -> Response {
    let request: WalkForwardRunRequest = match parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    spawn_plain_op(&state, "run-walk-forward-version", |desktop| async move {
        run_walk_forward_version_core(&desktop, request).await
    })
}

/// `POST /api/v1/ops/start-demo-stream` — `{"steps": n}`. The `op_id` IS the
/// `RunId`, so every event's `runId` equals it. The wrapper's caps apply: the
/// route clamps `steps` at 64 and the core raises it to 2.
pub(crate) async fn op_start_demo_stream(state: Arc<ServerState>, req: Request) -> Response {
    let body: Value = match parse_body(req).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Some(steps) = body["steps"].as_u64().and_then(|s| u32::try_from(s).ok()) else {
        return bus_422(&BusError::new(
            BusErrorCode::Validation,
            "start-demo-stream requires {\"steps\": <count>}".to_owned(),
        ));
    };
    let run_id = RunId::new();
    let op_id = run_id.as_str().to_owned();
    state.ops.issue("start-demo-stream", &op_id);

    let sink = OpEventSink {
        registry: state.ops.clone(),
        op_id: op_id.clone(),
    };
    let registry = state.ops.clone();
    let task_op_id = op_id.clone();
    tokio::spawn(async move {
        let outcome: Result<StreamOutcome, BusError> =
            demo_stream_core(&run_id, steps.min(64), &sink).await;
        registry.finish(&task_op_id, terminal_value(&outcome));
    });
    accepted(&op_id)
}

/// A core's typed outcome as the terminal value: the DTO's JSON on success,
/// the cloned `BusError` on failure.
fn terminal_value<T: Serialize>(outcome: &Result<T, BusError>) -> Result<Value, BusError> {
    match outcome {
        Ok(dto) => serde_json::to_value(dto)
            .map_err(|e| BusError::internal(format!("serialize result: {e}"))),
        Err(err) => Err(err.clone()),
    }
}

/// The non-streaming op body: one `started` frame at seq 0, then the terminal
/// at seq 1. Returns the 202 the route answers with. The work is a FACTORY
/// over an owned `Arc<DesktopState>` because the core's future borrows the
/// desktop state — and a spawned task cannot borrow the handler's stack.
fn spawn_plain_op<T, Fut, F>(
    state: &Arc<ServerState>,
    command: &'static str,
    make_work: F,
) -> Response
where
    T: Serialize + Send + 'static,
    Fut: Future<Output = Result<T, BusError>> + Send + 'static,
    F: FnOnce(Arc<DesktopState>) -> Fut + Send + 'static,
{
    let op_id = uuid::Uuid::new_v4().to_string();
    state.ops.issue(command, &op_id);
    let registry = state.ops.clone();
    let task_op_id = op_id.clone();
    let desktop = state.desktop.clone();
    let started = SseRecord {
        seq: 0,
        name: "started",
        data: json!({ "command": command }).to_string(),
    };
    tokio::spawn(async move {
        registry.push_event(&task_op_id, &started);
        let outcome = make_work(desktop).await;
        registry.finish(&task_op_id, terminal_value(&outcome));
    });
    accepted(&op_id)
}

/// `GET /api/v1/ops/{op_id}/events` — the SSE handler.
///
/// `Last-Event-ID: <n>` (the header `EventSource` sends on reconnect) replays
/// only events with `seq > n`, then continues live. A malformed value is a 400;
/// its absence replays from the top (a fresh attach, which the late-attach
/// scenario requires to see the whole buffer).
pub(crate) fn op_events(state: &Arc<ServerState>, op_id: &str, req: &Request) -> Response {
    let after = match req.headers().get("last-event-id") {
        None => None,
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
        {
            Some(n) => Some(n),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    axum::Json(json!({
                        "code": "validation",
                        "message": "Last-Event-ID must be a non-negative integer sequence"
                    })),
                )
                    .into_response();
            }
        },
    };
    match state.ops.subscribe(op_id, after) {
        Err(UnknownOp) => op_unknown(op_id),
        Ok(Subscription { backlog, live }) => {
            let stream = OpEventStream {
                backlog: backlog.into_iter(),
                live,
            };
            axum::response::Sse::new(stream).into_response()
        }
    }
}

/// `GET /api/v1/ops/{op_id}` — the status envelope, or 404 `op_unknown` with a
/// message naming the asked-for id and both possible causes.
pub(crate) fn op_status(state: &Arc<ServerState>, op_id: &str) -> Response {
    match state.ops.status(op_id) {
        Some(body) => (StatusCode::OK, axum::Json(body)).into_response(),
        None => op_unknown(op_id),
    }
}

/// The 404 both status and events answer for an unknown-or-expired id.
fn op_unknown(op_id: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(json!({
            "code": "op_unknown",
            "message": format!(
                "no operation with id {op_id} was issued by this server, or its record has expired"
            )
        })),
    )
        .into_response()
}
