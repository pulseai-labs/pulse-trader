//! r4.s2.w1 (demo line d68) — the macOS `LaunchAgent` units and the bounded start
//! counter.
//!
//! Two halves, mirroring the spec:
//!
//! 1. **The plists** (`deploy/com.pulsetrader.serve.plist` and
//!    `deploy/com.pulsetrader.logrotate.plist`), parsed with a small
//!    hand-rolled XML-plist reader over the subset the two files use — no new
//!    dependency, so `cargo deny` and the lockfile guard stay untouched. The
//!    assertions are the unit's contract: the bind stays inside the tailnet
//!    (`100.64.0.0/10`), `KeepAlive { SuccessfulExit = false }` plus
//!    `--start-limit 3/900` bound the restarts (launchd has no start limit of
//!    its own), the environment carries no credential, and the logs land under
//!    `~/Library/Logs/PulseTrader/` (G5, G7, G9).
//!
//! 2. **The start counter** (`pulse::record_start`, G5): with an injected clock
//!    and a temporary data directory, three starts inside the window run, the
//!    fourth writes `<data dir>/serve-start-limit` and refuses, a start with
//!    the marker already present is refused the same way, and entries older
//!    than the window stop counting. The last test drives the REAL binary:
//!    a refused start must exit **0**, because that is what makes launchd's
//!    `KeepAlive { SuccessfulExit = false }` stop relaunching.
//!
//! The test parses unit text and plist XML only: no launchd, no network, no
//! macOS.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use pulse::{StartGuard, StartLimit, record_start};

const SERVE_PLIST: &str = include_str!("../deploy/com.pulsetrader.serve.plist");
const LOGROTATE_PLIST: &str = include_str!("../deploy/com.pulsetrader.logrotate.plist");
/// r4.s2.w5 (C3/G9): the Mini's nightly backup — a launchd calendar job, the
/// same pattern as the serve agent.
const BACKUP_PLIST: &str = include_str!("../deploy/com.pulsetrader.backup.plist");

/// The plist the agent runs: 3 starts in 900 s — #342's parity.
const LIMIT: StartLimit = StartLimit {
    starts: 3,
    window_secs: 900,
};

// ---------------------------------------------------------------------------
// The small XML-plist reader.
// ---------------------------------------------------------------------------

/// One plist value, restricted to what these two files use.
#[derive(Debug, Clone, PartialEq)]
enum Plist {
    String(String),
    Bool(bool),
    Integer(i64),
    Dict(BTreeMap<String, Plist>),
    Array(Vec<Plist>),
}

impl Plist {
    fn dict(&self) -> &BTreeMap<String, Plist> {
        match self {
            Plist::Dict(map) => map,
            other => panic!("expected a <dict>, got {other:?}"),
        }
    }

    fn get(&self, key: &str) -> &Plist {
        self.dict()
            .get(key)
            .unwrap_or_else(|| panic!("no <key>{key}</key>"))
    }

    fn str_value(&self) -> &str {
        match self {
            Plist::String(s) => s,
            other => panic!("expected a <string>, got {other:?}"),
        }
    }

    fn bool_value(&self) -> bool {
        match self {
            Plist::Bool(b) => *b,
            other => panic!("expected <true/> or <false/>, got {other:?}"),
        }
    }

    fn integer(&self) -> i64 {
        match self {
            Plist::Integer(n) => *n,
            other => panic!("expected an <integer>, got {other:?}"),
        }
    }

    fn array(&self) -> &[Plist] {
        match self {
            Plist::Array(items) => items,
            other => panic!("expected an <array>, got {other:?}"),
        }
    }

    /// The `<string>` elements of an array, in order — the argument vectors.
    fn strings(&self) -> Vec<&str> {
        self.array().iter().map(Plist::str_value).collect()
    }
}

#[derive(Debug)]
enum Token {
    Open(String),
    Close(String),
    Empty(String),
    Text(String),
}

