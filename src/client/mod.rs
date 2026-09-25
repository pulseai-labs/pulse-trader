//! The thin-client core (r3.s3.w5): the HTTP seam between the Mac app's
//! command bus and the always-on server on draco-desk.
//!
//! [`ServerClient`] speaks the w2 wire — plain routes, `ops/` spawns, and the
//! SSE event stream with `Last-Event-ID` resume — and nothing else: it holds a
//! base URL, a bearer token, and one `reqwest` client. [`ClientState`] is the
//! Tauri-managed connection: which server, which token, and the renderable
//! [`ServerStatus`].
//!
//! Version pinning (D4): the client pins [`crate::server::API_VERSION`] and
//! checks the `X-Pulse-Api-Version` header on EVERY response — a server
//! upgraded mid-session is a skew refusal, not a deserialize accident.
//!
//! The one error shape crossing INTO the bus is still [`BusError`] (the bus
//! contract). [`ClientError`] is the client-core's refusal vocabulary: it
//! drives the [`ServerStatus`] the Connect screen renders, and a refusal seen
//! by any proxied command flips the connection state back to `refused` (the
//! spec's "refusals return the UI to Connect").
//!
//! The connection file (`mcp-connection.toml`, beside the app's
//! `server-connection.toml` in the `PulseTrader` data dir) is written by a
//! successful `server_connect`, deleted by `server_disconnect`, and loaded at
//! start-up — see [`connection`].

pub(crate) mod connection;

use std::future::poll_fn;
use std::sync::RwLock;

use futures_core::Stream as _;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::server::API_VERSION;
use crate::tauri::error::{BusError, BusErrorCode};
use crate::tauri::events::{BusEvent, EventSink};

// ---------------------------------------------------------------------------
// The client core
// ---------------------------------------------------------------------------

/// The re-attach retry timing: capped exponential backoff, default 1s growing
/// to a 30s cap (spec ~121). Injectable so tests use milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryBackoff {
    /// The first re-attach delay.
    pub base: std::time::Duration,
    /// The ceiling every later delay clamps to.
    pub cap: std::time::Duration,
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self {
            base: std::time::Duration::from_secs(1),
            cap: std::time::Duration::from_secs(30),
        }
    }
}

impl RetryBackoff {
    /// A policy with explicit bounds (tests: milliseconds).
    #[must_use]
    pub fn new(base: std::time::Duration, cap: std::time::Duration) -> Self {
        Self { base, cap }
    }

    /// The wait before re-attach attempt `attempt` (0-based): the base
    /// doubling each attempt, clamped at the cap.
    fn delay(&self, attempt: u32) -> std::time::Duration {
        self.base
            .saturating_mul(1_u32 << attempt.min(30))
            .min(self.cap)
    }
}

/// How long a TCP connection may take to establish before the request gives
/// up. A black-holed route (a dropped VPN, a host that stopped answering)
/// fails HERE — on the status poll's handshake, on every proxied command and
/// on an SSE attach alike — instead of stalling them forever.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The whole-request deadline for ONE plain wire call: the handshake, a
/// proxied read, an operation's issue POST (answered 202 immediately).
///
/// Applied PER REQUEST, deliberately never as a client-wide default: an SSE
/// stream's body is open-ended by design and this wire sends no keep-alives, so
/// a deadline covering the body would cut a healthy idle stream mid-operation.
/// The streaming path bounds the attach instead — [`SSE_ATTACH_TIMEOUT`].
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long the response HEAD of an SSE attach may take. Once the head is in,
/// the body streams untimed: an operation may legitimately run for minutes
/// between two events, and a dropped stream is the resume loop's business, not
/// a timeout's.
const SSE_ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// The shared HTTP client: a bounded connect, NO system proxy, and nothing else
/// client-wide (see [`REQUEST_TIMEOUT`] for why the request deadline is per
/// request).
///
/// `no_proxy` is a SECURITY requirement, not a preference (r3.s3.w5 review round
/// 6). The server's documented URLs are plain `http://` on the tailnet, and
/// every request here carries `Authorization: Bearer <app token>`: with
/// reqwest's default behaviour a configured `HTTP_PROXY`/`HTTPS_PROXY` receives
/// those requests — and the bearer header with them — so the app token would
/// leave the tailnet through whatever proxy the machine happens to have set.
/// This client talks to ONE private server, so there is never a proxy to honour;
/// every surface that reaches the server rides this ONE constructor — the app's
/// [`ServerClient`], the `pulse mcp login` handshake AND its `/mcp` probe
/// (`src/cli/mcp.rs`), and the MCP relay's own client (which has the same
/// `no_proxy` builder in `src/mcp/relay.rs`, its module boundary keeping the
/// SDK off this path).
pub(crate) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// One authenticated connection to the server: base URL + bearer token + one
/// HTTP client.
#[derive(Clone)]
pub struct ServerClient {
    /// `http://host:port` — no trailing slash.
    base: String,
    token: String,
    http: reqwest::Client,
    backoff: RetryBackoff,
    /// The scope this connection's work needs; see [`TokenScope`]. The app's
    /// constructors default to [`TokenScope::App`] — every surface they drive is
    /// app-scoped — and [`ServerClient::connect_for`] states another one.
    required_scope: TokenScope,
}

