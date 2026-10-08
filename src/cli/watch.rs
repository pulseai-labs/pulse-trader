//! `pulse watch` (r4.s2.w4, Q2) — one probe cycle of the off-box watcher.
//!
//! `pulse watch` runs from `deploy/pulse-watch.timer` on draco-desk, once every
//! 60 seconds. One run is one cycle:
//!
//! 1. probe `<url>/healthz` with a short timeout; a 200 whose body says
//!    `"status":"ok"` is up, and `"status":"degraded"` counts as up — the paper
//!    runtime not running is named in a recovery, never alerted on its own;
//! 2. on a failed probe, check the host (a TCP connect to the Mini's port 22)
//!    and the start-limit marker over the dedicated watcher key (C3);
//! 3. alert after three failed probes in a row, or **at once** when the
//!    start-limit marker is present; de-duplicate to one alert per incident, one
//!    "recovered" when a probe succeeds again, and a "still down" reminder every
//!    six hours;
//! 4. a watcher error (topic file missing or not 0600, state file unwritable,
//!    ssh key missing, malformed `--url`) pushes one "watcher error" per distinct
//!    error episode — not every minute — and makes the run exit non-zero.
//!
//! **Nothing sent or logged ever carries a token, a credential URL, the topic or
//! session data** (Q2). The topic is the watcher's only secret: it is read from
//! its 0600 file inside the one notifier that needs it (per push, never kept),
//! it rides only the ntfy request's path, and no log line, message body or error
//! text names it.
//!
//! The cycle is a state machine over injected seams ([`WatchSeams`]), which is
//! what lets `tests/healthz_watch.rs` drive every branch with a scripted probe,
//! host, marker, clock and log, and a local fake ntfy listener — never the real
//! `ntfy.sh`, never the Mini.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::time::Duration;

use clap::Args;

/// Failed probes in a row before the first alert.
pub const ALERT_AFTER_FAILURES: u32 = 3;

/// The "still down" reminder period: six hours (Q2).
pub const STILL_DOWN_REMINDER_SECS: u64 = 6 * 60 * 60;

/// The probe's request timeout. The timer fires every 60 seconds, so a probe
/// that hangs must give up well inside one period.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The host check's connect timeout.
const HOST_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// The marker check's ssh `ConnectTimeout`, in seconds.
const SSH_CONNECT_TIMEOUT_SECS: u64 = 5;

/// The ntfy push's request timeout.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);

/// The Mini's ssh port — the host-reachability check's target (Q2 step 2).
const HOST_SSH_PORT: u16 = 22;

/// The watcher key's path under `$HOME`. There is no flag for it: the operator
/// installs the key (and its forced command) at cutover, SPINE.md step 6.
const WATCHER_KEY_RELATIVE: &str = ".ssh/pulse_watch_ed25519";

// ---------------------------------------------------------------------------
// The seams — one injected dependency per thing this process cannot do in a test
// ---------------------------------------------------------------------------

/// The boxed future every seam returns. A trait with borrowed receivers cannot
/// be `dyn`-dispatched with `async fn`, so the boundary boxes once — the
/// `server::routes::BoxedFut` pattern.
pub type WatchFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Probe `<url>/healthz` once.
pub trait ProbeSeam: Send + Sync {
    /// One probe's report.
    fn probe(&self, url: String) -> WatchFuture<ProbeReport>;
}

/// Whether the Mini answers at all (a TCP connect to its ssh port).
pub trait HostSeam: Send + Sync {
    /// `true` when a TCP connect to port 22 succeeded.
    fn host_reachable(&self, ssh_host: String) -> WatchFuture<bool>;
}

/// Read the start-limit marker over the dedicated watcher key.
pub trait MarkerSeam: Send + Sync {
    /// The marker check's report.
    fn marker(&self, ssh_host: String) -> WatchFuture<MarkerReport>;
}

/// Deliver one push. The production implementation resolves the topic from its
/// 0600 file; an `Err` carries a reason that never names the topic or the URL.
pub trait NotifySeam: Send + Sync {
    /// Deliver `title` + one-line `body`; `Err(reason)` when it could not be.
    fn notify(&self, title: String, body: String) -> WatchFuture<Result<(), String>>;
}

/// The wall clock (injected so the 3-failure and 6-hour rules are testable).
pub trait ClockSeam: Send + Sync {
    /// UNIX seconds.
    fn now_unix_secs(&self) -> u64;
}

