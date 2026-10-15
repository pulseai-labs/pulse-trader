# Changelog

All notable changes to PulseTrader will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **The Chart tab: a backtest drawn on its candles (r5.s2.w2, PDR-0036, PDR-0004).** The Mac app
  gains a **Chart** tab — one full-width candle chart behind an adapter of our own in
  `ui/src/chart/`, the only place in the app that imports TradingView's Lightweight Charts
  (`5.2.1`, pinned exactly; its attribution logo stays on the chart). It opens on a pair and
  timeframe (BTCUSDT 4h) with no run, and `#/chart?run=<id>` — the link the Backtest Lab's run
  detail now carries — opens it on a backtest: the run's candles, **every entry and exit in the
  loaded window** as markers (winners in bull, losers in bear, an open trade in amber), and the rule's **S/R
  levels, pivots, candle patterns and EMA lines** exactly as `chart_data` returns them. Levels
  draw stepped and dashed from the engine's per-bar values, never from a later bar; a pivot's
  marker sits on its **confirmation** bar with a faint ghost on the bar it peaked `right` bars
  earlier; a pattern draws below the bar in bull and above it in bear. The **primary / higher
  timeframe switch** draws each overlay only on the timeframe the engine computed it on. A leg
  whose whole span is over the 20,000-bar read cap is never sent to be refused: the tab says how
  many bars it would be and asks for a trade, then reads a window around that trade — and
  picking another trade outside that window reads around *it* rather than scrolling to a bar
  that was never loaded. An open freeze's holdout shows as a plain withheld sentence, the
  rule's other operands (RSI, ADX, MACD, …) are named as not drawn, and loading, a failed read
  and an empty window each say so — no zero or placeholder ever stands in for a result
  (PDR-0011). Nothing is reachable only through the canvas: the tab carries a table copy of the
  window's trades, of every pivot and pattern event (the ghost's peak bar included) and of the
  per-bar level and line values. Settings gains a minimal **About** section with the app's own
  version and the line "Charts by TradingView Lightweight Charts™ — tradingview.com", linking
  out to tradingview.com in a new tab so the app view is never replaced by it. No Rust, no
  bindings and no fixture changed: this item is the tab.

- **The chart read: a backtest drawn on its candles (r5.s2.w1, PDR-0036).** The app now has one
  app-scope read — `POST /api/v1/chart`, the `chart_data` bus command — that answers everything
  the Chart tab draws: a window of candles for a pair and timeframe (at most **20,000** bars per
  read; a window over the cap is refused with the cap and the count named, never silently
  truncated) and, when a backtest run is named, that run's **stored trades** (entry and exit
  time and price, side, exit reason, realized R — drawn for the pair *and* the higher timeframe)
  plus one **overlay series per S/R, pivot, candle-pattern and EMA operand** its version's rule
  uses on that timeframe, each labelled with the exact token `export_indicators` accepts
  (`support:3-3-0.005-2-100`, `cdl:engulfing`, `pivot_high:1-1`, `ema:50`) and each computed by
  the engine over the whole cut series, so an overlay's values equal what an agent sees for the
  same token, pair, timeframe and data version. Levels and EMA lines arrive as per-bar values
  (drawn as they stood at each bar, never from a later bar), pivots as one event on their
  confirmation bar with the peak bar `right` bars earlier, patterns as one event per non-zero
  bar; the rule's other operands (RSI, ADX, MACD, ATR, wicks, necklines, trendlines) come back
  named in `notDrawn` instead of guessed. Every read cuts candles at an open freeze's holdout
  start **before** the engine steps and reports how many bars it withheld inside the window;
  an unknown run is a 422 `not_found`, and a pair or timeframe the run does not use is a 422
  naming the field. The token formatter is the parser's inverse, proven by
  `parse(label(spec)) == spec`, and [ADR-0030](docs/adr/0030-chart-adapter-and-overlay-read.md)
  records the chart adapter boundary this read serves.