/// The scope a connection's WORK needs from its token.
///
/// The handshake is mounted for either scope on purpose (any live token may ask
/// what is running), so the requirement belongs to the caller: the app's command
/// surface is `app`-scoped, an MCP client's (`/mcp`) is `agent`-scoped, and a
/// token holding the other scope is refused at connect — by name — instead of
/// being saved and 403ing on the first real request. The server reports the
/// caller's own scope in the handshake's additive `scope` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenScope {
    /// Every `/api/v1` command route (the app's surface).
    App,
    /// `/mcp` (an MCP client's surface: `pulse mcp login`, the relay).
    Agent,
}

impl TokenScope {
    /// The wire spelling (`auth::Scope`'s own).
    fn as_str(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Agent => "agent",
        }
    }
}

/// The handshake body the server answers `GET /api/v1/handshake` with (D4).
///
/// Field names match the server's serialization exactly (no rename): the
/// server's `HandshakeBody` has no `rename_all`, so neither does this.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct HandshakeDto {
    api_version: u32,
    binary_version: String,
    engine_fingerprint: String,
    target_triple: String,
    /// The scope of the token that asked, when the server reports it.
    ///
    /// ADDITIVE wire field: a server older than it omits the field, and an
    /// absent scope behaves EXACTLY as this client behaved before the field
    /// existed — connected, no refusal (see `handshake_body`).
    #[serde(default)]
    scope: Option<String>,
}

/// What `server_connect` learned — the spec's named outcomes: `connected`,
/// `unreachable`, `token_refused` or `skew`.
///
/// This is a plain outcome, not a `Result`: the Connect screen renders every
/// arm, and a refusal additionally flips the connection state (see
/// [`ClientState::connect`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ConnectOutcome {
    /// The handshake succeeded; the server's identity for the status strip.
    Connected {
        /// The server binary's version.
        binary_version: String,
        /// The engine fingerprint the server runs.
        engine_fingerprint: String,
    },
    /// The server could not be reached (or answered unintelligibly).
    Unreachable {
        /// Human-readable reason.
        reason: String,
    },
    /// The server refused this token.
    TokenRefused {
        /// Human-readable reason.
        reason: String,
    },
    /// The server speaks a different API generation than this app.
    Skew {
        /// The API version the server announced.
        server_api_version: u32,
    },
}

/// Why a wire call did not deliver its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientError {
    /// The server could not be reached, or answered unintelligibly.
    Unreachable {
        /// Human-readable reason.
        reason: String,
    },
    /// The server refused the token (401/403).
    TokenRefused {
        /// Human-readable reason.
        reason: String,
    },
    /// The server speaks a different API generation than this app.
    Skew {
        /// The API version the server announced.
        server_api_version: u32,
    },
    /// The server no longer knows the operation (a 404 `op_unknown` met while
    /// re-attaching): its retention elapsed. The run may still be in the
    /// library — the mapped error's message says so.
    OperationExpired {
        /// The operation the server forgot.
        op_id: String,
    },
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable { reason } | Self::TokenRefused { reason } => f.write_str(reason),
            Self::Skew { server_api_version } => write!(
                f,
                "server speaks API v{server_api_version}, this app speaks v{API_VERSION} — \
                 update one side before connecting"
            ),
            Self::OperationExpired { op_id } => write!(
                f,
                "operation {op_id} expired on the server (op_unknown) — the run may still be \
                 in the library"
            ),
        }
    }
}

impl From<ClientError> for BusError {
    fn from(err: ClientError) -> Self {
        match &err {
            // The typed operation-expired error: its own family member (the
            // route's own `not_found`), with the message the spec pins — the
            // run may still be in the library.
            ClientError::OperationExpired { op_id } => BusError::new(
                BusErrorCode::NotFound,
                format!(
                    "operation {op_id} expired on the server (op_unknown) — the run may still \
                     be in the library"
                ),
            ),
            // A connect refusal is infrastructure, not a domain family —
            // `internal` with the display reason is the honest flat mapping.
            // The Connect screen reads the structured refusal from
            // `server_status`; the error path is the fallback rendering.
            _ => BusError::new(BusErrorCode::Internal, err.to_string()),
        }
    }
}

/// How a proxied wire call failed: as the bus's one error shape, or as a
/// refusal the connection state must learn about.
#[derive(Debug, Clone)]
pub(crate) enum WireFailure {
    /// A mapped [`BusError`] — the command boundary carries it as-is.
    Bus(BusError),
    /// A refusal (unreachable / token refused / skew) — the state records it,
    /// and the command still carries the flattened [`BusError`].
    Refusal(ClientError),
}

impl WireFailure {
    fn into_bus(self) -> BusError {
        match self {
            Self::Bus(err) => err,
            Self::Refusal(err) => err.into(),
        }
    }
}

/// What `run_op` observed while driving one operation.
#[derive(Debug, Clone, PartialEq)]
pub struct RunOpOutcome<T> {
    /// The operation's terminal `result` payload.
    pub value: T,
    /// How many events were successfully delivered to the sink.
    pub emitted: u32,
    /// True when at least one event could not be delivered (the far end went
    /// away). The SERVER-OWNED operation still runs to completion; the client
    /// keeps consuming so the terminal is never lost.
    pub cancelled: bool,
}

