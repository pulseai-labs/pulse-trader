//! The `pulse mcp` relay (r3.s3.w5): an agent that today speaks MCP-over-stdio
//! keeps doing exactly that, while the frames ride HTTP to the always-on
//! server's `/mcp` route.
//!
//! A **byte-level bridge**, deliberately: one POST per stdin message (the
//! request body forwarded unchanged), the response body written to stdout
//! unchanged, the `Mcp-Session-Id` the server hands back learned once and
//! attached to every later request, one GET SSE stream per connection for the
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
/// per-connection GET stream.
///
/// # Errors
///
/// An unreachable server or an initialize that never yields a session id ends
/// the relay with the named error; a per-message failure is written to stderr
/// and the relay continues (one bad line must not kill an agent session).
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

    while let Some(line) = lines.next_line().await? {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let is_initialize = trimmed.contains("\"method\":\"initialize\"")
            || trimmed.contains("\"method\": \"initialize\"");
        let body = post_message(&http, &config, &session, trimmed, is_initialize).await;
        match body {
            Ok(frames) => {
                if is_initialize {
                    // The session identity rides the FIRST answered POST.
                    for frame in &frames {
                        learn_session(frame, &session).await;
                    }
                    // One GET stream per connection, started after initialize:
                    // the server-initiated half of the transport. A server
                    // that does not answer GET (405) simply produces nothing.
                    spawn_server_stream(
                        http.clone(),
                        config.clone(),
                        Arc::clone(&session),
                        server_frames_tx.clone(),
                    );
                }
                // The stdio side speaks bare newline-delimited JSON-RPC, so
                // the SSE envelope is unwrapped here: every `data:` payload
                // becomes one stdout line. An empty body (a 202 ack for a
                // notification) writes nothing.
                for frame in frames {
                    write_frame(&mut stdout, &frame).await?;
                }
            }
            Err(error) => {
                eprintln!("pulse mcp: relay request failed: {error}");
            }
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
        let guard = session.read().await;
        if let Some(id) = &guard.session_id {
            request = request.header("mcp-session-id", id);
        }
        if !is_initialize && let Some(version) = &guard.protocol_version {
            request = request.header("mcp-protocol-version", version);
        }
    }
    let response = request
        .send()
        .await
        .map_err(|error| anyhow::anyhow!("POST {}/mcp failed: {error}", config.base))?;
    if let Some(id) = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
    {
        session.write().await.session_id = Some(id.to_owned());
    }
    let status = response.status();
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
    if content_type.contains("text/event-stream") {
        // Unwrap the SSE envelope: each `data:` payload is one stdio frame.
        let frames: Vec<String> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|data| !data.trim().is_empty())
            .map(|data| format!("{data}\n"))
            .collect();
        if frames.is_empty() && !status.is_success() {
            // An SSE-shaped error still carries something to show.
            return Ok(vec![format!("{body}\n")]);
        }
        Ok(frames)
    } else if body.trim().is_empty() {
        // The 202 ack for a notification: nothing for the stdio side.
        Ok(Vec::new())
    } else {
        // A JSON body (or an error body): forward verbatim as one line.
        Ok(vec![format!("{body}\n")])
    }
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
