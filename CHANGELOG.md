# Changelog

All notable changes to PulseTrader will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Off-box backup: the Mini's nightly `pulse backup` as a launchd calendar job, a read-only pull to
  draco-desk over a dedicated forced-command key, and a restore drill from the pulled copy (r4.s2.w5).**
  `deploy/com.pulsetrader.backup.plist` runs the installed binary's `backup` (defaults: the platform
  data dir, `~/pulse-backups`, keep 14) at 03:30 local, with its logs under `~/Library/Logs/PulseTrader/`
  and no credential in the plist; `just deploy-mac` installs and loads it beside the serve agent.
  `deploy/pulse-backup-pull.{sh,service,timer}` pulls at 04:30 (`Persistent=true`) with the dedicated
  key `~/.ssh/pulse_backup_ed25519`: `rsync -a --ignore-existing` and never `--delete`, so a backup
  deleted or corrupted on the Mini cannot erase the off-box copy. The newest pulled backup is then
  verified in place by the new read-only `pulse backup-verify <file>` — the backup's own `.heads.json`
  manifest, every snapshot it names, and every version and run reading back — exiting non-zero on a
  mismatch before any pruning; retention keeps the newest 30 databases with their manifests and never
  prunes `candles/`. The Mini's rsync is openrsync with no `rrsync` (checked read-only), so the key's
  `authorized_keys` line forces `deploy/pulse-backup-serve.sh`, which serves only an rsync sender
  invocation rooted at `~/pulse-backups` and refuses a write, a delete, a `..` path, a shell and any
  path outside the root by name. `just restore-drill <file>` restores a pulled backup into a fresh
  scratch directory, serves it on loopback, checks `/healthz` and the tokenless handshake, then removes
  the scratch; the daily log rotation now covers the backup job's logs too. `pulse backup --keep 0` is
  refused at parse time (#240 — it used to prune the backup it had just made and then fail).
  `tests/backup_offbox_restore.rs` drives the whole line, the forced-command refusals included (demo
  line d72). Refs [#240](https://github.com/pulseai-labs/pulse-trader/issues/240).

- **draco-desk becomes QA: the data-dir role marker, the QA unit on 8421, `just deploy` retargeted,
  QA seeding with token revocation, the handshake's `role` field and the app's QA badge (r4.s2.w3).**
  `pulse serve --role <prod|qa>` pins a data dir in `<data dir>/server-role` (one line, mode 0600):
  an unmarked dir is marked, the same role continues, and the other role is refused by name — before
  the database is opened, so a refused start creates no database file, no instance lock, no
  start-log entry and no marker change; with `--role`, the database must sit inside the data dir.
  The Mini's plist gains `--role prod`; the new `deploy/pulse-qa.service` runs `pulse serve --role qa
  --bind 100.90.203.21:8421 --db %h/.local/share/pulse-qa/pulse.db --data-dir
  %h/.local/share/pulse-qa` with the same 3-starts-in-900-s bound as prod's unit, on 8421 so QA can
  run beside draco-desk's current prod until the cutover. `just deploy <tag>` now installs and
  restarts QA only (prod's unit and the backup units are untouched, G2), creates
  `~/.local/share/pulse-qa` mode 0700, and refuses without `Linger=yes` before it builds (#248);
  `just deploy-check` rehearses the QA unit. `pulse qa-seed --db <path> --data-dir <path>` revokes
  every token a copied prod database carries (each revoke audited exactly like any other) and issues
  the two fresh QA tokens (`qa-app`, `qa-agent`) in one transaction, then writes the `qa` marker and
  prints the two tokens once on stdout — refusing a `prod`-marked data dir and a database a live
  server holds. The handshake gains an ADDITIVE optional `role` field, and the app's status strip
  shows a QA badge when the connected server reports `qa`, so a stale URL cannot pass for prod
  unnoticed. `tests/data_dir_role.rs` drives the real binary through the refusals, the marker, the
  seed and the handshake (demo line d70). Refs
  [#248](https://github.com/pulseai-labs/pulse-trader/issues/248).

- **Prod's service on the Mac Mini: a launchd LaunchAgent with bounded restarts, `just deploy-mac`,
  a real deploy health gate, and ADR-0029 (r4.s2.w1).** The always-on server gains a launchd home:
  `deploy/com.pulsetrader.serve.plist` runs `pulse serve --bind 100.103.30.74:8420` from the
  installed binary in `draco`'s `gui/` domain with `RunAtLoad`, `KeepAlive { SuccessfulExit =
  false }` and the ONE environment entry `PULSE_CONFIG_DIR` — no credential in the plist (G7) — and
  launchd's stdout/stderr under `~/Library/Logs/PulseTrader/`, rotated daily into dated files by
  `deploy/com.pulsetrader.logrotate.plist` + `deploy/pulse-logrotate.sh` (7 days kept, G9). launchd
  has no start limit, so the bound moves into the server: `pulse serve --start-limit <N>/<SECONDS>`
  counts starts in a sliding window in `<data dir>/serve-starts` and, when a start would exceed it,
  writes `<data dir>/serve-start-limit` and exits 0 before binding — `KeepAlive {
  SuccessfulExit = false }` then stops relaunching (#342's parity; `just prod-reset` clears the
  marker and the start log and kickstarts the agent). `just deploy-mac <tag>` ships the tagged
  source over ssh (`git archive`, no GitHub credential on the Mini) and builds there with
  `PULSE_ALLOW_PLACEHOLDER_DIST=1` (#314: the server never serves the embedded frontend), checks
  G10 (data dir 0700, `.env` 0600 and owned by `draco` — contents never printed), (re)bootstraps
  both agents, and only reports success once the tailnet address answers. That gate is the new
  `scripts/wait-healthy.sh` (#346): it polls `/healthz` for a 200 with `"status":"ok"` and, until
  w4 lands the route, accepts a 401 from `/api/v1/handshake` carrying `X-Pulse-Api-Version` —
  `just deploy` on draco-desk uses it in place of `systemctl is-active`, which read `active`
  through the whole 120-second bind retry. ADR-0029 amends ADR-0026's operations: prod on the Mini,
  draco-desk becomes QA. `tests/launchd_units.rs` parses both plists and drives the counter with an
  injected clock and the real binary's exit 0 (demo line d68). Refs
  [#346](https://github.com/pulseai-labs/pulse-trader/issues/346),
  [#314](https://github.com/pulseai-labs/pulse-trader/issues/314),
  [#342](https://github.com/pulseai-labs/pulse-trader/issues/342).

- **The move made safe: the instance lock, a source that is the target refused, and the paper tables
  verified by digest (r4.s2.w2).** `pulse serve` takes a non-blocking instance lock on its database
  (`<db>.serve.lock`) and holds it for its process lifetime; `pulse import` and `pulse restore` take
  the same lock on their target before any write and hold it until the install completes, so a data
  op against a database a live server holds refuses by name ("a running pulse serve holds it") and a
  second server on a held database refuses too (#250). An import refuses a source database or data
  dir that resolves to the target — the path itself, a symlink to it, a hard link (#264) — before
  anything is touched. `paper_session`, `paper_event` and `paper_bar` are compared by SHA-256
  content digest (every row's columns, primary-key order, length-prefixed) between the source and
  the copy: a changed column, a missing row or a schema difference refuses the import naming the
  table and the first differing key, the verification summary prints the three digests, and
  `pulse restore` runs the same check through the shared engine. CI's determinism matrix gains an
  `arm64-darwin` (`macos-latest`) lane whose hash is compared with both Linux arches (#62), and a
  new pinned `paper_replay_golden` suite replays a committed fixture of recorded bars and events —
  the certify fixture's synthetic BTCUSDT series — so a darwin engine difference fails the
  `macos-latest` job.

- **The certification step: one hypothesis, one holdout, one immutable record, and `certify_version`
  (r4.s1.w5).** While a freeze is open, certifying a version walks it forward under `wf-v2` on the
  search span — the guard clamps the span's end to the holdout start — runs ONE backtest over the
  holdout (never persisted: no run read may surface holdout trades) and applies the C1 holdout test
  at the freeze's H, then writes one immutable `certification` record (migration `0020`) whatever the
  outcome: the search verdict, the pair, the per-timeframe data versions, the holdout's window, trade
  count, mean and bound, the engine fingerprint and the calling client's label. The record is
  immutable by trigger, and the write transaction refuses a call that would take the freeze past its
  H hypotheses (the unique `(freeze_id, hypothesis_index)` index and a new budget trigger hold the
  same law in the schema), so two overlapping calls cannot overrun the budget; a refused or errored
  call writes nothing. The step refuses typed and by name — no open freeze, a
  lineage root created before the freeze (C4), and the (H+1)th hypothesis — and MCP gains
  `certify_version` (agent scope): one call is one hypothesis, and the answer carries the
  certification's id, its pass/fail, the search-span verdict, `holdout_passed` and the hypotheses
  used and left — **no holdout number** (grill Q5). The app reads the full record through a new
  `app`-scope `certification_records` route, one row per record in the version detail view. A
  version's `certified` flag and paper promotion now read the record: a `wf-v2` search-span pass
  certifies and promotes nothing by itself — the `wf-v1` path, the override path and paper sessions
  are unchanged.

- **The holdout the tools enforce: the certification freeze, its guard, #327's echo, and the
  archive-hole fill (r4.s1.w4).** `pulse certify freeze --holdout-start <YYYY-MM-DD> --h <N>
  --alpha <decimal> --test <name>` opens ONE immutable freeze record (migration `0019`:
  `certification_freeze`, immutable by trigger, at most one open, a new holdout strictly after
  the last close — a spent holdout is never reused), `pulse certify close-freeze` closes it once,
  and `pulse certify status` prints the open freeze or `no open freeze`. While a freeze is open,
  ONE application-layer guard covers every entry point (MCP `run_backtest`/`run_walk_forward`,
  the app's backtest and walk-forward, the coach's certification gate, and `pulse backtest`): a
  window reaching into the holdout is refused by name — the pair and the holdout start — and a
  defaulted end is clamped to the holdout start (the app and CLI's whole-snapshot runs stop
  there, and the run records the clamped window). MCP `export_candles`/`export_indicators` return
  only candles before the holdout start and say how many rows were withheld (the parquet byte
  copy is refused while a freeze is open: it cannot be cut). Every MCP run result now echoes its
  effective window under `effective_window` (`from` inclusive, `to` exclusive, per-bound
  `defaulted`/`clamped`) — #327's settled echo — and both run-tool descriptions state the
  exclusive bound. `pulse fetch-data` fills interior archive holes (the SOLUSDT/XRPUSDT
  2022-02/2022-04 gaps) from the REST klines endpoint through the bounded incremental path —
  funding included, one new snapshot, the prior file kept — and the summary reports
  `filled_candle_count` beside the `gap_count` that remains. The certification step, the
  certify-fixture seed and paper sessions are exempt by name (grill Q4). Refs
  [#327](https://github.com/pulseai-labs/pulse-trader/issues/327).

- **Four pairs, a start date for `fetch-data`, and pair-validated runs (r4.s1.w2).** ETHUSDT,
  SOLUSDT and XRPUSDT now fetch, backtest and walk forward exactly as BTCUSDT does: the broker
  adapter pins each pair's dated USD-M filters (`LOT_SIZE.stepSize`/`minQty`, `MIN_NOTIONAL`, the
  top leverage tier) and its 8h funding interval beside BTCUSDT's unchanged values; the MCP
  `run_backtest` and `run_walk_forward` tools take an optional `pair` argument (omitted = today's
  inheritance; an unknown pair is refused naming `pair`; a known pair with no `HEAD` snapshot is
  refused naming the pair and the timeframe); and `pulse fetch-data` takes
  `--from <YYYY-MM-DD>` (UTC, floored to the first of its month, mutually exclusive with
  `--years`). A `--from` earlier than the snapshot's first candle backfills the missing earlier
  months through the same bulk + checksum path, tops up to now and commits ONE new snapshot — a
  new `data_version` with `HEAD` moved and the prior file kept — reported as the `backfill`
  action. `save_run` validates `inputs.pair` before writing (#148), so a path-hostile symbol can
  no longer mint an unreadable run row, and the CLI's unknown-pair refusal is covered by a test
  (#52). The engine fingerprint changes with the broker table, as expected. Refs
  [#148](https://github.com/pulseai-labs/pulse-trader/issues/148),
  [#52](https://github.com/pulseai-labs/pulse-trader/issues/52).

- **`wf-v2`, the C1 holdout test, and the frozen hypothesis budget (r4.s1.w3).** The walk-forward
  gains a second named verdict rule beside the byte-identical `wf-v1`: **`wf-v2`** holds a fold on
  a positive mean expectancy (`n >= 20`) instead of a positive lower bound, and keeps `wf-v1`'s
  `⌈2K/3⌉` holding-fold requirement and pooled one-sided 95% bound. The rule is request-selectable
  — the MCP `run_walk_forward` tool takes an optional `rule` argument (`wf-v1` by default; an
  unknown value is refused naming `rule`), the store decoder accepts `wf-v2`, the save gate
  re-derives every fold and the run verdict under the draft's own rule, and the coach's
  certification gate re-runs a candidate under the parent's own rule. A new domain function
  `holdout_test` implements the **C1 holdout test** — the one-sided lower confidence bound of a
  holdout's expectancy at a family-wise 5% split over the hypothesis budget, `z = z(1 − 0.05/H)` —
  with its power measured over seeds 1..=1000. `tests/wf_v2_calibration.rs` calibrates the rule (a
  planted +0.25R edge passes on at least 19 of 20 seeds, a zero-edge series on at most 4, with the
  exact counts pinned) and `tests/wf_v2_measurement.rs` re-derives ADR-0028's frozen α, power,
  real-path-null and runtime numbers on demand. **ADR-0028** freezes the rule, the C1 test, the
  holdout start (2025-07-01, all four pairs) and **H = 12**.

- **The prod watcher and its push alert: an unauthenticated `/healthz` and `pulse watch` (r4.s2.w4).**
  `GET /healthz` is the ONE route outside the auth middleware (ADR-0029's Q1): 200 with exactly
  `{"status":"ok"|"degraded","api_version":1}` — `degraded` means the paper runtime is not running —
  keeping the request-log and API-version layers, writing no `token_audit` row and touching no table,
  so the deploy gate (`scripts/wait-healthy.sh`) and an off-box watcher read it with no token.
  `pulse watch` (`--url`, `--ssh-host`, `--topic-file`, `--state-file`, `--ntfy-url`) is one probe
  cycle, meant for `deploy/pulse-watch.timer` on draco-desk (every 60 s): it probes
  `<url>/healthz`, and on failure tells "service down" (the host answers on port 22) from "Mini
  unreachable — it may need a FileVault unlock", and checks the start-limit marker over the dedicated
  forced-command key — alerting **at once** when the marker is present ("start limit reached on prod
  — run just prod-reset"). It alerts after 3 failed probes, de-duplicates to one alert per incident,
  sends one "recovered", reminds every 6 hours while the outage lasts, and pushes one "watcher error"
  per distinct error (missing or non-0600 topic file, unwritable 0600 state file, missing key,
  malformed `--url`) and exits non-zero. Nothing it sends or logs carries a token, a credential URL,
  the topic or session data. `deploy/pulse-watch.{service,timer}` are parsed by `tests/deploy_units.rs`,
  and the whole behaviour is pinned by `tests/healthz_watch.rs` (demo line d71), which drives the real
  probe and the real notifier against a local fake listener — never the real ntfy.sh — and closes the
  known limit "a failed unit raises no push alert".

### Changed

- **Engine-sensitive dependency bumps: `polars` 0.54.4, `zip` 8.6.0, `rust_decimal` 1.42.1.** The
  three pins move together under the determinism gate: the determinism lanes, the golden fixture
  and every `data_version` stay identical, and the engine fingerprint moves (it hashes
  `Cargo.lock`, which is expected). `polars` 0.54.4 requires `chrono ^0.4.42` where 0.53 pinned
  `<=0.4.41`, so `chrono` 0.4.41 → 0.4.45 rides along; `ta`, `sqlx` and every other direct pin are
  unchanged. Refs [#59](https://github.com/pulseai-labs/pulse-trader/issues/59),
  [#60](https://github.com/pulseai-labs/pulse-trader/issues/60),
  [#61](https://github.com/pulseai-labs/pulse-trader/issues/61).

- **The lockfile guard is a script CI runs.** `scripts/check-lockfile-guard.sh` checks
  `build_support/lockfile-guard.txt` against `Cargo.lock` and fails — naming the crate — when a
  listed crate's version differs, the crate appears twice, or it is missing. A bump of a guarded
  crate (`polars` + `polars-*`, `rust_decimal`, `ta`, `zip`, `sqlx` + `sqlx-*`) now takes a visible
  edit of the guard file.

- **A snapshot re-write after a writer-version change is idempotent again**
  ([#5](https://github.com/pulseai-labs/pulse-trader/issues/5)). `CandleStore::write_snapshot`
  reconciles a re-write against the file at the same content-addressed path by comparing the
  decoded candles and provenance instead of a byte image that carried the writer's `created_by`
  string, so a snapshot written by a different Polars version no longer wrongly returns
  `SnapshotExists`; a same-path file holding different candles is still refused.

- **Safe dependency bumps (r4 chore).** GitHub Actions: `actions/checkout` 7.0.1, `cargo-deny-action` 2.1.1, `install-action` 2.86.8, `upload-artifact` 7.0.1, `download-artifact` 8.0.1. Cargo: `uuid` 1.24.0, `anyhow` 1.0.104, `quinn-proto` 0.11.17 (security). UI dev tooling: `vitest` ^5.0.0 (`vite` stays on ^6.4; the vite 8 move is deferred to r4.s5), plus lockfile-only `undici` 8.11.2 and `source-map-js` 1.2.2. Cargo also takes `xxhash-rust` 0.8.16. Together these clear 13 of the 14 open Dependabot alerts; `glib` (GTK/tauri stack) stays open. No product version bump; `rust_decimal`, `ta`, `polars` and `zip` are unchanged.

- **The coach turn got its own output cap, transport timeout and temperature.** A
  coach turn on `glm-5.3-flash` spent the whole 4096-token cap reasoning and emitted
  no tool call, which the taxonomy could only record as `ZeroCalls` — a cap that was
  too small, reading as a model that declined. Refs
  [#164](https://github.com/pulseai-labs/pulse-trader/issues/164),
  [#124](https://github.com/pulseai-labs/pulse-trader/issues/124).

  - **Output cap 4096 → 16384, for the coach only.** The composer and `llm-check`
    keep 4096. Real turns need 5 615–9 074 output tokens before their tool call.
  - **Coach transport timeout 60s → 100s, again for the coach only**, sitting inside
    the unchanged 120s turn guard with a 20s reserve for the ledger write that
    follows the response. Worst-case wall time per turn rises accordingly.
  - **Desktop coach temperature 0.2 → 0.0**, unified with `pulse coach`: the rail was
    wired to the composer's config and sent a different temperature than the CLI for
    the same question.
  - Both coach surfaces now build one shared config and one shared provider
    constructor, so the two cannot drift apart again. The coach's request fingerprint
    changes with the cap and the temperature — a turn asked under a different cap is a
    different request — and no prompt text, schema or migration changed.

- **Default LLM model bumped `glm-5.2` → `glm-5.3-flash`** on Ollama Cloud. The
  provider, endpoint (`https://ollama.com/v1`), credential (`OLLAMA_API_KEY`) and
  ledger backend label are all unchanged — this moves a model id and its price row.
  See [ADR-0023](docs/adr/0023-retain-ollama-cloud-bump-default-model-to-glm-5-3-flash.md).

  Notes for anyone upgrading or reading the diff:

  - **The model id is written bare — `glm-5.3-flash`, no `:cloud` tag.** Ollama's
    library page publishes only a `cloud` tag and its examples show
    `glm-5.3-flash:cloud`, but the endpoint accepts the bare id (verified by a live
    call, tool-calling included), and the bare form matches how `glm-5.2` was
    written. Noted because the docs and the endpoint disagree here.
  - **A `$PULSE_CONFIG_DIR/prices.toml` overlay wins over the shipped default, and
    the two verbs then diverge.** `pulse compose` reads `[llm].model` from the
    overlay, so it keeps running `glm-5.2`. `pulse llm-check` does **not** read that
    table — it is const-driven and now asks for `glm-5.3-flash`, a model the stale
    overlay never priced, so it fails closed *before* the billed call with
    `no price for model glm-5.3-flash`. Fix either way: delete the overlay to
    inherit the new default, or edit its `[llm].model` **and** add a matching
    `[models."glm-5.3-flash"]` row — the added row is what unblocks `llm-check`.
  - **Verified end to end before landing.** A live `pulse compose` run on the new
    default dispatched six tool calls, finalized a schema-valid strategy, and
    persisted six `LlmCall` rows (peak `output_tokens` 701 against the 4096 cap, so
    no truncation; no secret in the ledger). This mattered more than a transport
    check: `gpt-oss:120b` once passed API-level tool-calling on this same endpoint
    and then failed mid-loop.
  - Multi-model tiering (`glm-5.3` for harder tasks, `gpt-oss:120b` for light ones)
    is planned work and is **not** implemented; exactly one model id is read.

  A flip to z.ai's GLM Coding Plan endpoint was drafted and fully reviewed before
  this, then rejected: that plan's terms prohibit spending its quota from a custom
  application calling the API directly, by usage shape rather than by user count.
  Preserved unmerged as [PR #123](https://github.com/pulseai-labs/pulse-trader/pull/123).

### Fixed

- **`pulse-serve.service` now gives up after repeated failed starts (0.1.3).** The
  unit set no start limit, so systemd's default (5 starts in 10 s) applied. `pulse
  serve` retries its bind for 120 s before it exits non-zero, and the unit waits
  10 s between starts, so the limit never tripped and a server that always failed
  restarted forever while looking up. Now the unit's `[Unit]` section sets
  `StartLimitIntervalSec=900` and `StartLimitBurst=3`: three failed starts in 15
  minutes (3 x 130 s = 390 s for a slow failure) leave the unit in the `failed`
  state, visible to `systemctl --user --failed`. `Restart=on-failure` and
  `RestartSec=10` are unchanged. Install needs only the unit copy and `systemctl
  --user daemon-reload`; the `justfile` comment above `deploy` has the recovery
  steps (`reset-failed`, then start), and `just deploy` and `just restore` now run
  `reset-failed` before they start the unit, because the start limit counts manual
  starts. Closes
  [#342](https://github.com/pulseai-labs/pulse-trader/issues/342). Known limits: no
  push alert yet (moved to r4); `just deploy` restarts the service; and a fast,
  non-retried bind failure such as a port conflict (`AddrInUse` exits at once) now
  latches `failed` after three starts, where 0.1.2 restarted until it cleared.

- **`pulse fetch-data` no longer fails in the first days of a month (0.1.2).** The
  bulk phase asks for every complete month, which includes the month that just
  ended, and Binance publishes that month's archive a few days late. The loader read
  that absent month as a coverage hole and returned an error, so the REST top-up
  that would cover it never ran, and no snapshot could be made for any pair or
  timeframe. Now `fetch-data`'s first run names the month before the current one as
  the single month the bulk loader may leave to the REST top-up, and the top-up
  covers it. The loader stays fail-closed for everything else: any other caller, any
  other absent month, two or more trailing absent months, and an absent month
  followed by a loaded one still return the same coverage-hole error. Closes
  [#289](https://github.com/pulseai-labs/pulse-trader/issues/289). Known limits:
  [#312](https://github.com/pulseai-labs/pulse-trader/issues/312) (a funding
  archive or checksum that is also unpublished still fails),
  [#313](https://github.com/pulseai-labs/pulse-trader/issues/313) (a month taken
  from REST is never checksum-verified against its bulk archive).

- **Paper `fill` events now carry the engine's fill time (0.1.1).** The log stamped
  each `fill` with the runtime's poll instant, so a replayed position, closed trade
  and session detail showed a fill time about one bar late (and the restart time
  after an outage). A `fill` event now carries an optional `fill_time_ms`, written
  from the engine's entry or exit fill time, and replay reads the fill times from
  it. `at` stays the row's instant, and an event written before the field existed
  still reads and replays as before. Closes
  [#303](https://github.com/pulseai-labs/pulse-trader/issues/303).

- **A live paper session no longer stalls when Binance finalizes a bar after the
  poll.** The runtime polled 5 s past a bar's close and recorded the first kline it
  read, but Binance can still update a kline after that. The next re-fetch then
  disagreed with the recorded bar, and the "changed re-fetch" `data_event` held the
  session for good — both sessions on the always-on server stopped within an hour
  of promotion. The runtime now records and steps a bar (live or lead-in) only once
  it is settled: read at least 30 s after its close and confirmed by an identical
  read at least 10 s later, re-polled every 10 s until then — after a restart and
  at a first start too, and a primary bar waits for a higher bar that closes with
  it. A steady bar lands 40 s after its close; each read that differs from the one
  before adds 10 s; a bar still unsettled 5 minutes after its close is logged once.
  A disagreement with a bar that was already recorded is still a `data_event`, and
  recorded bars are never replaced. The initialization lifecycle got the same
  treatment: an absent or zero-consumed read never proves completeness. A first
  start confirms its settled lead-in over ONE pinned eligible window, whose
  read deadline is stamped when each read returns — and each timeframe's reply is
  judged by the instant its OWN read returned, never by a later fetch's — so an
  empty, shortened, changed or failed re-read can neither erase the history it
  already holds nor pass as complete; recovered history joins that window, and
  the probe keeps growing while the window is not yet warm and the source can
  still supply it. Lead-in eligibility takes bars that CLOSED before the pinned
  cutoff, so a higher bar that opens before it but closes after it stays with the
  live drain (and the rebuild's `close_time` drain) that owns it. The window
  keeps two views of what a probe operation saw: an OBSERVED union — every
  eligible bar any of its replies returned, across every depth and timeframe,
  which no later shorter, empty, changed or failed reply of that operation can
  erase, and which joins the pin even when a later timeframe's fetch fails — and
  the LAST read operation alone, whose whole raw response (every configured
  timeframe, nothing failed) is the only thing that may witness completeness: a
  union or a maximum-length view never proves the source currently serves that
  window, so only a raw response equal to the whole retained candidate — the one
  a spaced confirming read already witnessed before that operation — releases
  a not-yet-warm window. A bar, or a revised copy of a known bar, that the
  deepening operation's own reads are the first to see is not discharged by
  their raw completeness: it waits for a later confirming read.
  A due primary bar keeps the 10 s retry until every bar of its owed span is in
  hand — even before any counting read exists, a transient first failure
  included — and nothing due means no short poll. A restart's confirmation-
  delayed backlog is shadow-checked over the caught-up state once it commits,
  not only beforehand, and the required checkpoint stays pending until it
  succeeds on every attach path. The engine fingerprint is unchanged. Refs
  [#306](https://github.com/pulseai-labs/pulse-trader/issues/306).

- **`pulse import` can no longer publish a database whose committed rows were
  left behind outside it, and every rename that publishes a snapshot, a backup
  database or a backup's `HEAD` manifest is now made durable.** The import's
  temporary copy ran in WAL mode, and the migration protocol opened a second pool
  on it that was never closed — only *dropped*, which leaves the SQLite handle to
  the connection's own background worker thread. SQLite checkpoints on its own at
  the WAL threshold, and its final checkpoint normally removes `-wal`/`-shm` when
  the LAST connection to the database closes — so which close removes them, and
  whether it has happened yet, is nothing the import awaited. The install could
  therefore rename the database file while its `-wal` — holding every row
  committed to it — was still beside it under the temporary's name, with nothing
  to carry it: silent data loss, and the intermittent flake the round-4
  regression test caught. The copy now runs in rollback-journal mode (read back,
  never assumed), its bytes are fsynced before the rename, the protocol closes
  its pool on every path, and the install REFUSES with a named, typed reason if
  any sidecar is still beside the copy. The old target's own sidecars are moved
  aside rather than deleted, so a stale hot journal can never be replayed into
  the freshly installed file (and a failed rename cannot strip the old database
  of its un-checkpointed rows), and the installed file is put back in WAL and
  read back before the command reports success. The renames that publish a
  snapshot (`candles/<PAIR>/<TF>/`, including the directory levels a copy
  creates), the installed database, the backup database and the backup's own
  manifest each fsync the directory they landed in, so a power loss cannot keep a
  backup's database while losing a snapshot it references. An install interrupted
  between moving the replaced database's sidecars aside and landing its rename
  leaves those files beside the target under quarantine names; the import,
  `pulse backup` (including the `--replace` safety backup) and the app's own open
  (`pulse serve`, the desktop app — checked before any connection is made) now
  REFUSE such a target — naming every file and what to do with it: the database's
  own sidecars move back, while the stale ones a REPLACED database left are to be
  deleted (never moved back, which would replay the old database's pages into the
  new file) and the ones whose ownership the name does not record are to be
  recovered from the safety backup. Refs
  [#258](https://github.com/pulseai-labs/pulse-trader/issues/258),
  [#259](https://github.com/pulseai-labs/pulse-trader/issues/259).

### Security

- Added repository security hardening, non-commercial license, and supply-chain checks (VS-1.2.3).