impl ServerClient {
    /// Build a client for the APP's surface ([`TokenScope::App`]). No I/O — see
    /// [`connect`](Self::connect).
    #[must_use]
    pub fn new(base_url: &str, token: &str) -> Self {
        Self::with_scope(base_url, token, TokenScope::App, RetryBackoff::default())
    }

    /// [`new`](Self::new) with an explicit re-attach backoff (tests:
    /// milliseconds).
    #[must_use]
    pub fn with_backoff(base_url: &str, token: &str, backoff: RetryBackoff) -> Self {
        Self::with_scope(base_url, token, TokenScope::App, backoff)
    }

    /// A client whose work needs `required_scope` from its token.
    #[must_use]
    pub fn with_scope(
        base_url: &str,
        token: &str,
        required_scope: TokenScope,
        backoff: RetryBackoff,
    ) -> Self {
        Self {
            base: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
            http: http_client(),
            backoff,
            required_scope,
        }
    }

    /// Handshake, pin the API version, and return the live client plus what
    /// the handshake taught — requiring [`TokenScope::App`], the scope every
    /// surface this client drives is mounted under.
    ///
    /// # Errors
    ///
    /// The three [`ClientError`] arms: unreachable, token refused, skew — plus
    /// the refusal a token that does NOT hold the required scope earns here,
    /// before anything is persisted (`handshake_body`), so a wrong-scope token
    /// can never be saved as a working connection.
    pub async fn connect(
        base_url: &str,
        token: &str,
    ) -> Result<(Self, ConnectOutcome), ClientError> {
        Self::connect_for(base_url, token, TokenScope::App).await
    }

    /// [`connect`](Self::connect) for a caller whose work needs `required`:
    /// `pulse mcp login` requires [`TokenScope::Agent`] — its connection drives
    /// `/mcp` — while the app requires [`TokenScope::App`].
    ///
    /// # Errors
    ///
    /// The three [`ClientError`] arms (unreachable, token refused, skew) plus
    /// the scope refusal: a token whose reported scope is not `required` is
    /// refused by name, before anything is persisted.
    pub async fn connect_for(
        base_url: &str,
        token: &str,
        required: TokenScope,
    ) -> Result<(Self, ConnectOutcome), ClientError> {
        let client = Self::with_scope(base_url, token, required, RetryBackoff::default());
        let (binary_version, engine_fingerprint) = client.handshake().await?;
        Ok((
            client,
            ConnectOutcome::Connected {
                binary_version,
                engine_fingerprint,
            },
        ))
    }

    /// The handshake, as [`ClientState::status`] re-runs it for a fresh
    /// up/down/refused answer (the 15 s poll; d28).
    pub(crate) async fn handshake(&self) -> Result<(String, String), ClientError> {
        let body = self.handshake_body().await?;
        Ok((body.binary_version, body.engine_fingerprint))
    }

