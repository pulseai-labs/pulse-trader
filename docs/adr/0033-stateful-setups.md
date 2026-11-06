# 33. Stateful setups: arm, dip, trigger and expiry in the strategy grammar and the engine

Date: 2026-11-06

## Status

Accepted. Registers the stateful **setup** that release r6 needs for Cipher v1: a strategy whose rule is a sequence
(arm on a higher-timeframe close, then a dip, then a trigger, or lapse), not one condition on one bar. It also
registers the **structure stop** that setup defines. Rules the grammar of schema 1.6.0 (built in `r6.s2.w1`) and fixes
the engine behaviour that `r6.s2.w2`, `w3` and `w4` build to. Rides
[ADR-0024](0024-dsl-schema-evolution-and-indicator-seam.md) (additive minor bump, identity migration) and
[ADR-0018](0018-evolution-forward-only-migrations-immutable-snapshots.md) (forward-only). Takes its rulings from
PDR-0045 (two sides written in full, one position slot, the timing rules, this ADR), PDR-0042 (engine gaps) and
PDR-0044, and its rules from STR-0003 (v1 rules 2–9).

## Context

Cipher v1 (STR-0003) is not one condition on one bar. A trader reads it as a sequence: a 4H bar closes with the
context right (the setup **arms**), the price pulls back on the 15m chart (the **dip**), then a 15m bar shows the
turn (the **trigger**), all within a bounded time, and the stop sits at the swing the setup made. The DSL before
schema 1.6.0 holds `entry` ∧ `filters` on one bar plus a fixed-distance or ATR stop. It cannot say "after", it keeps
no state between bars, and it has no stop that comes from the price structure.

PDR-0045 settled the open design at plan-spine r6.s2 (2026-11-04):

1. A version is **one document with a long side and a short side**, each written in full, sharing one position
   slot. The engine never derives a mirror of one side itself.
2. A side arms **only on a 15m bar where a new 4H bar closed**.
3. The dip comes **strictly after** the arm bar and the trigger **strictly after** the dip bar, within a bounded
   number of bars; the stop window runs from the arm to the trigger bar; a trigger that finds a position or a
   pending entry uses up the setup and enters nothing; **setup state advances during lead-in**, so a fold boundary
   never drops a setup.
4. A new ADR records all of it and registers as a bone at the spine's close.

## Decision

### 1. Grammar

`StrategyDsl.setup: Option<Setup>` joins the document under schema 1.6.0, written as `setup`, absent when none. (The
Rust field is `Option<Box<Setup>>`: a setup holds up to eight conditions inline, and an unboxed one pushed several async
futures past `clippy::large_futures`. The wire form is the same.)
`Setup { expiry_bars, long?, short? }`; each side is `SetupSide { arm, dip, trigger, stop_anchor? }`, every field a
`Condition` (`expiry_bars` a `SweepableValue<u32>`). `ExitRule::StructureStop { buffer_pct }` is the new exit.

With a setup, `entry ∧ filters` is a **gate both sides share**, checked on the trigger bar. The setup's own
conditions add to it; they never replace it.

The shape rules, enforced by `validate` (a `Sweep` is refused on both numeric fields, as everywhere):

| Rule | Path | Code |
|---|---|---|
| S1: a setup has at least one side | `setup` | `InvalidSetup` |
| S2: one side ⇒ `direction` equals that side | `direction` | `InvalidSetup` |
| S3: both sides ⇒ `direction` is `long` | `direction` | `InvalidSetup` |
| S4: a `StructureStop` needs a setup | `exits[<i>]` | `InvalidSetup` |
| S5: `expiry_bars` is in 1..=2000 | `setup.expiry_bars` | `FieldRange` |
| S6: `buffer_pct` is in [0, 0.05] | `exits[<i>].buffer_pct` | `FieldRange` |

`StructureStop` joins the `StopLoss` / `AtrStop` exclusive family: at most one of the three, and a `TakeProfit` may
rest on any of them. Every side condition is checked by the ordinary condition rules, at paths such as
`setup.long.dip`.

Each side is **written in full**. The engine never mirrors a long side into a short one (PDR-0045).

The grammar adds one exception to ADR-0024's write-path strictness. A raw key whose value is `null` and which the
parsed document omits reads as **absent**, not as an unknown field. The served schema (`schemars`) admits `null`
for the `Option` fields `setup`, `long`, `short` and `stop_anchor`, so an agent that follows the schema may write
`"setup": null`, and the document must still load. A null means absent either way. A raw key with a non-null value
that the parsed document omits is still an `UnknownField`. The published conventions text still says to omit an
absent field.

### 2. Arming

A side's `arm` is evaluated only on a primary bar whose step handed over at least one newly closed higher-timeframe
candle, and only when the entry gate is warm. A true `arm` **arms or re-arms** the side: the dip, the stop anchor
and both extremes restart from scratch. Context rules that a trader states as "checked once, when the setup arms"
are conjuncts of `arm`.

