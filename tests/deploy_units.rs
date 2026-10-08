//! #342 — `deploy/pulse-serve.service` must bound its restarts.
//!
//! `pulse serve` retries its bind for the whole `RetryPolicy` budget before it
//! exits non-zero, and the unit waits `RestartSec` between starts. systemd's
//! default start limit (5 starts in 10 s) is never reached at that rate, so a
//! server that always fails restarted forever and looked "up". The unit must
//! carry its own `StartLimitIntervalSec=` / `StartLimitBurst=` in `[Unit]`,
//! sized so BOTH failure shapes trip it: a fast exit (about `RestartSec` per
//! cycle) and a slow failure (the full bind budget plus `RestartSec`).
//!
//! The test parses the unit text only: no systemd, no network.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::net::SocketAddr;

use pulse::RetryPolicy;

const UNIT: &str = include_str!("../deploy/pulse-serve.service");
// r4.s2.w4 (Q2): the off-box watcher's units, parsed by the same tiny reader.
const WATCH_SERVICE: &str = include_str!("../deploy/pulse-watch.service");
const WATCH_TIMER: &str = include_str!("../deploy/pulse-watch.timer");

/// r4.s2.w3 (C5/ADR-0029): draco-desk's QA unit. QA runs BESIDE the current
/// prod on draco-desk until the cutover retires `pulse-serve.service`, so its
/// bind, its database and its data dir are its own — and the role flag is what
/// makes the server refuse the other role's data.
const QA_UNIT: &str = include_str!("../deploy/pulse-qa.service");

/// The `key=value` lines of one `[Section]` of `unit`, comments and blanks skipped.
fn section_of(unit: &str, name: &str) -> HashMap<String, String> {
    let header = format!("[{name}]");
    let mut in_section = false;
    let mut keys = HashMap::new();
    for line in unit.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_section = line == header;
            continue;
        }
        if in_section && let Some((key, value)) = line.split_once('=') {
            keys.insert(key.trim().to_owned(), value.trim().to_owned());
        }
    }
    keys
}

/// The `key=value` lines of one `[Section]` of the prod unit.
fn section(name: &str) -> HashMap<String, String> {
    section_of(UNIT, name)
}

/// The whitespace-separated value following `flag` in an `ExecStart` line.
fn arg_value<'a>(exec: &'a str, flag: &str) -> &'a str {
    exec.split_whitespace()
        .skip_while(|arg| *arg != flag)
        .nth(1)
        .unwrap_or_else(|| panic!("{flag} has no value in {exec}"))
}