    async fn handshake_body(&self) -> Result<HandshakeDto, ClientError> {
        let response = self
            .http
            .get(format!("{}/api/v1/handshake", self.base))
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| ClientError::Unreachable {
                reason: format!("cannot reach the server at {}: {error}", self.base),
            })?;
        check_skew(response.headers())?;
        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(ClientError::TokenRefused {
                reason: format!("the server refused this token (HTTP {status})"),
            });
        }
        if !status.is_success() {
            return Err(ClientError::Unreachable {
                reason: format!("handshake failed: HTTP {status}"),
            });
        }
        let body: HandshakeDto =
            response
                .json()
                .await
                .map_err(|error| ClientError::Unreachable {
                    reason: format!("the handshake body was unreadable: {error}"),
                })?;
        // A token that cannot do app work must not be saved as a connection.
        // The handshake is mounted for EITHER scope on purpose (any live token
        // may ask what is running), while every `/api/v1` command route is
        // app-scoped: an agent token would handshake, persist, open the shell —
        // and 403 on the very first request. The server names the caller's own
        // scope, so the refusal lands HERE, before anything is stored.
        //
        // An absent scope is an older server: exactly today's behaviour, no
        // refusal.
        let required = self.required_scope.as_str();
        if let Some(scope) = body.scope.as_deref()
            && scope != required
        {
            return Err(ClientError::TokenRefused {
                reason: format!(
                    "the presented token is {scope}-scoped and this connection needs an \
                     {required}-scoped token (ask the server operator for a token issued \
                     with `--scope {required}`)"
                ),
            });
        }
        Ok(body)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// GET a plain route and decode its body.
    ///
    /// # Errors
    ///
    /// A non-2xx answers with the route's [`BusError`] body when it parses as
    /// one, else a synthetic internal error carrying the status; an unreadable
    /// 2xx body is an internal error too; a 401/403 or a skewed
    /// `X-Pulse-Api-Version` is a [`ClientError`] refusal.
    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, BusError> {
        self.get_inner(path).await.map_err(WireFailure::into_bus)
    }

    pub(crate) async fn get_inner<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, WireFailure> {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(|error| {
                WireFailure::Refusal(ClientError::Unreachable {
                    reason: format!("GET {path} failed: {error}"),
                })
            })?;
        decode(response, path).await
    }

    /// POST a plain route's JSON body and decode its answer.
    ///
    /// # Errors
    ///
    /// Same contract as [`get_json`](Self::get_json).
    pub async fn post_json<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, BusError> {
        self.post_inner(path, body)
            .await
            .map_err(WireFailure::into_bus)
    }

    pub(crate) async fn post_inner<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, WireFailure> {
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .timeout(REQUEST_TIMEOUT)
            .json(body)
            .send()
            .await
            .map_err(|error| {
                WireFailure::Refusal(ClientError::Unreachable {
                    reason: format!("POST {path} failed: {error}"),
                })
            })?;
        decode(response, path).await
    }

    /// Spawn a server-owned operation and drive its event stream.
    ///
    /// POSTs for the 202 `{op_id}`, then consumes
    /// `GET /api/v1/ops/{op_id}/events`: every `bus` frame is deserialized into
    /// a [`BusEvent`] and handed to `sink`; the terminal `result` frame is
    /// decoded as `T`; a terminal `error` frame is returned as the typed
    /// [`BusError`]. A stream that drops before the terminal is re-attached
    /// with `Last-Event-ID` (no duplicates, no gaps), at most five times;
    /// exhausted, the error names the op id.
    ///
    /// # Errors
    ///
    /// The 202 POST's refusal (422 body), an `op_unknown` events route, the
    /// operation's own terminal `error`, or an exhausted resume budget — plus
    /// the [`ClientError`] refusals the wire helpers carry.
    pub async fn run_op<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        op: &str,
        body: &B,
        sink: &(dyn EventSink + Send + Sync),
    ) -> Result<RunOpOutcome<T>, BusError> {
        self.op_inner(op, body, sink)
            .await
            .map_err(WireFailure::into_bus)
    }

    pub(crate) async fn op_inner<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        op: &str,
        body: &B,
        sink: &(dyn EventSink + Send + Sync),
    ) -> Result<RunOpOutcome<T>, WireFailure> {
        #[derive(Deserialize)]
        struct Accepted {
            op_id: String,
        }
        let accepted: Accepted = self.post_inner(&format!("/api/v1/{op}"), body).await?;
        let op_id = accepted.op_id;

        let mut last_seen: Option<u32> = None;
        let mut emitted: u32 = 0;
        let mut cancelled = false;
        // One live connection + at most five resumes, each after the capped
        // backoff delay (spec ~121: injectable, 1s -> 30s by default).
        for attempt in 0..=5usize {
            let end = self
                .stream_events(&op_id, &mut last_seen, &mut emitted, &mut cancelled, sink)
                .await?;
            match end {
                StreamEnd::Terminal(value) => {
                    return Ok(RunOpOutcome {
                        value,
                        emitted,
                        cancelled,
                    });
                }
                StreamEnd::Dropped if attempt < 5 => {
                    let delay = self.backoff.delay(u32::try_from(attempt).unwrap_or(30));
                    tokio::time::sleep(delay).await;
                }
                StreamEnd::Dropped => {
                    return Err(WireFailure::Bus(BusError::internal(format!(
                        "operation {op_id}: the event stream dropped without a terminal frame \
                         after 5 reconnects"
                    ))));
                }
            }
        }
        unreachable!("the loop returns from every branch")
    }

    /// Consume one SSE connection until the terminal or the wire drops.
    ///
    /// `seen` is the resume cursor the caller owns: entered holding the seq to
    /// resume after, left holding the last forwarded seq (so a reconnect
    /// carries the right `Last-Event-ID`).
    async fn stream_events<T: DeserializeOwned>(
        &self,
        op_id: &str,
        seen: &mut Option<u32>,
        emitted: &mut u32,
        cancelled: &mut bool,
        sink: &(dyn EventSink + Send + Sync),
    ) -> Result<StreamEnd<T>, WireFailure> {
        let path = format!("/api/v1/ops/{op_id}/events");
        let mut request = self.http.get(self.url(&path)).bearer_auth(&self.token);
        if let Some(seq) = *seen {
            request = request.header("Last-Event-ID", seq.to_string());
        }
        // The ATTACH is bounded — the response HEAD, and only the head, so a
        // half-open socket cannot hang this call forever. The body is not: no
        // client-wide request deadline exists for exactly this reason.
        let attach = tokio::time::timeout(SSE_ATTACH_TIMEOUT, request.send()).await;
        let Ok(Ok(response)) = attach else {
            // A connect-level failure (or a head that never came) to re-attach
            // is a DROP: the operation is server-owned, and the resume loop
            // re-tries with the same cursor.
            return Ok(StreamEnd::Dropped);
        };
        check_skew(response.headers()).map_err(WireFailure::Refusal)?;
        let status = response.status();
        // A refusal mid-operation must surface AS a refusal, the way `decode`
        // does it: the managed state has to be able to flip to `refused` from
        // an operation path, not report a generic internal error.
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(WireFailure::Refusal(ClientError::TokenRefused {
                reason: format!("the server refused this token (HTTP {status} on {path})"),
            }));
        }
        if !status.is_success() {
            let bytes = response.bytes().await.unwrap_or_default();
            // Two refusal shapes reach this path: the bus's own error body
            // (`{"code":"validation",…}`, decodable as `BusError`) and the ops
            // routes' bare `{"code":"op_unknown","message":…}`
            // (`src/server/ops.rs::op_unknown`). The route token is NOT a bus
            // family, so that body cannot deserialize as a `BusError` — read
            // the JSON `code` field first, or the typed expiry below would be
            // unreachable on the wire the server actually sends.
            let route_code = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|body| {
                    body.get("code")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                });
            // The spec's typed expiry (spec ~123): a 404 `op_unknown` met
            // while attaching or re-attaching is the operation-expired error,
            // whose message says the run may still be in the library.
            if status == reqwest::StatusCode::NOT_FOUND
                && route_code.as_deref() == Some("op_unknown")
            {
                return Err(WireFailure::Refusal(ClientError::OperationExpired {
                    op_id: op_id.to_owned(),
                }));
            }
            let err = serde_json::from_slice::<BusError>(&bytes)
                .unwrap_or_else(|_| BusError::internal(format!("HTTP {status} from {path}")));
            // A bus-shaped 404 carrying the same token is the same expiry — the
            // shape a caller-side stub or an older server answers with.
            if status == reqwest::StatusCode::NOT_FOUND
                && err.code == BusErrorCode::NotFound
                && err.message.contains("op_unknown")
            {
                return Err(WireFailure::Refusal(ClientError::OperationExpired {
                    op_id: op_id.to_owned(),
                }));
            }
            return Err(WireFailure::Bus(err));
        }

        let mut parser = SseParser::default();
        let body = response.bytes_stream();
        tokio::pin!(body);
        loop {
            let chunk = match poll_fn(|cx| body.as_mut().poll_next(cx)).await {
                // A network error mid-stream is a DROP, not a failure: the
                // operation is server-owned and the resume loop re-attaches.
                Some(Err(_)) | None => return Ok(StreamEnd::Dropped),
                Some(Ok(bytes)) => bytes,
            };
            for frame in parser.push(&chunk) {
                match frame.event.as_str() {
                    "bus" => {
                        let event: BusEvent = match serde_json::from_str(frame.data.trim()) {
                            Ok(event) => event,
                            Err(error) => {
                                return Err(WireFailure::Bus(BusError::internal(format!(
                                    "operation {op_id}: unreadable bus frame: {error}"
                                ))));
                            }
                        };
                        // Defensive replay guard: the server already resumes
                        // from Last-Event-ID, but a duplicate must never reach
                        // the channel twice.
                        if seen.is_some_and(|seen| event.seq <= seen) {
                            continue;
                        }
                        let seq = event.seq;
                        if sink.send_event(event).is_ok() {
                            *emitted += 1;
                        } else {
                            // The far end is gone — cancellation, not an error.
                            // Stop sending; keep consuming to the terminal.
                            *cancelled = true;
                        }
                        *seen = Some(seq);
                    }
                    "result" => {
                        let value: T =
                            serde_json::from_str(frame.data.trim()).map_err(|error| {
                                WireFailure::Bus(BusError::internal(format!(
                                    "operation {op_id}: unreadable result frame: {error}"
                                )))
                            })?;
                        return Ok(StreamEnd::Terminal(value));
                    }
                    "error" => {
                        let err = serde_json::from_str::<BusError>(frame.data.trim())
                            .unwrap_or_else(|_| {
                                BusError::internal(format!(
                                    "operation {op_id} failed with an unreadable error frame"
                                ))
                            });
                        return Err(WireFailure::Bus(err));
                    }
                    // `started` and any keep-alive/comment frames carry no
                    // channel traffic.
                    _ => {}
                }
            }
        }
    }
}

