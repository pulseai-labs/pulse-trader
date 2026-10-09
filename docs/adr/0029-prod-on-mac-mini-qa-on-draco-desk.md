# 29. Prod on the Mac Mini under launchd, draco-desk becomes QA

Date: 2026-10-08

## Status

Accepted. Amends [ADR-0026](0026-client-server-split.md)'s **Operations and
topology**: the always-on server moves from draco-desk's systemd user manager to
a launchd LaunchAgent in `draco`'s `gui/` domain on the Mac Mini, and draco-desk
becomes QA. ADR-0026's access model — the tailnet-only bind, per-client revocable
tokens, the audit table, the credential profile and the one-writer rule — is
unchanged and still binds; nothing here moves data. The move itself is the
operator's cutover after r4.s2's round 3. Sibling: [ADR-0028](0028-wf-v2-holdout-and-hypothesis-budget.md)
(r4.s1).

## Context

ADR-0026 put the one always-on process on draco-desk, a Linux workstation. Two
things changed. First, the trader's Mac app should reach a machine that is
always on and never rebuilt: draco-desk is where builds, walks and patches run,
so it is exactly the host whose restarts, toolchain bumps and experiments must
NOT be able to take prod down. Second, the Mini is already on the tailnet,
already fails over to AC with `sleep 0`/`disksleep 0`/`autorestart 1`, and its
readiness was checked read-only at planning (SPINE.md's readiness table).

The move changes the service manager, and the service managers differ in exactly
one way that matters: **systemd has a start limit, launchd does not.**
`deploy/pulse-serve.service` bounds restarts with `StartLimitIntervalSec=900` /
`StartLimitBurst=3` (#342) and latches `failed`. launchd's `KeepAlive`
relaunches forever, throttled only by `ThrottleInterval`; the only lever that
stops it is the EXIT CODE — with `KeepAlive { SuccessfulExit = false }` a process
that exits 0 is not relaunched. A server that crash-loops on the Mini would
therefore restart indefinitely with no record and no alert, which is the same
defect #342 closed on Linux.

The other facts that shape this decision:

- **launchd expands no `~`** in `ProgramArguments`, `EnvironmentVariables` or the
  log paths — every path in a plist is absolute — and it has no specifier
  syntax (`%h`) like systemd's.
- **launchd holds `StandardOutPath`/`StandardErrorPath` open and appends to the
  inode.** A log rotated by `mv` would leave the server writing to the moved
  file; rotation must copy and TRUNCATE in place.
- **The Mini has no Node and no `just`**, and no GitHub credential may reach it;
  the source arrives as `git archive <tag>` over ssh and is built there.
- **`pulse serve` runs on macOS without modification** (the server code is
  `cfg(unix)`; CI already runs clippy and nextest on `macos-latest`), and the
  macOS data dir is `~/Library/Application Support/PulseTrader/`.
- The `secret-provisioning` boundary was rewritten at planning (D4b): the LLM
  credential is a 0600 file owned by the service account,
  `<data dir>/.env`, written by the operator (G7). No credential belongs in a
  plist, in a unit, or in any log.

## Decision

**Prod runs on the Mac Mini as a launchd LaunchAgent; draco-desk becomes QA.**

**The agent.** `deploy/com.pulsetrader.serve.plist`, installed by `just
deploy-mac <tag>` into `~/Library/LaunchAgents/` and loaded with `launchctl
bootstrap gui/$(id -u)`. `Label` `com.pulsetrader.serve`; `ProgramArguments`
`/Users/draco/.local/share/pulse-serve/bin/pulse serve --bind
100.103.30.74:8420 --start-limit 3/900`; `RunAtLoad true`; `KeepAlive {
SuccessfulExit = false }`; `ThrottleInterval 10`. It binds the Mini's TAILNET
address only — the D6 policy and its 120-second retry are unchanged and
transfer intact; `0.0.0.0` appears nowhere. Its ONLY environment entry is
`PULSE_CONFIG_DIR=/Users/draco/.local/share/pulse-serve/config`, so the server
resolves its credential from the environment or the permission-checked `.env`
files and never from a checkout's build tree. `StandardOutPath` and
`StandardErrorPath` are `/Users/draco/Library/Logs/PulseTrader/serve.log` and
`.err`.

**The start limit (G5) is the server's own.** `pulse serve --start-limit
<N>/<SECONDS>` (off by default; the systemd unit passes nothing and is
unchanged) counts starts in a sliding window in `<data dir>/serve-starts` — one
UNIX-second line per start, pruned to the window, written through a temporary
file plus rename. When a start would make more than N starts inside the window,
the server writes `<data dir>/serve-start-limit` (the UTC time, the UNIX time,
the count, the window, the limit) and **exits 0, before binding** — so launchd
stops relaunching. A start with the marker already present exits 0 the same way
and records nothing. `just prod-reset` deletes the marker and the start log and
kickstarts the agent; `just deploy-mac` clears them before it bootstraps. The
window is the one systemd applies: a start counts while `now - started <
window`, and the count includes the start being decided. An IO failure in the
counter is fail-open (one named stderr line, the start proceeds): a non-zero
exit would relaunch-loop too, and a healthy server must not die for a counter
file — but a reached trip is never undone by an IO failure, marker or not.

**The health gate (#346) is a real probe.** `scripts/wait-healthy.sh <url>
[timeout]` polls `<url>/healthz` every 2 seconds until a 200 with
`"status":"ok"`, and exits non-zero printing the last response on timeout
(default 150 s, which covers the 120-second bind retry). Until w4 lands
`/healthz`, a 404 there falls back to a 401 from `/api/v1/handshake` carrying
`X-Pulse-Api-Version` — a refusal that proves the listener and the auth stack
answer — and says so. Both `just deploy` (draco-desk) and `just deploy-mac` end
with it, in place of `systemctl is-active`, which read `active` for the whole
bind retry and let a bind that never succeeded report success.

**`just deploy-mac <tag>`** runs on draco-desk and drives the Mini over ssh
(`Host macmini`): refuse a missing/unknown tag; `git archive <tag>` piped over
ssh into `~/.cache/pulse-deploy/src` (cleared first) — no GitHub credential on
the Mini; build there with `PULSE_ALLOW_PLACEHOLDER_DIST=1 cargo build
--release --bin pulse`; install the binary, `config/`, the rotation script and
both plists; check G10 (the data dir is mode 0700; `<data dir>/.env`, if
present, is 0600 and owned by `draco` — refused by name, its contents never
read or printed); clear the start limit; `bootout` (when loaded) and
`bootstrap` both agents and `kickstart -k gui/$(id -u)/com.pulsetrader.serve`;
then `wait-healthy.sh http://100.103.30.74:8420 150` and print the installed
`pulse --version`. The placeholder-dist setting is deliberate and documented:
`pulse serve` never serves the embedded frontend — its router mounts only the
`/api/v1` routes and `/mcp`, and no static handler exists under `src/server/`
(#314) — so a prod deploy must not depend on a frontend build the server never
reads.

**Log rotation (G9).** `deploy/com.pulsetrader.logrotate.plist` is a daily
`StartCalendarInterval` job (04:00 local) running
`~/.local/share/pulse-serve/deploy/pulse-logrotate.sh`, which copy-truncates
`serve.log` and `serve.err` into `serve.log.<YYYY-MM-DD>` / `.err.<date>` files
and deletes dated files older than 7 days. It deletes only inside
`~/Library/Logs/PulseTrader/`, and only files matching those two dated patterns.

**FileVault (R0).** The Mini keeps FileVault ON. After a power loss the operator
unlocks it, and the unlock logs `draco` in, so the LaunchAgent and Tailscale come
back. Planned restarts use `fdesetup authrestart`. An unattended reboot is
therefore NOT a supported recovery path, which is why an off-box watcher (below)
is what tells the operator prod is down.

**`/healthz` (Q1) — landed by w4.** One route outside the auth middleware,
reachable only through the tailnet bind, answering only
`{"status":"ok"|"degraded","api_version":N}` (`degraded` = the paper runtime is
not running) — status 200 in both states, since the route reports the server's
own state and the deploy gate waits for `ok` specifically — writing no
`token_audit` row and touching no table. It keeps the router-level request-log
and API-version layers and nothing else, so it is also the one route with no
auth pass. It is the monitoring surface the watcher and the deploy gate read,
and its contract is pinned by `tests/healthz_watch.rs` (demo line d71).

**The off-box watcher and the backup pull (Q2) — recorded here, implemented by
w4 and w5.** `pulse watch` runs from a draco-desk systemd timer, probes every 60
seconds, and pushes to ntfy.sh (a random 32-character topic in a 0600 file on
draco-desk) after 3 failed probes, or at once on the start-limit marker. It tells
"Mini unreachable — it may need a FileVault unlock" from "service down", and
carries no token, no credential URL and no session data. The nightly backup is
pulled OFF the Mini by draco-desk (the Mini has no outbound credential and
`draco-desk:22` is closed from the Mini) over a dedicated read-only key, with a
restore drill against the pulled copy. "Off-box" is the same home LAN — a known
limit recorded at planning (C6); an off-site copy is a later candidate.

**The off-box backup (w5).** The Mini's nightly backup is a launchd calendar
job: `deploy/com.pulsetrader.backup.plist` runs the installed binary's `backup`
(defaults: the platform data dir, `~/pulse-backups`, keep 14) at 03:30 local,
with `RunAtLoad` and `KeepAlive` absent — launchd runs a missed occurrence when
the machine wakes, which is the draco-desk timers' `Persistent=true` behaviour —
no `EnvironmentVariables` (a backup reads no credential), and
`backup.log`/`backup.err` under `~/Library/Logs/PulseTrader/`, rotated by the
same 7-day job as the server's logs. `just deploy-mac` installs it beside the
serve agent and loads it; it is never kickstarted, because a deploy must not
take an extra backup.

draco-desk pulls it at 04:30 (`deploy/pulse-backup-pull.timer`,
`Persistent=true`) through `deploy/pulse-backup-pull.sh`: `rsync -a
--ignore-existing` with the dedicated key `~/.ssh/pulse_backup_ed25519`, never
`--delete`. The artifacts are immutable, so a backup deleted or corrupted on the
Mini cannot erase the off-box copy, and a tampered off-box file stays visible to
the verify instead of being papered over by the next pull. The newest pulled
backup is then checked in place, read-only, by `pulse backup-verify <file>` — the
restore's checks that need no second database: the backup's own `.heads.json`
manifest, every snapshot it names, every snapshot a run references, and every
version and run reading back — and a mismatch exits non-zero BEFORE any pruning.
Retention keeps the newest 30 databases, each with its manifest; `candles/` is
one shared additive store and is never pruned. An empty off-box directory after
a pull is a named failure. `pulse backup --keep 0` is refused at parse time
(#240): it used to prune the backup it had just made and then fail.

**The forced command.** The Mini's rsync is macOS's own (openrsync, "rsync
version 2.6.9 compatible") and carries no `rrsync` — checked read-only at
planning — so the pull key's `authorized_keys` line forces
`deploy/pulse-backup-serve.sh` (installed by `deploy-mac` to
`~/.local/share/pulse-serve/deploy/`). It reads `$SSH_ORIGINAL_COMMAND` and
serves exactly one shape — an rsync SENDER invocation rooted at
`~/pulse-backups` — refusing by name everything else: a write, a delete, a `..`
path, a shell, a path outside the root, any shell metacharacter. The key can
therefore never write, delete or read outside the backup directory. The line the
operator adds at cutover (step 6, a credential stop), with the public half of the
key generated there:

```text
from="100.90.203.21",restrict,command="/Users/draco/.local/share/pulse-serve/deploy/pulse-backup-serve.sh" ssh-ed25519 <the pull key> draco-desk off-box backup pull
```

`just restore-drill <file>` is the drill: it restores a pulled backup into a
fresh scratch directory under `~/.cache/pulse-scratch/`, starts `pulse serve
--dev-loopback` on it, waits for `/healthz`, shows the tokenless 401 handshake
refusal (the auth stack answering — the drill holds no token, and never prints
one), prints the restored library's version and run counts, then stops the server
and removes the scratch directory. `tests/backup_offbox_restore.rs` drives the
whole line (demo line d72), the forced-command refusals included.

**draco-desk is QA (w3).** It runs its own database and its own data dir, and QA
and prod refuse each other's data. `pulse serve --role <prod|qa>` pins a data
dir in `<data dir>/server-role` (one line, mode 0600): an unmarked dir is marked,
the same role continues, and the other role is refused by name BEFORE the
database is opened — a QA server started with prod's `--db` by mistake creates no
database file, no instance lock, no start-log entry and no marker change. With
`--role`, the database must sit inside the data dir. The Mini's plist passes
`--role prod`; `deploy/pulse-qa.service` runs `pulse serve --role qa --bind
100.90.203.21:8421 --db %h/.local/share/pulse-qa/pulse.db --data-dir
%h/.local/share/pulse-qa` with the same restart settings as the systemd prod
unit, on port 8421 so it can run BESIDE draco-desk's current prod until the
cutover retires that unit. `just deploy <tag>` targets QA only — it installs and
restarts `pulse-qa.service`, creates `~/.local/share/pulse-qa` mode 0700, and
refuses without `Linger=yes` before it builds (#248) — and touches neither
`pulse-serve.service` nor the backup units (G2). QA seeding (`pulse qa-seed`)
moved to issue #355. The handshake gains an ADDITIVE optional `role` field (`prod` /
`qa`), and the app's status strip shows a QA badge when the connected server
reports `qa`, so a stale URL can no longer pass for prod unnoticed. Builds, walks
and patches run there against QA only; prod's data is touched by nothing but the
cutover.

**What does NOT change.** ADR-0026's access model (tailnet-only bind, per-client
revocable `app`/`agent` tokens, the append-only `token_audit`, the server
credential profile, the one-process-one-DB rule), the systemd unit's arithmetic
on draco-desk (`deploy/pulse-serve.service` keeps its start limit and its
`Restart=on-failure`; `tests/deploy_units.rs` still pins it), and the binary
itself (ADR-0015: one crate, one artifact, now built for
`aarch64-apple-darwin` on the Mini under the pinned toolchain of ADR-0022).

## Consequences

- **Prod survivability no longer depends on the workstation.** draco-desk can be
  rebuilt, re-toolchained and rebooted while prod serves. What prod now depends
  on is macOS uptime and a FileVault unlock after power loss — an accepted,
  alerted limit (R0/Q2), not a silent one.
- **The start bound is app-level, not manager-level.** systemd would enforce
  limits even against a server that never ran; launchd cannot, so the counter is
  the bound. It is tested (`tests/launchd_units.rs`, demo line d68) with an
  injected clock and through the real binary's exit 0, and the counter's file
  formats are now a compatibility surface: w4's watcher reads
  `serve-start-limit` over its forced-command key.
- **A refused start means the service STAYS DOWN** until `just prod-reset` or
  `just deploy-mac`. That is the intent (#342's parity): a crash loop is visible
  as a marker and an alert, not as an endless restart. The recovery is one
  command, and the watcher's alert names it.
- **Deploys are ssh-shaped.** `just deploy-mac` needs `Host macmini`, the ssh
  key and the Mini's rustup toolchain; nothing else about the deploy is new. The
  recipe is the cutover step, never run by an item against the real service.
- **Two hosts now need the same discipline** — binary not on `PATH`, no
  credential in any unit or plist, logs rotated, start limits bounded — and the
  plists join `deploy/**` as ADR-0026 touch surfaces.
- **G10 has a machine check.** The data dir's 0700 and the credential file's
  0600/owner are verified by the deploy that installs beside them, so the
  Actions runner account (uid 502) cannot read prod's data even by accident.

## Alternatives considered

- **A restart-bounding wrapper script or a launchd keep-alive shim** (rejected at
  plan-spine): the counter would live outside the server, where no test in the
  crate can reach it, and the recovery would lose its named message. The counter
  in `pulse serve` is testable, and the same flag is what systemd's parity
  arithmetic is already written against.
- **Bounding with launchd's `ThrottleInterval` alone**: a throttle spaces
  restarts; it never stops them. The service would still restart forever.
- **`KeepAlive` off, with `RunAtLoad` only**: no crash recovery at all — a
  transient failure would need an operator, and G5's bound becomes "never
  restart", which is worse than a bounded loop.
- **Running the Mini's server as a LaunchDaemon or in `user/`**: the tailnet
  address, the operator's data dir and `fdesetup`-unlocked login all live in
  `draco`'s `gui/` session; `gui/` is also what `launchctl bootstrap` from an
  operator shell reaches.
- **Installing the log-rotation script from the caller's checkout** instead of
  the tagged source: a rollback to an old tag would pair an old binary with a
  new rotation script. Everything `deploy-mac` installs comes from the tag.