/// `100.64.0.0/10` membership: first octet 100, second octet 64..=127 (D6).
fn assert_tailnet(addr: SocketAddr) {
    let SocketAddr::V4(v4) = addr else {
        panic!("the tailnet bind is a v4 address, got {addr}");
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
}
fn unit_number(unit: &HashMap<String, String>, key: &str) -> u64 {
    unit.get(key)
        .unwrap_or_else(|| panic!("[Unit] sets no {key}= (systemd's default 5 starts / 10 s never trips at this restart rate)"))
        .parse()
        .unwrap_or_else(|_| panic!("{key}= must be a plain number of seconds"))
}

/// `RestartSec` as plain seconds; both tests read it here so they agree.
fn restart_sec(service: &HashMap<String, String>) -> u64 {
    service["RestartSec"]
        .parse()
        .expect("RestartSec is plain seconds")
}

#[test]
fn unit_bounds_restarts_for_fast_and_slow_failures() {
    let unit = section("Unit");
    let service = section("Service");
    assert!(
        !service.contains_key("StartLimitIntervalSec") && !service.contains_key("StartLimitBurst"),
        "the start limit belongs in [Unit], not [Service]"
    );
    let interval = unit_number(&unit, "StartLimitIntervalSec");
    let burst = unit_number(&unit, "StartLimitBurst");
    assert_eq!(burst, 3, "the unit must allow three starts");
    assert!(
        interval > 0,
        "StartLimitIntervalSec=0 would disable the limit"
    );

    let restart_sec = restart_sec(&service);
    let bind_budget = RetryPolicy::default().budget.as_secs();
    assert_eq!(bind_budget, 120, "the unit's sizing comment assumes 120 s");

    let fast_cycle = restart_sec;
    let slow_cycle = bind_budget + restart_sec;
    assert!(
        burst * fast_cycle < interval,
        "fast failure: {burst} starts x {fast_cycle} s must fit in {interval} s"
    );
    assert!(
        burst * slow_cycle < interval,
        "slow failure: {burst} starts x {slow_cycle} s must fit in {interval} s"
    );
}

#[test]
fn unit_keeps_the_d6_boot_race_contract_and_the_tailnet_bind() {
    let service = section("Service");
    assert_eq!(service["Restart"], "on-failure");
    assert_eq!(restart_sec(&service), 10);
    let exec = &service["ExecStart"];
    assert!(
        exec.ends_with("pulse serve --bind 100.90.203.21:8420"),
        "{exec}"
    );
    assert!(
        !exec.contains("0.0.0.0"),
        "the server must never listen on a wildcard"
    );
    assert!(
        !service
            .keys()
            .any(|k| k == "Environment" || k == "EnvironmentFile"),
        "the unit sets no credential"
    );
    assert_eq!(
        section("Install")["WantedBy"],
        "default.target",
        "`just deploy` enables the unit for the default target"
    );
}

// ---------------------------------------------------------------------------
// r4.s2.w3 — the QA unit: draco-desk's own server (C5/ADR-0029)
// ---------------------------------------------------------------------------

#[test]
fn qa_unit_bounds_restarts_like_the_prod_unit() {
    let unit = section_of(QA_UNIT, "Unit");
    let service = section_of(QA_UNIT, "Service");
    assert!(
        !service.contains_key("StartLimitIntervalSec") && !service.contains_key("StartLimitBurst"),
        "the start limit belongs in [Unit], not [Service]"
    );
    let interval = unit_number(&unit, "StartLimitIntervalSec");
    let burst = unit_number(&unit, "StartLimitBurst");
    assert_eq!(burst, 3, "the QA unit must allow three starts like prod's");
    assert!(
        interval > 0,
        "StartLimitIntervalSec=0 would disable the limit"
    );

    let restart_sec = restart_sec(&service);
    let bind_budget = RetryPolicy::default().budget.as_secs();
    let fast_cycle = restart_sec;
    let slow_cycle = bind_budget + restart_sec;
    assert!(
        burst * fast_cycle < interval,
        "fast failure: {burst} starts x {fast_cycle} s must fit in {interval} s"
    );
    assert!(
        burst * slow_cycle < interval,
        "slow failure: {burst} starts x {slow_cycle} s must fit in {interval} s"
    );
}

#[test]
fn qa_unit_serves_qa_on_its_own_port_database_and_data_dir() {
    let service = section_of(QA_UNIT, "Service");
    assert_eq!(service["Restart"], "on-failure");
    assert_eq!(restart_sec(&service), 10);
    let exec = &service["ExecStart"];
    assert!(
        exec.contains("pulse serve --role qa"),
        "the QA unit names its role: {exec}"
    );
    assert_eq!(
        arg_value(exec, "--bind"),
        "100.90.203.21:8421",
        "QA listens on 8421 so it can run beside the current prod on 8420: {exec}"
    );
    assert_tailnet(
        arg_value(exec, "--bind")
            .parse()
            .expect("the bind address parses"),
    );
    assert!(
        !exec.contains("0.0.0.0") && !exec.contains("8420"),
        "no wildcard, and no collision with the prod port: {exec}"
    );
    assert_eq!(
        arg_value(exec, "--db"),
        "%h/.local/share/pulse-qa/pulse.db",
        "{exec}"
    );
    assert_eq!(
        arg_value(exec, "--data-dir"),
        "%h/.local/share/pulse-qa",
        "{exec}"
    );
    assert!(
        !service
            .keys()
            .any(|k| k == "Environment" || k == "EnvironmentFile"),
        "the unit sets no credential"
    );
    assert_eq!(
        section_of(QA_UNIT, "Install")["WantedBy"],
        "default.target",
        "`just deploy` enables the QA unit for the default target"
    );
}

// ---------------------------------------------------------------------------
// r4.s2.w4 (Q2): `deploy/pulse-watch.{service,timer}` — one probe cycle a
// minute against the Mini, with the operator's topic/state paths. Installing and
// enabling them is the cutover's (SPINE.md step 6); this test pins their text.
// ---------------------------------------------------------------------------

#[test]
fn watch_service_is_one_probe_cycle_with_the_operator_paths() {
    let service = section_of(WATCH_SERVICE, "Service");
    assert_eq!(
        service["Type"], "oneshot",
        "one run is one probe cycle, not a daemon"
    );
    let exec = &service["ExecStart"];
    assert!(
        exec.starts_with("%h/.local/share/pulse-serve/bin/pulse watch"),
        "the installed binary runs `pulse watch`: {exec}"
    );
    for argument in [
        "--url http://100.103.30.74:8420",
        "--ssh-host macmini",
        "--topic-file %h/.config/pulse-watch/topic",
        "--state-file %h/.local/state/pulse-watch/state",
    ] {
        assert!(
            exec.contains(argument),
            "ExecStart is missing `{argument}`: {exec}"
        );
    }
    assert!(
        exec.contains("--url ") && !exec.contains("0.0.0.0"),
        "the watcher probes the Mini's tailnet address: {exec}"
    );
}

#[test]
fn watch_service_carries_no_credential() {
    let service = section_of(WATCH_SERVICE, "Service");
    // The topic is the watcher's only secret and it lives in its 0600 file; no
    // unit — this one included — sets a credential in the environment.
    assert!(
        !service
            .keys()
            .any(|k| k == "Environment" || k == "EnvironmentFile"),
        "the watcher unit sets no credential"
    );
    assert!(
        !service["ExecStart"].contains("--ntfy-url"),
        "the unit uses the default ntfy base URL: {}",
        service["ExecStart"]
    );
}

#[test]
fn watch_timer_probes_every_sixty_seconds() {
    let timer = section_of(WATCH_TIMER, "Timer");
    assert_eq!(
        timer["OnBootSec"], "60",
        "a rebooted draco-desk starts probing a minute after boot"
    );
    assert_eq!(timer["OnUnitActiveSec"], "60", "one probe a minute (Q2)");
    assert_eq!(
        section_of(WATCH_TIMER, "Install")["WantedBy"],
        "timers.target",
        "a timer is enabled for timers.target"
    );
}