/// How one SSE connection ended.
enum StreamEnd<T> {
    /// The terminal `result` frame, decoded.
    Terminal(T),
    /// The connection ended without a terminal — resume is allowed.
    Dropped,
}

/// The version-pin check the spec pins on EVERY response: a skewed
/// `X-Pulse-Api-Version` is a refusal, never a parse accident. A response
/// without the header (a raw stub) skips the check.
fn check_skew(headers: &reqwest::header::HeaderMap) -> Result<(), ClientError> {
    let announced = headers
        .get("x-pulse-api-version")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u32>().ok());
    match announced {
        Some(version) if version != API_VERSION => Err(ClientError::Skew {
            server_api_version: version,
        }),
        _ => Ok(()),
    }
}

/// Shared decode for the plain routes: success decodes `T`; a 401/403 is a
/// token refusal; any other failure decodes the route's `BusError` body,
/// falling back to a synthetic internal error.
async fn decode<T: DeserializeOwned>(
    response: reqwest::Response,
    path: &str,
) -> Result<T, WireFailure> {
    check_skew(response.headers()).map_err(WireFailure::Refusal)?;
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(WireFailure::Refusal(ClientError::TokenRefused {
            reason: format!("the server refused this token (HTTP {status} on {path})"),
        }));
    }
    let bytes = response.bytes().await.map_err(|error| {
        WireFailure::Refusal(ClientError::Unreachable {
            reason: format!("reading {path} failed: {error}"),
        })
    })?;
    if !status.is_success() {
        return Err(WireFailure::Bus(
            serde_json::from_slice::<BusError>(&bytes).unwrap_or_else(|_| {
                BusError::internal(format!(
                    "HTTP {status} from {path}: {}",
                    String::from_utf8_lossy(&bytes)
                ))
            }),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        WireFailure::Bus(BusError::internal(format!(
            "{path} answered an unexpected shape: {error}"
        )))
    })
}

