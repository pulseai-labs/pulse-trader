# `pulse mcp` — MCP client configuration

The desktop bundle ships an MCP server as a subcommand of the app binary, so
the server an agent talks to is always the same build as the app it affects.
Point your client at the bundle binary — **not** a separate server package.

## Claude Code

```sh
claude mcp add pulse -- /Applications/PulseTrader.app/Contents/MacOS/pulse mcp --agent-name claude-code
```

The equivalent `.mcp.json` entry:

```json
{
  "mcpServers": {
    "pulse": {
      "command": "/Applications/PulseTrader.app/Contents/MacOS/pulse",
      "args": ["mcp", "--agent-name", "claude-code"]
    }
  }
}
```

## One database, no overrides

The documented configuration passes **no `--db` and no `--data-dir`**. That is
deliberate: with no overrides the server opens the same
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
fresh build, `target/debug/pulse mcp --agent-name claude-code` serves the same
tools from the working tree. It shares the release schema only while the tree
is at or ahead of the installed app's migrations; treat it as a rehearsal
config, never the demo config.

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

Nine tools — seven reads, two writes. All calls are synchronous; write tools
commit before they return.

| Tool | Arguments | Returns |
|---|---|---|
| `list_strategies` | `{ include_archived?: bool }` | `{ strategies: [...] }` — strategy/version tree, parent-first |
| `get_version` | `{ version_id }` | migrated DSL document, verbatim original, hash, provenance |
| `list_runs` | `{ version_id }` | `{ runs: [...] }` — runs against the version, headline stats + input provenance |
| `get_run` | `{ run_id }` | summary stats, regime breakdown, skipped entries, MFE/MAE, persisted inputs, `open_position` (the window-edge mark, or `null`) |
| `export_trades` | `{ run_id }` | absolute CSV path, row count, column names |
| `export_candles` | `{ pair, timeframe, data_version?, format? }` | absolute path under exports dir, row count, resolved `data_version` |
| `export_indicators` | `{ pair, timeframe, data_version?, indicators }` | absolute CSV path (`open_time` + one column per spec) |
| `submit_strategy_version` | `{ parent_version_id? \| strategy_name?, dsl, hypothesis }` — exactly one of the first two | `{ version_id, strategy_id, version_hash, created_by: "external_agent", agent_name, submission_id, created_at }` |
| `run_backtest` | `{ version_id, from?, to? }` — `from`/`to` are RFC 3339 UTC (`"2025-03-01T00:00:00Z"`), both or neither | `{ run_id, version_id, run }` — `run.inputs.window` carries `{from_ms, to_ms}` |

Field-level rejections come back as `{ "field", "message" }` tool errors —
correctable, nothing persisted. A structural failure (busy database, missing
snapshot) surfaces as `{ "message" }`.

## See also

- `docs/walks/r2.s1-agent-discovery-walk.md` — the end-to-end runbook this
  configuration exists for.
