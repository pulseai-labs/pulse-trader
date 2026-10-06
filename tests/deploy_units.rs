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

use pulse::RetryPolicy;

const UNIT: &str = include_str!("../deploy/pulse-serve.service");

/// The `key=value` lines of one `[Section]`, comments and blanks skipped.
fn section(name: &str) -> HashMap<String, String> {
    let header = format!("[{name}]");
    let mut in_section = false;
    let mut keys = HashMap::new();
    for line in UNIT.lines().map(str::trim) {
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

fn unit_secs(unit: &HashMap<String, String>, key: &str) -> u64 {
    unit.get(key)
        .unwrap_or_else(|| panic!("[Unit] sets no {key}= (systemd's default 5 starts / 10 s never trips at this restart rate)"))
        .parse()
        .unwrap_or_else(|_| panic!("{key}= must be a plain number of seconds"))
}

#[test]
fn unit_bounds_restarts_for_fast_and_slow_failures() {
    let unit = section("Unit");
    let service = section("Service");
    assert!(
        !service.contains_key("StartLimitIntervalSec") && !service.contains_key("StartLimitBurst"),
        "the start limit belongs in [Unit], not [Service]"
    );
    let interval = unit_secs(&unit, "StartLimitIntervalSec");
    let burst = unit_secs(&unit, "StartLimitBurst");
    assert!(burst > 0, "StartLimitBurst=0 would disable the limit");
    assert!(
        interval > 0,
        "StartLimitIntervalSec=0 would disable the limit"
    );

    let restart_sec: u64 = service["RestartSec"]
        .trim_end_matches('s')
        .parse()
        .expect("RestartSec is plain seconds");
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
    assert_eq!(service["RestartSec"], "10");
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
}