/// Incremental SSE framing: feed bytes, get complete frames.
///
/// Mirrors the server's writer (`id:`/`event:`/`data:` lines, blank-line
/// separated) and accepts both separators — `\n\n` and a CRLF stream's
/// `\r\n\r\n` — including one split across two network chunks.
#[derive(Default)]
struct SseParser {
    /// RAW bytes, never a decoded string: a multibyte character split across
    /// two chunks must not be lossily decoded before its whole frame is here
    /// (`from_utf8_lossy` on a partial chunk would put U+FFFD inside a payload).
    buffer: Vec<u8>,
}

impl SseParser {
    /// Push bytes and drain every complete frame.
    fn push(&mut self, chunk: &[u8]) -> Vec<SseFrame> {
        self.buffer.extend_from_slice(chunk);
        let mut frames = Vec::new();
        while let Some(end) = frame_end(&self.buffer) {
            // Only a COMPLETE frame is decoded, so its bytes are all present.
            let block = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
            self.buffer.drain(..end);
            let mut event = String::new();
            let mut data = String::new();
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    value.trim().clone_into(&mut event);
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data.push_str(value.trim());
                }
                // `id:` frames are the seq the SERVER tracks; the client's own
                // resume position comes from the forwarded events themselves.
            }
            if !event.is_empty() {
                frames.push(SseFrame { event, data });
            }
        }
        frames
    }
}

/// A parsed server-sent event frame.
struct SseFrame {
    event: String,
    data: String,
}