/// Where the cycle's log lines go.
pub trait LogSeam: Send + Sync {
    /// Write one complete line.
    fn log(&self, line: String);
}

/// The six seams one cycle reads — bundled so [`run_once`] takes one argument
/// and a test can mix scripted and real halves (a scripted probe with the real
/// notifier, or the reverse).
pub struct WatchSeams<'a> {
    /// The probe seam.
    pub probe: &'a dyn ProbeSeam,
    /// The host-reachability seam.
    pub host: &'a dyn HostSeam,
    /// The start-limit marker seam.
    pub marker: &'a dyn MarkerSeam,
    /// The push seam.
    pub notify: &'a dyn NotifySeam,
    /// The clock seam.
    pub clock: &'a dyn ClockSeam,
    /// The log seam.
    pub log: &'a dyn LogSeam,
}

// ---------------------------------------------------------------------------
// The value types
// ---------------------------------------------------------------------------

/// What one probe saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeReport {
    /// 200 with `"status":"ok"` — up.
    Ok,
    /// 200 with `"status":"degraded"` — up, the paper runtime is not running.
    Degraded,
    /// Anything else: a non-200, an unparseable body, a timeout, a refused
    /// connection.
    Down,
}

/// What the start-limit marker check saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerReport {
    /// The forced command printed `none`: no trip.
    Absent,
    /// The forced command printed the marker: the start limit tripped.
    Present,
    /// The watcher key file is not there — a watcher error (the marker cannot
    /// be checked at all).
    KeyMissing,
    /// The check ran and failed (ssh refused, timed out, or printed something
    /// unparseable). The marker is unknown; the probe's own alert still stands.
    Failed,
}

/// The watcher errors. Every one makes the run exit non-zero; the first four
/// also push one "watcher error" per distinct error episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchErrorKind {
    /// `--url` is not a parseable `http(s)` URL.
    UrlInvalid,
    /// The topic file is not there — nothing could be delivered.
    TopicMissing,
    /// The topic file's mode is not 0600.
    TopicMode,
    /// The topic file is empty or malformed.
    TopicInvalid,
    /// The state file cannot be written: the counters cannot be de-duplicated.
    StateUnwritable,
    /// The watcher key is missing; the marker check cannot run.
    SshKeyMissing,
}

impl WatchErrorKind {
    /// The one-line detail, safe to push and to log: it never names the topic,
    /// a path or a credential.
    #[must_use]
    pub fn detail(self) -> &'static str {
        match self {
            Self::UrlInvalid => "the --url is not a valid http(s) URL",
            Self::TopicMissing => "the topic file is missing",
            Self::TopicMode => "the topic file mode is not 0600",
            Self::TopicInvalid => "the topic file is empty or malformed",
            Self::StateUnwritable => "the state file is not writable",
            Self::SshKeyMissing => "the ssh key is missing",
        }
    }

    /// The state-file spelling (the de-duplication key).
    fn as_state(self) -> &'static str {
        match self {
            Self::UrlInvalid => "url-invalid",
            Self::TopicMissing => "topic-missing",
            Self::TopicMode => "topic-mode",
            Self::TopicInvalid => "topic-invalid",
            Self::StateUnwritable => "state-unwritable",
            Self::SshKeyMissing => "ssh-key-missing",
        }
    }

    /// Parse the state-file spelling; an unknown value reads as "no error".
    fn from_state(text: &str) -> Option<Self> {
        match text {
            "url-invalid" => Some(Self::UrlInvalid),
            "topic-missing" => Some(Self::TopicMissing),
            "topic-mode" => Some(Self::TopicMode),
            "topic-invalid" => Some(Self::TopicInvalid),
            "state-unwritable" => Some(Self::StateUnwritable),
            "ssh-key-missing" => Some(Self::SshKeyMissing),
            _ => None,
        }
    }
}

impl std::fmt::Display for WatchErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.detail())
    }
}

/// What one cycle did — the evidence the caller (and the report) reads.
#[derive(Debug, Default, Clone)]
pub struct WatchRun {
    /// The watcher error this cycle raised, if any. Every such run exits
    /// non-zero, including the de-duplicated repeats.
    pub error: Option<WatchErrorKind>,
    /// The bodies this cycle delivered, in order (never a secret).
    pub pushed_bodies: Vec<String>,
    /// Whether a push was attempted and could not be delivered. The incident is
    /// deliberately left un-alerted then, so the next cycle retries.
    pub push_failed: bool,
}

