//! The bind policy and the retrying bind (r3.s3.w1, D6), plus the `pulse serve`
//! startup orchestration with the w3 credential seam.
//!
//! **The bind table (D6):** the ONLY accepted addresses are IPv4 inside the
//! Tailscale CGNAT range `100.64.0.0/10` — that is `100.64.0.0` through
//! `100.127.255.255` — plus loopback (`127.0.0.0/8`) when `--dev-loopback` is
//! passed. Every other address refuses at STARTUP with a named reason, before
//! any listener exists: nothing outside the tailnet can ever be exposed, and an
//! operator typo dies loudly instead of silently binding a LAN address.
//!
//! **The retry loop (D6):** `AddrNotAvailable` is the transient case — Tailscale
//! may not have assigned the address yet — so it retries on a 5s interval for up
//! to 120s, logging one stderr line per attempt, then exits non-zero with the
//! named timeout error. Every other bind error (for example `AddrInUse`) fails
//! at once. The retry clock and sleeper are injectable, so the tests run the
//! full 120s budget instantly.
//!
//! **Startup order** (one stderr line per step, through the state's sink):
//! migrate-then-open the DB (step 1, the composition root), resolve the data
//! dir (step 2), *credential resolution (step 3 — the w3 seam, marked below)*,
//! `check_bind` (step 4), retrying bind (step 5), the one `listening on` line
//! (step 6), serve until SIGTERM/SIGINT (step 7), then the shutdown line.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;

use crate::adapters::secrets::{self, CredentialProfile, StartupCredential};
use crate::domain::CredentialSource;

use super::log::RequestLog;
use super::{ServerState, router};

/// The Tailscale CGNAT range `100.64.0.0/10` — the first accepted octet pair
/// is `100.64`–`100.127` (D6). Loopback and IPv6 have their own rules.
const TAILNET_FIRST: u8 = 64;
const TAILNET_LAST: u8 = 127;

/// A named bind refusal (D6). The `Display` names the reason in words — these
/// errors reach the operator's terminal, not an API client.
#[derive(Debug, thiserror::Error)]
pub enum BindRefused {
    /// An IPv4 address outside `100.64.0.0/10` (includes `0.0.0.0`).
    #[error(
        "{addr} is outside the Tailscale range 100.64.0.0/10 (100.64.0.0-100.127.255.255); refusing to bind anything but the tailnet"
    )]
    NotTailnet { addr: SocketAddr },
    /// Loopback without `--dev-loopback`.
    #[error("{addr} is loopback; pass --dev-loopback to allow a loopback bind (development only)")]
    LoopbackRequiresDev { addr: SocketAddr },
    /// Any IPv6 address, including Tailscale's own `fd7a:115c:a1e0::/48`.
    #[error(
        "{addr} is IPv6; pulse serve binds IPv4 addresses only (the tailnet v4 address, loopback under --dev-loopback)"
    )]
    Ipv6Unsupported { addr: SocketAddr },
}

