# `pulse mcp` — MCP client configuration

The desktop bundle ships an MCP server as a subcommand of the app binary, so
the server an agent talks to is always the same build as the app it affects.
Point your client at the bundle binary — **not** a separate server package.

## Claude Code

```sh
claude mcp add pulse -- /Applications/PulseTrader.app/Contents/MacOS/pulse mcp --local --agent-name claude-code
```

The equivalent `.mcp.json` entry:

```json
{
  "mcpServers": {
    "pulse": {
      "command": "/Applications/PulseTrader.app/Contents/MacOS/pulse",
      "args": ["mcp", "--local", "--agent-name", "claude-code"]
    }
  }
}
```

## Modes

`pulse mcp` has three modes (`src/cli/mcp.rs`):

- **`--local`** — the mode the configuration above pins: serve MCP over
  stdio against the **local** database. `--db`, `--data-dir` and
  `--agent-name` all apply.
- **bare `pulse mcp`** — the thin relay: stdin's JSON-RPC frames ride HTTP
  to the always-on server's `/mcp` route with an **agent token**. The
  connection comes from the `mcp-connection.toml` file `login` writes; no
  login and no `--local` is a non-zero exit with the reason on stderr.
- **`pulse mcp login --server <url>`** — verify a token against the server
  (handshake + one MCP `initialize` probe) and write that connection file.
  The token is read **from stdin only** — never an argument, never a flag.

In relay mode the database the tools read and write is the **server's**;
`--db` and `--data-dir` do not apply.

## One database, no overrides

In `--local` mode the documented configuration passes **no `--db` and no
`--data-dir`**. That is deliberate: with no overrides the server opens the
`~/Library/Application Support/PulseTrader/pulse.db` and the same candle store
the Finder-launched app uses — so a version the agent submits appears in the
app's Library on next focus. The app cannot be redirected to a different data
dir; pointing the server elsewhere would split the demo in two.

`--db` and `--data-dir` exist on the subcommand for development only. Passing
them makes the server operate on a different database than the app on screen —
fine for a rehearsal, wrong for the documented client.

## Schema skew

The server migrates the database it opens. If the binary behind
`Contents/MacOS/pulse` is *older* than the app that last ran, the server may
fail to open a database written by a newer schema — or, worse, silently serve
a stale snapshot. Keep the client pointed at the bundle you actually run, and
re-run `claude mcp add` after reinstalling the app.

**Dev override (development only, not the documented config):** against a
fresh build, `target/debug/pulse mcp --local --agent-name claude-code`
serves the same tools from the working tree. It shares the release schema
only while the tree is at or ahead of the installed app's migrations; treat
it as a rehearsal config, never the demo config.

## Agent identity

`--agent-name claude-code` resolves the agent identity at startup — 1–64
characters of `[A-Za-z0-9._-]`, stored lowercase, final for the process. With
no flag the server adopts the client's `clientInfo.name` at the `initialize`
handshake (also lowercased); a client that names itself something else still
works, but versions it submits carry that name instead. With neither, the
identity is `unknown` — submissions still land, labelled
`external_agent · unknown`. The flag exists so the documented config pins the
label regardless of what the client reports.

## Exports

`export_*` tools write files under the **server-side** exports directory:

```
<app data dir>/exports/<pid>-<start-unix-ms>/
```

On macOS that is `~/Library/Application Support/PulseTrader/exports/…`. The
directory is **operator-owned**: the server creates it per process and never
deletes anything in it. Files accumulate across sessions; prune them by hand
(or don't — they are the walk's audit trail). The tools return the absolute
path of every file they write.

## Tools

Twenty tools — nine library/run/paper reads, three exports, two writes,
two walk-forward/certification tools, four example-mark tools. All calls
are synchronous; write tools commit before they return.

**Library, runs and paper reads**

| Tool | What it does |
|---|---|
| `list_strategies` | List strategies with their version trees in parent-first order; `include_archived` opts in |
| `get_version` | Fetch one version by id — migrated DSL document, verbatim original, hash, provenance |
| `list_runs` | List the runs recorded against a version — headline stats plus persisted input provenance |
| `get_run` | Fetch one run — summary stats, regime breakdown, skipped entries, MFE/MAE, persisted inputs, integrity fields |
| `get_walk_forward_run` | Fetch one walk-forward run by id — the same detail shape `run_walk_forward` returns, one row per fold |
| `list_paper_sessions` | List paper sessions with status, graduation, engine epochs, closed-trade count, out-of-sample comparison |
| `get_paper_session` | Fetch one paper session by id — status, graduation, epochs, open position, recorded verdicts, comparison |
| `get_paper_trades` | Fetch one session's closed trades with their realized R-multiples, plus its open position |
| `get_paper_comparison` | Fetch one session's out-of-sample comparison — live mean R against the certifying walk-forward's fold-expectancy range |

**Exports**

| Tool | What it does |
|---|---|
| `export_trades` | Export a run's full trade log as CSV under the server exports dir — absolute path, row count, column names |
| `export_candles` | Export a candle snapshot — CSV rows, or a byte copy of the Parquet file when `format=parquet`; `data_version` omitted selects HEAD |
| `export_indicators` | Export per-candle indicator values (e.g. `rsi:14`, `ema:50`) as CSV — `open_time` plus one column per spec, blank while the engine warms |

**Writes**

| Tool | What it does |
|---|---|
| `submit_strategy_version` | Submit a DSL variant with a hypothesis — `parent_version_id` clones, `strategy_name` roots a new strategy (exactly one); persisted as `external_agent` plus its submission row |
| `run_backtest` | Run a backtest of one version — optional `from`/`to` window (RFC 3339, both or neither), optional `pair` override, effective window echoed; holdout-guarded |

**Walk-forward and certification**

| Tool | What it does |
|---|---|
| `run_walk_forward` | Walk a version forward — `rolling-oos/v1` cuts the counted span into K folds (2..=12, default 6), judged under `wf-v1` (default) or `wf-v2`; each fold is an ordinary persisted run |
| `certify_version` | Certify a version against the frozen holdout — one call is one hypothesis; requires an open freeze and a qualifying power review; recorded whether or not it certifies |

**Example marks**

| Tool | What it does |
|---|---|
| `write_mark` | Mark one candle as an example trade — a reference trade, never an order; a candle reaching the open freeze's holdout start is refused |
| `list_marks` | List the chart's example marks in candle order, narrowed by pair/timeframe/setup; withdrawn marks absent, holdout-hidden marks reported under `hidden_by_holdout` |
| `withdraw_mark` | Withdraw one mark by id — one-way, the row stays stored, the mark leaves every list; only the withdrawing agent's own marks |
| `explain_marks` | Explain, per marked trade, which conjuncts of a candidate rule held at the entry bar, with the operand values and the 17 candle-pattern values |

Field-level rejections come back as `{ "field", "message" }` tool errors —
correctable, nothing persisted. A structural failure (busy database, missing
snapshot) surfaces as `{ "message" }`.

## See also

- `docs/walks/r2.s1-agent-discovery-walk.md` — the end-to-end runbook this
  configuration exists for.
