# 26. The client/server split

Date: 2026-09-24

## Status

Accepted. Refines [ADR-0015](0015-crates-and-targets.md) to "one crate, one binary, two targets,
one database — on the server host", and supersedes the MASTER-SPEC's local-database and
Keychain-only statements (the app no longer owns a local `pulse.db`; secrets that were Keychain
material move behind the server credential profile, r3.s4.w3).

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
  `api_version`, `binary_version`, `engine_fingerprint`, `target_triple`; every response carries
  `X-Pulse-Api-Version`, so a client can refuse a server it does not understand (D4).
- **Operations are server-owned (R2):** backtests, LLM calls and MCP run inside the server
  process; routes and scopes are mounted per item (w2 the command routes, w5 the app and MCP
  surfaces, with MCP bounded by the `agent` scope). The server runs the FULL binary (R1) — no
  feature-gating, no separate server profile.
- **Credentials:** the server credential profile (exchange access, LLM keys) resolves at server
  startup (r3.s4.w3, R3); clients hold no market credentials at all. The Mac client's own access
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