/// Split plist XML into tags and text, skipping the XML declaration, the
/// DOCTYPE and comments.
fn tokenize(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut rest = text;
    loop {
        let Some(start) = rest.find('<') else {
            let tail = rest.trim();
            assert!(
                tail.is_empty(),
                "unexpected text after the last element: {tail:?}"
            );
            return tokens;
        };
        let before = rest[..start].trim();
        if !before.is_empty() {
            tokens.push(Token::Text(before.to_owned()));
        }
        if rest[start..].starts_with("<!--") {
            let close = rest[start..].find("-->").expect("unterminated comment") + start;
            let body = &rest[start + 4..close];
            // XML forbids `--` inside a comment, and launchd does not load a
            // plist whose comment carries one: the same `--flag` typo that
            // looks harmless in a comment makes the whole file unparseable.
            assert!(
                !body.contains("--"),
                "the XML comment contains '--' (the plist would not parse): {body}"
            );
            rest = &rest[close + 3..];
            continue;
        }
        let end = rest[start..].find('>').expect("unterminated tag") + start + 1;
        let inner = rest[start + 1..end - 1].trim();
        rest = &rest[end..];
        if inner.starts_with('?') || inner.starts_with('!') {
            // The XML declaration and the DOCTYPE carry no plist value.
            continue;
        }
        if let Some(name) = inner.strip_prefix('/') {
            tokens.push(Token::Close(name.trim().to_owned()));
        } else if let Some(name) = inner.strip_suffix('/') {
            tokens.push(Token::Empty(name.trim().to_owned()));
        } else {
            // `plist version="1.0"` is the element `plist`.
            let name = inner
                .split_whitespace()
                .next()
                .expect("an element name")
                .to_owned();
            tokens.push(Token::Open(name));
        }
    }
}

fn parse_plist(text: &str) -> Plist {
    let tokens = tokenize(text);
    let mut index = 0;
    match tokens.get(index) {
        Some(Token::Open(name)) if name == "plist" => index += 1,
        other => panic!("expected <plist>, got {other:?}"),
    }
    let value = parse_value(&tokens, &mut index);
    match tokens.get(index) {
        Some(Token::Close(name)) if name == "plist" => index += 1,
        other => panic!("expected </plist>, got {other:?}"),
    }
    assert_eq!(index, tokens.len(), "trailing tokens after </plist>");
    value
}

fn parse_value(tokens: &[Token], index: &mut usize) -> Plist {
    let token = tokens.get(*index).expect("unexpected end of plist");
    *index += 1;
    match token {
        Token::Empty(name) => match name.as_str() {
            "true" => Plist::Bool(true),
            "false" => Plist::Bool(false),
            other => panic!("unexpected empty element <{other}/>"),
        },
        Token::Open(name) => match name.as_str() {
            "string" => {
                let text = take_text(tokens, index);
                take_close(tokens, index, "string");
                Plist::String(text)
            }
            "integer" => {
                let text = take_text(tokens, index);
                take_close(tokens, index, "integer");
                Plist::Integer(
                    text.parse()
                        .unwrap_or_else(|_| panic!("<integer>{text}</integer> is not a number")),
                )
            }
            "dict" => {
                let mut map = BTreeMap::new();
                loop {
                    match tokens.get(*index) {
                        Some(Token::Close(close)) if close == "dict" => {
                            *index += 1;
                            break;
                        }
                        Some(Token::Open(open)) if open == "key" => {
                            *index += 1;
                            let key = take_text(tokens, index);
                            take_close(tokens, index, "key");
                            let value = parse_value(tokens, index);
                            map.insert(key, value);
                        }
                        other => panic!("inside <dict>: expected <key> or </dict>, got {other:?}"),
                    }
                }
                Plist::Dict(map)
            }
            "array" => {
                let mut items = Vec::new();
                loop {
                    match tokens.get(*index) {
                        Some(Token::Close(close)) if close == "array" => {
                            *index += 1;
                            break;
                        }
                        Some(_) => items.push(parse_value(tokens, index)),
                        None => panic!("unterminated <array>"),
                    }
                }
                Plist::Array(items)
            }
            other => panic!("unexpected element <{other}>"),
        },
        Token::Text(text) => panic!("unexpected text {text:?}"),
        Token::Close(name) => panic!("unexpected </{name}>"),
    }
}