- **The streak and drawdown card in the Promote sheet (r5.s4.w2, PDR-0035).** Before the
  confirm button — in the certified body and in the override confirm step — the Promote sheet
  now shows **"A normal bad run"**: the longest losing streak, and the max drawdown in R and in
  USDT (with the USDT figure as a percentage of the fixed 10,000 USDT equity), at the 50th
  ("typical") and 95th ("bad but still normal") percentile over 100 trades from that version's
  **latest full-span backtest run**, with the run's own observed values beside them, naming the
  run the card read. A run with fewer than 30 closed trades says so and shows no figures — as do
  a version with no run and a card read that fails — and none of those states changes the
  variant, the refusal or the buttons: the card informs, the server's gate decides.
- **The streak and drawdown card, and its app read (r5.s4.w1, PDR-0035).** Before a
  strategy is promoted to paper, the engine now answers what a *normal bad run* of it
  looks like: a seeded Monte Carlo (one fixed documented seed, 10,000 paths, 100 drawn
  trades a path, with replacement, `Decimal` and integer draws only) over that version's
  **latest full-span backtest run** — never a walk-forward fold — reporting the longest
  losing streak and the max drawdown in R, in USDT and as a percentage of the fixed
  10,000 USDT equity, at the 50th ("typical") and 95th ("bad but still normal")
  percentile, with the run's own observed values beside them. A 100-trade run at the
  PDR-0006 40% win rate reads a typical longest losing streak of **7** (p95 **12**);
  fewer than 30 closed trades answer "not enough trades" and show no figures. The card
  is served by the new app-scope `POST /api/v1/streak-card` route and the `streak_card`
  bus command, which name the run they read. The paper fixture's splitmix64 moved to
  `src/domain/rng.rs` unchanged — its pinned data versions and the paper replay golden
  stay byte-identical — and is now shared with the card's draws.
- **DSL schema 1.4.0: named candlestick patterns as indicators (r5.s1.w1, completed by
  r5.s1.w2).** A strategy can
  now name a candlestick pattern — TA-Lib's own definitions — and read it through the
  comparisons that already existed: `Compare(CandlePattern{Engulfing}, Gt, 0)` is "a bullish
  engulfing fired", `Eq 100` is "the full bullish form". One new `IndicatorSpec` variant,
  `CandlePattern { pattern }`, serialized `{"indicator":"CandlePattern","pattern":"Engulfing"}`,
  with the spine's full **17** names: the one-bar `Doji`, `DragonflyDoji`, `GravestoneDoji`,
  `Hammer`, `InvertedHammer`, `HangingMan`, `ShootingStar`, `Marubozu` and `SpinningTop`; the
  two-bar `Engulfing`, `Harami`, `Piercing` and `DarkCloudCover`; and the three-bar
  `MorningStar`, `EveningStar`, `ThreeWhiteSoldiers` and `ThreeBlackCrows`. The four
  three-bar patterns landed **additively inside 1.4.0** with no second version step, on the
  same candle ring: `MorningStar` and `ThreeWhiteSoldiers` read `+100`, `EveningStar` and
  `ThreeBlackCrows` read `-100`, and the crows read **four** bars (a white bar, then three
  black ones). Their lookbacks are 12, 12, 12 and **13** — TA-Lib's declared values, used
  exactly as listed — and the deepest window any pattern reads is bar `t-12`, inside the
  existing ring. The value is **TA-Lib's own integer on every bar after the pattern's
  warm-up** and is never missing: `+100` bullish, `-100` bearish, `0` no pattern, and `±80`
  for the **partial** Engulfing and Harami forms, where a body end coincides with the prior
  body's edge — the common case on continuous crypto bars, which open at the prior close
  (on the fixture, `engulfing` reads `±80` 401 times against `±100` 161 times). The
  distinction is kept, so "fired" and "the full form" stay expressible. No boolean type and
  no condition kind is added: the ADR-0024 addendum records that a pattern is a scalar by
  TA-Lib's convention, and answers the 1.3.0 addendum's revisit trigger. Parity is exact:
  on both committed BTCUSDT reference series (15m, 2,976 bars; 4h, 186 bars) and three
  hand-built sequences, all 17 patterns' values equal TA-Lib's on every bar, with an
  **empty** exception list — the reference is pinned to TA-Lib Python 0.6.8 / C 0.6.4 and
  committed under `tests/fixtures/candle-patterns/`. `ThreeBlackCrows`, `Piercing` and
  `DarkCloudCover` never fire on the fixture bars — crypto perpetual bars open at the prior
  close, so the strict real-body tests reject every bar — and each is read from its own
  hand-built sequence instead. Each pattern is warm-up safe, never
  repaints under truncation, streams byte-identically twice, and works on the `htf` operand.
  The `1.3.0 → 1.4.0` migration is identity (only the version string is rewritten), every
  pre-1.4.0 document keeps its meaning and its frozen `result_content_hash`, and the
  patterns were implemented from a plain-words specification and TA-Lib's output only — no
  TA-Lib source was read or ported (PDR-0003).

