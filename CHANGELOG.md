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

### Security

- Added repository security hardening, non-commercial license, and supply-chain checks (VS-1.2.3).
