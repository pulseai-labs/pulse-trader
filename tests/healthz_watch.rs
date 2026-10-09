//! r4.s2.w4 (demo line d71) — the unauthenticated `/healthz` and the `pulse watch` alert core.
//!
//! Two halves:
//!
//! 1. **`/healthz`** over the real router (the shared in-process harness): 200
//!    with exactly the two keys and no `Authorization` header, no `token_audit`
//!    row, `degraded` without a paper runtime and `ok` with one, while every
//!    other route keeps its D5 refusal.
//! 2. **`pulse watch`** over the real one-cycle core: scripted probe/host/marker
//!    results and a scripted clock drive every branch (the three-failure
//!    threshold, de-duplication, the six-hour reminder, the recovery, the
//!    "unreachable" versus "down" texts, the at-once start-limit alert, the
//!    one-watcher-error-per-distinct-error rule), and a local fake ntfy listener
//!    runs the real probe and the real notifier — never the real `ntfy.sh`,
//!    never the Mini.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use pulse::{
    ALERT_AFTER_FAILURES, ClockSeam, Db, HostSeam, HttpProbe, LogSeam, MarkerReport, MarkerSeam,
    NotifySeam, NtfyNotifier, ProbeReport, ProbeSeam, STILL_DOWN_REMINDER_SECS, SshMarker,
    WatchConfig, WatchErrorKind, WatchFuture, WatchSeams, run_once,
};
use serde_json::Value;
use support::server::{ServerOptions, spawn_server};
use tempfile::TempDir;

mod support;

// ---------------------------------------------------------------------------
// /healthz (Q1): the one route outside the auth middleware.
// ---------------------------------------------------------------------------

async fn audit_count(db: &Db) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM token_audit")
        .fetch_one(db.pool())
        .await
        .expect("count token_audit")
}

