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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

/// r4.s2.w5 (C3): the off-box backup pull and the Mini's forced command.
const PULL_SERVICE: &str = include_str!("../deploy/pulse-backup-pull.service");
const PULL_TIMER: &str = include_str!("../deploy/pulse-backup-pull.timer");

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
        exec.starts_with("%h/.local/share/pulse-qa/bin/pulse watch"),
        "the installed binary runs `pulse watch` — the one `just deploy` installs into \
         ~/.local/share/pulse-qa/bin, the same binary the pull unit verifies with: {exec}"
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

// ---------------------------------------------------------------------------
// r4.s2.w5 (C3) — `deploy/pulse-backup-pull.{service,timer}`: the pull runs on
// draco-desk an hour after the Mini's own backup, and the Mini's forced command
// (`deploy/pulse-backup-serve.sh`) serves exactly one shape — a read.
// Installing the units, the script and the key is the cutover's (SPINE.md step
// 6, a credential stop); these tests pin the text and the script's behaviour.
// ---------------------------------------------------------------------------

#[test]
fn pull_service_is_one_pull_cycle_with_the_ssh_target() {
    let service = section_of(PULL_SERVICE, "Service");
    assert_eq!(service["Type"], "oneshot", "one run is one pull");
    let exec = &service["ExecStart"];
    let words: Vec<&str> = exec.split_whitespace().collect();
    assert_eq!(words.len(), 2, "the script and its one argument: {exec}");
    assert!(
        words[0].starts_with("%h/") && words[0].ends_with("deploy/pulse-backup-pull.sh"),
        "the installed script runs the pull: {exec}"
    );
    assert_eq!(
        words[1], "macmini:/Users/draco/pulse-backups",
        "the ssh target, an ABSOLUTE remote path — a forced command expands no `~`: {exec}"
    );
    assert!(
        !service
            .keys()
            .any(|k| k == "Environment" || k == "EnvironmentFile"),
        "the unit sets no credential; the key, the destination and the verify binary are \
         the script's defaults"
    );
}

#[test]
fn pull_timer_pulls_after_the_mini_backup() {
    let timer = section_of(PULL_TIMER, "Timer");
    assert_eq!(
        timer["OnCalendar"], "*-*-* 04:30:00",
        "an hour after the Mini's own 03:30 backup"
    );
    assert_eq!(
        timer["Persistent"], "true",
        "a powered-off draco-desk runs the missed pull at boot"
    );
    assert_eq!(
        section_of(PULL_TIMER, "Install")["WantedBy"],
        "timers.target",
        "a timer is enabled for timers.target"
    );
}

/// The forced-command script, run the way sshd would: `bash
/// deploy/pulse-backup-serve.sh` with `SSH_ORIGINAL_COMMAND` set.
fn serve_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/pulse-backup-serve.sh")
}

/// A temp `rsync` that records its argv and exits 0. The forced command must
/// EXEC rsync with the client's own words, and this is how a test sees what it
/// exec'd — a refusal never reaches it.
struct FakeRsync {
    dir: tempfile::TempDir,
    log: PathBuf,
}

impl FakeRsync {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("rsync-argv.log");
        let rsync = dir.path().join("rsync");
        std::fs::write(
            &rsync,
            "#!/usr/bin/env bash\nset -euo pipefail\nprintf '%s\\n' \"$@\" > \"$RSYNC_ARGV_LOG\"\nexit 0\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&rsync).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&rsync, permissions).unwrap();
        Self { dir, log }
    }

    fn run(&self, root: &Path, command: &str) -> Output {
        Command::new("bash")
            .arg(serve_script())
            .env("SSH_ORIGINAL_COMMAND", command)
            .env("PULSE_BACKUP_ROOT", root)
            .env("RSYNC_ARGV_LOG", &self.log)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.dir.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .output()
            .expect("run the forced command")
    }

    /// What rsync was exec'd with; empty when a refusal stopped it first.
    fn argv(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

#[test]
fn forced_command_serves_a_protocol_29_sender_invocation() {
    let fake = FakeRsync::new();
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().display().to_string();

    // The protocol-29 (openrsync / "rsync version 2.6.9 compatible") shape the
    // Mini's client sends — the acceptance case the plan gate asked for — and
    // the modern rsync 3.x bundle beside it. Both are SERVED, never refused.
    // The third is what macOS's openrsync ACTUALLY sends for the pull's own
    // invocation (PR-354 fix C4): the short flags one word each, plus the
    // pull's `--ignore-existing` as a long option.
    for command in [
        format!("rsync --server --sender -logDtpr . {root_path}/"),
        format!("rsync --server --sender -logDtpre.iLsfxC . {root_path}"),
        format!(
            "rsync --server --sender -g -l -o -p -r -t -D --ignore-existing . {root_path}/"
        ),
    ] {
        let output = fake.run(root.path(), &command);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "a sender invocation must be served: {command}: {stderr}"
        );
        assert!(!stderr.contains("refused"), "nothing was refused: {stderr}");
        let argv = fake.argv();
        assert!(
            argv.contains("--server") && argv.contains("--sender"),
            "rsync got the client's own words: {argv}"
        );
        assert!(argv.contains(&root_path), "the root travels: {argv}");
    }
}

#[test]
fn forced_command_refuses_everything_but_a_read() {
    let fake = FakeRsync::new();
    let root = tempfile::tempdir().unwrap();
    let root_path = root.path().display().to_string();

    // A write (an rsync invocation without `--sender`), a delete, a `..` path,
    // a path outside the root, a shell, and a shell metacharacter.
    for command in [
        format!("rsync --server -logDtpre.iLsfxC . {root_path}"),
        format!("rsync --server --sender --delete . {root_path}"),
        format!("rsync --server --sender -logDtpre.iLsfxC . {root_path}/../etc"),
        "rsync --server --sender -logDtpre.iLsfxC . /etc".to_owned(),
        format!("rm -rf {root_path}"),
        format!("rsync --server --sender -logDtpre.iLsfxC . {root_path}; rm -rf /"),
        "sh -c id".to_owned(),
        // The delete family's unique abbreviations and long spellings
        // (PR-354 fix C4): rsync and openrsync both accept `--remove-source`
        // for `--remove-source-files` and `--del` for `--delete*`, so an exact
        // allowlist is the only defense — a spelling denylist let these reach
        // the exec and delete files under $ROOT.
        format!("rsync --server --sender --remove-source . {root_path}"),
        format!("rsync --server --sender --remove-source-files . {root_path}"),
        format!("rsync --server --sender --del . {root_path}"),
        format!("rsync --server --sender --delete-after . {root_path}"),
    ] {
        let output = fake.run(root.path(), &command);
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(!output.status.success(), "must be refused: {command}");
        assert!(stderr.contains("refused"), "by name: {command}: {stderr}");
        assert!(
            fake.argv().is_empty(),
            "a refused command is never executed: {command}"
        );
    }
}