fn take_text(tokens: &[Token], index: &mut usize) -> String {
    match tokens.get(*index) {
        Some(Token::Text(text)) => {
            *index += 1;
            text.clone()
        }
        other => panic!("expected element text, got {other:?}"),
    }
}

fn take_close(tokens: &[Token], index: &mut usize, name: &str) {
    match tokens.get(*index) {
        Some(Token::Close(close)) if close == name => *index += 1,
        other => panic!("expected </{name}>, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The serve plist: the launchd contract (G5, G6, G7, G9, G10).
// ---------------------------------------------------------------------------

#[test]
fn serve_plist_binds_the_tailnet_and_bounds_restarts() {
    let plist = parse_plist(SERVE_PLIST);
    assert_eq!(plist.get("Label").str_value(), "com.pulsetrader.serve");

    let args = plist.get("ProgramArguments").strings();
    assert!(
        args[0].starts_with('/') && args[0].ends_with("/bin/pulse"),
        "the first argument is the installed binary: {args:?}"
    );
    assert!(args.contains(&"serve"), "{args:?}");

    // The bind: `--bind <addr>`, one argument per element, inside 100.64.0.0/10.
    let bind_at = args
        .iter()
        .position(|arg| *arg == "--bind")
        .expect("--bind is in ProgramArguments");
    let addr: SocketAddr = args[bind_at + 1].parse().expect("the bind address parses");
    let SocketAddr::V4(v4) = addr else {
        panic!("the Mini's bind is the v4 tailnet address, got {addr}");
    };
    let octets = v4.ip().octets();
    assert_eq!(
        octets[0], 100,
        "the tailnet range starts at 100.64.0.0, got {addr}"
    );
    assert!(
        (64..=127).contains(&octets[1]),
        "100.64.0.0/10 covers 100.64-100.127, got {addr}"
    );
    assert!(
        !args.iter().any(|arg| arg.contains("0.0.0.0")),
        "the server must never listen on a wildcard: {args:?}"
    );

    // Restart bound (G5): launchd has no start limit of its own.
    let limit_at = args
        .iter()
        .position(|arg| *arg == "--start-limit")
        .expect("--start-limit is in ProgramArguments");
    assert_eq!(args[limit_at + 1], "3/900");

    // r4.s2.w3 (C5): the Mini's server is PROD, pinned by the role marker it
    // writes into its data dir — with the database inside that dir, a QA
    // server and a prod server refuse each other's data by name.
    let role_at = args
        .iter()
        .position(|arg| *arg == "--role")
        .expect("--role is in ProgramArguments");
    assert_eq!(args[role_at + 1], "prod", "the Mini's server is prod");

    assert!(plist.get("RunAtLoad").bool_value());
    assert_eq!(
        plist.get("KeepAlive").dict().get("SuccessfulExit"),
        Some(&Plist::Bool(false)),
        "KeepAlive {{ SuccessfulExit = false }} is what makes exit 0 stop the relaunch"
    );
    assert_eq!(plist.get("ThrottleInterval").integer(), 10);
}

#[test]
fn serve_plist_carries_no_credential_and_the_rotated_log_paths() {
    let plist = parse_plist(SERVE_PLIST);

    // G7: the ONLY environment entry is the installed config dir. No token, no
    // key, nothing credential-shaped.
    let env = plist.get("EnvironmentVariables").dict();
    assert_eq!(
        env.keys().collect::<Vec<_>>(),
        vec!["PULSE_CONFIG_DIR"],
        "the environment carries the config dir and nothing else"
    );
    assert!(
        env["PULSE_CONFIG_DIR"].str_value().starts_with('/'),
        "the config dir is the installed absolute path: {}",
        env["PULSE_CONFIG_DIR"].str_value()
    );
    for key in env.keys() {
        let upper = key.to_ascii_uppercase();
        assert!(
            !upper.contains("KEY") && !upper.contains("TOKEN") && !upper.contains("SECRET"),
            "credential-shaped environment key {key}"
        );
    }

    // G9: launchd's stdout/stderr land in the rotated log directory.
    for key in ["StandardOutPath", "StandardErrorPath"] {
        let path = plist.get(key).str_value();
        assert!(
            path.contains("Library/Logs/PulseTrader"),
            "{key} = {path} is outside ~/Library/Logs/PulseTrader"
        );
    }
}

#[test]
fn logrotate_plist_is_a_daily_calendar_job_over_the_installed_script() {
    let plist = parse_plist(LOGROTATE_PLIST);
    assert_eq!(plist.get("Label").str_value(), "com.pulsetrader.logrotate");

    let args = plist.get("ProgramArguments").strings();
    assert_eq!(args[0], "/bin/bash");
    assert!(
        args[1].starts_with('/') && args[1].ends_with("deploy/pulse-logrotate.sh"),
        "the rotation runs the installed script: {args:?}"
    );

    let calendar = plist.get("StartCalendarInterval").dict();
    let hour = calendar.get("Hour").expect("an Hour").integer();
    let minute = calendar.get("Minute").expect("a Minute").integer();
    assert!(
        (0..24).contains(&hour) && (0..60).contains(&minute),
        "daily at {hour}:{minute:02} is not a wall-clock time"
    );

    assert!(
        plist.dict().get("EnvironmentVariables").is_none(),
        "the rotation job needs no environment, and certainly no credential"
    );
}

// ---------------------------------------------------------------------------
// The start counter (G5), with an injected clock and a temporary data dir.
// ---------------------------------------------------------------------------

fn marker(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("serve-start-limit")).expect("the marker exists")
}

#[test]
fn three_starts_inside_the_window_and_the_fourth_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for now in [1_000_u64, 1_100, 1_200] {
        assert_eq!(
            record_start(dir.path(), LIMIT, now).unwrap(),
            StartGuard::Proceed,
            "start at {now} is inside 3/900"
        );
    }

    assert_eq!(
        record_start(dir.path(), LIMIT, 1_300).unwrap(),
        StartGuard::Limited {
            count: 4,
            marker_written: true
        },
        "the 4th start inside 900 s is refused"
    );
    let written = marker(dir.path());
    assert!(
        written.contains("utc=1970-01-01T00:21:40Z"),
        "the marker holds the UTC time: {written}"
    );
    assert!(written.contains("unix=1300"), "{written}");
    assert!(
        written.contains("starts=4"),
        "the marker holds the count: {written}"
    );
    assert!(written.contains("window_seconds=900"), "{written}");
    assert!(written.contains("limit=3"), "{written}");

    // A start with the marker already present is refused the same way, and
    // records nothing new: `just prod-reset` clears the marker and the log
    // together, and it is the reset that re-arms the agent.
    let log = std::fs::read_to_string(dir.path().join("serve-starts")).unwrap();
    assert_eq!(
        record_start(dir.path(), LIMIT, 1_400).unwrap(),
        StartGuard::Limited {
            count: 4,
            marker_written: true
        }
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("serve-starts")).unwrap(),
        log,
        "a marker-present start appends nothing"
    );
}

