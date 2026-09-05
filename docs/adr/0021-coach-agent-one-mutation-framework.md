# 21. Coach agent and the one-mutation framework

Date: 2026-08-29T00:00:00Z

## Status

Accepted

(Authored `Proposed` as spine `r1.s2`'s first deliverable — the class-declaration
rung-3 rule for a spine that owes a bone ADR — and **flipped to `Accepted` at
`r1.s2`'s close, 2026-08-29**. `scripts/check-adr-0021.sh` was updated in the same
act, per the bones protocol `ADR-0020` set: it asserts `Accepted` now, and it also
asserts the seventh failure kind, so the `w4` amendment in decision 6 cannot be
dropped without reddening the gate.

Accepted on implementation, not intent — every decision here is built and covered.
`r1.s2.w1` implemented the mutation half; `w2` the coaching domain and migration
`0005`; `w3` the coach turn end to end; `w4` the seventh failure kind. That is why
the whole spine's architecture was decided here rather than one work item at a
time, and why this flip needs no fresh argument.)

## Context

`r1`'s coach capability is one sentence: a trader's persisted backtest run yields
**exactly one validated DSL mutation with a stated hypothesis, or a recorded failed
turn — never silence**, with the turn's cost and coach-prompt version in the
`LlmCall` ledger. `r1.s4` (the coach rail) is the committed consumer; this spine is
an admitted internal-enabler and claims no user journey of its own.

Four things about the existing codebase shape the decision.

**The DSL already enumerates its own tunable surface.** `SweepableValue<T>`
(`src/domain/dsl/sweepable.rs`) wraps every tunable numeric — `SweepableValue<u32>`
indicator periods, `SweepableValue<Decimal>` thresholds and risk params — precisely
so a future sweep feature could enumerate them. That set is exactly the surface a
parameter mutation can address, and it is already closed and typed.

**Validation and compilation already exist and are already total.**
`dsl/validate.rs` returns either a `ValidatedDsl` or a collect-all list of
field-pathed `FieldError`s; `dsl/compile.rs` turns a `ValidatedDsl` into a
`CompiledStrategy`. `ValidatedDsl` is constructible *only* via `validate`, so
"compile something unvalidated" is already a compile error rather than a
convention. A mutation framework that introduced its own notion of validity would
be a second source of truth for the same question.

**`validate.rs` already has an address language.** Every `FieldError` carries a
dotted/indexed locator (`entry.and[0].not.lhs.indicator.rsi.period`,
`exits[0].distance_pct`, `risk.risk_per_trade_pct`) built to let a UI or an LLM
point at one field.

**The LLM path is already decided.** `ADR-0012` gives a thin `LlmProvider` port
behind a redacting decorator; `ADR-0013` extends it with tool calling;
`ADR-0014` makes prompts overlay assets resolved through `$PULSE_PROMPT_DIR`. The
composer already runs on all three.

What is *not* decided, and what this ADR decides, is the shape of the coach itself:
what a mutation is, what makes one valid and when that is established, what the
coach is allowed to see, how many provider calls a turn may make, what happens on
every deviation, and what is persisted.

## Decision

**1. The mutation vocabulary is parameter-only in `r1`** (grill L1). A typed enum
with exactly one variant now:

```rust
Mutation::SetParam { path, new_value }
```

`path` addresses one sweepable numeric leaf; `new_value` is a typed numeric
(`Decimal` or `u32` — never `f64`, NFR-2). The enum is designed for extension:
adding a variant is additive for serde and forces every `match` to be revisited at
compile time. Structural mutations — add or remove a condition, swap an indicator,
change an exit kind — are **excluded from `r1`** and are the named rejected
alternative below.

**2. A mutation is applied through `apply` → `validate` → `compile`, and success
means all three passed.**

```rust
apply(&StrategyDsl, &Mutation) -> Result<CandidateDsl, MutationError>
```

`apply` writes the new value into a *clone* of the input DSL, then runs the
existing `dsl/validate.rs` and `dsl/compile.rs` over the result. `CandidateDsl` is
returned only when the mutated strategy validated **and** compiled, and it carries
the `ValidatedDsl` that proves it. No second validation path is introduced: every
rule the composer's output must satisfy, a coach mutation must satisfy, by
construction and not by duplication.

`MutationError` is typed and total over the ways this fails — unknown path, a
type or domain mismatch (a `Decimal` offered where the leaf is `u32`), validation
failure (carrying the `FieldError`s), and compile failure — each carrying enough
context to be persisted verbatim as a recorded failure reason. The input DSL is
never partially mutated: an error means nothing was written anywhere.