/// The offset just past the next frame separator: `\n\n`, or a CRLF stream's
/// `\r\n\r\n`. Scanning BYTES (not a string) is what lets a separator split
/// across two chunks complete on the second append.
fn frame_end(buffer: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < buffer.len() {
        if buffer[i] == b'\n' {
            if buffer.get(i + 1) == Some(&b'\n') {
                return Some(i + 2);
            }
            if buffer.get(i + 1) == Some(&b'\r') && buffer.get(i + 2) == Some(&b'\n') {
                return Some(i + 3);
            }
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// The managed connection state
// ---------------------------------------------------------------------------

/// The live link inside [`ClientState`].
enum Link {
    Down,
    Up(ServerClient),
    /// A recorded refusal — a `token_refused`/`skew` seen by a connect or by
    /// ANY proxied command. The status strip renders it and the UI returns to
    /// Connect.
    Failed(ClientError),
}

/// The Tauri-managed connection to the always-on server (spec: "The app state
/// becomes `ClientState`").
///
/// In-memory for the app run, seeded from the connection file at start-up: a
/// file that exists puts the state straight to `Up` (the 15 s status poll then
/// shows up/down as the server comes and goes, d28 — no relaunch). A file that
/// fails its safety vetting starts the state `refused` with the named reason.
pub struct ClientState {
    link: RwLock<Link>,
}

impl ClientState {
    /// Lock the link for writing, adopting a poisoned guard — the
    /// ops.rs/commands.rs convention: a panic in a holder must not turn into a
    /// permanently poisoned state for every later call, and the link enum is
    /// always whole between updates, so adoption is sound.
    fn write_link(&self) -> std::sync::RwLockWriteGuard<'_, Link> {
        self.link
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Lock the link for reading, adopting a poisoned guard (see
    /// [`Self::write_link`]).
    fn read_link(&self) -> std::sync::RwLockReadGuard<'_, Link> {
        self.link
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A disconnected app state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            link: RwLock::new(Link::Down),
        }
    }

    /// The start-up state: the connection file, if one exists and is safe.
    #[must_use]
    pub fn loaded() -> Self {
        match connection::load_app() {
            Ok(Some(file)) => Self {
                link: RwLock::new(Link::Up(ServerClient::new(&file.url, &file.token))),
            },
            Ok(None) => Self::new(),
            Err(reason) => Self {
                link: RwLock::new(Link::Failed(ClientError::TokenRefused {
                    reason: format!("the saved connection is not safe to use: {reason}"),
                })),
            },
        }
    }

    /// Handshake against `server_url` with `token`; on success, persist the
    /// connection file and swap the connection.
    ///
    /// A `token_refused`/`skew` outcome ALSO records the refusal as the state
    /// (the spec: a refusal from any command returns the UI to Connect with
    /// the reason). An `unreachable` outcome leaves the previous state alone —
    /// there is nothing wrong with the saved connection.
    pub async fn connect(&self, server_url: &str, token: &str) -> ConnectOutcome {
        match ServerClient::connect(server_url, token).await {
            Ok((client, outcome @ ConnectOutcome::Connected { .. })) => {
                if let Err(reason) = connection::store_app(&connection::ConnectionFile {
                    url: server_url.trim_end_matches('/').to_owned(),
                    token: token.to_owned(),
                }) {
                    // The handshake succeeded but the file could not be
                    // written: report it through the outcome rather than
                    // silently running un-persisted.
                    *self.write_link() = Link::Failed(ClientError::Unreachable {
                        reason: format!("connected, but saving the connection failed: {reason}"),
                    });
                    return ConnectOutcome::Unreachable {
                        reason: format!("connected, but saving the connection failed: {reason}"),
                    };
                }
                *self.write_link() = Link::Up(client);
                outcome
            }
            Ok((_, outcome)) => outcome,
            Err(refusal @ (ClientError::TokenRefused { .. } | ClientError::Skew { .. })) => {
                *self.write_link() = Link::Failed(refusal.clone());
                match refusal {
                    ClientError::TokenRefused { reason } => ConnectOutcome::TokenRefused { reason },
                    ClientError::Skew { server_api_version } => {
                        ConnectOutcome::Skew { server_api_version }
                    }
                    ClientError::Unreachable { .. } | ClientError::OperationExpired { .. } => {
                        unreachable!("matched above")
                    }
                }
            }
            Err(ClientError::Unreachable { reason }) => ConnectOutcome::Unreachable { reason },
            // A connect cannot meet an expired OPERATION (nothing was posted);
            // the type is exhaustive, so this arm names the impossibility.
            Err(ClientError::OperationExpired { op_id }) => ConnectOutcome::Unreachable {
                reason: format!(
                    "operation {op_id} expired — this was a handshake, not an operation"
                ),
            },
        }
    }

    /// What the status strip renders: the last refusal, not-connected, or a
    /// FRESH handshake against the stored connection (the 15 s poll — a server
    /// restart shows down, then up, without relaunching the app; d28).
    pub async fn status(&self) -> ServerStatus {
        let snapshot = match &*self.read_link() {
            Link::Down => return ServerStatus::not_connected(),
            Link::Failed(err) => {
                return ServerStatus::refused(err.to_string());
            }
            Link::Up(client) => client.clone(),
        };
        match snapshot.handshake().await {
            Ok((binary_version, engine_fingerprint)) => ServerStatus {
                state: ServerStatusState::Up,
                binary_version: Some(binary_version),
                engine_fingerprint: Some(engine_fingerprint),
                reason: None,
            },
            // The token stopped working server-side: refused with the reason.
            Err(err @ ClientError::TokenRefused { .. }) => {
                *self.write_link() = Link::Failed(err.clone());
                ServerStatus::refused(err.to_string())
            }
            // Unreachable (a restart in progress) or a skew (the server
            // upgraded past this app): down — the poll shows up again when the
            // server is back. The connection itself stays stored.
            Err(_) => ServerStatus {
                state: ServerStatusState::Down,
                binary_version: None,
                engine_fingerprint: None,
                reason: None,
            },
        }
    }

    /// Delete the connection file and drop the live connection.
    ///
    /// # Errors
    ///
    /// Never — the `Result` is the bus's uniform command shape; a file-removal
    /// failure is recorded as the status reason instead of an error, since the
    /// in-memory disconnection has already taken effect.
    pub fn disconnect(&self) -> Result<(), BusError> {
        *self.write_link() = Link::Down;
        if let Err(reason) = connection::remove_app() {
            *self.write_link() = Link::Failed(ClientError::Unreachable {
                reason: format!("disconnected, but removing the saved connection failed: {reason}"),
            });
        }
        Ok(())
    }

    /// The proxied-command entry point: run `call` against the live client,
    /// recording a refusal as the connection state before flattening it into
    /// the one [`BusError`] shape.
    async fn proxy<T>(
        &self,
        call: impl std::future::Future<Output = Result<T, WireFailure>>,
    ) -> Result<T, BusError> {
        match call.await {
            Ok(value) => Ok(value),
            Err(WireFailure::Bus(err)) => Err(err),
            Err(WireFailure::Refusal(err)) => {
                if matches!(
                    err,
                    ClientError::TokenRefused { .. } | ClientError::Skew { .. }
                ) {
                    *self.write_link() = Link::Failed(err.clone());
                }
                Err(err.into())
            }
        }
    }

    fn client_for_call(&self) -> Result<ServerClient, BusError> {
        match &*self.read_link() {
            Link::Up(client) => Ok(client.clone()),
            Link::Down => Err(BusError::internal(
                "not connected to the server — connect first (the Connect screen)",
            )),
            Link::Failed(err) => Err(BusError::internal(format!("not connected: {err}"))),
        }
    }

    /// GET a plain route through the live connection (the proxied read
    /// commands' seam).
    ///
    /// # Errors
    ///
    /// The not-connected refusal, or the wire failure flattened.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, BusError> {
        let client = self.client_for_call()?;
        self.proxy(client.get_inner(path)).await
    }

    /// POST a plain route through the live connection.
    ///
    /// # Errors
    ///
    /// Same contract as [`get`](Self::get).
    pub async fn post<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, BusError> {
        let client = self.client_for_call()?;
        self.proxy(client.post_inner(path, body)).await
    }

    /// Run a server-owned operation through the live connection, forwarding
    /// its events into the command's channel.
    ///
    /// # Errors
    ///
    /// Same contract as [`get`](Self::get).
    pub async fn op<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        op: &str,
        body: &B,
        sink: &(dyn EventSink + Send + Sync),
    ) -> Result<RunOpOutcome<T>, BusError> {
        let client = self.client_for_call()?;
        self.proxy(client.op_inner(op, body, sink)).await
    }
}

impl Default for ClientState {
    fn default() -> Self {
        Self::new()
    }
}

