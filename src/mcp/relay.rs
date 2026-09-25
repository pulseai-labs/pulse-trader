//! The `pulse mcp` relay (r3.s3.w5): an agent that today speaks MCP-over-stdio
//! keeps doing exactly that, while the frames ride HTTP to the always-on
//! server's `/mcp` route.
//!
//! A **byte-level bridge**, deliberately: one POST per stdin message (the
//! request body forwarded unchanged), the response body written to stdout
//! unchanged, the `Mcp-Session-Id` the server hands back learned once and
//! attached to every later request — never to an `initialize`, which always
//! starts a fresh session — one GET SSE stream per session for the
//! server-initiated frames, and the negotiated protocol version echoed back in
//! the `MCP-Protocol-Version` header once initialize answers. No rmcp on this
//! path — the boundary gate keeps the SDK inside the serving module, and a
//! transparent relay has no business holding tool logic anyway.
//!
//! Stdout stays protocol-only: the bridge writes response bytes and SSE
//! `data:` frames and NOTHING else (diagnostics go to stderr, the same
//! discipline `check-mcp-boundary.sh` enforces on the serving path).

use std::sync::Arc;

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::sync::RwLock;

/// Everything the relay needs: where the server is and the token that works.
#[derive(Clone)]
pub(crate) struct RelayConfig {
    /// The server base URL, trimmed of a trailing slash.
    pub(crate) base: String,
    /// The bearer token from the connection file.
    pub(crate) token: String,
}

/// Shared relay state: the session id the server issued at initialize, and
/// the protocol version the initialize RESULT named (echoed back on later
/// requests per the streamable-HTTP transport contract).
#[derive(Default)]
struct RelaySession {
    session_id: Option<String>,
    protocol_version: Option<String>,
}

