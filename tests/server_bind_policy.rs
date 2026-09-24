//! r3.s3.w1 AC-2 — the bind-policy proof (`tests/server_bind_policy.rs`).
//!
//! Two halves, mirroring the spec's split:
//! 1. the pure `check_bind` table — the tailnet range (100.64.0.0/10) is the
//!    only accepted non-loopback family, loopback hides behind
//!    `--dev-loopback`, IPv6 is refused, everything else is refused with a
//!    NAMED reason;
//! 2. the bind retry loop — `AddrNotAvailable` retries on the 5s interval until
//!    the 120s budget runs out (one stderr line per attempt, then the named
//!    timeout error), `AddrInUse` fails on the first attempt with zero sleeps,
//!    and a later-attempt success returns the listener. The sleeper is a test
//!    double, so no test actually waits.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pulse::{
    BindRefused, CaptureLog, RetryPolicy, RetrySleep, ServeError, TokioSleep, bind_with_retry,
    check_bind,
};

/// Build a v4 socket address.
fn v4(octets: [u8; 4], port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::from(octets)), port)
}

/// Parse a socket address literal (keeps the v6 cases readable).
fn parse(s: &str) -> SocketAddr {
    s.parse().expect("valid socket address")
}

// ---------------------------------------------------------------------------
// The check_bind table.
// ---------------------------------------------------------------------------

#[test]
fn the_bind_policy_accepts_the_tailnet_range_and_port_zero() {
    for ip in [[100, 64, 0, 0], [100, 90, 203, 21], [100, 127, 255, 255]] {
        assert!(
            check_bind(v4(ip, 8080), false).is_ok(),
            "{ip:?} is inside 100.64/10"
        );
        assert!(
            check_bind(v4(ip, 0), false).is_ok(),
            "{ip:?}:0 is allowed (ephemeral port)"
        );
    }
}

#[test]
fn the_bind_policy_refuses_everything_outside_the_range_by_name() {
    for ip in [
        [100, 63, 255, 255],
        [100, 128, 0, 0],
        [0, 0, 0, 0],
        [192, 168, 1, 5],
        [10, 0, 0, 1],
    ] {
        let err = check_bind(v4(ip, 8080), false).expect_err("outside 100.64/10 must refuse");
        assert!(
            matches!(err, BindRefused::NotTailnet { .. }),
            "{ip:?}: {err}"
        );
        let message = err.to_string();
        assert!(
            message.contains("100.64"),
            "the refusal names the accepted range: {message}"
        );
    }
}

#[test]
fn the_bind_policy_hides_loopback_behind_the_dev_flag() {
    let err = check_bind(v4([127, 0, 0, 1], 8080), false)
        .expect_err("loopback without --dev-loopback must refuse");
    assert!(
        matches!(err, BindRefused::LoopbackRequiresDev { .. }),
        "the refusal is the named dev-loopback one: {err}"
    );
    assert!(
        check_bind(v4([127, 0, 0, 1], 8080), true).is_ok(),
        "--dev-loopback accepts it"
    );
    assert!(
        check_bind(v4([127, 0, 0, 2], 0), true).is_ok(),
        "the whole 127/8, any port"
    );
}

#[test]
fn the_bind_policy_refuses_ipv6() {
    for literal in ["[::]:8080", "[::1]:8080", "[fd7a:115c:a1e0::1]:8080"] {
        let err = check_bind(parse(literal), false).expect_err("IPv6 must refuse");
        assert!(
            matches!(err, BindRefused::Ipv6Unsupported { .. }),
            "{literal}: {err}"
        );
        let message = err.to_string();
        assert!(
            message.contains("IPv4"),
            "the refusal names the IPv4-only rule: {message}"
        );
    }
}

// ---------------------------------------------------------------------------
// The retry loop, with a recorded sleeper standing in for the clock.
// ---------------------------------------------------------------------------

/// A sleeper that advances instantly but RECORDS every advance — the injected
/// virtual clock: total slept time is the sum of the advances.
struct RecordedSleep {
    advances: Mutex<Vec<Duration>>,
}