/// One cycle's configuration.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// The prod server's base URL; the probe appends `/healthz`.
    pub url: String,
    /// The Mini's ssh host or alias.
    pub ssh_host: String,
    /// The 0600 topic file (read per push, never kept, never printed).
    pub topic_file: PathBuf,
    /// The 0600 state file: counters, timestamps and the last alert kind.
    pub state_file: PathBuf,
}

// ---------------------------------------------------------------------------
// The CLI surface and the composition root
// ---------------------------------------------------------------------------

/// `pulse watch --url <base> --ssh-host <alias> --topic-file <path> --state-file <path> [--ntfy-url <url>]`.
#[derive(Debug, Args)]
pub struct WatchArgs {
    /// The prod server's base URL; each cycle probes `<url>/healthz`.
    #[arg(long)]
    pub url: String,
    /// The Mini's ssh host or alias (the host check and the marker key's target).
    #[arg(long)]
    pub ssh_host: String,
    /// The 0600 file holding the ntfy topic. Read per push; never printed.
    #[arg(long)]
    pub topic_file: PathBuf,
    /// The 0600 state file: counters, timestamps and the last alert kind.
    #[arg(long)]
    pub state_file: PathBuf,
    /// The ntfy base URL; the operator's phone subscribes to `<url>/<topic>`.
    /// Overridable so tests and the live check use a local fake listener.
    #[arg(long, default_value = "https://ntfy.sh")]
    pub ntfy_url: String,
}

/// Run one cycle from the parsed CLI arguments — the composition root the timer
/// hits every 60 seconds.
///
/// # Errors
///
/// Any watcher error, or a push that could not be delivered: the cycle exits
/// non-zero so a failed timer unit is visible.
pub(crate) async fn run_watch(args: &WatchArgs) -> anyhow::Result<()> {
    let config = WatchConfig {
        url: args.url.clone(),
        ssh_host: args.ssh_host.clone(),
        topic_file: args.topic_file.clone(),
        state_file: args.state_file.clone(),
    };
    let probe = HttpProbe::new();
    let host = TcpHost;
    let marker = SshMarker::new(watcher_key_path());
    let notify = NtfyNotifier::new(args.ntfy_url.clone(), args.topic_file.clone());
    let clock = SystemClock;
    let log = StderrSink;
    let seams = WatchSeams {
        probe: &probe,
        host: &host,
        marker: &marker,
        notify: &notify,
        clock: &clock,
        log: &log,
    };
    let run = run_once(&config, &seams).await;
    if let Some(kind) = run.error {
        anyhow::bail!("pulse watch: watcher error: {kind}");
    }
    if run.push_failed {
        anyhow::bail!("pulse watch: a push could not be delivered");
    }
    Ok(())
}

/// The watcher key's default path: `$HOME/.ssh/pulse_watch_ed25519`. A user
/// unit has `$HOME`; without it the path stays relative, does not exist, and the
/// marker check reports the missing key.
fn watcher_key_path() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from(WATCHER_KEY_RELATIVE),
        |home| PathBuf::from(home).join(WATCHER_KEY_RELATIVE),
    )
}

// ---------------------------------------------------------------------------
// The cycle
// ---------------------------------------------------------------------------