/// What the status strip renders (spec: `state: up | down | not_connected |
/// refused`, with the server's version and fingerprint when known).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
pub struct ServerStatus {
    /// The connection state.
    pub state: ServerStatusState,
    /// The server binary's version, when a handshake answered.
    pub binary_version: Option<String>,
    /// The server's engine fingerprint, when a handshake answered.
    pub engine_fingerprint: Option<String>,
    /// Why the last exchange refused, when it did — the Connect screen's
    /// "reason for the last refusal" comes from here.
    pub reason: Option<String>,
}

/// The connection state the strip switches on. Wire tokens are the spec's own:
/// `up`, `down`, `not_connected`, `refused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, specta::Type)]
#[serde(rename_all = "snake_case")]
pub enum ServerStatusState {
    /// Connected, and a fresh handshake answered.
    Up,
    /// A connection is stored but the server is not answering (a restart, or a
    /// version the app cannot speak).
    Down,
    /// No connection is stored.
    NotConnected,
    /// The last exchange refused the token (or the connection file failed its
    /// safety vetting).
    Refused,
}

impl ServerStatus {
    fn not_connected() -> Self {
        Self {
            state: ServerStatusState::NotConnected,
            binary_version: None,
            engine_fingerprint: None,
            reason: None,
        }
    }

    fn refused(reason: String) -> Self {
        Self {
            state: ServerStatusState::Refused,
            binary_version: None,
            engine_fingerprint: None,
            reason: Some(reason),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The `(event, data)` pairs one push produced.
    fn pushed(parser: &mut SseParser, chunk: &[u8]) -> Vec<(String, String)> {
        parser
            .push(chunk)
            .into_iter()
            .map(|frame| (frame.event, frame.data))
            .collect()
    }

    /// Both separators frame: the LF writer and a CRLF one, byte for byte.
    #[test]
    fn the_parser_frames_lf_and_crlf_streams() {
        let mut parser = SseParser::default();
        let frames = pushed(&mut parser, b"id: 0\nevent: bus\ndata: {\"seq\":0}\n\n");
        assert_eq!(
            frames,
            vec![("bus".to_owned(), "{\"seq\":0}".to_owned())],
            "an LF frame"
        );

        let mut parser = SseParser::default();
        let frames = pushed(
            &mut parser,
            b"id: 1\r\nevent: result\r\ndata: {\"ok\":true}\r\n\r\n",
        );
        assert_eq!(
            frames,
            vec![("result".to_owned(), "{\"ok\":true}".to_owned())],
            "a CRLF frame completes too"
        );

        // Two frames in one chunk drain in order; a trailing partial stays.
        let mut parser = SseParser::default();
        let frames = pushed(
            &mut parser,
            b"event: bus\ndata: {\"seq\":1}\n\nevent: bus\ndata: {\"seq\":2}\n\nevent: bu",
        );
        assert_eq!(frames.len(), 2, "both complete frames: {frames:?}");
        assert_eq!(frames[1].1, "{\"seq\":2}");
    }

    /// The separator itself may be split across chunks: `\r\n` then `\r\n`, or
    /// `\n` then `\n`, completes the frame on the second push and not before.
    #[test]
    fn a_separator_split_across_chunks_completes_on_the_second_push() {
        let body = b"event: bus\ndata: {\"seq\":3}";
        for (first, second) in [
            (&b"\r\n"[..], &b"\r\n"[..]),
            (&b"\n"[..], &b"\n"[..]),
            (&b"\r\n\r"[..], &b"\n"[..]),
        ] {
            let mut parser = SseParser::default();
            let mut chunk = body.to_vec();
            chunk.extend_from_slice(first);
            assert!(
                pushed(&mut parser, &chunk).is_empty(),
                "no frame before the separator is complete: {first:?}"
            );
            let frames = pushed(&mut parser, second);
            assert_eq!(
                frames,
                vec![("bus".to_owned(), "{\"seq\":3}".to_owned())],
                "the frame completes on the split separator: {first:?} {second:?}"
            );
        }
    }

    /// A multibyte character split across two network chunks must survive whole
    /// — the lossy decode of a partial chunk is the defect this pins.
    #[test]
    fn a_multibyte_character_split_across_chunks_is_not_mangled() {
        let payload = "{\"message\":\"café — ✓ 日本語\"}";
        let frame = format!("event: bus\ndata: {payload}\n\n");
        let bytes = frame.as_bytes();
        // Split INSIDE the first multibyte character (and inside the trailing
        // separator, so both hazards are live at once).
        let inside_char = frame.find('é').expect("a multibyte character") + 1;
        let inside_separator = bytes.len() - 1;
        let mut parser = SseParser::default();
        assert!(pushed(&mut parser, &bytes[..inside_char]).is_empty());
        assert!(pushed(&mut parser, &bytes[inside_char..inside_separator]).is_empty());
        let frames = pushed(&mut parser, &bytes[inside_separator..]);
        assert_eq!(frames.len(), 1, "one intact frame: {frames:?}");
        assert_eq!(frames[0].1, payload, "no U+FFFD crept in");
        assert!(
            serde_json::from_str::<serde_json::Value>(&frames[0].1).is_ok(),
            "the payload is still intact JSON"
        );
    }
}
