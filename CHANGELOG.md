# Changelog

All notable changes to PulseTrader will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Changed

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
  steps (`reset-failed`, then start). Closes
  [#342](https://github.com/pulseai-labs/pulse-trader/issues/342). Known limit: no
  push alert yet (moved to r4); `just deploy` restarts the service.

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