/// Run the relay: read newline-delimited JSON-RPC messages from stdin until
/// EOF, bridge each over POST, and forward server-initiated frames from the
/// per-session GET stream as they arrive.
///
/// # Errors
///
/// Only the stdio side ends the relay: a stdin read error or a stdout write
/// failure. A message the transport could not deliver is answered with a
/// synthesized JSON-RPC error carrying that request's id (see
/// [`transport_error_frames`]) and the relay continues — one failed request
/// must not kill an agent session.
pub(crate) async fn run(
    config: RelayConfig,
    stdin: impl tokio::io::AsyncRead + Unpin,
    mut stdout: impl tokio::io::AsyncWrite + Unpin,
) -> anyhow::Result<()> {
    let http = reqwest::Client::new();
    let session = Arc::new(RwLock::new(RelaySession::default()));
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    let (server_frames_tx, mut server_frames_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    loop {
        // Both halves of the transport race here: a stdin line, and a frame the
        // server pushed down the GET stream. The GET reader never writes stdout
        // itself — frames queue on the channel and THIS loop writes them, so a
        // frame can never interleave with a POST response mid-line.
        // `Lines::next_line` is cancel safe, so losing the race to a frame
        // reads no stdin bytes.
        let line = tokio::select! {
            line = lines.next_line() => line?,
            Some(frame) = server_frames_rx.recv() => {
                write_frame(&mut stdout, &frame).await?;
                continue;
            }
        };
        let Some(line) = line else {
            break; // stdin EOF
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_initialize = is_initialize_message(trimmed);
        let (frames, delivered) =
            match post_message(&http, &config, &session, trimmed, is_initialize).await {
                Ok(frames) => (frames, true),
                Err(error) => {
                    // No JSON-RPC answer exists for this message: the request's
                    // own id is answered here instead, so the agent's pending
                    // call settles rather than hanging on a dead transport.
                    eprintln!("pulse mcp: relay request failed: {error}");
                    (transport_error_frames(trimmed, &error.to_string()), false)
                }
            };
        if is_initialize && delivered {
            // The protocol version rides the initialize RESULT.
            for frame in &frames {
                learn_session(frame, &session).await;
            }
            // One GET stream per session, started after initialize: the
            // server-initiated half of the transport. A server that does not
            // answer GET (405) simply produces nothing.
            spawn_server_stream(
                http.clone(),
                config.clone(),
                Arc::clone(&session),
                server_frames_tx.clone(),
            );
        }
        // The stdio side speaks bare newline-delimited JSON-RPC, so the SSE
        // envelope is unwrapped by then: every `data:` payload is one stdout
        // line. An empty body (a 202 ack for a notification) writes nothing.
        for frame in frames {
            write_frame(&mut stdout, &frame).await?;
        }
    }

    // stdin closed: drain whatever the GET stream still holds, then stop.
    drop(server_frames_tx);
    while let Ok(frame) = server_frames_rx.try_recv() {
        write_frame(&mut stdout, &frame).await?;
    }
    Ok(())
}

/// POST one JSON-RPC message; return the payloads to write to stdout, one per
/// string — a `data:`-unwrapped list for an SSE response, the bare body for a
/// JSON one, empty for a bodyless ack.
///
/// # Errors
///
/// `Err` means the POST produced no JSON-RPC answer at all: the connection
/// failed, the status was not a success, or the response body could not be
/// read. The error text is the transport's own (a non-2xx body included); the
/// caller answers the request's id with it.
async fn post_message(
    http: &reqwest::Client,
    config: &RelayConfig,
    session: &RwLock<RelaySession>,
    message: &str,
    is_initialize: bool,
) -> anyhow::Result<Vec<String>> {
    let mut request = http
        .post(format!("{}/mcp", config.base))
        .bearer_auth(&config.token)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(message.to_owned());
    {
        let mut guard = session.write().await;
        if is_initialize {
            // An initialize starts a NEW session: it must never carry the
            // previous one's identity. After a `pulse serve` restart the stored
            // id is dead, and a request that carries it is refused — with
            // nothing ever clearing it, the relay would stay wedged for the
            // agent process's whole life. Drop both learned values before the
            // request; the response re-issues the session id, and
            // `learn_session` re-learns the protocol version.
            guard.session_id = None;
            guard.protocol_version = None;
        } else {
            if let Some(id) = &guard.session_id {
                request = request.header("mcp-session-id", id);
            }
            if let Some(version) = &guard.protocol_version {
                request = request.header("mcp-protocol-version", version);
            }
        }
    }
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("POST {}/mcp failed: {error}", config.base))?;
    let status = response.status();
    let session_id = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let body = response
        .text()
        .await
        .map_err(|error| anyhow::anyhow!("reading the response body failed: {error}"))?;
    if !status.is_success() {
        // A non-2xx body is NOT a JSON-RPC response: forwarding it verbatim
        // would desync the agent's stdio stream (it would parse a route error
        // body as the answer to its request). The body text rides the
        // synthesized error's `message` instead.
        return Err(anyhow::anyhow!("{status}: {}", body.trim()));
    }
    if let Some(id) = session_id {
        // The session identity rides the FIRST answered POST.
        session.write().await.session_id = Some(id);
    }
    if content_type.contains("text/event-stream") {
        // Unwrap the SSE envelope: each `data:` payload is one stdio frame.
        let frames: Vec<String> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| !data.trim().is_empty())
            .map(|data| format!("{data}\n"))
            .collect();
        Ok(frames)
    } else if body.trim().is_empty() {
        // The 202 ack for a notification: nothing for the stdio side.
        Ok(Vec::new())
    } else {
        // A JSON body: forward verbatim as one line.
        Ok(vec![format!("{body}\n")])
    }
}

/// The JSON-RPC error code the relay uses for a transport failure: `-32000`,
/// the head of the implementation-defined server-error range (-32000..-32099)
/// — which is what this is, the transport could not produce an answer.
const TRANSPORT_ERROR_CODE: i64 = -32000;

/// Answer one message whose transport failed. The request's own `id` is echoed
/// so the agent's pending call settles, and `detail` (the transport's text, a
/// non-2xx body included) becomes `error.message`. A notification carries no
/// `id` and has nothing to answer; an unparseable line cannot be answered:
/// both produce no frame.
fn transport_error_frames(message: &str, detail: &str) -> Vec<String> {
    let Some(id) = serde_json::from_str::<serde_json::Value>(message)
        .ok()
        .and_then(|value| value.get("id").cloned())
    else {
        return Vec::new();
    };
    let frame = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": TRANSPORT_ERROR_CODE, "message": detail },
    });
    vec![format!("{frame}\n")]
}