/// Run one probe cycle: the preconditions, the probe, the alert decision, the
/// state write. Pure over [`WatchSeams`] — every branch is a test case.
pub async fn run_once(config: &WatchConfig, seams: &WatchSeams<'_>) -> WatchRun {
    let mut run = WatchRun::default();
    let now = seams.clock.now_unix_secs();
    let mut state = match load_state(&config.state_file) {
        Ok(state) => state,
        Err(error) => {
            seams.log.log(format!(
                "pulse watch: the state file could not be read ({error}); starting from a fresh state"
            ));
            WatchState::default()
        }
    };

    // The probe target's precondition: a malformed `--url` cannot be probed at
    // all, and it must not read as three failed probes ("prod is down") either.
    if !url_is_valid(&config.url) {
        watcher_error(seams, &mut state, WatchErrorKind::UrlInvalid, &mut run).await;
        return finish(&config.state_file, seams, &state, run);
    }

    // The push channel's precondition: without a usable topic file nothing this
    // cycle could be delivered, so the cycle ends as a watcher error.
    if let Err(kind) = topic_ok(&config.topic_file) {
        watcher_error(seams, &mut state, kind, &mut run).await;
        return finish(&config.state_file, seams, &state, run);
    }
    // The de-duplication channel's precondition. Proving it by an early save
    // matters: an unwritable state file cannot remember an alert, so alerting
    // from it would repeat the same push every minute. It is the ONE error that
    // is logged rather than pushed — de-duplicating the push itself is exactly
    // what is broken.
    if let Err(error) = save_state(&config.state_file, &state) {
        seams.log.log(format!(
            "pulse watch: watcher error: {} ({error})",
            WatchErrorKind::StateUnwritable.detail()
        ));
        run.error = Some(WatchErrorKind::StateUnwritable);
        return run;
    }

    let report = seams.probe.probe(config.url.clone()).await;
    match report {
        ProbeReport::Ok | ProbeReport::Degraded => {
            recover(seams, &mut state, &mut run, report).await;
        }
        ProbeReport::Down => down_cycle(config, seams, &mut state, &mut run, now).await,
    }

    finish(&config.state_file, seams, &state, run)
}

/// Persist the cycle's state and return the run — the one exit every path
/// shares, so the de-duplication record is written even on a watcher-error
/// cycle. A failed write is itself a watcher error (the counters cannot be
/// trusted next cycle).
fn finish(
    state_file: &Path,
    seams: &WatchSeams<'_>,
    state: &WatchState,
    mut run: WatchRun,
) -> WatchRun {
    if let Err(error) = save_state(state_file, state) {
        seams.log.log(format!(
            "pulse watch: watcher error: {} ({error})",
            WatchErrorKind::StateUnwritable.detail()
        ));
        run.error = Some(WatchErrorKind::StateUnwritable);
    }
    run
}

/// A probe that came back up: close an open incident with one "recovered"
/// (naming a persisting degraded paper runtime, which never alerts on its own).
async fn recover(
    seams: &WatchSeams<'_>,
    state: &mut WatchState,
    run: &mut WatchRun,
    report: ProbeReport,
) {
    state.failures = 0;
    state.last_error = None;
    if let Some(incident) = state.incident.take() {
        let body = if report == ProbeReport::Degraded {
            "prod recovered, paper runtime degraded".to_owned()
        } else {
            "prod recovered".to_owned()
        };
        if push(seams, run, "prod recovered", body).await {
            state.last_alert_unix = 0;
        } else {
            // Not delivered: keep the incident so the next cycle retries.
            state.incident = Some(incident);
        }
    }
}

/// A failed probe: the host check, the marker check, and the alert decision.
async fn down_cycle(
    config: &WatchConfig,
    seams: &WatchSeams<'_>,
    state: &mut WatchState,
    run: &mut WatchRun,
    now: u64,
) {
    state.failures = state.failures.saturating_add(1);
    let host_up = seams.host.host_reachable(config.ssh_host.clone()).await;
    let marker = seams.marker.marker(config.ssh_host.clone()).await;

    match marker {
        MarkerReport::Present => {
            let start_limit = Incident::StartLimit;
            if state.incident != Some(start_limit) {
                if push(
                    seams,
                    run,
                    "prod start limit",
                    start_limit.reason_text().to_owned(),
                )
                .await
                {
                    state.incident = Some(start_limit);
                    state.last_alert_unix = now;
                }
            } else if reminder_due(state, now) {
                let body = format!("prod still down: {}", start_limit.reason_text());
                if push(seams, run, "prod still down", body).await {
                    state.last_alert_unix = now;
                }
            }
        }
        MarkerReport::Absent | MarkerReport::Failed | MarkerReport::KeyMissing => {
            let reason = if host_up {
                Incident::DownService
            } else {
                Incident::DownUnreachable
            };
            if let Some(open) = state.incident {
                if reminder_due(state, now) {
                    let body = format!("prod still down: {}", open.reason_text());
                    if push(seams, run, "prod still down", body).await {
                        state.last_alert_unix = now;
                    }
                }
            } else if state.failures >= ALERT_AFTER_FAILURES {
                let body = format!("prod is down: {}", reason.reason_text());
                if push(seams, run, "prod down", body).await {
                    state.incident = Some(reason);
                    state.last_alert_unix = now;
                }
            }
        }
    }

    if marker == MarkerReport::KeyMissing {
        // A watcher error, but never at the outage's expense: the alert decision
        // above already ran, and this only adds the de-duplicated error push.
        watcher_error(seams, state, WatchErrorKind::SshKeyMissing, run).await;
    }
}

