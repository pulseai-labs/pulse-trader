# 23. Retain Ollama Cloud; bump the default model to `glm-5.3-flash`

Date: 2026-08-29T00:00:00Z

## Status

Accepted

(Answers the open question ADR-0001's index note has carried since 2026-08-23 —
"the current default is an open question needing its own ADR, not a settled
decision this entry still describes." Confirms ADR-0013's provider choice rather
than superseding it: Ollama Cloud stays. Only the model moves.

Authored `Accepted` on evidence, not intent: the subscription was restored and the
new default was driven end to end through the composer's real tool loop before this
ADR was finalized. The measured run is recorded below.)

## Context

The shipped default was `glm-5.2` over Ollama Cloud (`https://ollama.com/v1`),
pinned in `config/prices.toml`'s `[llm]` table with compiled-in `const` fallbacks.
Two things forced a decision.

**The default had become unrunnable.** The operator's ollama.com subscription had
lapsed and the endpoint returned HTTP 402, so nobody could build a clean checkout and
reach a model. An account state rather than a statement about the service, but the
effect on this repository was total. The subscription has since been restored
(2026-08-29) and the endpoint answers again — which is what made the evidence below
possible, and what makes this decision testable rather than aspirational.

**A z.ai flip was drafted, fully reviewed, and rejected on its licensing terms.**
The obvious response to a dead subscription was to move providers, and that is what
was first attempted: the default was flipped to z.ai's GLM Coding Plan endpoint
(`https://api.z.ai/api/coding/paas/v4`, `glm-5.3`), which the operator had verified
by hand on 2026-08-28 as driving the composer's full tool-calling loop. That change
was written, reviewed by CodeRabbit, Codex and a local review pass, and **closed
unmerged** — see [PR #123](https://github.com/pulseai-labs/pulse-trader/pull/123),
retained as the record.

Review is what killed it, and the specific finding is worth preserving because it is
easy to get wrong twice. z.ai's subscription terms and usage policy restrict Coding
Plan quota to officially supported coding tools, and prohibit spending it by invoking
the model API directly from a custom application, bot, website or SaaS product,
absent a separate written agreement; detected use through an unsupported path may
have the subscription restricted or terminated. **The restriction is on the shape of
the usage, not on the number of users.** PulseTrader is such an application, opening
its own HTTP connection to the endpoint, so it sits outside the plan's letter even on
the plan holder's own machine with the plan holder's own key. The draft ADR had
argued the exposure "does not exist while there is exactly one user and that user is
the plan holder"; that read a usage-shape restriction as a user-count one, and it was
wrong. ADR-0013 had reached the correct conclusion on this endpoint back on
2026-07-10 for exactly this reason, and the review re-derived it independently.

That leaves the licensing argument for Ollama Cloud intact and unchallenged: it is a
subscription API sold for programmatic use, which is precisely how PulseTrader
consumes it.

## Decision

**Stay on Ollama Cloud. Bump the default model `glm-5.2` → `glm-5.3-flash`.**

The provider, the endpoint and the credential path are all unchanged. `[llm]`'s
`base_url` stays `https://ollama.com/v1`, `OLLAMA_API_KEY` still carries the key,
`LlmBackend::Ollama` remains accurate provenance, and the compiled-in base-URL
`const` is untouched. This ADR moves a model id and a price row.

```toml
[llm]
base_url = "https://ollama.com/v1"
model = "glm-5.3-flash"
```

**The id is written bare, with no `:cloud` tag — checked, not assumed.** Ollama's
library page publishes only a `cloud` tag for this model
([ollama.com/library/glm-5.3-flash/tags](https://ollama.com/library/glm-5.3-flash/tags))
and its usage examples show `glm-5.3-flash:cloud`, which is a reasonable read of the
docs and would have been the wrong thing to ship. A live probe against
`https://ollama.com/v1` returned **HTTP 200 for the bare id**, tool-calling included,
so the endpoint resolves it. The bare form is adopted because it is the one actually
exercised, and because it matches how `glm-5.2` was written when it worked in
VS-1.3.2. Recorded because the documentation and the endpoint disagree, and the next
person reading that tags page will reach the same wrong conclusion.

The three compiled-in model `const`s move with the config, for the same reason they
always must: `OLLAMA_MODEL_ID` (`openai_compat.rs`), `COMPOSE_MODEL`
(`cli/compose.rs`) and `DEMO_MODEL` (`cli/llm.rs`). `pulse compose` and the Tauri
compose command read `[llm].model`; `pulse llm-check` does not, and takes `DEMO_MODEL`
directly. A config-only edit would leave `llm-check` on the old model silently. That
five-site duplication (those three, plus `[llm].model` and its price-row key) is
registered on the ossify feature map with the provider seam; collapsing it is that
work, not this bump.

**The price row is nominal, and sourced.** Ollama Cloud is flat-rate, so no per-token
tariff is levied — the framing `prices.toml` has always carried. The nominal figure is
z.ai's published **list** tariff for the same model: **$0.15 / 1M input, $0.50 / 1M
output** ([docs.z.ai pricing](https://docs.z.ai/guides/overview/pricing), retrieved
2026-08-29). List, deliberately, not the 50%-off promotional rate ($0.075 / $0.25)
running until 2026-09-09 — pinning the ledger to a price that expires in days would
make every row written after it wrong. ADR-0014's discipline holds: values live in
the data file, and `src/agent/config.rs` carries no price numbers.

**Multi-model tiering is deferred, and nothing here implements it.** The intended
shape is `glm-5.3-flash` as primary, `glm-5.3` for harder tasks, `gpt-oss:120b` for
light ones. That needs routing — a policy for classifying a task and selecting a
model — which does not exist and is not started. Registered on the feature map as
"Multi-model routing on Ollama Cloud". Today exactly one model id is read.

### Evidence

Two gates were set before this bump could land, and both are met.

**1. The subscription is restored.** `https://ollama.com/v1` answers for the
operator's account again; an API-level probe returned HTTP 200 with a clean
tool-call round trip (`finish_reason: tool_calls`, valid JSON arguments).

**2. `glm-5.3-flash` completes the composer's real tool loop.** This gate is the one
that mattered, because API-level tool-calling is necessary but *not sufficient* —
`gpt-oss:120b` transported fine on this same endpoint and then returned reproducible
HTTP 500s **mid-loop** at VS-1.3.2 slice-close, which is why `glm-5.2` was chosen over
it. A live `pulse compose` run on 2026-08-29, against this endpoint with this model,
over a scratch database:

- **Six tool calls dispatched in sequence**, all accepted by the builder tools:
  `create_strategy` → `add_entry_signal` → `add_filter` → `set_exit_rules` →
  `set_risk_params` → `finalize_strategy`. No mid-loop failure — the specific thing
  being guarded against did not occur.
- **A schema-valid strategy was finalized and persisted**: version
  `69cd195b-6fb3-4d76-b19f-5a3e7dd506e7`, `created_by = ComposerLlm`, with 6
  `creating_llm_call_ids` — so provenance reconstructs (ADR-0010).
- **Six `LlmCall` rows**, each `backend = ollama`, `model = glm-5.3-flash`: 23,008
  input and 1,021 output tokens, nominal cost $0.0039617 USD off the new price row.
- **No truncation.** Peak `output_tokens` was **701** against the 4096 cap — the
  `output_tokens >= 4096` check that issue #124 uses as its truncation detector
  returns zero rows. Worth stating because flash spends hidden reasoning tokens
  against the same cap, so a low visible answer length is not by itself proof of
  headroom; the measured peak is.
- **No secret leaked into the ledger** (NFR-6): zero rows with an `sk-`-shaped
  string in `prompt_messages` or `completion`.

> **Note (2026-09-06, #164 / PR #165).** The `output_tokens >= 4096` detector above
> reads the composer's cap, which is still 4096. It is not the coach's: the coach now
> asks for `COACH_MAX_TOKENS` (16 384), so a truncated COACH turn shows
> `output_tokens = 16384`, not 4096, and the literal check returns zero rows for it.
> Stated generally, the truncation detector is `output_tokens >= the caller's cap` —
> per surface, not per repository. The evidence: the #164 walk's two `zero_calls`
> turns hit exactly 4096 under the old shared cap, and the raw provider response for
> the reproduction carried `finish_reason: "length"` with an empty `content` and a
> non-empty `reasoning` field. Nothing above is retracted; the detector is scoped.

That is the composer's full path exercised on the new default, not a transport
smoke test.

## Consequences

**(+)** The licensing question is closed, not deferred. Ollama Cloud is sold for
programmatic use, so no term is being stretched and there is no accepted risk to
carry — unlike the rejected z.ai path, where the cost of the default would have been
the operator's own subscription.

**(+)** The diff is a model id, a price row and the record. No provider migration, no
credential change, no ledger-provenance change, no naming debt incurred:
`LlmBackend::Ollama`, `OLLAMA_API_KEY` and the `OLLAMA_*` consts all still say what
is true.

**(+)** `glm-5.3-flash` is the cheap tier by design, so the nominal per-token figures
drop roughly 4× on input and 4.4× on output against `glm-5.2`'s. If the composer's
quality holds, the coach loop's iteration cost falls with it — which is the point of
choosing flash as primary.

**(+)** The default is runnable again and demonstrably so, which is the state the
repository has not been in for some time.

**(−) The default now depends on a subscription that has already lapsed once.**
Nothing about this change reduces that exposure — it is the same single point of
failure, restored. The difference from the z.ai path is only that losing it costs
access rather than a terms violation. If it lapses again the answer is the same one
that was reached this time: a provider decision, made deliberately, not a scramble.

**(−) One composer run is evidence, not coverage.** The smoke test proves the tool
loop completes for one strategy target on one day. It does not establish that
`glm-5.3-flash` matches `glm-5.2`'s composition quality across the targets the coach
loop will actually put through it, nor that its peak token use stays at the 701
observed here for a harder target. Issue **#124** carries the ongoing verification.

**(−) The five-site model-id duplication is unchanged and was paid again.** This bump
touched five places to move one value, the second time in as many attempts.
Registered on the feature map, not fixed here.

**(−) The bare-vs-tagged id rests on one probe.** The endpoint accepted
`glm-5.3-flash`; Ollama's own tags page says `glm-5.3-flash:cloud`. Documentation and
behaviour disagree, and this ADR follows the behaviour. If Ollama tightens resolution
later, the bare form breaks and the fix is one config line — but it would break at
runtime, not at build time.

## Alternatives considered

**Flip to z.ai's coding endpoint** (the drafted change, PR #123). Rejected on the
terms: Coding Plan quota may not be spent by a custom application calling the API
directly, regardless of who runs it, on pain of losing the subscription. The full
analysis and the review that produced it are preserved on #123 rather than
summarized away, so the next person to propose this finds the reasoning instead of
repeating the work.

**Flip to z.ai's per-token API** (`https://api.z.ai/api/paas/v4`). Not rejected on
merit — it is unambiguously licensed for programmatic use and is a genuine option.
Set aside because it means paying per token for iteration the operator already has
subscription capacity for, and because it is a provider migration when the operator's
ruling was to keep the provider. Remains the natural candidate if Ollama Cloud is not
restored, or at deployment.

**Bump to `glm-5.3` rather than `glm-5.3-flash`.** Rejected as the *default*, not as
an option: `glm-5.3` is the more capable and more expensive tier, and the operator's
intent is flash-primary with `glm-5.3` reserved for harder tasks. Making the
expensive model the default would prejudge the tiering this ADR explicitly defers.
`glm-5.3` is available on Ollama Cloud and reachable with a one-line `[llm].model`
edit plus a price row whenever it is wanted.

**Implement the tiering now.** Rejected as scope. Routing needs a task-difficulty
policy, a selection mechanism, and a way to evaluate whether the split helps —
none of which exists, and none of which belongs in a change whose whole content is
one model id. Registered on the feature map instead.

**Restore the subscription and keep `glm-5.2`.** Rejected, though it is the smallest
possible action and deserves a straight answer. Restoring the account alone would
resurrect a default that is a model generation behind and roughly 4× more expensive
per nominal token, for a composer loop whose cost is the thing the coach iteration
budget is measured against. The subscription had to be restored either way; doing it
and *not* taking the cheaper, current model would be paying the cost of the outage
without collecting anything for it.

**Ship the bump without running the composer smoke test.** Rejected, and worth
recording as a rejected alternative rather than an obvious step, because the
temptation was real: the API-level probe already showed clean tool-calling, and that
looks like sufficient evidence. It is not. `gpt-oss:120b` passed exactly that bar on
this exact endpoint and then failed mid-loop, which is the entire reason `glm-5.2`
was the incumbent. Transport-level success and agent-loop success are different
claims, and only the second one is what this default has to make.