ADR-0024's rule that a higher-timeframe value is read only after its bar closes is unchanged. Arming adds the **edge
rule**, because an `htf` comparison stays true on every 15m bar until the next higher-timeframe close and would
otherwise re-arm the side on each of them. A setup always needs the higher timeframe: running one without an HTF
input fails as `HtfRequired`.

### 3. Order and expiry

The **dip** is the first bar after the arm bar where the side's `dip` holds. The **trigger** is the first bar after
the dip bar where `trigger ∧ entry ∧ filters` holds. Only bars arm+1 … arm+`expiry_bars` count: a trigger on bar
arm+k counts for 1 ≤ k ≤ `expiry_bars`, and on bar arm+`expiry_bars`+1 the side **lapses**. A re-arm takes
precedence over everything else on its bar.

### 4. Firing

A fired side becomes **idle at once**. It enters at the next bar's open when the bar is counted and no position and
no pending entry exists. Otherwise it enters nothing, and nothing counts it: a trigger that finds a position or a
pending entry uses the setup up. When both sides fire on one bar, the long side is evaluated first and the short
side's fire finds the pending entry taken. Both sides share one position slot; the position takes the direction of
the side that triggered.

### 5. State

Setup state is a **pure function of the bars stepped**. It advances on lead-in bars too, so a walk-forward fold and
a resumed paper session reach the same state as a full run. It is part of the step frame and the state digest.
Determinism is tested across a fold boundary and across repeat runs.

### 6. The structure stop

The stop window **starts** at the first bar after the arm bar where the side's `stop_anchor` holds, or at the bar
after the arm bar when `stop_anchor` is absent or never holds through the trigger bar. It **ends at the trigger
bar, inclusive**. The stop is:

- long: the window's lowest low × (1 − `buffer_pct`);
- short: the window's highest high × (1 + `buffer_pct`).

**1R is `|fill − stop|`, frozen at the fill**, and a `TakeProfit` sits `target_r` × 1R from the fill. A fill at or
past the stop **skips the entry and counts it** (`SkipReason::StopBeyondFill`): a gap past a structure stop is an
ordinary market event, not a broken strategy, so it does not fail the run the way an impossible fixed stop
(`ImpossibleStop`) does.

### 7. The trade record

A setup trade carries its **arm time, dip time and stop-anchor time**; the trigger time is its entry signal time.
These are persisted and fed to the content hash **only when present**, so every earlier run re-derives its hash.

### Build order

| Item | Delivers |
|---|---|
| `r6.s2.w1` | The grammar, schema 1.6.0, validation, compile, render, mutate paths, the MCP conventions, this ADR. The engine **refuses** a setup (`BacktestError::UnsupportedExit`, "setup: this engine build does not run setups yet (r6.s2.w2)") and a `StructureStop` (`UnsupportedExit`, "StructureStop (r6.s2.w4)"). |
| `r6.s2.w2` | The state machine (decisions 2, 3, 4 for one side), lead-in stepping, the setup times on a trade. Removes the first refusal. |
| `r6.s2.w3` | Two sides on one position slot; the setup trace persisted (decision 7). |
| `r6.s2.w4` | The structure stop and its 3R target (decision 6), `StopBeyondFill` persisted on the run. Removes the second refusal. |
| `r6.s2.w5` | Cipher v1 as one two-sided document; explain shows the setup state. |

## Consequences

- **Schema 1.6.0 is additive.** The new `setup` field deserializes to its default and `skip_serializing_if` keeps
  every pre-1.6.0 document's canonical serialization **byte-identical**; the `1.5.0 → 1.6.0` step rewrites only the
  `schema_version` string. No frozen golden or content hash moves. The engine fingerprint moves once, with the
  schema string.
- **A setup validates and compiles before the engine can run it.** An agent can write, validate, render and submit
  a setup through the MCP surface in this item; a backtest of one fails with a typed refusal until `w2`. The refusal
  is the honest state, not a silent run of the entry gate alone.
- **The in-app composer gains no setup vocabulary.** Its builder tools (ADR-0012, ADR-0013) do not name `setup`;
  the MCP surface is the agent path for setups (PDR-0042 D3). Only the builder's `StrategyDsl` literal changes
  (`setup: None`).
- **The risk gate's surface changes only by the refusals.** The money-math and backtest-determinism gate
  (`src/adapters/backtest/**`, `src/domain/backtest/**`) sees `setup: None` on `StrategyDsl` literals and the two
  typed refusals in `EngineSession::new` and `ExitPlan::from_strategy` in this item. The step machine, and its
  determinism and audit-trail controls, arrive with `w2`–`w4`.
- **The touch surface** joins the bones registry at the r6.s2 close: `src/domain/dsl/setup.rs` and
  `src/adapters/backtest/setup.rs` (the latter lands with `w2`).
- **The null exception is narrow.** It widens what the write path accepts by one shape (a `null` for an absent
  optional) and changes no meaning, because the parsed value omits the key either way.

## Revisit when

A setup needs a step beyond arm, dip and trigger; arming on a series other than the higher timeframe; or state that
is not a pure function of the stepped bars.