/// Whether the six-hour reminder is due for the open incident.
fn reminder_due(state: &WatchState, now: u64) -> bool {
    state
        .last_alert_unix
        .saturating_add(STILL_DOWN_REMINDER_SECS)
        <= now
}

/// Deliver one push. `false` when it could not be delivered — the caller must
/// then not mark the incident alerted, so the next cycle retries it.
async fn push(seams: &WatchSeams<'_>, run: &mut WatchRun, title: &str, body: String) -> bool {
    match seams.notify.notify(title.to_owned(), body.clone()).await {
        Ok(()) => {
            run.pushed_bodies.push(body);
            true
        }
        Err(reason) => {
            seams
                .log
                .log(format!("pulse watch: a push failed ({reason})"));
            run.push_failed = true;
            false
        }
    }
}

/// Raise one watcher error: one push per distinct error episode (de-duplicated
/// through the state file's `last_error`), and the run is marked errored even
/// when the push is de-duplicated.
async fn watcher_error(
    seams: &WatchSeams<'_>,
    state: &mut WatchState,
    kind: WatchErrorKind,
    run: &mut WatchRun,
) {
    run.error = Some(kind);
    if state.last_error == Some(kind) {
        return;
    }
    state.last_error = Some(kind);
    let body = format!("watcher error: {}", kind.detail());
    match seams
        .notify
        .notify("watch error".to_owned(), body.clone())
        .await
    {
        Ok(()) => run.pushed_bodies.push(body),
        Err(reason) => {
            seams.log.log(format!(
                "pulse watch: {body} (the push could not be delivered: {reason})"
            ));
            run.push_failed = true;
        }
    }
}

// ---------------------------------------------------------------------------
// The state file: counters, timestamps and the last alert kind
// ---------------------------------------------------------------------------

/// The open incident, or none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Incident {
    /// The host answers and the service is down.
    DownService,
    /// The host does not answer at all (`FileVault`, power, network).
    DownUnreachable,
    /// The start-limit marker is present: the service refused its own start.
    StartLimit,
}

impl Incident {
    /// The reason text the alert bodies carry.
    fn reason_text(self) -> &'static str {
        match self {
            Self::DownService => "service down",
            Self::DownUnreachable => "Mini unreachable — it may need a FileVault unlock",
            Self::StartLimit => "start limit reached on prod — run just prod-reset",
        }
    }

    /// The state-file spelling.
    fn as_state(self) -> &'static str {
        match self {
            Self::DownService => "down-service",
            Self::DownUnreachable => "down-unreachable",
            Self::StartLimit => "start-limit",
        }
    }

    /// Parse the state-file spelling.
    fn from_state(text: &str) -> Option<Self> {
        match text {
            "down-service" => Some(Self::DownService),
            "down-unreachable" => Some(Self::DownUnreachable),
            "start-limit" => Some(Self::StartLimit),
            _ => None,
        }
    }
}

/// The persisted state: counters, timestamps and the last alert kind (plus the
/// last watcher-error kind, which is what de-duplicates the error push).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct WatchState {
    /// Consecutive failed probes.
    failures: u32,
    /// The open incident, if any.
    incident: Option<Incident>,
    /// When the last alert (or reminder) was pushed.
    last_alert_unix: u64,
    /// The last watcher-error kind — cleared by a clean cycle.
    last_error: Option<WatchErrorKind>,
}

impl WatchState {
    /// One `key=value` line each, in a fixed order.
    fn to_text(&self) -> String {
        let incident = self.incident.map_or("none", Incident::as_state);
        let last_error = self.last_error.map_or("none", WatchErrorKind::as_state);
        format!(
            "failures={}\nincident={}\nlast_alert_unix={}\nlast_error={}\n",
            self.failures, incident, self.last_alert_unix, last_error
        )
    }