/// The named startup/bind failures of `pulse serve`.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// `check_bind` refused the address.
    #[error("bind refused: {0}")]
    BindRefused(#[from] BindRefused),
    /// The address stayed `AddrNotAvailable` for the whole retry budget.
    #[error("{addr} was still not available after {budget_secs}s of retries")]
    BindTimedOut { addr: SocketAddr, budget_secs: u64 },
    /// A non-retryable bind failure (for example `AddrInUse`).
    #[error("cannot bind {addr}: {source}")]
    BindFailed {
        addr: SocketAddr,
        source: std::io::Error,
    },
    /// The serve loop itself failed.
    #[error("serve failed: {0}")]
    Serve(#[source] std::io::Error),
    /// A credential file was found but refused at startup (r3.s3.w3, R3): the
    /// server exits non-zero BEFORE binding rather than serve with an exposed
    /// or broken credential source. The refusal names the path and the reason
    /// and never the value — the file's bytes are never read before the very
    /// checks that refused it.
    #[error("pulse serve: refusing to start: {reason}")]
    CredentialRefused {
        /// Which file, and why — the owner/mode/open/read failure
        /// ([`CredentialFileRefusal`]'s one-line rendering), worded for the
        /// operator.
        reason: String,
    },
}

/// The pure bind-policy check (D6): accept or refuse with a named reason.
/// Pure — no syscalls, no logging — so the whole table is testable directly.
///
/// # Errors
///
/// [`BindRefused`] naming the exact rule the address violates.
pub fn check_bind(addr: SocketAddr, dev_loopback: bool) -> Result<(), BindRefused> {
    match addr.ip() {
        IpAddr::V6(_) => Err(BindRefused::Ipv6Unsupported { addr }),
        IpAddr::V4(ip) if ip.is_loopback() => {
            if dev_loopback {
                Ok(())
            } else {
                Err(BindRefused::LoopbackRequiresDev { addr })
            }
        }
        IpAddr::V4(ip) if in_tailnet_range(ip) => Ok(()),
        IpAddr::V4(_) => Err(BindRefused::NotTailnet { addr }),
    }
}

/// `100.64.0.0/10` membership: first octet 100, second octet 64..=127.
fn in_tailnet_range(ip: Ipv4Addr) -> bool {
    let octets = ip.octets();
    octets[0] == 100 && (TAILNET_FIRST..=TAILNET_LAST).contains(&octets[1])
}

/// The retry schedule (D6): retry every 5 seconds for up to 120 seconds.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Seconds between attempts after a transient failure.
    pub interval: Duration,
    /// Total budget; once the slept time reaches it, the next failure is final.
    pub budget: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(5),
            budget: Duration::from_secs(120),
        }
    }
}

/// The injected sleeper (D6): production sleeps for real; tests record the
/// advances so the full 120s budget elapses instantly.
pub trait RetrySleep: Send + Sync {
    /// Sleep for `duration`, asynchronously.
    fn sleep<'a>(&'a self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}

/// The production sleeper: tokio's timer.
pub struct TokioSleep;

impl RetrySleep for TokioSleep {
    fn sleep<'a>(&'a self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(tokio::time::sleep(duration))
    }
}

/// Bind `addr`, retrying ONLY the transient `AddrNotAvailable` case on the
/// policy's interval until the policy's budget runs out. One stderr line per
/// retry attempt, through `sink` (the same sink the request log uses). Any
/// other error — `AddrInUse` included — fails on its first attempt.
///
/// # Errors
///
/// [`ServeError::BindTimedOut`] when the budget elapses;
/// [`ServeError::BindFailed`] for every other bind error.
pub async fn bind_with_retry<F, Fut>(
    addr: SocketAddr,
    policy: RetryPolicy,
    sink: Arc<dyn RequestLog>,
    mut make_listener: F,
    sleeper: Arc<dyn RetrySleep>,
) -> Result<TcpListener, ServeError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = std::io::Result<TcpListener>>,
{
    // The virtual clock: elapsed time is the sum of the (injected) sleeps, so
    // the loop is honest about the budget while staying instantly testable.
    let mut elapsed = Duration::ZERO;
    loop {
        match make_listener().await {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == std::io::ErrorKind::AddrNotAvailable => {
                if elapsed >= policy.budget {
                    return Err(ServeError::BindTimedOut {
                        addr,
                        budget_secs: policy.budget.as_secs(),
                    });
                }
                sink.write(format!(
                    "pulse serve: {addr} not available yet, retrying in {}s",
                    policy.interval.as_secs()
                ));
                sleeper.sleep(policy.interval).await;
                elapsed += policy.interval;
            }
            Err(err) => return Err(ServeError::BindFailed { addr, source: err }),
        }
    }
}

/// The `pulse serve` startup configuration, handed in by the composition root.
pub struct ServeConfig {
    /// The address to bind (already parsed by the composition root's flag
    /// handling — the POLICY check happens in [`serve`]).
    pub bind: SocketAddr,
    /// Accept loopback binds (development only).
    pub dev_loopback: bool,
    /// The migrated database pool.
    pub db: crate::adapters::db::Db,
    /// The data dir (snapshot/export base).
    pub data_dir: std::path::PathBuf,
}