#[test]
fn starts_outside_the_window_do_not_count() {
    let dir = tempfile::tempdir().unwrap();
    for now in [0_u64, 100, 200] {
        assert_eq!(
            record_start(dir.path(), LIMIT, now).unwrap(),
            StartGuard::Proceed
        );
    }

    // At t = 900 the t = 0 start has left the window: 100, 200 and this one are
    // exactly the limit, and the log keeps only the in-window entries.
    assert_eq!(
        record_start(dir.path(), LIMIT, 900).unwrap(),
        StartGuard::Proceed
    );
    let log = std::fs::read_to_string(dir.path().join("serve-starts")).unwrap();
    assert_eq!(log.lines().count(), 3, "the stale entry is pruned: {log}");
    assert!(
        !log.lines().any(|line| line == "0"),
        "0 is outside the window: {log}"
    );

    // The next start inside the window is the 4th.
    assert_eq!(
        record_start(dir.path(), LIMIT, 950).unwrap(),
        StartGuard::Limited {
            count: 4,
            marker_written: true
        }
    );
}

// ---------------------------------------------------------------------------
// The launchd contract end to end: a refused start EXITS 0.
// ---------------------------------------------------------------------------

#[test]
fn a_refused_start_exits_zero_through_the_real_binary() {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // Three starts "now": the server's own next start is the 4th inside 900 s.
    std::fs::write(
        dir.path().join("serve-starts"),
        format!("{now}\n{now}\n{now}\n"),
    )
    .unwrap();
    let db = dir.path().join("pulse.db");

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_pulse"))
            .args([
                "serve",
                "--bind",
                "127.0.0.1:0",
                "--dev-loopback",
                "--start-limit",
                "3/900",
                "--db",
                db.to_str().expect("utf-8 temp path"),
                "--data-dir",
                dir.path().to_str().expect("utf-8 temp path"),
            ])
            .output()
            .expect("run pulse serve")
    };

    let fourth = run();
    let stderr = String::from_utf8_lossy(&fourth.stderr).into_owned();
    assert_eq!(
        fourth.status.code(),
        Some(0),
        "a refused start must exit 0 (KeepAlive{{SuccessfulExit=false}} stops relaunching); stderr: {stderr}"
    );
    assert!(
        stderr.contains("start limit"),
        "the refusal is named: {stderr}"
    );
    let written = marker(dir.path());
    assert!(written.contains("starts=4"), "{written}");
    let log_after_fourth = std::fs::read_to_string(dir.path().join("serve-starts")).unwrap();
    assert_eq!(log_after_fourth.lines().count(), 4);

    // With the marker present, the next start exits 0 the same way.
    let with_marker = run();
    assert_eq!(
        with_marker.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&with_marker.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("serve-starts")).unwrap(),
        log_after_fourth,
        "a marker-present start appends nothing"
    );
}