    /// Tolerant parse: an unknown key, a torn line or a bad value is skipped —
    /// this is a bound and a de-duplication aid, not a ledger.
    fn from_text(text: &str) -> Self {
        let mut state = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "failures" => state.failures = value.parse().unwrap_or(0),
                "incident" => state.incident = Incident::from_state(value),
                "last_alert_unix" => state.last_alert_unix = value.parse().unwrap_or(0),
                "last_error" => state.last_error = WatchErrorKind::from_state(value),
                _ => {}
            }
        }
        state
    }
}

/// Read the state file; a missing file is a fresh state.
fn load_state(path: &Path) -> Result<WatchState, std::io::Error> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(WatchState::from_text(&text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(WatchState::default()),
        Err(error) => Err(error),
    }
}

/// Write the state file (0600, through a temporary file plus rename), creating
/// its directory if the operator has not yet.
fn save_state(path: &Path, state: &WatchState) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut temp = path.as_os_str().to_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    std::fs::write(&temp, state.to_text())?;
    set_private_mode(&temp)?;
    std::fs::rename(&temp, path)
}

/// Whether `url` parses as an `http(s)` URL — the probe appends `/healthz` to it.
fn url_is_valid(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|parsed| matches!(parsed.scheme(), "http" | "https"))
}

/// The topic's non-secret precondition: exists, a file, mode 0600, and a value
/// that could be a topic. The VALUE is never returned here — the notifier is the
/// only reader that needs it.
fn topic_ok(path: &Path) -> Result<(), WatchErrorKind> {
    read_topic(path).map(|_topic| ())
}

/// Read and validate the topic. The value never leaves this function except as
/// the returned `String`, which only the notifier uses.
fn read_topic(path: &Path) -> Result<String, WatchErrorKind> {
    let metadata = std::fs::metadata(path).map_err(|_| WatchErrorKind::TopicMissing)?;
    if !metadata.is_file() {
        return Err(WatchErrorKind::TopicMissing);
    }
    if mode_of(&metadata) != 0o600 {
        return Err(WatchErrorKind::TopicMode);
    }
    let text = std::fs::read_to_string(path).map_err(|_| WatchErrorKind::TopicMissing)?;
    let topic = text.trim();
    if topic.is_empty() || topic.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(WatchErrorKind::TopicInvalid);
    }
    Ok(topic.to_owned())
}