/// The credential source's kebab-case LABEL — the same strings the ledger's
/// `key_source` stores (the serde tags on
/// [`CredentialSource`](crate::domain::CredentialSource)) — so the startup
/// line and the audit trail name a source identically. A label only, never a
/// value.
fn credential_source_label(source: CredentialSource) -> &'static str {
    match source {
        CredentialSource::Env => "env",
        CredentialSource::ConfigDir => "config-dir",
        CredentialSource::CwdDotenv => "cwd-dotenv",
        CredentialSource::AppDataDir => "app-data-dir",
        CredentialSource::Keychain => "keychain",
    }
}

/// Run the server until SIGTERM/SIGINT. See the module docs for the ordered
/// startup steps and the w3 seam.
///
/// # Errors
///
/// [`ServeError`] on any refused bind, exhausted retry budget, bind failure or
/// serve-loop failure.
pub async fn serve(config: ServeConfig) -> Result<(), ServeError> {
    let state = Arc::new(ServerState::new(config.db, config.data_dir));
    let sink = state.log().clone();

    // ---- Step 3: CREDENTIAL RESOLUTION — THE w3 SEAM (r3.s3.w3) ------------
    // The server credential profile (R3): this process resolves the LLM
    // credential only from the environment and the two permission-checked file
    // locations — never the cwd/manifest dotenv, never the Keychain. The cell
    // set here shapes EVERY later call through the ordinary resolver in this
    // process (w2's compose and coach handlers included). The value below is
    // dropped at once; handlers resolve per call as the desktop always has,
    // and nothing joins `ServerState`.
    secrets::set_credential_profile(CredentialProfile::Server);
    match secrets::resolve_credential_for_startup() {
        StartupCredential::Found(key) => sink.write(format!(
            "pulse serve: LLM credential from {}",
            credential_source_label(key.source())
        )),
        StartupCredential::Absent => sink.write(
            "pulse serve: no LLM credential; compose and coach will refuse until one is provided"
                .to_owned(),
        ),
        // A refused credential file stops the startup BEFORE the bind check:
        // one line naming the file and the reason, never the value.
        StartupCredential::Refused(refusal) => {
            return Err(ServeError::CredentialRefused {
                reason: refusal.to_string(),
            });
        }
    }
    // ------------------------------------------------------------------------

    // ---- Step 4: the bind policy.
    check_bind(config.bind, config.dev_loopback)?;

    // ---- Step 5: the retrying bind (AddrNotAvailable retries, 5s / 120s).
    let listener = bind_with_retry(
        config.bind,
        RetryPolicy::default(),
        sink.clone(),
        || tokio::net::TcpListener::bind(config.bind),
        Arc::new(TokioSleep),
    )
    .await?;

    // ---- Step 6: the ONE listening line (the actual port, :0 resolved).
    let bound = listener.local_addr().map_err(|e| ServeError::BindFailed {
        addr: config.bind,
        source: e,
    })?;
    sink.write(format!("pulse serve: listening on {bound}"));

    // ---- Step 7: serve until SIGTERM/SIGINT, then say goodbye.
    let app = router(Arc::clone(&state));
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal(bound))
    .await
    .map_err(ServeError::Serve)?;
    sink.write(format!("pulse serve: shutdown complete ({bound})"));
    Ok(())
}

/// Resolve on SIGTERM (the service-manager case) or SIGINT (Ctrl-C).
async fn shutdown_signal(bound: SocketAddr) {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    {
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => term,
                Err(err) => {
                    eprintln!("pulse serve: cannot listen for SIGTERM on {bound}: {err}");
                    let _ = ctrl_c.await;
                    return;
                }
            };
        tokio::select! {
            _ = ctrl_c => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = bound;
        let _ = ctrl_c.await;
    }
}