// ---------------------------------------------------------------------------
// r4.s2.w5 (C3/G9) — the Mini's nightly backup job
// ---------------------------------------------------------------------------

#[test]
fn backup_plist_is_a_daily_calendar_job_over_the_installed_binary() {
    let plist = parse_plist(BACKUP_PLIST);
    assert_eq!(
        plist.get("Label").str_value(),
        "com.pulsetrader.backup",
        "the agent's label"
    );
    assert_eq!(
        plist.get("ProgramArguments").strings(),
        vec!["/Users/draco/.local/share/pulse-serve/bin/pulse", "backup"],
        "the installed binary's `backup` with its defaults: the platform data dir, ~/pulse-backups and keep 14"
    );
    let calendar = plist.get("StartCalendarInterval");
    assert_eq!(calendar.get("Hour").integer(), 3, "03:30 local");
    assert_eq!(calendar.get("Minute").integer(), 30, "03:30 local");
}

#[test]
fn backup_plist_carries_no_credential_and_writes_the_logs() {
    let plist = parse_plist(BACKUP_PLIST);
    let dict = plist.dict();
    assert!(
        !dict.contains_key("EnvironmentVariables"),
        "`pulse backup` reads no credential, and no plist carries one (G7)"
    );
    assert!(
        !dict.contains_key("RunAtLoad") && !dict.contains_key("KeepAlive"),
        "a calendar job neither runs at load nor is kept alive; launchd runs a missed \
         occurrence when the machine wakes"
    );
    let out = plist.get("StandardOutPath").str_value();
    let err = plist.get("StandardErrorPath").str_value();
    assert_eq!(out, "/Users/draco/Library/Logs/PulseTrader/backup.log");
    assert_eq!(err, "/Users/draco/Library/Logs/PulseTrader/backup.err");
    assert!(
        out.starts_with('/') && err.starts_with('/'),
        "launchd expands no `~`: every path in a plist is absolute"
    );
}