**3. Mutation validity is established at use-time by `apply()` and is never
persisted** (audit C4). There is no `valid` column, no validated flag, and no
"validated at" timestamp anywhere in migration `0005`. A proposal row stores the
typed mutation; whether that mutation still applies is answered by running
`apply()` at the moment it is used. `r1.s4`'s modify-then-accept path therefore
**re-runs `apply()` at accept**, after the trader's edit, and treats a
`MutationError` there as an ordinary recorded outcome rather than an invariant
violation. This is what keeps a stored proposal honest across a DSL schema
migration or a version-tree change.

**4. The mutation path scheme reuses `validate.rs`'s dotted/indexed locator
grammar** (audit C6). `entry.and[0].not.lhs.indicator.rsi.period` addresses the
same leaf whether it appears in a `FieldError` or in a `Mutation::SetParam`, so
coach errors, validation errors and mutation targets all speak one address
language, and a coach failure can be shown in the UI against the same field a
validation failure would highlight. Paths are **total over the tunable surface**:
every sweepable numeric leaf of a valid strategy is addressable, and anything else
is a typed inapplicability rather than a partial write.

**5. Migration `0005` carries the full coaching schema now** (grill L2).
`coaching_sessions` + `coaching_proposals`, with the disposition columns `r1.s4`
needs already present and dormant: disposition (`proposed` / `accepted` /
`rejected` / `modified`), a nullable `child_version_id`, an accept idempotency key,
and a nullable `coaching_sessions.llm_call_id` (audit C3). `llm_call` gains a
nullable `prompt_version` (composer rows stay `NULL`). `w2` writes only the
`proposed` state; `r1.s4` exercises the rest. The
session/proposal state machine the columns encode is
`proposed → accepted → child version → run`, with accept idempotent on the session
id, and with every failure state a recorded row of its own. The dormant columns are
schema stability for a consumer committed in this same release — the
`SweepableValue::Sweep` precedent — not a shell.

*(This decision originally added "**without a second migration**" to the sentence
above. `r1.s4.w4` shipped `0008_coaching_lifecycle` and struck the claim; the
Consequences entry "Dormant columns shipped — and `0008` is what they cost" records
what was found and why the four missing states could not be reached from `0005`.
`0005` itself is untouched, per ADR-0018.)*

The **session row is the audit trail** (audit C3). An `LlmCall` row exists if and
only if a provider call was actually made, so a pre-call failure (an oversized DSL,
a context-overflow refusal) records a failed session with `llm_call_id` `NULL`.

**6. One provider call per coach turn; every deviation is terminal and typed**
(grill L3). Zero calls, several calls, malformed tool arguments, an inapplicable
mutation, a provider timeout, context overflow, **and a provider transport failure**
— each ends the turn and is recorded as that session's typed failure.