async fn get_healthz(base: &str) -> (reqwest::StatusCode, reqwest::header::HeaderMap, Value) {
    let response = reqwest::Client::new()
        .get(format!("{base}/healthz"))
        .send()
        .await
        .expect("GET /healthz");
    let status = response.status();
    let headers = response.headers().clone();
    let text = response.text().await.expect("read the /healthz body");
    let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, headers, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_answers_two_keys_without_a_token_and_writes_no_audit_row() {
    let ts = spawn_server(ServerOptions::default()).await;
    let before = audit_count(&ts.db).await;

    let (status, headers, body) = get_healthz(&ts.base).await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "/healthz answers a request with no Authorization header"
    );
    let mut keys: Vec<String> = body
        .as_object()
        .expect("a JSON object")
        .keys()
        .cloned()
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["api_version".to_owned(), "status".to_owned()],
        "the body is exactly the two keys"
    );
    assert_eq!(body["api_version"], pulse::API_VERSION);
    assert_eq!(body["status"], "degraded", "no paper runtime is installed");
    assert_eq!(
        headers
            .get("x-pulse-api-version")
            .and_then(|value| value.to_str().ok()),
        Some("1"),
        "/healthz keeps the router-level API-version layer"
    );

    assert_eq!(
        audit_count(&ts.db).await,
        before,
        "/healthz writes no token_audit row"
    );

    // …and the refusal counter is live: a tokenless request to any other route
    // adds exactly one row, so the equality above is not a dead count.
    let refused = reqwest::Client::new()
        .get(format!("{}/api/v1/handshake", ts.base))
        .send()
        .await
        .expect("GET handshake");
    assert_eq!(refused.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(
        audit_count(&ts.db).await,
        before + 1,
        "a refusal still writes its audit row"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthz_reports_ok_while_the_paper_runtime_runs() {
    let ts = spawn_server(ServerOptions {
        paper: Some(Box::new(|db: &Db, dir: &std::path::Path| {
            support::paper::PaperHost::spawn(db, dir)
        })),
        ..ServerOptions::default()
    })
    .await;
    let (status, _, body) = get_healthz(&ts.base).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["status"], "ok", "the runtime's control handle is live");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_other_route_still_refuses_a_tokenless_request() {
    let ts = spawn_server(ServerOptions::default()).await;
    let before = audit_count(&ts.db).await;
    let client = reqwest::Client::new();
    // /healthz is the ONLY route outside the middleware: the handshake, a
    // command route, an ops route and the MCP mount all keep the D5 refusal.
    for path in [
        "/api/v1/handshake",
        "/api/v1/shell-info",
        "/api/v1/ops/no-such-op",
        "/mcp",
    ] {
        let response = client
            .get(format!("{}{path}", ts.base))
            .send()
            .await
            .expect("send");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "{path} must refuse without a token"
        );
        let body: Value = response.json().await.expect("refusal body");
        assert_eq!(body["code"], "token_missing", "{path}");
    }
    assert_eq!(
        audit_count(&ts.db).await,
        before + 4,
        "each refusal writes one row"
    );
}

// ---------------------------------------------------------------------------
// `pulse watch` (Q2/C3/C4): the one-cycle state machine over injected seams.
// ---------------------------------------------------------------------------

/// Lock a scripted/test mutex, recovering from poisoning (a panicking assertion
/// must not take the rest of the suite's doubles down with it) — the same
/// `PoisonError::into_inner` recovery `server::log` uses. `std::sync` on purpose:
/// `parking_lot` is not a direct dependency of this crate, and this item adds none.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The topic the scratch files hold. It stands in for the operator's real secret
/// and is asserted absent from every log line and pushed body.
const TEST_TOPIC: &str = "watch-topic-under-test-0123456789";

/// A scripted seam set: probe/host/marker results come from queues whose LAST
/// entry repeats, while the clock, the pushes and the log lines are recorded.
#[derive(Default)]
struct Scripted {
    now: Mutex<u64>,
    probes: Mutex<VecDeque<ProbeReport>>,
    hosts: Mutex<VecDeque<bool>>,
    markers: Mutex<VecDeque<MarkerReport>>,
    pushes: Mutex<Vec<(String, String)>>,
    logs: Mutex<Vec<String>>,
    probe_calls: AtomicUsize,
    fail_push: AtomicBool,
}

impl Scripted {
    fn push_probe(&self, report: ProbeReport) {
        lock(&self.probes).push_back(report);
    }

    fn push_host(&self, up: bool) {
        lock(&self.hosts).push_back(up);
    }

    fn push_marker(&self, report: MarkerReport) {
        lock(&self.markers).push_back(report);
    }

    fn advance(&self, seconds: u64) {
        *lock(&self.now) += seconds;
    }

    fn pushes(&self) -> Vec<(String, String)> {
        lock(&self.pushes).clone()
    }

    fn logs(&self) -> Vec<String> {
        lock(&self.logs).clone()
    }
}

/// Pop the next queued value; the last one repeats, so a test queues one value
/// per distinct phase, not one per cycle.
fn take<T: Copy>(queue: &Mutex<VecDeque<T>>, fallback: T) -> T {
    let mut queue = lock(queue);
    if queue.len() > 1 {
        queue.pop_front()
    } else {
        queue.front().copied()
    }
    .unwrap_or(fallback)
}

impl ProbeSeam for Scripted {
    fn probe(&self, _url: String) -> WatchFuture<ProbeReport> {
        self.probe_calls.fetch_add(1, Ordering::SeqCst);
        let report = take(&self.probes, ProbeReport::Down);
        Box::pin(async move { report })
    }
}

impl HostSeam for Scripted {
    fn host_reachable(&self, _ssh_host: String) -> WatchFuture<bool> {
        let up = take(&self.hosts, true);
        Box::pin(async move { up })
    }
}

impl MarkerSeam for Scripted {
    fn marker(&self, _ssh_host: String) -> WatchFuture<MarkerReport> {
        let report = take(&self.markers, MarkerReport::Absent);
        Box::pin(async move { report })
    }
}

impl NotifySeam for Scripted {
    fn notify(&self, title: String, body: String) -> WatchFuture<Result<(), String>> {
        if self.fail_push.load(Ordering::SeqCst) {
            return Box::pin(async { Err("synthetic push failure".to_owned()) });
        }
        lock(&self.pushes).push((title, body));
        Box::pin(async { Ok(()) })
    }
}

impl ClockSeam for Scripted {
    fn now_unix_secs(&self) -> u64 {
        *lock(&self.now)
    }
}

impl LogSeam for Scripted {
    fn log(&self, line: String) {
        lock(&self.logs).push(line);
    }
}

/// The scratch config every watcher test runs over: a real 0600 topic file, a
/// real state path, and a url/ssh host the seam set replaces in the scripted
/// cases.
struct Scratch {
    _tmp: TempDir,
    config: WatchConfig,
}

fn scratch() -> Scratch {
    let tmp = TempDir::new().expect("tempdir");
    let topic = tmp.path().join("topic");
    std::fs::write(&topic, format!("{TEST_TOPIC}\n")).expect("write the topic file");
    set_mode(&topic, 0o600);
    let config = WatchConfig {
        url: "http://127.0.0.1:9".to_owned(),
        ssh_host: "macmini".to_owned(),
        topic_file: topic,
        state_file: tmp.path().join("state"),
    };
    Scratch { _tmp: tmp, config }
}

fn set_mode(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("set mode");
}

fn file_mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

fn seams(scripted: &Scripted) -> WatchSeams<'_> {
    WatchSeams {
        probe: scripted,
        host: scripted,
        marker: scripted,
        notify: scripted,
        clock: scripted,
        log: scripted,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_failed_probes_alert_once_remind_at_six_hours_and_recover_once() {
    let scratch = scratch();
    let scripted = Scripted::default();
    // The outage lasts through the dedup and reminder runs (1-6), then the
    // probe succeeds for good (run 7 onward).
    for _ in 0..(ALERT_AFTER_FAILURES + 3) {
        scripted.push_probe(ProbeReport::Down);
    }
    scripted.push_probe(ProbeReport::Ok);
    scripted.push_host(true);
    scripted.push_marker(MarkerReport::Absent);
    let seams = seams(&scripted);

    // Runs 1 and 2 are below the threshold: nothing is pushed.
    for _ in 0..2 {
        let run = run_once(&scratch.config, &seams).await;
        assert!(run.error.is_none());
        assert!(run.pushed_bodies.is_empty(), "no alert below the threshold");
    }
    let state = std::fs::read_to_string(&scratch.config.state_file).expect("state file");
    assert!(
        state.contains("failures=2"),
        "the counter is persisted: {state}"
    );

    // Run 3: the one alert for this incident.
    let run = run_once(&scratch.config, &seams).await;
    assert_eq!(run.pushed_bodies, ["prod is down: service down"]);
    assert_eq!(
        scripted.pushes(),
        [(
            "prod down".to_owned(),
            "prod is down: service down".to_owned()
        )]
    );

    // Run 4: de-duplicated — one alert per incident.
    let run = run_once(&scratch.config, &seams).await;
    assert!(run.pushed_bodies.is_empty(), "one alert per incident");

    // Run 5: six hours on, one reminder — and no second one on run 6.
    scripted.advance(STILL_DOWN_REMINDER_SECS);
    let run = run_once(&scratch.config, &seams).await;
    assert_eq!(run.pushed_bodies, ["prod still down: service down"]);
    let run = run_once(&scratch.config, &seams).await;
    assert!(
        run.pushed_bodies.is_empty(),
        "one reminder per six hours, not per cycle"
    );

    // Run 7: the probe succeeds — exactly one "recovered"…
    let run = run_once(&scratch.config, &seams).await;
    assert_eq!(run.pushed_bodies, ["prod recovered"]);
    // …and nothing while it stays up.
    let run = run_once(&scratch.config, &seams).await;
    assert!(run.pushed_bodies.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_host_names_filevault_and_a_degraded_recovery_is_named() {
    let scratch = scratch();
    let scripted = Scripted::default();
    // Runs 1-3 fail with the host unreachable: the FileVault text, not "service down".
    for _ in 0..ALERT_AFTER_FAILURES {
        scripted.push_probe(ProbeReport::Down);
    }
    // Run 4 recovers, but the paper runtime is not running: the recovery names it.
    scripted.push_probe(ProbeReport::Degraded);
    scripted.push_host(false);
    scripted.push_marker(MarkerReport::Absent);
    let seams = seams(&scripted);

    let first = run_once(&scratch.config, &seams).await;
    assert!(first.pushed_bodies.is_empty());
    let _ = run_once(&scratch.config, &seams).await;
    let third = run_once(&scratch.config, &seams).await;
    assert_eq!(
        third.pushed_bodies,
        ["prod is down: Mini unreachable — it may need a FileVault unlock"]
    );

    let fourth = run_once(&scratch.config, &seams).await;
    assert_eq!(
        fourth.pushed_bodies,
        ["prod recovered, paper runtime degraded"]
    );

    // A degraded probe on its own never alerts, and the host recovering later
    // needs three fresh failures to alert again.
    let fifth = run_once(&scratch.config, &seams).await;
    assert!(
        fifth.pushed_bodies.is_empty(),
        "degraded alone never alerts"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_limit_marker_alerts_at_once_and_then_reminds() {
    let scratch = scratch();
    let scripted = Scripted::default();
    scripted.push_probe(ProbeReport::Down);
    scripted.push_host(true);
    scripted.push_marker(MarkerReport::Present);
    let seams = seams(&scripted);

    // The marker alerts on the FIRST failed probe — the 3-failure threshold does
    // not apply to it.
    let first = run_once(&scratch.config, &seams).await;
    assert_eq!(
        first.pushed_bodies,
        ["start limit reached on prod — run just prod-reset"]
    );
    assert_eq!(scripted.pushes()[0].0, "prod start limit");

    // One alert per incident: the marker alone does not repeat…
    let second = run_once(&scratch.config, &seams).await;
    assert!(second.pushed_bodies.is_empty(), "one alert per incident");

    // …and the six-hour reminder covers the still-tripped limit.
    scripted.advance(STILL_DOWN_REMINDER_SECS);
    let third = run_once(&scratch.config, &seams).await;
    assert_eq!(
        third.pushed_bodies,
        ["prod still down: start limit reached on prod — run just prod-reset"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_topic_file_raises_one_watcher_error_and_never_probes() {
    let scratch = scratch();
    std::fs::remove_file(&scratch.config.topic_file).expect("remove the topic file");
    let scripted = Scripted::default();
    let seams = seams(&scripted);

    let first = run_once(&scratch.config, &seams).await;
    assert_eq!(first.error, Some(WatchErrorKind::TopicMissing));
    assert_eq!(
        first.pushed_bodies,
        ["watcher error: the topic file is missing"]
    );

    let second = run_once(&scratch.config, &seams).await;
    assert_eq!(
        second.error,
        Some(WatchErrorKind::TopicMissing),
        "a watcher error still fails the run"
    );
    assert!(
        second.pushed_bodies.is_empty(),
        "one watcher error per distinct error, not every minute"
    );
    assert_eq!(scripted.pushes().len(), 1);
    assert_eq!(
        scripted.probe_calls.load(Ordering::SeqCst),
        0,
        "with no channel to report an outage on, nothing is probed"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_file_with_the_wrong_mode_is_a_watcher_error() {
    let scratch = scratch();
    set_mode(&scratch.config.topic_file, 0o644);
    let scripted = Scripted::default();
    let run = run_once(&scratch.config, &seams(&scripted)).await;
    assert_eq!(run.error, Some(WatchErrorKind::TopicMode));
    assert_eq!(
        run.pushed_bodies,
        ["watcher error: the topic file mode is not 0600"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_watcher_key_is_one_error_and_the_outage_still_alerts() {
    let scratch = scratch();
    let scripted = Scripted::default();
    scripted.push_probe(ProbeReport::Down);
    scripted.push_host(true);
    scripted.push_marker(MarkerReport::KeyMissing);
    let seams = seams(&scripted);

    let first = run_once(&scratch.config, &seams).await;
    assert_eq!(first.error, Some(WatchErrorKind::SshKeyMissing));
    assert_eq!(
        first.pushed_bodies,
        ["watcher error: the ssh key is missing"]
    );

    let second = run_once(&scratch.config, &seams).await;
    assert_eq!(second.error, Some(WatchErrorKind::SshKeyMissing));
    assert!(
        second.pushed_bodies.is_empty(),
        "the same error is not pushed twice"
    );

    // The probe failures still count: the third one alerts the outage, not a
    // second copy of the watcher error.
    let third = run_once(&scratch.config, &seams).await;
    assert_eq!(third.pushed_bodies, ["prod is down: service down"]);
}

// ---------------------------------------------------------------------------
// The real probe and the real notifier against a local fake listener.
// ---------------------------------------------------------------------------

/// One recorded HTTP request.
#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    path: String,
    title: String,
    body: String,
}

/// A local stand-in for the healthz server or the ntfy listener: one thread, a
/// canned reply the test can change, and every request recorded.
struct FakeHttp {
    base: String,
    seen: Arc<Mutex<Vec<Recorded>>>,
    reply: Arc<Mutex<(u16, String)>>,
}

impl FakeHttp {
    fn spawn(status: u16, body: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake listener");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new((status, body.to_owned())));
        let thread_seen = Arc::clone(&seen);
        let thread_reply = Arc::clone(&reply);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let request = read_request(&mut stream);
                let (status, body) = {
                    let reply = lock(&thread_reply);
                    (reply.0, reply.1.clone())
                };
                if let Some(request) = request {
                    lock(&thread_seen).push(request);
                }
                let phrase = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {status} {phrase}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { base, seen, reply }
    }

    fn set_reply(&self, status: u16, body: &str) {
        *lock(&self.reply) = (status, body.to_owned());
    }

    fn requests(&self) -> Vec<Recorded> {
        lock(&self.seen).clone()
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Option<Recorded> {
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                if let Some(end) = head_end(&bytes) {
                    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
                    if bytes.len() >= end + 4 + content_length(&head) {
                        break;
                    }
                }
            }
        }
    }
    let end = head_end(&bytes)?;
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let body = String::from_utf8_lossy(&bytes[(end + 4).min(bytes.len())..]).into_owned();
    let mut request_line = head.lines().next().unwrap_or_default().split_whitespace();
    let method = request_line.next().unwrap_or_default().to_owned();
    let path = request_line.next().unwrap_or_default().to_owned();
    let title = head
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("title"))
        .map_or_else(String::new, |(_, value)| value.trim().to_owned());
    Some(Recorded {
        method,
        path,
        title,
        body,
    })
}

fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn content_length(head: &str) -> usize {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}

/// Nothing pushed or logged may name the topic, the credentials or a URL.
fn assert_no_secret(text: &str) {
    for needle in [TEST_TOPIC, "leak-user", "leak-pass", "http://", "pt_"] {
        assert!(
            !text.contains(needle),
            "a pushed body, a log line or the state file leaks '{needle}': {text}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_probe_reads_ok_degraded_and_down_from_healthz() {
    let healthz = FakeHttp::spawn(200, r#"{"status":"ok","api_version":1}"#);
    let probe = HttpProbe::new();
    assert_eq!(
        probe.probe(format!("{}/", healthz.base)).await,
        ProbeReport::Ok,
        "a trailing slash still targets /healthz"
    );
    healthz.set_reply(200, r#"{"status":"degraded","api_version":1}"#);
    assert_eq!(
        probe.probe(healthz.base.clone()).await,
        ProbeReport::Degraded
    );
    healthz.set_reply(503, "");
    assert_eq!(probe.probe(healthz.base.clone()).await, ProbeReport::Down);
    healthz.set_reply(200, "not json");
    assert_eq!(probe.probe(healthz.base.clone()).await, ProbeReport::Down);
    assert!(
        healthz
            .requests()
            .iter()
            .all(|request| request.path == "/healthz"),
        "{:?}",
        healthz.requests()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_push_is_one_line_and_nothing_leaks() {
    let ntfy = FakeHttp::spawn(500, "");
    let scratch = scratch();
    let mut config = scratch.config.clone();
    // A credential-shaped probe URL on a dead port: the real probe fails fast,
    // and neither the userinfo nor the URL may appear in a log or a push.
    config.url = "http://leak-user:leak-pass@127.0.0.1:1".to_owned();

    let scripted = Scripted::default();
    scripted.push_host(true);
    scripted.push_marker(MarkerReport::Absent);
    let probe = HttpProbe::new();
    let notify = NtfyNotifier::new(ntfy.base.clone(), config.topic_file.clone());
    let seams = WatchSeams {
        probe: &probe,
        host: &scripted,
        marker: &scripted,
        notify: &notify,
        clock: &scripted,
        log: &scripted,
    };

    // Three failing cycles: the third alerts, and the push is refused (500), so
    // the incident stays un-alerted and the next cycle retries.
    let mut last = None;
    for _ in 0..3 {
        last = Some(run_once(&config, &seams).await);
    }
    let third = last.expect("three runs");
    assert!(third.push_failed, "a 500 from ntfy is a failed push");
    assert!(third.pushed_bodies.is_empty());
    assert_eq!(
        ntfy.requests().len(),
        1,
        "the refused push reached the listener but was not counted as delivered"
    );

    // The next cycle retries and delivers the alert.
    ntfy.set_reply(200, "");
    let fourth = run_once(&config, &seams).await;
    assert_eq!(fourth.pushed_bodies, ["prod is down: service down"]);

    let requests = ntfy.requests();
    assert_eq!(requests.len(), 2, "one refused push, then one delivered");
    let delivered = &requests[1];
    assert_eq!(delivered.method, "POST");
    assert_eq!(
        delivered.path,
        format!("/{TEST_TOPIC}"),
        "the topic rides only the ntfy request's path"
    );
    assert_eq!(delivered.title, "prod down");
    assert_eq!(delivered.body, "prod is down: service down");
    assert!(!delivered.body.contains('\n'), "the body is one line");

    for line in scripted.logs() {
        assert_no_secret(&line);
    }
    for (title, body) in scripted.pushes() {
        assert_no_secret(&title);
        assert_no_secret(&body);
    }
    assert_eq!(
        file_mode(&config.state_file),
        0o600,
        "the state file is 0600"
    );
    assert_no_secret(&std::fs::read_to_string(&config.state_file).expect("state file"));
}

// ---------------------------------------------------------------------------
// The marker check's ssh (r4.s2 close-review F2). Two rules:
//
// 1. the check PINNS the dedicated key with `-o IdentitiesOnly=yes`, so no
//    agent key and no `~/.ssh/config` IdentityFile for the same host can
//    authenticate first — that would bypass the key's forced command and hand a
//    timer an unrestricted session;
// 2. only the forced command's two answers are read: the marker `pulse serve`
//    writes at `<data dir>/serve-start-limit` is `Present`, the literal `none` is
//    `Absent`. Everything else — an ssh banner, an error text, empty output, a
//    truncated marker — is `Failed`, an alert that names a failed check, and is
//    NEVER `Present`. A false "start limit reached — run just prod-reset" push
//    costs more than an unanswered check.
//
// The ssh is a fake on this process's PATH, and every call asserts the fake ran
// (r4.s2.w5's incident: a fake ssh without the executable bit let a real
// `ssh macmini` run from a test). The host name cannot resolve, so even a run
// that escaped the fake reaches no machine.
// ---------------------------------------------------------------------------

/// The marker `pulse serve` writes at `<data dir>/serve-start-limit`: five
/// `key=value` lines, in the order `server::start_limit` writes them.
const MARKER_LINE: &str =
    "utc=2026-10-09T00:00:00Z\nunix=1760000000\nstarts=3\nwindow_seconds=900\nlimit=3\n";

/// A temp `ssh` that records each run's argv and prints the marker fixture's
/// bytes. It sits FIRST on this process's `PATH`. With `hangs()` armed it
/// records its PID and sleeps instead of answering — the shape a session that
/// connects and then stalls has (PR-354 fix C6).
struct FakeSsh {
    dir: TempDir,
    log: PathBuf,
    fixture: PathBuf,
    hang: PathBuf,
    pid: PathBuf,
    finished: PathBuf,
}

impl FakeSsh {
    /// How long the hanging fake sleeps: longer than any bound the test waits
    /// on, so a check that returns in time can only have killed it.
    const HANG_SECS: u64 = 45;

    fn new() -> Self {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = TempDir::new().expect("a temp dir for the fake ssh");
        let log = dir.path().join("ssh-runs.log");
        let fixture = dir.path().join("stdout");
        let hang = dir.path().join("hang");
        let pid = dir.path().join("ssh-pid");
        let finished = dir.path().join("finished");
        let script = dir.path().join("ssh");
        std::fs::write(
            &script,
            format!(
                "#!/usr/bin/env bash\n\
                 set -euo pipefail\n\
                 printf 'run\\n' >> '{log}'\n\
                 printf '%s\\n' \"$@\" >> '{log}'\n\
                 printf '%s\\n' \"$$\" > '{pid}'\n\
                 if [ -e '{hang}' ]; then\n\
                   sleep {hang_secs}\n\
                   printf 'finished\\n' >> '{finished}'\n\
                 fi\n\
                 cat '{fixture}'\n",
                log = log.display(),
                pid = pid.display(),
                hang = hang.display(),
                hang_secs = Self::HANG_SECS,
                finished = finished.display(),
                fixture = fixture.display(),
            ),
        )
        .expect("write the fake ssh");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make the fake ssh executable");
        Self {
            dir,
            log,
            fixture,
            hang,
            pid,
            finished,
        }
    }

    /// What the next check's ssh prints on stdout.
    fn prints(&self, stdout: &str) {
        std::fs::write(&self.fixture, stdout).expect("write the marker fixture");
    }

    /// Make every run hang: the fake sleeps after recording its PID.
    fn hangs(&self) {
        std::fs::write(&self.hang, "hang").expect("arm the hang");
    }

    /// The PID the LAST run recorded.
    fn last_pid(&self) -> String {
        std::fs::read_to_string(&self.pid)
            .expect("the fake ssh recorded its pid")
            .trim()
            .to_owned()
    }

    /// Whether any run reached the end of the script (the hang's last line).
    fn finished(&self) -> bool {
        self.finished.exists()
    }

    /// How many runs the log holds. One run writes one `run` line, so a call that
    /// ran something else is caught by the caller's count assertion.
    fn runs(&self) -> usize {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == "run")
            .count()
    }

    /// The words of the LAST run, one per line.
    fn argv(&self) -> Vec<String> {
        let mut runs: Vec<Vec<String>> = Vec::new();
        for line in std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
        {
            if line == "run" {
                runs.push(Vec::new());
            } else if let Some(last) = runs.last_mut() {
                last.push(line.to_owned());
            }
        }
        runs.pop().unwrap_or_default()
    }
}

/// Whether `flag` is immediately followed by `value` in an argv.
fn arg_pair(argv: &[String], flag: &str, value: &str) -> bool {
    argv.windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_marker_check_pins_the_key_and_reads_only_the_forced_commands_two_answers() {
    let ssh = FakeSsh::new();
    let key_dir = TempDir::new().expect("a temp dir for the key");
    let key = key_dir.path().join("pulse_watch_ed25519");
    std::fs::write(&key, "a stub key").expect("write the key");
    let key_arg = key.display().to_string();
    // An alias that cannot resolve: a run that escaped the fake still reaches no
    // machine, and the Mini is never its target.
    let host = "pulse-marker-fake.invalid";

    // SAFETY: nextest runs one test per process, so this process's PATH is this
    // test's alone; no other thread can be inside an env window. The value is
    // left set — the process ends with the test.
    let previous = std::env::var("PATH").unwrap_or_default();
    unsafe {
        std::env::set_var("PATH", format!("{}:{previous}", ssh.dir.path().display()));
    }

    let marker = SshMarker::new(key.clone());
    let cases = [
        (
            MARKER_LINE,
            MarkerReport::Present,
            "the marker the server writes",
        ),
        (
            "none\n",
            MarkerReport::Absent,
            "the forced command's no-trip answer",
        ),
        (
            "ssh: connect to host pulse-marker-fake.invalid port 22: Connection refused\n",
            MarkerReport::Failed,
            "an ssh error text",
        ),
        ("", MarkerReport::Failed, "empty stdout"),
        (
            "utc=2026-10-09T00:00:00Z\nstarts=3\n",
            MarkerReport::Failed,
            "a truncated marker",
        ),
        (
            "none\nlimit=3\n",
            MarkerReport::Failed,
            "more than the two answers",
        ),
    ];
    for (index, (stdout, expected, what)) in cases.iter().enumerate() {
        ssh.prints(stdout);
        let report = marker.marker(host.to_owned()).await;
        assert_eq!(report, *expected, "{what} — stdout {stdout:?}");
        assert_eq!(
            ssh.runs(),
            index + 1,
            "the fake ssh ran, never a real one: {what}"
        );
        let argv = ssh.argv();
        assert!(
            arg_pair(&argv, "-o", "IdentitiesOnly=yes"),
            "the check offers only the key it was given, so no other key can \
             authenticate first and bypass the forced command: {what}: {argv:?}"
        );
        assert!(
            arg_pair(&argv, "-o", "BatchMode=yes"),
            "the check never prompts (the spec's literal command): {what}: {argv:?}"
        );
        let host_arg = host.to_owned();
        assert!(
            argv.contains(&key_arg) && argv.contains(&host_arg),
            "the key and the host travel: {what}: {argv:?}"
        );
    }
}

/// PR-354 fix C6: a marker ssh that CONNECTS and then hangs must not stall the
/// watcher's cycle — `ConnectTimeout` bounds only the connect — so the check is
/// bounded hard: it reads FAILED (never "start limit") and the child is killed,
/// not left running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hanging_marker_ssh_is_killed_and_reads_failed() {
    let ssh = FakeSsh::new();
    let key_dir = TempDir::new().expect("a temp dir for the key");
    let key = key_dir.path().join("pulse_watch_ed25519");
    std::fs::write(&key, "a stub key").expect("write the key");

    // SAFETY: nextest runs one test per process, so this process's PATH is this
    // test's alone; no other thread can be inside an env window.
    let previous = std::env::var("PATH").unwrap_or_default();
    unsafe {
        std::env::set_var("PATH", format!("{}:{previous}", ssh.dir.path().display()));
    }

    let marker = SshMarker::new(key.clone());
    ssh.hangs();
    let started = Instant::now();
    let report = marker.marker("pulse-marker-fake.invalid".to_owned()).await;
    let elapsed = started.elapsed();

    assert_eq!(
        report,
        MarkerReport::Failed,
        "a hung check is FAILED, never Present"
    );
    assert_eq!(ssh.runs(), 1, "the fake ssh ran, never a real one");
    assert!(
        elapsed < Duration::from_secs(20),
        "the hard bound ended the check, not the fake's own {}-second sleep: {elapsed:?}",
        FakeSsh::HANG_SECS
    );
    assert!(
        !ssh.finished(),
        "the hung ssh never reached the end of its script"
    );
    // The child was KILLED, not left running: its recorded pid is gone.
    let pid = ssh.last_pid();
    let alive = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("kill -0 {pid}"))
        .status()
        .expect("run kill -0");
    assert!(!alive.success(), "the ssh child ({pid}) is gone");
    // And the argv carries the keepalive pair beside the connect timeout: the
    // session that goes quiet after connecting ends itself, the hard bound is
    // only the backstop.
    let argv = ssh.argv();
    assert!(
        arg_pair(&argv, "-o", "ServerAliveInterval=5"),
        "the check probes the session's liveness: {argv:?}"
    );
    assert!(
        arg_pair(&argv, "-o", "ServerAliveCountMax=2"),
        "and gives up after two unanswered keepalives: {argv:?}"
    );
}

/// PR-354 fix C6b: the probe's client carries `no_proxy` — a SECURITY
/// requirement (`src/client/mod.rs` declares it for every surface that reaches
/// the server) — so an ambient proxy can never carry the probe, and the server
/// URL with it, through a third party.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_real_probe_never_goes_through_an_ambient_proxy() {
    let healthz = FakeHttp::spawn(200, r#"{"status":"ok","api_version":1}"#);

    // A dead proxy on port 1: a client that honored it could not reach the
    // listener at all.
    let dead = "http://127.0.0.1:1";
    // SAFETY: nextest runs one test per process, so these are this test's alone.
    // NO_PROXY/no_proxy are cleared too: with NO_PROXY=127.0.0.1 inherited, the
    // probe would reach the listener even WITHOUT `no_proxy` on the client, and
    // this test would pass without the fix it exists to prove (PR-354 fix D8).
    unsafe {
        for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(name, dead);
        }
        for name in ["NO_PROXY", "no_proxy"] {
            std::env::remove_var(name);
        }
    }

    let probe = HttpProbe::new();
    assert_eq!(
        probe.probe(healthz.base.clone()).await,
        ProbeReport::Ok,
        "the probe reached the server with a proxy in the environment"
    );
    assert!(
        !healthz.requests().is_empty(),
        "the request reached the listener, not the proxy"
    );
}