- **The agent learns the 1.4.0 candle patterns, and a 4h support-rejection example ships with a
  golden (r5.s1.w3).** Every agent surface now names the 17 patterns. `pulse mcp`'s
  `export_indicators` takes a `cdl:<name>` token — the name matches ignoring case and underscores,
  and the column is always labelled `cdl:<snake_case>`, so `cdl:Engulfing` and `cdl:engulfing` are
  one column — and the CLI's `--indicator` flag shares that parser. The composer's builder tools
  take a `candle_pattern` operand with a `pattern` field: the advertised enum lists all 17 names,
  and a missing or unknown one comes back as a correctable error on `pattern`. The composer prompt
  gains the schema-1.4.0 candle-pattern operand section — the 17 names with their signs, the
  `Gt 0` = "a bullish form fired" / `Lt 0` / `Eq 100` reading rule, closed-bar evaluation, the ±80
  partial `Engulfing`/`Harami` forms, and `htf` support — plus a worked "4h support rejection"
  example, and `pulse://dsl/schema`'s conventions gain a `candle_patterns` entry. The example,
  `examples/strategies/support-rejection-4h.json`, is a schema-1.4.0 long-only strategy: a closed
  H4 bar must reach the level the **previous** H4 bar carried as `Support` (within a 0.5% band) and
  close back above it, and that same bar must be a bullish **hammer**.
  `tests/candle_pattern_example.rs` pins its frozen golden — 4 trades on the committed fixture —
  and checks, from independently streamed series, that every entry sits on a bar where both the
  support touch and the named pattern held.

