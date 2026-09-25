# 26. The client/server split

Date: 2026-09-24

## Status

Accepted. Refines [ADR-0015](0015-system-shape-modular-monolith-hexagonal-one-artifact.md) to "one crate, one binary, two targets,
one database — on the server host", and supersedes the MASTER-SPEC's local-database and
Keychain-only statements (the app no longer owns a local `pulse.db`; secrets that were Keychain
material move behind the server credential profile, r3.s3.w3).

## Context

The desktop app grew into the only place a backtest can run: it owns `pulse.db`, the snapshot
tree and every runtime, so a laptop closing its lid ends the workday. The spine plan
(`r3.s3`) instead gives PulseTrader one always-on process on draco-desk, reachable over
Tailscale, with the Mac app as a thin client. The network now carries market trust boundaries:
whatever listens must refuse to listen anywhere but the tailnet, whatever connects must prove it
holds a live, scoped, revocable credential, and every grant or refusal must leave a record that
outlives the request. The server keeps the process-wide rule that made the desktop app
safe — one process, one DB, no second writer — so the client/server split must not multiply
writers either.

## Decision

`pulse serve` — a new subcommand of the one binary (ADR-0015) — runs on draco-desk and owns the
one `pulse.db`, the snapshots and every runtime (in-process engines, local LLM calls, MCP). The
Mac app is a thin client: it holds no market state and talks to the server through a Rust-side
proxy, so no HTTP client, token storage or networking crate enters the Swift surface.

The access model:

- **Bind:** `pulse serve` binds only an IPv4 Tailscale address (`100.64.0.0/10`), or loopback
  under an explicit `--dev-loopback`; every other address refuses at startup with a named error,
  before any listener exists (the D6 policy, `check_bind` + the 5s/120s retrying bind for the
  not-yet-routable address).
- **Tokens:** per-client revocable bearer tokens (`pt_` + 43 base64url chars from the OS CSPRNG),
  stored ONLY as their SHA-256 hex, each carrying exactly one scope — `app` (the human surface)
  or `agent` (MCP) — with no nesting; revocation is a once-only `revoked_at` write and labels are
  never reused (D5).
- **Audit:** every issue, revoke and refusal appends one row to `token_audit`
  (event, time, token id, label, reason, route, peer); the table is append-only at the database.
  `pulse token issue|revoke|list` is the ONE sanctioned second writer to `pulse.db` beside a
  running server (WAL permits it; it exists so a first token can be created without a server).
- **Handshake:** `GET /api/v1/handshake` — accepted for either scope — answers
  `api_version`, `binary_version`, `engine_fingerprint`, `target_triple` (plus `scope`, the
  caller's own token scope: an ADDITIVE field added at the r3.s3.w5 review round 2, so a client
  can refuse to save a token that cannot do its work — a server that omits it is still
  understood); every response carries
  `X-Pulse-Api-Version`, so a client can refuse a server it does not understand (D4).
- **Operations are server-owned (R2):** backtests, LLM calls and MCP run inside the server
  process; routes and scopes are mounted per item (w2 the command routes, w5 the app and MCP
  surfaces, with MCP bounded by the `agent` scope). The server runs the FULL binary (R1) — no
  feature-gating, no separate server profile.
- **Credentials:** the server credential profile (exchange access, LLM keys) resolves at server
  startup (r3.s3.w3, R3); clients hold no market credentials at all. The LLM credential comes only
  from the process environment and the two permission-checked `.env` locations — never the
  working/manifest dotenv, never the Keychain — and a found-but-refused file stops the startup,
  naming the file, with the existing source labels unchanged (R3). The Mac client's own access
  token lives behind the `client-token-storage` fake (D10), so the client tests stay hermetic.
- **Logging:** one stderr line per request (`method, path, status, label, elapsed`); no header
  value, query string or token ever reaches a log.

## Consequences

- Offline use is gone: nothing market-facing works without the tailnet (design R2). The laptop
  closing its lid no longer ends anything.
- Server availability equals draco-desk's uptime (design R3); w4 puts the binary under systemd
  with the log/journal hygiene that implies.
- Off-box backup of `pulse.db` is deferred (D12) until w4 wires the backup protocol's tables;
  the local backup-before-migrate protocol is unchanged.
- `deny(warnings)`/pedantic still holds; the server is an in-crate ring (`src/server/**`) with
  `axum` as the only new HTTP dependency, riding the already-locked hyper/tower/http graph.
- Touch surfaces for registration at spine close: `src/server/**`, `src/cli/serve.rs`,
  `src/cli/token.rs`, `src/cli/import.rs`, `src/adapters/db/client_token_repo.rs`, `deploy/**`.

## Operations (r3.s3.w4)

draco-desk runs the server under systemd's user manager: `deploy/pulse-serve.service` execs
`~/.local/share/pulse-serve/bin/pulse serve --bind 100.90.203.21:8420` — the tailnet address and
the port live in that unit only — and `just deploy <tag>` is the cutover step: build a tagged
commit in `~/.cache/pulse-deploy/src`, install the binary (deliberately not on `PATH`) and the
three units, then enable and restart. `just deploy-check` is the rehearsal: it verifies the units
with `systemd-analyze` and dry-runs the deploy, installing nothing. `pulse import` moves the Mac's
`pulse.db` and snapshots across with full hash verification, refusing the whole import on any
mismatch; `pulse backup` takes the nightly online copy plus its snapshots into `~/pulse-backups/`
(14 kept, `pulse-backup.timer` at 03:30 local); `pulse restore` verifies a backup the way import
verifies a source, and `just restore` swaps it in with the unit stopped.

## The thin client (r3.s3.w5)

The Mac app is a thin client: every command speaks HTTP to the always-on
server through a single `ServerClient` (`src/client/`) — plain routes, the
`ops/` spawns, and the SSE event stream with `Last-Event-ID` resume under a
capped, injectable backoff (1s → 30s). The app state is the connection
(`ClientState`): `server_connect` handshakes, then persists
`<data dir>/server-connection.toml` (`{ url, token }`, written through a
temporary file plus rename at mode `0600`); `server_disconnect` deletes it;
a relaunched app loads it and the 15 s status poll shows down, then up,
across a server restart. `pulse mcp login` writes the relay's
`mcp-connection.toml` BESIDE it under the same rules, and bare `pulse mcp`
relays stdio to `/mcp` as a transparent byte bridge. MCP over HTTP serves the
same `PulseMcp` the stdio transport serves, Agent-scoped, with the identity
rule the spec pins: the authenticated token's LABEL is the agent's final
identity — the auth middleware stamps it on the request, rmcp nests that
request's `http::request::Parts` into the message extensions, and
`initialize` reads the label through the nested Parts, so a version submitted
through a token labelled `claude-code` records `agent_name = claude-code`
no matter what the client calls itself.