/// Is this message the `initialize` request? The JSON-RPC method is a top-level
/// field, so it is PARSED, never text-matched: matching source spellings misses
/// other valid spacing (a tab, two spaces, `"method" :`) and false-positives on
/// the same text inside a string parameter.
fn is_initialize_message(message: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(message).is_ok_and(|value| {
        value.get("method").and_then(serde_json::Value::as_str) == Some("initialize")
    })
}

/// Learn the session id and protocol version out of an initialize response.
async fn learn_session(response: &str, session: &RwLock<RelaySession>) {
    // The response may be raw JSON or an SSE frame; both carry the same
    // fields. Find the protocolVersion the SERVER named.
    let version = response
        .split(['\n', '\r'])
        .filter_map(|line| line.strip_prefix("data: "))
        .chain(std::iter::once(response))
        .find_map(|candidate| {
            serde_json::from_str::<serde_json::Value>(candidate.trim())
                .ok()?
                .get("result")?
                .get("protocolVersion")?
                .as_str()
                .map(str::to_owned)
        });
    if let Some(version) = version {
        session.write().await.protocol_version = Some(version);
    }
}

/// Open the GET stream (the server-initiated half) and forward every SSE
/// frame's payload to stdout until the connection closes. Failures are quiet:
/// the stream is optional on the wire.
fn spawn_server_stream(
    http: reqwest::Client,
    config: RelayConfig,
    session: Arc<RwLock<RelaySession>>,
    tx: tokio::sync::mpsc::UnboundedSender<String>,
) {
    tokio::spawn(async move {
        use futures_core::Stream as _;
        use std::future::poll_fn;

        let mut request = http
            .get(format!("{}/mcp", config.base))
            .bearer_auth(&config.token)
            .header("accept", "text/event-stream");
        {
            let guard = session.read().await;
            if let Some(id) = &guard.session_id {
                request = request.header("mcp-session-id", id);
            }
            if let Some(version) = &guard.protocol_version {
                request = request.header("mcp-protocol-version", version);
            }
        }
        let Ok(response) = request.send().await else {
            return;
        };
        if !response.status().is_success() {
            return;
        }
        // Frame-split the SSE body: payload = everything after "data: ".
        let mut buffer = String::new();
        let body = response.bytes_stream();
        tokio::pin!(body);
        loop {
            let Some(Ok(chunk)) = poll_fn(|cx| body.as_mut().poll_next(cx)).await else {
                break;
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find("\n\n") {
                let block: String = buffer.drain(..pos + 2).collect();
                for line in block.lines() {
                    if let Some(data) = line.strip_prefix("data: ") {
                        if data.trim().is_empty() {
                            continue; // a keep-alive, not a message
                        }
                        if tx.send(format!("{data}\n")).is_err() {
                            return;
                        }
                    }
                }
            }
        }
    });
}

/// Write one protocol frame to stdout — the ONLY writes this module performs.
async fn write_frame(
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    frame: &str,
) -> anyhow::Result<()> {
    stdout.write_all(frame.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::{Read as _, Write as _};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // `AsyncBufReadExt`/`AsyncWriteExt`/`BufReader` come in through `super::*`
    // (the relay's own imports); `AsyncReadExt` is the one this module adds.
    use tokio::io::AsyncReadExt as _;

    use super::*;

    /// The initialize request and the result the sink answers it with (the
    /// server's own shape: the protocol version is in the RESULT).
    const INITIALIZE_LINE: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"0\"}}}\n";
    const INITIALIZE_RESULT: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"serverInfo\":{\"name\":\"pulse\",\"version\":\"0.1.0\"}}}";
    const TOOLS_RESULT: &str = "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}";

    /// One request the sink accepted, as the relay put it on the wire.
    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        head: String,
        body: String,
    }

    impl Recorded {
        /// One request header's value, case-insensitive.
        fn header(&self, name: &str) -> Option<String> {
            self.head.lines().find_map(|line| {
                let (field, value) = line.split_once(':')?;
                field
                    .trim()
                    .eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
        }
    }

    /// What the sink answers one accepted connection with.
    enum Answer {
        /// 200 with a JSON body; `session_id` rides the response headers.
        Json {
            session_id: Option<String>,
            body: String,
        },
        /// A non-2xx status with a body — the wire's refusal shape.
        Refusal { status: u16, body: String },
        /// Accept, then close without answering: a transport failure.
        Drop,
        /// An SSE stream: the headers, then `frames` chunked as they go, the
        /// last one after `gap_ms`, then held open — the server-initiated half.
        Stream { frames: Vec<String>, gap_ms: u64 },
    }

    /// A loopback HTTP sink scripted per request. It records what the relay put
    /// on the wire and answers from the test's own script, so a test can drive
    /// the transport failures and mid-session frames a real server will not
    /// produce on demand.
    struct Sink {
        base: String,
        requests: Arc<Mutex<Vec<Recorded>>>,
        _server: std::thread::JoinHandle<()>,
    }

    impl Sink {
        fn start(script: impl Fn(&Recorded, usize) -> Answer + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
            let port = listener.local_addr().expect("local addr").port();
            let requests: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
            let captured = Arc::clone(&requests);
            let script = Arc::new(script);
            let server = std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let captured = Arc::clone(&captured);
                    let script = Arc::clone(&script);
                    std::thread::spawn(move || {
                        let Some((head, body)) = read_request(&mut stream) else {
                            return; // a probe, or a dropped connection
                        };
                        let method = head
                            .split_whitespace()
                            .next()
                            .unwrap_or_default()
                            .to_owned();
                        let record = Recorded { method, head, body };
                        let index = {
                            let mut guard = captured.lock().expect("sink lock");
                            guard.push(record.clone());
                            guard.len() - 1
                        };
                        match script(&record, index) {
                            Answer::Json { session_id, body } => {
                                let session = session_id
                                    .map(|id| format!("mcp-session-id: {id}\r\n"))
                                    .unwrap_or_default();
                                let reply = format!(
                                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n{session}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = stream.write_all(reply.as_bytes());
                            }
                            Answer::Refusal { status, body } => {
                                let reply = format!(
                                    "HTTP/1.1 {status} Refused\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                                    body.len()
                                );
                                let _ = stream.write_all(reply.as_bytes());
                            }
                            Answer::Drop => {}
                            Answer::Stream { frames, gap_ms } => {
                                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
                                let _ = stream.write_all(head.as_bytes());
                                let _ = stream.flush();
                                for (index, frame) in frames.iter().enumerate() {
                                    if index + 1 == frames.len() {
                                        std::thread::sleep(Duration::from_millis(gap_ms));
                                    }
                                    let _ = write_chunk(&mut stream, frame.as_bytes());
                                }
                                std::thread::sleep(Duration::from_secs(5));
                            }
                        }
                    });
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                requests,
                _server: server,
            }
        }

        fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().expect("sink lock").clone()
        }
    }

    /// Read one request off the wire: `(head, body)`. `None` for a connection
    /// that carried no request at all.
    fn read_request(stream: &mut TcpStream) -> Option<(String, String)> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let headers_end = loop {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..headers_end]).into_owned();
        let content_length: usize = head
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buf.len() - headers_end < content_length {
            let n = stream.read(&mut chunk).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let end = (headers_end + content_length).min(buf.len());
        let body = String::from_utf8_lossy(&buf[headers_end..end]).into_owned();
        Some((head, body))
    }

    /// One HTTP/1.1 chunk (the SSE body stays open — no terminal chunk).
    fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
        stream.write_all(format!("{:x}\r\n", bytes.len()).as_bytes())?;
        stream.write_all(bytes)?;
        stream.write_all(b"\r\n")?;
        stream.flush()
    }

    /// The relay's three stdio ends, over in-memory pipes.
    struct Relay {
        stdin: tokio::io::DuplexStream,
        stdout: BufReader<tokio::io::DuplexStream>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    impl Relay {
        fn start(base: &str) -> Self {
            let (stdin, stdin_rx) = tokio::io::duplex(64 * 1024);
            let (stdout_tx, stdout_rx) = tokio::io::duplex(64 * 1024);
            let config = RelayConfig {
                base: base.to_owned(),
                token: "test-token".to_owned(),
            };
            let task = tokio::spawn(run(config, stdin_rx, stdout_tx));
            Self {
                stdin,
                stdout: BufReader::new(stdout_rx),
                task,
            }
        }

        async fn send(&mut self, line: &str) {
            self.stdin
                .write_all(line.as_bytes())
                .await
                .expect("write the stdin line");
        }

        /// One stdout line, guarded so a broken relay fails instead of hanging.
        async fn frame(&mut self) -> serde_json::Value {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(5), self.stdout.read_line(&mut line))
                    .await
                    .expect("a frame crossed stdout before the timeout")
                    .expect("read stdout");
            assert!(read > 0, "stdout closed with no frame");
            serde_json::from_str(line.trim()).expect("the frame is JSON")
        }

        /// Close stdin — the relay's EOF — without waiting for the exit.
        async fn close_stdin(&mut self) {
            self.stdin.shutdown().await.expect("close stdin");
        }

        /// Everything still on stdout, read to EOF (which needs the relay gone).
        async fn rest(&mut self) -> String {
            let mut rest = String::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                self.stdout.read_to_string(&mut rest),
            )
            .await
            .expect("stdout reached EOF after the relay exited")
            .expect("read the rest of stdout");
            rest
        }

        /// Close stdin and wait for the relay to exit.
        async fn finish(mut self) {
            self.stdin.shutdown().await.expect("close stdin");
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("the relay exits after stdin closes")
                .expect("join the relay")
                .expect("stdin EOF is not an error");
        }
    }

    /// C1.4: the method is PARSED, not text-matched — other spacing still
    /// matches, and the same text inside a parameter does not.
    #[test]
    fn initialize_is_detected_by_the_parsed_method() {
        for message in [
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"method" : "initialize"}"#,
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\t\"initialize\"}",
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
        ] {
            assert!(
                is_initialize_message(message),
                "the parsed method is initialize: {message}"
            );
        }
        for message in [
            // The same text as a STRING PARAMETER: the source-text match called
            // this an initialize.
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"arguments":{"method":"initialize"}}}"#,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#,
            r#"{"jsonrpc":"2.0","id":4,"method":"notifications/initialized"}"#,
            "not json at all",
        ] {
            assert!(
                !is_initialize_message(message),
                "not an initialize: {message}"
            );
        }
    }

    /// C1.2: a POST that never reaches a server still answers the request's id,
    /// and a notification (no id) is not answered at all.
    #[tokio::test]
    async fn a_dropped_post_answers_the_request_id_and_not_a_notification() {
        let sink = Sink::start(|_request, _index| Answer::Drop);
        let mut relay = Relay::start(&sink.base);

        relay
            .send("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/list\"}\n")
            .await;
        relay
            .send("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await;
        relay
            .send("{\"jsonrpc\":\"2.0\",\"id\":8,\"method\":\"tools/list\"}\n")
            .await;

        let frame = relay.frame().await;
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], 7, "the request's own id is answered: {frame}");
        assert!(
            frame["error"]["message"]
                .as_str()
                .expect("an error message")
                .contains("failed"),
            "the transport's text rides the error: {frame}"
        );

        // The notification was NOT answered: the next frame is id 8 — proof the
        // relay stayed alive and answered in order.
        let frame = relay.frame().await;
        assert_eq!(frame["id"], 8, "the relay stayed alive: {frame}");

        relay.finish().await;
    }

    /// C1.2: a non-2xx body is never forwarded as if it were the response; it
    /// becomes the synthesized error's message.
    #[tokio::test]
    async fn a_non_2xx_body_rides_the_synthesized_error_not_stdout() {
        const BODY: &str = r#"{"code":"session_not_found","message":"unknown session"}"#;
        let sink = Sink::start(|_request, _index| Answer::Refusal {
            status: 404,
            body: BODY.to_owned(),
        });
        let mut relay = Relay::start(&sink.base);

        relay
            .send("{\"jsonrpc\":\"2.0\",\"id\":\"abc\",\"method\":\"tools/list\"}\n")
            .await;

        let frame = relay.frame().await;
        assert_eq!(frame["id"], "abc", "the string id is echoed: {frame}");
        assert!(
            frame["error"]["message"]
                .as_str()
                .expect("an error message")
                .contains("session_not_found"),
            "the refusal body rides error.message: {frame}"
        );
        assert!(
            frame.get("code").is_none(),
            "the route's own body is NOT the frame: {frame}"
        );

        relay.close_stdin().await;
        let rest = relay.rest().await;
        assert!(rest.is_empty(), "nothing else crossed stdout: {rest}");
        relay.finish().await;
    }

    /// C1.1: the initialize POST carries no session id (the stale one is
    /// dropped), while the requests after it carry the learned id and version.
    #[tokio::test]
    async fn initialize_drops_the_learned_session_and_later_requests_carry_it() {
        let sink = Sink::start(|request, _index| Answer::Json {
            session_id: Some("s-1".to_owned()),
            body: if request.body.contains("\"method\":\"initialize\"") {
                INITIALIZE_RESULT.to_owned()
            } else {
                TOOLS_RESULT.to_owned()
            },
        });
        let mut relay = Relay::start(&sink.base);

        relay.send(INITIALIZE_LINE).await;
        relay
            .send("{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n")
            .await;
        // The recovery initialize an agent sends after a `pulse serve` restart.
        relay.send(INITIALIZE_LINE).await;
        for _ in 0..3 {
            let frame = relay.frame().await;
            assert_eq!(frame["jsonrpc"], "2.0", "an answered POST: {frame}");
        }
        relay.finish().await;

        let posts: Vec<Recorded> = sink
            .requests()
            .into_iter()
            .filter(|request| request.method == "POST")
            .collect();
        assert_eq!(posts.len(), 3, "one POST per stdin line: {posts:?}");
        assert!(
            posts[0].header("mcp-session-id").is_none(),
            "the first initialize carries no session id: {}",
            posts[0].head
        );
        assert_eq!(
            posts[1].header("mcp-session-id").as_deref(),
            Some("s-1"),
            "the learned session id rides the next request: {}",
            posts[1].head
        );
        assert_eq!(
            posts[1].header("mcp-protocol-version").as_deref(),
            Some("2025-06-18"),
            "the learned protocol version rides the next request: {}",
            posts[1].head
        );
        assert!(
            posts[2].header("mcp-session-id").is_none(),
            "the recovery initialize carries NO stale session id: {}",
            posts[2].head
        );
        assert!(
            posts[2].header("mcp-protocol-version").is_none(),
            "the recovery initialize carries no stale protocol version: {}",
            posts[2].head
        );
    }

    /// C1.3: a server-initiated frame reaches stdout while the session is LIVE —
    /// before stdin closes, which is when the old relay drained it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_server_initiated_frame_reaches_stdout_mid_session() {
        let sink = Sink::start(|request, _index| {
            if request.method == "GET" {
                Answer::Stream {
                    frames: vec![
                        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"n\":1}}\n\n".to_owned(),
                        "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"n\":2}}\n\n".to_owned(),
                    ],
                    gap_ms: 150,
                }
            } else {
                Answer::Json {
                    session_id: Some("s-1".to_owned()),
                    body: INITIALIZE_RESULT.to_owned(),
                }
            }
        });
        let mut relay = Relay::start(&sink.base);

        relay.send(INITIALIZE_LINE).await;
        let init = relay.frame().await;
        assert_eq!(
            init["result"]["protocolVersion"], "2025-06-18",
            "the initialize answer crossed: {init}"
        );

        // Both pushed frames arrive with stdin STILL OPEN.
        let first = relay.frame().await;
        assert_eq!(first["params"]["n"], 1, "{first}");
        let second = relay.frame().await;
        assert_eq!(second["params"]["n"], 2, "{second}");

        relay.finish().await;
    }
}