- **The Backtest Lab's version picker, and the seeded QA library it searches (r4.s5.w2).** The Lab's
  native version `<select>` is replaced by a searchable combobox (grill A4): type-to-filter,
  case-insensitive, on the strategy name, the version label (`v<n>`) and the version id's 8-character
  short id; options grouped by strategy in `version_tree` order, each row carrying the version number,
  the created date, the short id and the Library's own creator line; when two strategies share a name,
  every group with that name adds its created date and short id (#318); the pinned version is marked;
  an empty filter says so in one line; and the keyboard map is ↓/↑ to move the active option, Enter to
  select, Escape to close and Home/End to jump, with focus held in the input and
  `aria-activedescendant` naming the active option — so a 120-version library is searchable without a
  virtualisation library (#218). The selection now outlives the screen, so a reconnect — or an
  ordinary tab navigation — keeps both the chosen version and the run the trader was reading (#315),
  falling back to the first version only when the catalog no longer carries it.
  `scripts/seed-qa-library.sh` fills QA with 12 strategies × 9 versions through the real MCP
  `submit_strategy_version` (12 distinct names, both committed DSL families, one parameter varied per
  child; a run-scoped agent token held only in a 0600 scratch file and revoked by a trap on every exit
  path; the data dir's `server-role` and the server's handshake role are both checked `qa` before
  anything is written; idempotent per strategy name), `scripts/seed-qa-library-test.sh` proves it on
  scratch servers only, and `scripts/picker-test.sh` is the new ledger line over the picker and the
  Lab.

- **The paper session's money view reaches the Mac app (r4.s4.w2).** The session detail now shows, all
  from the server's own text: the running total (the Σ realized USDT and Σ R totals with their "n of
  m" counts, the open position's unrealized USDT and the session P&L); the open position marked at
  the last processed bar with its funding so far, entry fee, unrealized USDT and R and the running
  engine's initial stop, current stop and take-profit (absent, with one note, when no engine answers);
  the last processed bar (#340); every order and fill in log order, the entry fills included (#344);
  and each closed trade's R, USDT, entry/exit fees and funding. The view refetches on every
  `bar_processed` frame, so the marks and money figures move as bars arrive, and every figure is
  rendered as it crosses — the app computes no money (G3, ADR-0020). `tests/tauri_paper.rs` is back to
  its exact DTO-to-wire mirror, over a seeded session with an open position and a closed trade, with
  the committed bindings regenerated; `scripts/paper-view-test.sh` is the new ledger line over the
  mirror, the two UI test files and the bindings gate.

- **The paper session's money view: every order and fill, each closed trade's net USDT, the open position
  marked at the last processed bar with its live stops, and the running totals (r4.s4.w1).** The read
  side (ADR-0027 addendum) derives every USDT figure from the log at the session's `taker_fee_bps` with
  the engine's own cost functions — each closed trade's `realized_usdt` equals the engine's
  `Trade.realized_pnl` exactly — and attributes funding by step, so a bar that closes a position still
  accrues its funding to that trade. `session_summary` gains `realized_usdt_total`/`realized_usdt_count`,
  `realized_r_total`/`realized_r_count`, `unrealized_usdt`, `session_pnl_usdt` and a marked
  `open_position` (mark price and time, funding so far, entry fee, unrealized USDT and R, and the live
  initial stop, current stop and take-profit read from the running engine under a short timeout); the
  trades read gains `entries` (every order and fill in log order, the entry fills included) and each
  closed trade's `realized_usdt`, `funding_usdt`, `entry_fee_usdt` and `exit_fee_usdt`. Additive fields
  on the existing reads (ADR-0026): the routes and the MCP paper tools share `application::paper_read`,
  the `/mcp` mount beside the runtime passes it the server's control handle, and no event gains a money
  field. `tests/paper_pnl.rs` pins the derivation against the real engine (long, short, a funding-bar
  close, a same-step open and close, a missing entry fill, the marked position and the totals).

- **DSL schema 1.3.0: swing pivots, support/resistance and candle anatomy as price-level indicator
  variants (r4.s6.w1).** Eight new `IndicatorSpec` variants — `PivotHigh`/`PivotLow` (`left`,
  `right`), `Resistance`/`Support` (`left`, `right`, `tolerance_pct`, `min_touches`, `lookback`) and
  the parameterless `Body`/`UpperWick`/`LowerWick`/`Range` — so a breakout reads
  `CrossesAbove(close, lag(htf Resistance{…}, 1))` through the existing comparisons and the `htf` operand;
  the seam stays indicators, not conditions (ADR-0024 addendum,
  `docs/adr/0024-addendum-structure-indicators-1-3-0.md`). A swing pivot is confirmed only once its
  `right` bars have closed, and each structure indicator passes a truncation no-repaint test; the
  level variants keep confirmed pivots within `lookback`, group them by the decimal-fraction
  `tolerance_pct` (`0.003` = 0.3%, the `distance_pct` convention), keep groups with at least
  `min_touches` pivots, and return the mean of the nearest qualifying group above (below) the
  current close. `tolerance_pct` is a `SweepableValue<Decimal>`; the other parameters are
  `SweepableValue<u32>` with `left`/`right` ≥ 1, `min_touches` ≥ 2 and `lookback` > `left + right`.
  The `1.2.0 → 1.3.0` migration is identity (only the version string is rewritten), every
  pre-1.3.0 document keeps its meaning and its frozen `result_content_hash`, and the new vocabulary
  reaches the engine, validation, rendering, the one-mutation framework, the builder tools and the
  indicator read path. On the same pivots, r4.s6.w3 adds the four **pattern** variants —
  `DoubleTopNeckline`/`DoubleBottomNeckline` (`left`, `right`, `tolerance_pct`: the lowest low /
  highest high strictly between the last two within-tolerance pivot highs/lows, held until a newer
  pivot confirms) and `TrendlineLows`/`TrendlineHighs` (`left`, `right`: the line through the last
  two rising pivot lows / falling pivot highs, extended to the current bar, its quotient rounded to
  8 decimal places half-even) — through the same engine, validation, rendering, mutation,
  builder-tool and read-path arms, with a truncation no-repaint test and byte-identical two-run
  streaming per variant.

- **The agent learns the 1.3.0 structure vocabulary, and the first S/R breakout example ships with a
  golden backtest (r4.s6.w4).** The composer prompt's public default (`src/agent/prompts/composer.md`)
  names all twelve structure and pattern variants — `pivot_high`/`pivot_low`, `resistance`/`support`,
  `body`/`upper_wick`/`lower_wick`/`range`, `double_top_neckline`/`double_bottom_neckline`,
  `trendline_lows`/`trendline_highs` — with their operand fields, the `tolerance_pct` fraction
  convention, the confirmed-`right`-bars-late / never-repaints property, and a worked 4h resistance
  breakout whose entry compares the close with the **previous** bar's level
  (`crosses_above(close, lag(resistance{…}, 1))`) — never the same bar's, which can never hold — and
  the `pulse://dsl/schema` conventions gain the matching `structure_indicators` entry.
  `examples/strategies/sr-breakout-4h.json` is the initial checked-in 4h S/R breakout (an H4 close
  crossing the previous H4 bar's resistance, gated by an H4 `body > 0.5 × range` anatomy filter and an
  H4 trendline-highs break, with a 1.5% stop and a 1R target), and `tests/sr_breakout_example.rs` pins
  its golden trade count and net P&L on the fixture store, proves every entry sits on a bar where the
  zone break held (from an independently streamed `Resistance` series), and that two runs are
  byte-identical; `tests/compose_vocabulary_1_3.rs` drives the same strategy through the real builder
  tools and checks the prompt names every token.

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
  `just deploy-check` rehearses the QA unit. QA seeding (`pulse qa-seed`) moved to issue #355.
  The handshake gains an ADDITIVE optional `role` field, and the app's status strip
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
  (`<db>.serve.lock`) and holds it for its process lifetime, so a second server on a held database
  refuses by name (#250); `pulse import` and `pulse restore` take no instance lock in this release —
  the operator stops the server before either, and the lock for import/restore is tracked in
  [#355](https://github.com/pulseai-labs/pulse-trader/issues/355). An import refuses a source
  database or data dir that resolves to the target — the path itself, a symlink to it, a hard link
  (#264) — before anything is touched. `paper_session`, `paper_event` and `paper_bar` are compared
  by SHA-256 content digest (every row's columns, primary-key order, length-prefixed) between the
  source and the copy: a changed column, a missing row or a schema difference refuses the import
  naming the table and the first differing key, the verification summary prints the three digests,
  and `pulse restore` runs the same check through the shared engine. CI's determinism matrix gains an
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

- **The desktop frontend builds and tests on Vite 8, and `just ui-deps` follows the lockfile
  (r4.s5.w5).** `vite` moved `^6.4.3` → `^8.3.4` (rolldown + lightningcss), `@vitejs/plugin-react`
  `^5.2.0` → `^6.1.2` (its Vite 8 major) and `vitest` `^5.0.0` → `^5.0.4` (its peer range accepts
  `vite ^8`); esbuild and rollup leave the tree with the old bundler (#121). `vite.config.ts` needs
  no structural change — `build.target: "safari15"` is honoured by the rolldown/oxc transform, and
  an unknown target is a hard build error — so the bundle is the same app: same asset set, 2,043,935
  bytes total against the round-2 build's 2,038,368, with `emptyOutDir`, the jsdom test environment
  and one config file kept (the before/after asset list and totals are in the work item's report).
  Every existing vitest file passes unchanged on the new toolchain. `just ui-deps` now runs `npm ci`
  when `node_modules` is absent **or** when `package-lock.json` no longer matches the `cksum` stamp
  written after the last successful install, so a dependency bump can no longer leave a stale tree
  for the next gate to trip on (#114); `scripts/ui-deps-lockfile-test.sh` proves the reinstall, the
  no-op, the missing-stamp reinstall and the failed-install retry on scratch copies only.

- **Every money, price, quantity, R, percent and time value on the Mac app's screens now reads in
  human units (r4.s5.w1).** One shared format module — `ui/src/format/numbers.ts`, `time.ts` and the
  `Figure` display component — turns the server's exact decimal text into A2's display forms on the
  Backtest Lab (KPI tiles, regime rows and their twin, the 19-column trade log, the run provenance,
  the walk-forward span, the folds sentence and the fold table, the before/after comparison), the
  Library (version stats, the expectancies and the holdout window), the session detail (the running
  total, the marked open position and its stops, orders and fills, the closed trades, the OOS
  verdict), the deployment cards and the promote sheet. Money is `+1,234.56 USDT`, prices carry 2/4/6
  decimals by magnitude, quantities trim to 4, R figures read `+1.25 R`, ratios read `41.7%`, the
  folds sentence reads `6 of 6 folds hold (4 required)` (#305, #328), and every rendered time says it
  is UTC (`2026-10-09 14:15 UTC`). The server's exact string stays one hover away on every element
  (`title`/`data-exact`), the rounding is string arithmetic — the UI still computes no money
  (ADR-0020, ADR-0027 addendum) — and a guard test (`ui/src/format/no-float-money.test.ts`) fails the
  build if a `toFixed(` or an unmarked float conversion reappears on a screen. Wide tables keep
  scrolling inside their panels (A3); `scripts/ui-format-test.sh` is the new ledger line.

- **DSL schema 1.3.0 is the served and written version (r4.s6.w1).** `DSL_SCHEMA_VERSION` and
  `SchemaVersion::CURRENT` move to `1.3.0`; `1.0.0`, `1.1.0` and `1.2.0` documents still load
  (identity-migrated), the composer prompt's frontmatter pin and the `pulse://dsl/schema`
  conventions string move with them, and the engine fingerprint moves with the schema string
  while every frozen pre-bump `result_content_hash` stays identical.

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

- **A self-signed `just bundle` now says so, and the Keychain prompt on every new signed Mac build has a
  cause and a fix (#392).** Every new signed build asked for the login Keychain's password before it could
  read `PulseTrader/server_app_token`, and the answer did not persist — even for the same build. The cause
  is securityd's PARTITION check, not the item's ACL: keyring 3.6.3's `apple-native` store uses the
  file-based login keychain, whose items carry a partition list beside the ACL, and
  `ClientIdentification::partitionIdForProcess` partitions code signed by an Apple-issued certificate
  (Apple Development / Developer ID) as `teamid:<team>` but anything else — the self-signed certificate the
  build used — as `cdhash:<cdhash>`, which changes on every build, so every new build failed the partition
  check and prompted; no designated requirement can fix that. `just bundle` now tests the signed bundle
  against securityd's own team-ID requirement — `developmentOrDevIDReq`, carried in the recipe verbatim: the
  Developer ID CA with a Developer ID leaf, the WWDR CA with a Mac Development leaf (an Apple Development
  certificate) or the WWDR CA with a Mac Distribution leaf — through the `-R='<text>'` form codesign reads
  as literal requirement text (a bare `anchor apple generic` would accept ANY Apple-issued certificate,
  and `-R '...'` without the `=` makes codesign read the text as a file path) after
  `codesign --verify --strict`, prints the `Authority=` and `TeamIdentifier=` lines from `codesign -dvv`,
  and, when that test fails, WARNS on stderr (the build does not fail) that the bundle is not signed by an
  Apple Development or Developer ID identity — so the fix, signing with an Apple Development identity (a
  free Apple ID personal team, made in Xcode) or a Developer ID identity, is visible in the build's own
  output. The identity name still comes from `PULSE_SIGNING_IDENTITY` at build time; nothing about it is
  committed.
  One prompt remains: the first launch of the first team-signed build asks once ("Always Allow") for the
  item whose partition list still names the old `cdhash:` partition — answering it adds the new one, and
  later builds read the token silently. `scripts/check-signing-config.sh` binds to the recipe's code: the
  team-ID test must be an uncommented `if ! codesign --verify -R=` line carrying the three team-ID leaf
  OIDs, the #392 warning line must sit inside that test's `if` block on stderr, and the colon-less identity
  CLASS names (`Apple Development`, `Developer ID Application`, …) are refused as code too — anywhere in
  tauri.conf.json and package.json, and on every justfile line that is not a comment and not the #392
  warning line — because `PULSE_SIGNING_IDENTITY="Apple Development"` selects a certificate just as a full
  identity value does.

- **The Library version tree shows every node's provenance line again, and an agent version's
  hypothesis subline with it (#404, d37).** r4.s5.w3's one-line ellipsis rule on `.vnode-provenance`
  gave the line `overflow: hidden`, and a flex item with non-visible overflow gets an automatic minimum
  of 0 — so inside the fixed 64px node, whose `.vnode-summary` still held the mock's 30px two-line
  reserve and whose `.vnode-hypothesis` was already clamped, both lines shrank to zero height and every
  node showed only its name and summary line. The node is now as tall as its own lines: `NODE_H` 94px,
  and 128px when the version carries a hypothesis subline, whose two clamped lines render in full. A
  row's pitch is its tallest node plus the 14px the old fixed 78px pitch left, the edges anchor at each
  node's own centre and the canvas follows the rows; the summary's min-height reserve is gone, the head
  is pinned to ONE nowrap row (a crowded pin + certification chip + delta head used to wrap its delta
  onto a second line, 14px taller than the layout measured), and `flex-shrink: 0` keeps the provenance
  and hypothesis lines at their own height. jsdom performs no layout, so `LibraryScreen.test.tsx` grew
  a source-reading geometry guard: it reads the node's height constants, its line rules and the tokens
  they resolve through, and fails on the old 64px node (a plain node needs 105.15px) or on either line
  losing its `flex-shrink: 0`. The node is also as WIDE as its widest line now: the first cut kept the
  134px node, and in the Mac proof's face (SF Mono / Menlo, 0.6em ≈ 6.6px at 11px) a 33-character
  `external_agent · <agent name>` label ellipsized to `external_agent…` and the KPI row wrapped its
  `R` unit and trade count onto a second line, which pushed the summary out of the node — so `NODE_W`
  is 256px (with its 24px column gap), the kpis row is one nowrap inline flow that ellipsizes at its
  end instead of wrapping, the summary keeps its own height, and the tree's width maths stops at the
  rightmost node's edge (a two-version tree needs no scrollbar; a deeper one scrolls horizontally).
  The guard grew the width budget with it: at 0.6em per mono character — each rule's own font size
  read, the undefined `--fs-10` inheriting honestly — the 33-character label and a worst-case KPI
  string must fit the node's content box, and the kpis row may never wrap.

- **The Library, Backtest Lab and Designer defects the r3/r4 walks filed (r4.s5.w3).** A version's
  runs are reachable and legible again. Backtest Lab grows a **Runs** block beside the picker: the
  version's own backtests and its latest walk-forward run, each row opening the persisted run it
  names — `getBacktestRun` / `getWalkForwardRun`, nothing re-run (#331, #316) — plus an **Open run
  by id** field that takes any backtest or walk-forward id, selects that run's version and shows it
  (a refusal shows the server's own message); the walk-forward pane now names the version it ran on
  too. Runs that are folds of the version's latest walk-forward are labelled `fold <n> of WF <short
  id>` and grouped after the version's own runs, in the Lab and in the Library's Recent backtests
  alike (#330) — one walk-forward read per id, cached; a fold of an older walk-forward stays
  unmarked, because `LibraryRunSummary` carries no `walk_forward_run_id` (known limit, report §7).
  The compare-with-parent panel heads each column with its run id (`parentRunId` before,
  `childRunId` after, the full id on the element) and shows the `inputsNote` beside the `inputs
  differ` badge instead of only on its hover (#326). The Library's certification run id is now a
  button deep-linking `#/backtest?wf=<id>` — `resolveNavId` strips a query before resolving the nav
  id, and the Lab selects the run's version and opens it (#269). The Designer marks a refused
  composer tool call with ✕ and its error message and reads the composer's next call of that tool
  as `retry` (#322) — failure is decided from `toolCallResult.outcome`'s own error-shaped JSON
  (the serialized `FieldError` list or the unknown-tool object), never guessed — and the final step
  (`finalize_strategy`) now shows its ✓ and result line when the stream closes, because a
  successful finalize emits no `toolCallResult` and its closing line rides the `finished` event
  (#317). A version-tree node's creator label truncates to one ellipsized line with the full line
  on `title`, so `external_agent · claude-code` no longer pushes the hypothesis subline out of the
  node (#325), and the version picker's created dates now render through w1's `dateUtc` (`YYYY-MM-DD
  UTC`) instead of a local slice. `scripts/ui-polish-lab-test.sh` is the new ledger line over the
  four suites.

- **The r3/r4 walk-filed paper, title-strip and Settings defects no longer reproduce (r4.s5.w4).**
  Paper session cards and the detail header read `last seen active · <time> UTC` — never a live
  `● active` — while the server is not answering, both derive that badge from the same summary
  function, and a session the server reports `running` reads `active` from promotion instead of the
  invented `starting` (#338, #337, #304). The detail header and the cards show the session's own id
  (the 8-character short form, the full id on hover) and name the version separately (#333); a
  certified fixture session carries both the FIXTURE and CERTIFIED badges on the card and in the
  detail (#334); the detail's certificate names each certified snapshot (#304); an exit fill reads
  `close long`/`close short` and the table states the bar a fill belongs to and when an order was
  placed (#382); the promote confirm reads `Promoting…`, cannot be submitted twice, and refreshes
  the title strip's paper count at once (#336, #335); Settings stops offering Disconnect and Change
  server once the link is disconnected (#386); an older status poll no longer overwrites a newer one
  (#389); a failed session read no longer drops the queued trailing refresh (#371); the title strip
  never wraps its words onto a second line (#319); and the sidebar version comes from the status
  handshake instead of a hard-coded string (#310). `scripts/ui-polish-paper-test.sh` is the ledger
  line over the seven test files and the typecheck.

- **The `pulse://dsl/schema` resource read works from Claude Code (#353).** MCP clients on the
  2026-07-28 protocol revision validate `resources/read` and `resources/list` against that
  revision's result schema, where SEP-2549 makes `ttlMs` (integer ≥ 0) and `cacheScope`
  (`public` | `private`) REQUIRED on a cacheable result; rmcp models both as optional and left them
  unset, so Claude Code refused the read with `Invalid result for resources/read` naming exactly
  those two fields while tool calls kept working. Both results now carry `ttlMs: 0` — the schema
  text changes with the DSL version, so a cached copy must never be reused — and
  `cacheScope: "private"`. `tests/mcp_dsl_schema_resource.rs` pins the wire shape (the allowed key
  set plus the two required cache hints on both results), and a live Claude Code session reads the
  resource from a served build. Refs
  [#353](https://github.com/pulseai-labs/pulse-trader/issues/353).

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

- **Safe dependency bump: `rustls` (r5 chore).** Cargo: `rustls` 0.23.45 → 0.23.46, clearing RUSTSEC-2026-0346, RUSTSEC-2026-0347, RUSTSEC-2026-0348 and RUSTSEC-2026-0349. Lockfile only (`cargo update -p rustls --precise 0.23.46`; `Cargo.lock` alone, no `Cargo.toml` change), so the required `cargo deny check advisories` check is green again (#410). No product version bump.

- Added repository security hardening, non-commercial license, and supply-chain checks (VS-1.2.3).