*(Amended by operator ruling, 2026-08-29, r1.s2.w4.* The seventh kind,
`TransportFailure`, was argued out of the taxonomy while `w3` was built: an HTTP
5xx is an infrastructure fault rather than a coaching outcome, and recording it as
one of the other six would have put a false reason in the audit trail. That
reasoning held; the conclusion did not. A provider outage still left the ONE coach
turn that produced no row, and release exit criterion 4 — "a recorded failed turn,
never silence" — carries no infrastructure exemption. So the taxonomy gained an
honest variant instead of the turn gaining an exception. Surfaced by `w3`'s own
report §10 and ruled on before `0005` merged, which is why the schema edit was a
CHECK widened in place rather than a second migration. The CLI still preserves the
error at its edge (ADR-0017): recorded AND loud.)* **No composer-style nudge retries.** A coach turn
is cheap to re-ask by hand, and a silent retry loop against a
hidden-reasoning model is a cost trap (issue #124). Retry sophistication has to
earn its way in later from the recorded-failure evidence this design produces.

**7. A single `propose_mutation` tool call ends the turn** (A3). The coach's tool
surface is that one call, registered in `src/agent/tools.rs` under `ADR-0013`'s
conventions; its arguments are the typed mutation plus the stated hypothesis. The
first well-formed call ends the turn; a second call in the same turn is one of the
deviations in point 6.

**8. The coach sees a bounded projection only** (grill L4, as amended by audit C1).
A named `CoachContext` type carrying: summary stats, the regime breakdown, MFE/MAE
aggregates, skipped-entry counts, the engine fingerprint, and the version's DSL.
**Never** the raw trade log and never the equity curve. It draws from
`BacktestResult` and the version's DSL **only** — the run's config header is
deliberately excluded, because those are #110's eight input columns landing in
`r1.s3`, and including them would recreate the `S2 → S3` edge the release record
rebutted; the header joins the coach's view in `r1.s4`, which already depends on
`r1.s3`.

Every projected field is fixed-size except the DSL, so **context overflow collapses
into a pre-call checkable condition on one variable-length field** and is recorded
as the typed failure the capability names. Aggregating persisted per-trade `mfe_r`
/ `mae_r` into fixed-size figures is projection, not recomputation: the coach reads
the persisted `BacktestResult` and **recomputes no backtest** (A5). This is the
attached least-privilege control made concrete — the coach reads exactly the run
and version it was asked about.

**9. The coach prompt is an overlay asset, and the LLM path is the existing one**
(A1/A2). `coach.md` ships compiled-in and is overridden from `$PULSE_PROMPT_DIR`
by the composer's existing resolution (`src/agent/config.rs`, `ADR-0014`). Calls go
through the existing `LlmProvider` port behind the existing redacting decorator
(`ADR-0012`) — **no second LLM path**.

**10. `prompt_version` is the content hash of the *resolved* prompt** (audit C2) —
whichever of the compiled-in default or the `$PULSE_PROMPT_DIR` overlay actually
won, hashed per call and recorded on the `LlmCall` row. An overlay edit therefore
changes the recorded version with no release step, which is what makes the moat
overlay auditable rather than invisible.

**11. Mutations apply to a version's immutable DSL and create no child version
here** (`ADR-0010`). `r1.s2` reads the version tree and writes none of it; minting
the child `StrategyVersion` on accept is `r1.s4`'s path.

## Consequences

- **`src/domain/dsl/mutate.rs` is `r1.s2.w1`'s deliverable**, not `r1.s4`'s.
  `RELEASE.md` lists the file under `r1.s4`'s expected paths; that listing is
  `ADR-0021` *touch surface*, not ownership — `r1.s2`'s capability ("validated —
  applies and compiles") is unimplementable without it. Recorded in `SPINE.md` L5
  and accepted by the operator on 2026-08-29.
- **The coach cannot restructure a strategy in `r1`.** It can only retune what is
  already there. If a real coaching session in `r1.s4` wants "add an ADX filter",
  the coach must answer with a recorded inapplicability, not an approximation. That
  is the honest failure this vocabulary buys, and it is the trigger for the
  feature-map entry below.
- **Validity is a question, not a stored fact.** Anything that reads a stored
  proposal must call `apply()` before trusting it. A cached "this was valid"
  anywhere downstream reintroduces exactly the staleness this decision removes.
- **One address grammar means one blast radius.** Changing `validate.rs`'s locator
  format now also changes mutation paths, including any already persisted in
  `coaching_proposals`. The grammar is effectively frozen by this decision; a change
  to it is a migration, not a refactor.
- **A turn's failure is a row, not a log line.** Never-silence is a storage
  guarantee: `w3` cannot complete a turn without `w2`'s session recording, which is
  why the rounds are serial.
- **Dormant columns shipped — and `0008` is what they cost.** *(Amended at
  `r1.s4.w4`, 2026-09-05, with evidence. The original entry read: "`0005` carries
  disposition state nothing reads until `r1.s4`. That is a deliberate, bounded bet
  on a consumer committed in this release; if `r1.s4` slipped out of `r1`, these
  columns would become a shell and should be revisited rather than defended." The
  bet's stated risk — a slipped consumer leaving a shell — is not what happened.
  `r1.s4` arrived on schedule and the columns turned out to be the wrong shape,
  which is the failure mode a dormant-column bet does not protect against: nothing
  exercises them, so nothing discovers the gap until the consumer is built.)*

  Planning `r1.s4` found **four states `0005` cannot represent**, each verified
  against the merged file rather than argued:

  1. **A session id claimed before the provider call.** `outcome IN
     ('proposed','failed')` has no pre-call state, so a turn could only be recorded
     after the call — and a crash inside that window leaves the silent turn release
     exit criterion 4 forbids.
  2. **Two honest failures the seven-tag taxonomy cannot name** —
     `inapplicable_advice` (structural advice the `r1` parameter-only vocabulary
     cannot express, `#131`) and `missing_backtest_inputs`. Recording either as one
     of the seven would put a false reason in the audit trail, which is the exact
     argument that added `TransportFailure` rather than reusing a neighbour.
  3. **An accepted proposal's run.** `0005` stores `child_version_id` and has no
     column for the re-backtest OF that child, so "no child lacks its run" was
     unrepresentable rather than merely unenforced.
  4. **A failed accept.** An accept that dies at apply/compile/backtest had nowhere
     to be recorded, leaving a reader to infer it from a missing child.

  `migrations/0008_coaching_lifecycle` adds them by rebuilding the two tables
  forward. `0005` is applied history and is **not** edited (ADR-0018), the
  migration's pre-flight VERIFIES the dormant-row claim rather than trusting it —
  an existing accepted-with-child/no-run row fails the migration rather than having
  a run link invented for it — and the down migration refuses transactionally for
  any of the four new states rather than coercing one into an old tag.

  What that cost buys, for the next dormant-column bet: columns nothing exercises
  are not validated by shipping, only by a consumer. When the consumer is more than
  one spine away, the cheaper honest move is to ship the schema the current consumer
  needs and pay for the migration later — which is what happened here anyway, minus
  the confidence.

- **An accepted proposal names its child AND that child's run, atomically**
  (`r1.s4.w4`). This is the decision above made stronger, not reversed. `0008`'s
  `CHECK`s make both links non-NULL exactly when the disposition is `accepted`, a
  trigger proves the run belongs to the child and the child descends from the
  coached version, and a second trigger pins the transition matrix so a settled
  proposal cannot be un-settled by a column-presence-only write. In the domain,
  `Disposition::Accepted` carries both ids as its payload. One transaction writes
  the child, its run, its trades and the proposal's links, or none of them: the
  release rule "no accepted proposal lacks its child and no child lacks its run or
  a recorded failure" is now unreachable-by-construction rather than a convention
  the rail is trusted to keep.

- **Worth re-checking at `r1.s4`:** whether `SetParam` alone produces mutations
  traders actually accept in real sessions, and #124's token posture once the
  coach's larger prompts hit `glm-5.3-flash`.

> **Note (2026-09-06, #164 / PR #165).** The token-posture half of that re-check is
> resolved. The `r1.s4` acceptance walk hit it exactly as anticipated — a coach turn
> spent the whole 4096-token cap reasoning and emitted no tool call, recorded as
> `ZeroCalls` — and the coach now has its own cap (`COACH_MAX_TOKENS`, 16 384) and its
> own transport timeout (100s, inside the 120s turn guard, with a 20s reserve for the
> guard-wrapped ledger write). Three consecutive real turns on the same run then ended
> with one tool call each. The `SetParam`-sufficiency half stays open: two of those
> three turns recorded `record_inapplicable` for a regime filter no numeric leaf can
> express, which is this ADR's own trigger for revisiting structural mutations.

## Alternatives considered

**Structural mutations in `r1` (add/remove a condition, swap an indicator, change
an exit kind).** Rejected. They explode the validation space — a structural edit can
invalidate a strategy in ways parameter retuning cannot, so the coach would need to
reason about the grammar rather than about numbers — and they duplicate the
composer's job, which already builds strategies from a description through the same
validate/compile path. **Trigger for revisiting:** the first real `r1.s4` coaching
session in which the coach's best available advice is a structural change and it has
to record an inapplicability instead. That evidence puts a structural-mutation
variant on the feature map, with this ADR's extension path (an additive enum
variant) as the intended shape.

**An opaque JSON mutation payload in the proposal row**, letting `w1` and `w2` run
in parallel. Rejected as fake decoupling: `r1.s4`'s "modify the proposed mutation's
parameters" needs the *typed* mutation, so `w2` persisting `w1`'s type is a real
dependency edge, not an accident of sequencing.

**Persisting a validity flag on the proposal.** Rejected (audit C4). It is a cached
answer to a question whose inputs — the DSL schema, the version tree, the validation
rules — can all change between proposal and accept. Re-running `apply()` at accept
costs microseconds on a pure in-memory transform and cannot go stale.

**A mutation-specific validator, tuned to "just check the field I touched".**
Rejected. Faster, and wrong: a parameter change can violate a cross-field rule (MACD
`fast < slow`, `TakeProfit` requiring a `StopLoss`), so a narrow check would pass
mutations the composer's own output would have been rejected for. One validation
path, no exceptions.

**A second address grammar for mutation paths**, tuned for terseness rather than
matching `validate.rs`. Rejected (audit C6): two grammars for the same leaves means
a translation layer, and translation layers are where "the coach edited a field the
UI could not highlight" bugs live.

**Composer-style nudge retries on a deviant turn.** Rejected (grill L3). The
composer nudges because a half-built strategy is worth salvaging; a coach turn is
one call that either produced a proposal or did not, and re-asking is a human
gesture that costs nothing. Retrying a hidden-reasoning model silently is how token
spend disappears (#124).

**Including the run's config header in `CoachContext`.** Rejected on audit C1.
`BacktestResult` carries no run inputs; the header is #110's column set landing in
`r1.s3`, and reaching for it here would create the `S2 → S3` dependency the release
record explicitly rebutted.
