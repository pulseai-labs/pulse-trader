# Public boundary

What belongs in this repository, and what does not. This file is public; it
states rules and patterns, never the contents of anything withheld.

**Posture:** source-available. The code here is readable and runnable. Its
licence (PolyForm Noncommercial 1.0, see `LICENSE`) restricts commercial use;
`COMMERCIAL.md` covers commercial terms.

## Machine-checkable rules
<!-- Executed by the release-close boundary audit
     (close/references/boundary-audit.md) at every release close. That file
     is authoritative for which repos run these rules, which trees they are
     matched against, and what each arm does with a finding — do not restate
     any of it here. -->
never-tracked: **/.env, **/.env.*, **/*.pem, **/*.key, **/id_rsa*
never-tracked: **/secrets/**, **/credentials.json
never-tracked: **/SPEC.md, docs/planning/**
never-tracked: **/.superpowers/**, **/.claude/**
fixtures-must-be: synthetic

**One known tracked exception, so the rejection is pre-written:** the env rule
matches `.env.example`, which is tracked on purpose and holds variable *names*,
never values — it is the "shape of every configurable input" this file
sanctions below. A close that raises it rejects it in one sentence.

## Working-tree hygiene allowlist
<!-- Classes of untracked sensitive files known to exist in local clones,
     named by PATTERN only — never by content description. -->
- `.env*` (untracked, gitignored)

## What lives here

- The engine, the strategy DSL, the agent and its tool definitions, the CLI, the
  storage adapters, the migrations.
- The **shape** of every configurable input: prompt files, the price-table
  schema, and the default values compiled into the binary.
- Product architecture decisions, in `docs/adr/`.
- Tests, fixtures and determinism baselines.

## What does not live here

- **Secrets, in any committed file.** No API keys, no tokens, no credentials in
  any tracked file, any branch, or any test fixture. At runtime a credential
  reaches the process through the **macOS Keychain**, the **process
  environment**, or a **gitignored `.env`** — and **which sources a command
  consults is per-command, not a global fallback chain**:

  - `pulse compose` reads `OLLAMA_API_KEY` from the process environment, then
    from `.env`. It never consults the Keychain.
  - `pulse llm-check` reads the Keychain only. It never consults the
    environment or `.env`.

  So configuring one channel does not make every command work. The environment
  and `.env` are both plaintext and both supported development paths, so the
  rule is *never committed*, not *never on disk*: keep `.env` out of the index
  and out of every diff, and keep exported credentials out of shell history and
  CI logs. Persisted LLM records are redacted before they are written by
  the shipped composition roots.
- **Tuned runtime data.** Some configuration shipped here as a working default
  is superseded at runtime by an operator-supplied override directory. The
  override mechanism, the file format, and the defaults are public; the tuned
  contents of a private override are not, and nothing in this repository should
  be edited to embed them.
- **Planning and process material.** Specs, roadmaps, retrospectives, memory
  bank and session artifacts live in the private AI workspace, not here. That
  includes agent working directories — `.superpowers/`, `.claude/` — which are
  process state, not product.

## The rule for contributors

If a change would make this repository the only place a piece of information
exists, and that information is operational rather than structural — a tuned
value, a credential, a customer detail — it does not belong in the change.
Publish the mechanism, not the tuning.