#[cfg(unix)]
fn mode_of(metadata: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode_of(_metadata: &std::fs::Metadata) -> u32 {
    0o600
}

/// 0600 for the state file — the same discipline one level down from the data
/// dir, like `server::start_limit`'s counter files.
#[cfg(unix)]
fn set_private_mode(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_mode(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------------
// The production seams
// ---------------------------------------------------------------------------

/// The production probe: `GET <url>/healthz` through one reqwest client.
pub struct HttpProbe {
    client: reqwest::Client,
}

impl HttpProbe {
    /// The probe with its own client (the short timeout rides the client).
    #[must_use]
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(PROBE_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }
}

impl Default for HttpProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProbeSeam for HttpProbe {
    fn probe(&self, url: String) -> WatchFuture<ProbeReport> {
        let client = self.client.clone();
        Box::pin(async move {
            let target = format!("{}/healthz", url.trim_end_matches('/'));
            match client.get(&target).send().await {
                Ok(response) if response.status() == reqwest::StatusCode::OK => {
                    match response.text().await {
                        Ok(text) => match body_status(&text).as_deref() {
                            Some("ok") => ProbeReport::Ok,
                            Some("degraded") => ProbeReport::Degraded,
                            _ => ProbeReport::Down,
                        },
                        Err(_) => ProbeReport::Down,
                    }
                }
                _ => ProbeReport::Down,
            }
        })
    }
}

/// The `status` field of a `/healthz` body, if it parses.
fn body_status(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    value.get("status")?.as_str().map(str::to_owned)
}

/// The production host check: a TCP connect to the Mini's ssh port. The ssh
/// alias is resolved to its effective `HostName` through `ssh -G` first, so an
/// alias like `macmini` is checked exactly as ssh would resolve it (an ssh
/// alias is not necessarily a DNS name).
pub struct TcpHost;

impl HostSeam for TcpHost {
    fn host_reachable(&self, ssh_host: String) -> WatchFuture<bool> {
        Box::pin(async move {
            let resolved = tokio::task::spawn_blocking(move || resolve_ssh_host(&ssh_host))
                .await
                .unwrap_or_else(|_| String::new());
            if resolved.is_empty() {
                return false;
            }
            let connect = tokio::net::TcpStream::connect((resolved.as_str(), HOST_SSH_PORT));
            matches!(
                tokio::time::timeout(HOST_CONNECT_TIMEOUT, connect).await,
                Ok(Ok(_))
            )
        })
    }
}

/// `ssh -G <host>`'s effective `hostname`, falling back to the literal value
/// when ssh is missing or refuses the query.
fn resolve_ssh_host(ssh_host: &str) -> String {
    let output = std::process::Command::new("ssh")
        .arg("-G")
        .arg(ssh_host)
        .output();
    let Ok(output) = output else {
        return ssh_host.to_owned();
    };
    if !output.status.success() {
        return ssh_host.to_owned();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if let Some(value) = line.trim().strip_prefix("hostname ") {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    ssh_host.to_owned()
}

/// The production marker check (C3): the dedicated watcher key against the
/// Mini's forced command, which prints the marker or `none` and nothing else.
pub struct SshMarker {
    key: PathBuf,
}

impl SshMarker {
    /// The check with the given key path.
    #[must_use]
    pub fn new(key: PathBuf) -> Self {
        Self { key }
    }
}

impl MarkerSeam for SshMarker {
    fn marker(&self, ssh_host: String) -> WatchFuture<MarkerReport> {
        let key = self.key.clone();
        Box::pin(async move {
            if !key.is_file() {
                return MarkerReport::KeyMissing;
            }
            let run = tokio::task::spawn_blocking(move || {
                std::process::Command::new("ssh")
                    .arg("-i")
                    .arg(&key)
                    .arg("-o")
                    .arg("BatchMode=yes")
                    .arg("-o")
                    .arg(format!("ConnectTimeout={SSH_CONNECT_TIMEOUT_SECS}"))
                    .arg(&ssh_host)
                    .output()
            })
            .await;
            match run {
                Ok(Ok(output)) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    match stdout.trim() {
                        "" => MarkerReport::Failed,
                        "none" => MarkerReport::Absent,
                        _ => MarkerReport::Present,
                    }
                }
                _ => MarkerReport::Failed,
            }
        })
    }
}

/// The production notifier: `POST <ntfy-url>/<topic>` with a one-line body and a
/// short title. The topic is read from its 0600 file for every push; every error
/// text is a fixed phrase, so the URL — which carries the topic — can never
/// reach a log line.
pub struct NtfyNotifier {
    client: reqwest::Client,
    base_url: String,
    topic_file: PathBuf,
}

impl NtfyNotifier {
    /// The notifier over one base URL and one topic file.
    #[must_use]
    pub fn new(base_url: String, topic_file: PathBuf) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(PUSH_TIMEOUT)
                .build()
                .unwrap_or_default(),
            base_url,
            topic_file,
        }
    }
}

impl NotifySeam for NtfyNotifier {
    fn notify(&self, title: String, body: String) -> WatchFuture<Result<(), String>> {
        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let topic_file = self.topic_file.clone();
        Box::pin(async move {
            let topic = read_topic(&topic_file).map_err(|kind| kind.detail().to_owned())?;
            let url = format!("{}/{}", base_url.trim_end_matches('/'), topic);
            let response = client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "text/plain")
                .header("Title", title)
                .body(body)
                .send()
                .await
                .map_err(|error| push_reason(&error))?;
            if response.status().is_success() {
                Ok(())
            } else {
                Err(format!("ntfy answered {}", response.status().as_u16()))
            }
        })
    }
}

/// A push failure's reason — fixed phrases only, never a URL (the URL carries
/// the topic).
fn push_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "ntfy timed out".to_owned()
    } else if error.is_connect() {
        "ntfy is unreachable".to_owned()
    } else {
        "the ntfy request failed".to_owned()
    }
}

/// The production clock — the same UNIX-second helper the server's start counter
/// reads.
pub struct SystemClock;

impl ClockSeam for SystemClock {
    fn now_unix_secs(&self) -> u64 {
        crate::server::start_limit::now_unix_secs()
    }
}

/// The production log: one line per push/state event to stderr (the timer's
/// journal).
pub struct StderrSink;

impl LogSeam for StderrSink {
    fn log(&self, line: String) {
        eprintln!("{line}");
    }
}