impl RecordedSleep {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            advances: Mutex::new(Vec::new()),
        })
    }

    fn count(&self) -> usize {
        self.advances.lock().expect("advances").len()
    }

    fn total(&self) -> Duration {
        self.advances.lock().expect("advances").iter().sum()
    }
}

impl RetrySleep for RecordedSleep {
    fn sleep<'a>(&'a self, duration: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.advances.lock().expect("advances").push(duration);
        Box::pin(std::future::ready(()))
    }
}

/// An `AddrNotAvailable` io error, the retryable kind.
fn unavailable() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::AddrNotAvailable, "not routable yet")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unavailable_address_retries_on_the_interval_until_the_budget() {
    let sink = Arc::new(CaptureLog::default());
    let sleeper = RecordedSleep::new();
    let attempts = AtomicUsize::new(0);

    let result = bind_with_retry(
        v4([100, 64, 0, 0], 8080),
        RetryPolicy::default(),
        sink.clone(),
        || {
            attempts.fetch_add(1, Ordering::SeqCst);
            std::future::ready::<std::io::Result<tokio::net::TcpListener>>(Err(unavailable()))
        },
        sleeper.clone(),
    )
    .await;

    match result {
        Err(ServeError::BindTimedOut { addr, budget_secs }) => {
            assert_eq!(
                addr,
                v4([100, 64, 0, 0], 8080),
                "the timeout names the address"
            );
            assert_eq!(budget_secs, 120, "the default budget is 120s");
        }
        other => panic!("expected the named timeout error, got {other:?}"),
    }
    // Attempts at t=0, 5, ..., 120 → 25 attempts, 24 sleeps totalling 120s.
    assert_eq!(sleeper.count(), 24, "one sleep per interval");
    assert_eq!(
        sleeper.total(),
        Duration::from_secs(120),
        "the virtual clock ran the budget"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        25,
        "the last attempt happens at the budget"
    );
    assert_eq!(sink.lines().len(), 24, "one stderr line per retry attempt");
    for line in sink.lines() {
        assert!(
            line.contains("100.64.0.0:8080"),
            "the attempt line names the address: {line}"
        );
        assert!(
            line.contains("retrying"),
            "the attempt line says it retries: {line}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn addr_in_use_fails_on_the_first_attempt_with_zero_sleeps() {
    let holder = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("hold a port");
    let port = holder.local_addr().expect("local addr").port();

    let sink = Arc::new(CaptureLog::default());
    let sleeper = RecordedSleep::new();
    let attempts = AtomicUsize::new(0);

    let result = bind_with_retry(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
        RetryPolicy::default(),
        sink,
        || {
            attempts.fetch_add(1, Ordering::SeqCst);
            tokio::net::TcpListener::bind(("127.0.0.1", port))
        },
        sleeper.clone(),
    )
    .await;

    match result {
        Err(ServeError::BindFailed { addr, source }) => {
            assert_eq!(addr.port(), port, "the failure names the address");
            assert_eq!(
                source.kind(),
                std::io::ErrorKind::AddrInUse,
                "the kind survives"
            );
        }
        other => panic!("expected the immediate bind failure, got {other:?}"),
    }
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "exactly one attempt");
    assert_eq!(sleeper.count(), 0, "AddrInUse is never retried");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_attempt_succeeds_and_returns_the_listener() {
    let sink = Arc::new(CaptureLog::default());
    let sleeper = RecordedSleep::new();
    // Three attempts: two fabricated failures, then the real bind.
    let remaining = AtomicUsize::new(3);

    let result = bind_with_retry(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
        RetryPolicy::default(),
        sink,
        || {
            let left = remaining.fetch_sub(1, Ordering::SeqCst);
            async move {
                if left > 1 {
                    Err(unavailable())
                } else {
                    tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await
                }
            }
        },
        sleeper.clone(),
    )
    .await;

    let listener = result.expect("the third attempt binds");
    assert_eq!(sleeper.count(), 2, "two sleeps for the two failed attempts");
    assert!(
        listener.local_addr().expect("bound").port() != 0,
        "the returned listener holds a real port"
    );
    // The production sleeper is the tokio one; constructing it stays cheap and
    // the type is part of the public seam.
    let _prod: Arc<dyn RetrySleep> = Arc::new(TokioSleep);
}
