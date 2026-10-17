// The Backtest Lab (r1.s3.w4) — the rendering half of the spine's journey.
//
// A trader selects a persisted strategy version from the REAL library catalog
// (`commands.libraryOverview` — the Library's own read, no second catalog) and
// presses Run: `commands.runBacktestVersion({ versionId })` executes W3's
// fixed request/response command and this screen renders only the fresh
// `BacktestRunDto` it returns. Every money, price, quantity, R, percent and
// time value is formatted by `ui/src/format/` (r4.s5.w1, A2) and carries the
// DTO's own exact string on the element (`title`/`data-exact`) — display truth
// is still the string itself, one hover away. The ONLY numeric conversions in
// this file are the `// chart geometry` ones (pixel positions and bar widths),
// never for display, derivation or comparison; the screen does no binning, no
// money math, no deltas, no re-backtest, and invents no value the DTO does not
// carry. No sample option, no mock run, no progress percentage, no cancel
// affordance — the command is request/response and r1's target is <5s.
//
// Convention (UnbuiltScreen.tsx is the worked example): one file per screen,
// default export, zero props — the shell mounts it from the route table and
// all state is its own.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { commands } from "../bindings";
import type {
  AcceptedCoachDto,
  BacktestRunDto,
  BusError,
  CoachActionDto,
  CoachDecisionDto,
  CoachSessionDto,
  CompareChildRunDto,
  HistogramDto,
  LibraryOverview,
  LibraryVersion,
  RegimeCellDto,
  SummaryDto,
  TradeRowDto,
  WalkForwardRunDto,
} from "../bindings";
import {
  backtestKey,
  coachKey,
  useActiveOperations,
  walkForwardKey,
} from "../hooks/useActiveOperations";
import type {
  ActiveOperations,
  BusResult,
  OperationRecord,
} from "../hooks/useActiveOperations";
import { PromoteSheet } from "../components/PromoteSheet";
import { Figure } from "../format/Figure";
import {
  decimal,
  foldsHold,
  percentFromFraction,
  price,
  qty,
  rMultiple,
  usdt,
} from "../format/numbers";
import { dateRange, dateUtc, timeUtc } from "../format/time";
import { VersionPicker, buildVersionGroups } from "../components/VersionPicker";
import {
  foldLabel,
  foldMapOf,
  orderRunsWithFolds,
  readWalkForwardRunOnce,
} from "../hooks/useWalkForwardFolds";
import type { FoldMap } from "../hooks/useWalkForwardFolds";
import { useRefetchOnFocus } from "../hooks/useRefetchOnFocus";
import { openSessionDetail, takeRequestedBacktestVersion } from "../paper/handoff";
import { backtestDeepLink } from "../routes";
import { readBacktestSelection, rememberBacktestSelection } from "./backtestSelection";

/** The em dash every null renders — a statement that no value exists, never
 * a zero dressed up as data (the Library's grill A1 rule, applied here). */
const EM_DASH = "—";

/** How much of a run id a row shows: the Library rows' and the picker's own
 * short-id convention (`8`). */
const SHORT_ID_LEN = 8;

/** No folds known yet (the pane has no walk-forward in hand): every run is its
 * own run, no label — never a guess. */
const NO_FOLDS: FoldMap = new Map();

/** The one window into the chart geometry exception: a DTO string becomes a
 * number ONLY to place a pixel. Never displayed, never compared, never
 * re-formatted. */
function num(value: string): number {
  return Number.parseFloat(value); // chart geometry
}

/** A `number` DTO field (Sharpe, a fold bound — the wire refuses them as text)
 * as the exact decimal text the module reads. Serialization, never
 * arithmetic. */
function asText(value: number | null): string | null {
  return value === null ? null : String(value);
}

// ---------------------------------------------------------------------------
// Catalog state (the selector's four explicit states)
// ---------------------------------------------------------------------------

type CatalogState =
  | { kind: "loading" }
  | { kind: "error"; code: string; message: string }
  | { kind: "empty" }
  // The overview rides along (r2.s1.w4 C3): the compare section needs the
  // selected version's `parentId` and `recentRuns`, and the picker renders the
  // catalog's own groups. `ids` is that overview's version ids in catalog order
  // — where the default selection comes from and what a refetched selection is
  // checked against.
  | { kind: "ready"; overview: LibraryOverview; ids: string[] };

/** Every version id the catalog carries, in catalog order. */
function catalogVersionIds(overview: LibraryOverview): string[] {
  const ids: string[] = [];
  for (const strategy of overview.strategies) {
    for (const version of strategy.versions) {
      ids.push(version.id);
    }
  }
  return ids;
}

/** The promote sheet's version label: the picker's own row text for that
 * version, so the sheet and the picker never describe a version differently. */
function versionLabelOf(overview: LibraryOverview, versionId: string): string {
  for (const group of buildVersionGroups(overview.strategies)) {
    const option = group.options.find((candidate) => candidate.id === versionId);
    if (option !== undefined) {
      return option.text;
    }
  }
  return versionId;
}

// ---------------------------------------------------------------------------
// Run state (the Run button is the sole trigger)
// ---------------------------------------------------------------------------

type RunState =
  | { kind: "idle" }
  | { kind: "running" }
  | { kind: "failed"; error: BusError }
  | { kind: "done"; dto: BacktestRunDto };

/**
 * Read one operation record as this screen's own state machine (r1.s4.w3, #141).
 *
 * The record lives above the route, so this is a projection of shared state
 * rather than state of its own — which is exactly what makes a remount show the
 * run that is still going rather than an idle screen with a live command behind
 * it.
 */
function runStateOf(record: OperationRecord | undefined): RunState {
  if (record === undefined) {
    return { kind: "idle" };
  }
  if (record.running) {
    return { kind: "running" };
  }
  const outcome = record.outcome as BusResult<BacktestRunDto> | undefined;
  if (outcome === undefined) {
    return { kind: "idle" };
  }
  return outcome.status === "ok"
    ? { kind: "done", dto: outcome.data }
    : { kind: "failed", error: outcome.error };
}

// ---------------------------------------------------------------------------
// Walk-forward state (r2.s3.w5) — the same per-version operation discipline
// ---------------------------------------------------------------------------

type WalkForwardState =
  | { kind: "idle" }
  | { kind: "running" }
  | { kind: "failed"; error: BusError }
  | { kind: "done"; dto: WalkForwardRunDto };

/** The shared operation record projected into the walk-forward pane's states —
 * the same four shapes the run machine uses, over `WalkForwardRunDto`. */
function walkForwardStateOf(record: OperationRecord | undefined): WalkForwardState {
  if (record === undefined) {
    return { kind: "idle" };
  }
  if (record.running) {
    return { kind: "running" };
  }
  const outcome = record.outcome as BusResult<WalkForwardRunDto> | undefined;
  if (outcome === undefined) {
    return { kind: "idle" };
  }
  return outcome.status === "ok"
    ? { kind: "done", dto: outcome.data }
    : { kind: "failed", error: outcome.error };
}

/** The backend's own walk-forward fold bounds (`K_DEFAULT = 6`, range
 * `2..=12` in `FoldScheme::rolling_oos`) — the guard enforces the same range
 * client-side so a bad count never starts an operation (#212). */
const K_MIN = 2;
const K_MAX = 12;

/**
 * Parse the folds draft the way #212 requires. An empty (or whitespace-only)
 * field keeps today's meaning — "use the backend default" (the input's own
 * contract, and what the pre-#212 behaviour already did) — an exact integer in
 * `[K_MIN, K_MAX]` goes through as `k`, and ANYTHING else is refused without
 * starting an operation. The `/^\d+$/` guard above is what makes the parse
 * exact: it refuses the truncation (`"6.9"`) and the prefix (`"12x"`) that
 * `Number.parseInt` alone would silently accept, so the integer read here is
 * the whole input and never a prefix of it.
 */
export function parseFoldCount(
  raw: string,
): { kind: "default" } | { kind: "ok"; k: number } | { kind: "refused"; message: string } {
  const message = `Folds must be a whole number from ${K_MIN} to ${K_MAX}.`;
  const text = raw.trim();
  if (text === "") {
    return { kind: "default" };
  }
  if (!/^\d+$/.test(text)) {
    return { kind: "refused", message };
  }
  const k = Number.parseInt(text, 10);
  if (!Number.isSafeInteger(k) || k < K_MIN || k > K_MAX) {
    return { kind: "refused", message };
  }
  return { kind: "ok", k };
}

/**
 * The selected version's walk-forward operation: its record (keyed by version,
 * so a remount reattaches and a selection change stops attributing the old
 * version's result to the new one — the Lab's existing staleness rule), the
 * folds input's draft, and the two invocations.
 *
 * `start` is the Walk forward button's only path to the bus; `openFold` starts
 * `getBacktestRun` under the version's BACKTEST key (approved deviation F/G):
 * a fold run is an ordinary persisted run, so opening one is the existing
 * result view answering with it — and the store's own in-flight refusal on
 * that key is the only locking the open needs.
 *
 * #212: `start` guards the fold count FIRST — a non-integer or out-of-range
 * value refuses on the pane's error surface and starts no operation at all.
 */
function useWalkForward(operations: ActiveOperations, versionId: string | null) {
  // The input mirrors the backend's default (K_DEFAULT = 6); an emptied input
  // sends no `k` and the backend applies the same default itself.
  const [folds, setFolds] = useState("6");
  // The #212 refusal message, or null. A refusal is not a bus failure — no
  // operation record exists — so it rides its own state and renders on the
  // same `bt-run-error` surface the failed states use.
  const [refusal, setRefusal] = useState<string | null>(null);
  const state = walkForwardStateOf(
    versionId === null ? undefined : operations.lookup(walkForwardKey(versionId)),
  );

  const setFoldsDraft = useCallback((value: string) => {
    setFolds(value);
    // Editing the draft clears the stale refusal — it described the old draft.
    setRefusal(null);
  }, []);

  const start = useCallback(() => {
    if (versionId === null) return;
    // The toolbar's `busy` lock is rendered state; this is the structural half —
    // a click that beats the re-render still meets a live backtest record here.
    if (operations.lookup(backtestKey(versionId))?.running === true) return;
    const parsed = parseFoldCount(folds);
    if (parsed.kind === "refused") {
      setRefusal(parsed.message);
      return;
    }
    setRefusal(null);
    operations.start(walkForwardKey(versionId), () =>
      commands.runWalkForwardVersion({
        versionId,
        k: parsed.kind === "ok" ? parsed.k : undefined,
      }),
    );
  }, [operations, versionId, folds]);

  const openRun = useCallback(
    (runId: string) => {
      if (versionId === null) return;
      operations.start(backtestKey(versionId), () => commands.getBacktestRun({ runId }));
    },
    [operations, versionId],
  );

  return { state, folds, setFolds: setFoldsDraft, start, openRun, refusal };
}

// ---------------------------------------------------------------------------
// The persisted walk-forward run (#268) — the version's catalogue pointer
// ---------------------------------------------------------------------------
//
// r3.s3's d29: a walk-forward run FINISHES on the server while the client is
// away, and on relaunch its six fold rows and its verdict must still be on
// screen — the run that was STARTED, not a restarted one. The server keeps the
// run, the catalogue the Lab already reads carries the answer to "which run"
// (`LibraryVersion.latestWalkForwardRunId`), and `getWalkForwardRun` reads that
// run back in the SAME DTO `runWalkForwardVersion` answers with. So the Lab
// renders a persisted run through the same `WalkForwardResult` the fresh path
// uses, and starts nothing.

/** The persisted read, in the pane's own four shapes. */
type ReopenedState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "failed"; error: BusError }
  | { kind: "done"; dto: WalkForwardRunDto };

/** A read's state TOGETHER WITH the run id it is the answer for. The tag is what
 * keeps an answer off a version it was not read for (review F1). */
type HeldReopened = {
  /** The run id this state belongs to; `null` while nothing has been read. */
  readonly id: string | null;
  readonly state: ReopenedState;
};

/** The read's own operational failure — the shape every other IPC failure uses. */
function readFailure(): BusError {
  return {
    code: "internal",
    message: "The walk-forward read failed.",
    run_id: null, session_id: null, child_run_id: null,
  };
}

/** The read's state for THIS target. A state held for another run id is not this
 * target's answer, so it reads as the in-flight state — never as a run or a
 * failure belonging to a version the trader has already left (review F1). */
function reopenedStateFor(held: HeldReopened, target: string | null): ReopenedState {
  if (target === null) return { kind: "idle" };
  return held.id === target ? held.state : { kind: "loading" };
}

/**
 * Read the version's persisted walk-forward run, when this session holds no
 * walk-forward operation for it.
 *
 * **The record wins (#268).** The record lookup in `target` is what keeps this
 * read from ever standing in front of the run the trader asked for: a version
 * that has walked forward this session — running or settled, success or failure
 * — has a record, so it is never read for and never re-read. A version whose
 * catalogue row carries no pointer is never read for either, and renders no
 * pane at all.
 *
 * **An answer belongs to the id it was read for (review F1).** The state is held
 * WITH its run id and handed out through `reopenedStateFor`, so the render that
 * lands when the selection moves — before this hook's effect has re-armed — shows
 * the NEW target's in-flight state rather than the run (or the failure) of the
 * version the trader just left. There is no `alive` flag to lean on instead: a
 * tag is strictly stronger, because a late answer arrives carrying its own id and
 * is therefore ignored by a new target's render for as long as the ids differ.
 *
 * **A refetch re-reads (review F2).** The effect keys on the version OBJECT, not
 * on its id — `CompareWithParent`'s rule — so the focus refetch's fresh payload
 * re-reads the pointer, and a read that failed on a dead connection retries
 * instead of leaving a permanent error card. React's StrictMode re-runs a mount's
 * effects with the SAME props objects; `lastRead` deduplicates that, so one mount
 * is one read.
 *
 * **Only the newest read writes (review N1/N2).** An id tag alone still let a
 * slow answer for the version the trader left overwrite the live one — and since
 * that stale id no longer matches the target, the pane then read as loading for
 * good, with no further effect to run. So each started read takes a generation,
 * and `settle` drops anything that is not the current one.
 *
 * **An explicit request outranks both (#316).** `requestedId` is a run the trader
 * asked for by name — the Runs row or the "open run by id" field — and it is read
 * even when this session holds a walk-forward record for the version, because the
 * request names a specific run the record cannot stand in for. A selection change
 * clears it, so the pane never shows one version's requested run under another.
 */
function useReopenedWalkForward(
  operations: ActiveOperations,
  version: LibraryVersion | null,
  requestedId: string | null,
): ReopenedState {
  const target =
    requestedId ??
    (version !== null && operations.lookup(walkForwardKey(version.id)) === undefined
      ? version.latestWalkForwardRunId
      : null);
  const [held, setHeld] = useState<HeldReopened>({
    id: null,
    state: { kind: "idle" },
  });
  /** The inputs the last read was started for — see the StrictMode note above. */
  const lastRead = useRef<{
    version: LibraryVersion | null;
    target: string | null;
  } | null>(null);
  /** Which read the pane is waiting for. Only the NEWEST one may write: a
   * superseded read's answer is dropped whatever it says, the operation store's
   * own rule ("a late result drops instead of speaking for an operation that is
   * not it"). Without it, a slow answer for the version the trader left could
   * overwrite the live one and strand the pane on "Opening…". */
  const generation = useRef(0);

  useEffect(() => {
    if (target === null) {
      generation.current += 1;
      lastRead.current = null;
      // Same-shaped state in, same object out: a refetch of a version with no
      // pointer re-renders nothing.
      setHeld((current) =>
        current.id === null ? current : { id: null, state: { kind: "idle" } },
      );
      return;
    }
    if (lastRead.current?.version === version && lastRead.current.target === target) {
      return;
    }
    // A read starts only where NOTHING is held for this id yet, or where the last
    // one FAILED — F2's retry, and the only same-pointer re-read there is. A
    // loaded run is immutable and is therefore never revalidated: the focus
    // refetch cannot spend a second read on it, and a transient refresh failure
    // cannot evict a run the trader is reading (review P2-B). A read already in
    // flight is not restarted by a refetch either. `held` is read as the render
    // that changed these deps saw it, and is deliberately NOT a dependency: the
    // state this effect writes would otherwise re-run it.
    if (held.id === target && (held.state.kind === "done" || held.state.kind === "loading")) {
      return;
    }
    lastRead.current = { version, target };
    // Claimed only where a read actually starts, so the StrictMode replay above
    // cannot supersede the read it is deduplicating.
    const mine = (generation.current += 1);
    // A result already held for this id is kept while the re-read runs: a
    // refetch may confirm it, and a flash of "Opening…" over a rendered table
    // would be noise. A failure re-arm IS shown, because that is the retry.
    setHeld((current) =>
      current.id === target && current.state.kind === "done"
        ? current
        : { id: target, state: { kind: "loading" } },
    );
    const settle = (next: ReopenedState) => {
      if (generation.current !== mine) return;
      setHeld({ id: target, state: next });
    };
    try {
      commands
        .getWalkForwardRun({ walkForwardRunId: target })
        .then((result) => {
          settle(
            result.status === "ok"
              ? { kind: "done", dto: result.data }
              : { kind: "failed", error: result.error },
          );
        })
        .catch(() => {
          // The IPC call itself failing — the honest error state, never a
          // fabricated pane (the Library's read answers the same way).
          settle({ kind: "failed", error: readFailure() });
        });
    } catch {
      // A binding that is not wired throws BEFORE returning a promise, the same
      // path the operation store guards.
      settle({ kind: "failed", error: readFailure() });
    }
  }, [version, target]);

  return reopenedStateFor(held, target);
}

/** The pane's state: this session's own operation, or the persisted read behind
 * it. `reopened` marks the result as a run that was READ, not produced here. */
type PaneState =
  | { kind: "idle" }
  | { kind: "running" }
  | { kind: "reopening" }
  | { kind: "failed"; error: BusError }
  | { kind: "done"; dto: WalkForwardRunDto; reopened?: boolean };

/** The record wins wherever it exists; the persisted read answers only where no
 * record does (#268). */
function paneStateOf(record: WalkForwardState, reopened: ReopenedState): PaneState {
  if (record.kind === "running") return { kind: "running" };
  if (record.kind === "failed") return { kind: "failed", error: record.error };
  if (record.kind === "done") return { kind: "done", dto: record.dto };
  if (reopened.kind === "loading") return { kind: "reopening" };
  if (reopened.kind === "failed") {
    return { kind: "failed", error: reopened.error };
  }
  if (reopened.kind === "done") {
    return { kind: "done", dto: reopened.dto, reopened: true };
  }
  return { kind: "idle" };
}

// ---------------------------------------------------------------------------
// Coach rail state (r1.s4.w3) — opt-in, beneath the selected persisted run
// ---------------------------------------------------------------------------

/**
 * What the rail's one operation key holds: a turn, a decision on it, or a decision
 * the backend REFUSED.
 *
 * The refusal rides as a settled outcome rather than an error because it is not the
 * turn that failed — the proposal is untouched and still actionable, and dropping
 * it to show a validation message would throw away the thing the trader was editing.
 */
type CoachOutcome =
  | { kind: "turn"; session: CoachSessionDto }
  | { kind: "decision"; decision: CoachDecisionDto }
  | { kind: "refused"; session: CoachSessionDto; error: BusError };

/**
 * The rail's explicit states, and nothing outside them.
 *
 * A `busy` refusal has a state of its own. It used to project onto `running`, which
 * reads correctly — the answer IS coming from whoever holds the key — but the
 * record it lands on has already settled, so nothing further will ever arrive to
 * move it off `running`, and the rail sits on "Asking the coach…" for good. It is a
 * transient state with its own retry instead.
 *
 * A non-`busy` `BusError` is a `failed` state. Its recovery is the generic one: a
 * TYPED coach failure carries the backend's own named recovery, and an operational
 * error has no such thing — but a failure card with no way forward is a dead end,
 * so "try again" is stated rather than left blank.
 */
type RailState =
  | { kind: "idle" }
  | { kind: "running"; note: string | null }
  | { kind: "busy"; note: string; contested: string | null }
  | { kind: "modifying" }
  | { kind: "rejecting" }
  | { kind: "accepting" }
  | { kind: "proposal"; session: CoachSessionDto; refusal: BusError | null }
  | { kind: "failed"; session: CoachSessionDto | null; error: BusError | null }
  | { kind: "completed"; session: CoachSessionDto; accepted: AcceptedCoachDto };

/**
 * The states in which THIS rail has a call of its own still going, so asking again
 * would be a duplicate rather than a retry.
 *
 * `busy` is deliberately NOT one of them. A busy record has already settled — the
 * call it collided with belongs to someone else — so nothing further will arrive to
 * move it along, and asking again is exactly how the trader picks that other
 * result up. Listing it here made the "Check again" button a no-op.
 */
const IN_FLIGHT_KINDS = new Set<RailState["kind"]>([
  "running",
  "modifying",
  "rejecting",
  "accepting",
]);

/** The label each decision runs under, so a remount can still name what is in
 * flight rather than only that something is. */
const DECIDING: Record<string, RailState["kind"]> = {
  modify: "modifying",
  reject: "rejecting",
  accept: "accepting",
};

/** Project the shared operation record into the rail's own state machine. */
function railStateOf(record: OperationRecord | undefined): RailState {
  if (record === undefined) {
    return { kind: "idle" };
  }
  if (record.running) {
    const deciding = record.label === undefined ? undefined : DECIDING[record.label];
    if (deciding === "modifying") return { kind: "modifying" };
    if (deciding === "rejecting") return { kind: "rejecting" };
    if (deciding === "accepting") return { kind: "accepting" };
    return { kind: "running", note: null };
  }
  const outcome = record.outcome as BusResult<CoachOutcome> | undefined;
  if (outcome === undefined) {
    return { kind: "idle" };
  }
  if (outcome.status === "error") {
    return outcome.error.code === "busy"
      ? {
          kind: "busy",
          note: outcome.error.message,
          // The id crosses as a FIELD; the message names it too, but a screen that
          // parses prose to learn an id it must act on is one message rewording
          // away from starting a second billable turn.
          contested: outcome.error.session_id,
        }
      : { kind: "failed", session: null, error: outcome.error };
  }
  // A REFUSED decision keeps its proposal: the backend rejected the edit, not the
  // turn, so the card the trader was working in stays on screen with the reason
  // attached rather than being replaced by it.
  if (outcome.data.kind === "refused") {
    return { kind: "proposal", session: outcome.data.session, refusal: outcome.data.error };
  }
  const session =
    outcome.data.kind === "turn" ? outcome.data.session : outcome.data.decision.session;
  const accepted = outcome.data.kind === "decision" ? outcome.data.decision.accepted : null;
  if (accepted !== null) {
    return { kind: "completed", session, accepted };
  }
  if (session.outcome === "failed") {
    return { kind: "failed", session, error: null };
  }
  if (session.outcome === "proposed") {
    return { kind: "proposal", session, refusal: null };
  }
  // A `pending` session is a claim that has not settled — the honest reading is
  // that the turn is still going, which is what the trader sees.
  return { kind: "running", note: null };
}

/** The session id a settled record names, so a decision can quote it back. */
function sessionIdOf(state: RailState): string | null {
  switch (state.kind) {
    case "proposal":
    case "completed":
      return state.session.sessionId;
    case "failed":
      return state.session?.sessionId ?? null;
    default:
      return null;
  }
}

// ---------------------------------------------------------------------------
// Fixed display constants (pinned by the spec, asserted by the tests)
// ---------------------------------------------------------------------------

/** The regime labels in the DTO's fixed order — hue fixed by slot, sign by bar
 * position, never a second hue. */
const REGIME_LABELS: Record<string, string> = {
  trending_up: "Trending up",
  trending_down: "Trending down",
  ranging: "Ranging",
  unknown: "Unknown",
};

/** The 19 trade columns in the DTO's own field order. */
const TRADE_FIELDS = [
  "direction", "qty", "entryPrice", "exitPrice", "entrySignalTime", "entryFillTime",
  "exitSignalTime", "exitFillTime", "feesTotal", "fundingTotal", "slippageTotal",
  "realizedPnl", "realizedR", "mfeR", "maeR", "exitReason", "source", "regime",
  "stopPrice",
] as const;

// ---------------------------------------------------------------------------
// Screen
// ---------------------------------------------------------------------------

export default function BacktestLabScreen() {
  const [catalog, setCatalog] = useState<CatalogState>({ kind: "loading" });
  // #315: the selection outlives this screen — it re-mounts on every navigation
  // and whenever the gate takes the app down for a server restart. `null` until
  // the trader has chosen; the catalog read then reconciles it.
  const [selectedId, setSelectedId] = useState<string | null>(() => readBacktestSelection());
  const [promoting, setPromoting] = useState(false);
  /** The walk-forward a trader asked for by name (#316) — the Runs row, the
   * "open run by id" field, or the Library's certification deep link — read even
   * when this session holds a walk-forward record for the version, because the
   * request names a run the record cannot stand in for. A selection change
   * clears it (the deep link re-sets it in the same tick it selects). */
  const [requestedWf, setRequestedWf] = useState<string | null>(null);
  /** The "open run by id" draft, and the refusal it earned (#316/#331). */
  const [runIdDraft, setRunIdDraft] = useState("");
  const [openByIdError, setOpenByIdError] = useState<BusError | null>(null);
  /** The selection-request generation (#269/#316/#331): each selection read —
   * the deep-link take, every openById submit — bumps it and captures its
   * value, and every explicit selection (`selectVersion`) bumps it too. A read
   * applies its answer only while its captured generation is still current, so
   * a newer request or a later pick always supersedes it. */
  const selectionGeneration = useRef(0);
  /** Every active operation, held above the route (#141). The Lab READS it. */
  const operations = useActiveOperations();

  /**
   * Read the catalog and project it into the selector's state.
   *
   * Extracted from the mount effect because an ACCEPT mints a child version that
   * the once-fetched catalog cannot contain: selecting that child without
   * refetching sets the selector to an id no option carries, which renders it
   * blank, drops the rail and the result, and points Run at a version the trader
   * cannot see.
   *
   * `alive` is the caller's — the mount effect passes its own cleanup flag so a
   * response arriving after unmount sets no state.
   */
  const loadCatalog = useCallback(
    async (alive: () => boolean = () => true): Promise<Set<string>> => {
      try {
        const result = await commands.libraryOverview();
        if (!alive()) return new Set();
        if (result.status === "ok") {
          const ids = catalogVersionIds(result.data);
          setCatalog(
            ids.length === 0
              ? { kind: "empty" }
              : { kind: "ready", overview: result.data, ids },
          );
          const known = new Set(ids);
          // C2 (r2.s1.w4) + #315: a refetch keeps the selection ONLY while the
          // refreshed catalog still carries it — a vanished version falls back
          // to the default rather than rendering an empty picker. The
          // REMEMBERED id is not touched here: the catalog owns validity, and
          // the next remount re-reconciles against whatever it carries then.
          setSelectedId((current) =>
            current !== null && !known.has(current) ? null : current,
          );
          return known;
        }
        setCatalog({
          kind: "error",
          code: result.error.code,
          message: result.error.message,
        });
      } catch {
        // The IPC call itself failing (no app handle under a non-Tauri
        // preview) — the honest error state, never fabricated options.
        if (alive()) {
          setCatalog({ kind: "error", code: "internal", message: "The library read failed." });
        }
      }
      return new Set();
    },
    [],
  );

  useEffect(() => {
    let alive = true;
    void loadCatalog(() => alive);
    return () => {
      alive = false;
    };
  }, [loadCatalog]);

  // r3.s4.w5: the stale sheet's only action — "Re-run walk-forward" — asks for
  // THIS version and lands here. Consumed on mount (the navigation mounts this
  // screen) and on the next hash change (the screen was already open).
  useEffect(() => {
    const take = () => {
      const requested = takeRequestedBacktestVersion();
      if (requested !== null) selectVersion(requested);
    };
    take();
    window.addEventListener("hashchange", take);
    return () => window.removeEventListener("hashchange", take);
  }, []);

  // #269: the Library's certification line deep-links to `#/backtest?wf=<id>` —
  // the Lab selects that run's version and opens the run. A READ, like the Runs
  // rows: nothing on this path starts a run. Consumed on mount (the navigation
  // mounts this screen) and on the next hash change (the screen was already
  // open), and replaced with the plain `#/backtest` BEFORE the read so a
  // remount (a reconnect) cannot replay it over the remembered selection
  // (#315). A read a later link or pick supersedes never applies its result.
  // The read is shared with the fold labels' cache, so the run the fragment
  // names costs one `getWalkForwardRun` across both consumers.
  useEffect(() => {
    const take = () => {
      const link = backtestDeepLink(window.location.hash);
      if (link === null) return;
      // Consume the fragment before the async read (see the comment above).
      window.history.replaceState(null, "", "#/backtest");
      const generation = ++selectionGeneration.current;
      void (async () => {
        try {
          const result = await readWalkForwardRunOnce(link.wf);
          if (generation !== selectionGeneration.current) return;
          if (result.status !== "ok") {
            setOpenByIdError(result.error);
            return;
          }
          setOpenByIdError(null);
          selectVersion(result.data.versionId);
          setRequestedWf(result.data.walkForwardRunId);
        } catch {
          if (generation !== selectionGeneration.current) return;
          setOpenByIdError(readFailure());
        }
      })();
    };
    take();
    window.addEventListener("hashchange", take);
    return () => {
      window.removeEventListener("hashchange", take);
      // Unmount supersedes every selection read still in flight (the deep
      // link's, openById's): nothing from an unmounted Lab may select or start
      // a run.
      supersedeSelectionReads();
    };
  }, []);

  // C2 (r2.s1.w4): versions and runs written by `pulse mcp` surface on window
  // focus — DOM events only, throttled inside the hook. The refetch is the
  // same `loadCatalog` the mount and the accept paths use, so the selection
  // repair above applies to it too.
  useRefetchOnFocus(
    useCallback(() => {
      void loadCatalog();
    }, [loadCatalog]),
  );

  const ids = catalog.kind === "ready" ? catalog.ids : [];
  const currentId = selectedId ?? ids[0] ?? null;
  /** This version's operation, whoever started it and whenever. Keyed by version,
   * so a result can never be attributed to a version it was not run for — the
   * property the old bumped token approximated. */
  const run = runStateOf(
    currentId === null ? undefined : operations.lookup(backtestKey(currentId)),
  );

  /** This version's walk-forward operation (r2.s3.w5), same per-version keying —
   * changing the selector stops rendering the previous version's pane. */
  const wf = useWalkForward(operations, currentId);

  /** The selector and both actions lock while EITHER operation is in flight:
   * a walk-forward is K engine runs, so it holds Run's latch discipline. */
  const busy = run.kind === "running" || wf.state.kind === "running";

  /** This version's own record — the compare section needs its
   * `parentId` and `recentRuns`, which the selector's flattened options drop. */
  const selectedVersion =
    catalog.kind === "ready"
      ? (catalog.overview.strategies
          .flatMap((strategy) => strategy.versions)
          .find((version) => version.id === currentId) ?? null)
      : null;

  /** #268: the version's persisted walk-forward run, read when this session has
   * no record of its own — what makes a finished run openable after a relaunch.
   * #316: an explicitly requested run (#316) is read instead of the pointer, and
   * outranks this session's record — the trader named that run. */
  const reopened = useReopenedWalkForward(operations, selectedVersion, requestedWf);
  const pane =
    requestedWf !== null
      ? paneStateOf({ kind: "idle" }, reopened)
      : paneStateOf(wf.state, reopened);

  /** An explicit action that changes what the Lab shows — a pick, an accept, a
   * run opened from the Runs list or the fold table — supersedes every
   * selection read still in flight (#269/#316/#331): their captured generation
   * goes stale, so their answers cannot move the screen off what the trader
   * chose. */
  function supersedeSelectionReads() {
    selectionGeneration.current += 1;
  }

  /** Selecting a version shows THAT version's operation. Nothing is cleared and
   * nothing is cancelled: each version's record is its own, so a run still going
   * for the previous selection stays going, and coming back to it shows it. The
   * coach rail is per-run, so it closes with the selection.
   *
   * Every path that SELECTS — the picker's change, the accept's child select,
   * the stale sheet's request — lands here, so every one of them REMEMBERS the
   * id and a remount re-selects the same version (#315). The catalog still owns
   * whether the id is real: `loadCatalog`'s reconcile drops one it no longer
   * carries, and the next remount re-reconciles against whatever it carries then. */
  function selectVersion(id: string) {
    // A pick supersedes every selection read still in flight (see above).
    supersedeSelectionReads();
    rememberBacktestSelection(id);
    setSelectedId(id);
    // An explicitly requested run belongs to the version selected when it was
    // asked for; a new selection drops it (the deep link re-sets it in the same
    // tick it selects the run's version).
    setRequestedWf(null);
  }

  /**
   * Select the child an accept just minted — after refetching the catalog.
   *
   * The child did not exist when the catalog was read, so selecting it against the
   * stale list sets the selector to an id no option carries: the select renders
   * blank, this screen's projection finds no operation for it, and Run targets a
   * version the trader cannot see. Refetch first, and only select what the fresh
   * catalog actually contains.
   */
  async function onSelectChild(versionId: string) {
    // The accept's child select is an explicit action: it supersedes every
    // selection read still in flight before the catalog refetch it awaits.
    supersedeSelectionReads();
    const known = await loadCatalog();
    if (known.has(versionId)) {
      selectVersion(versionId);
    }
  }

  /** The screen's ONLY invocation of the backtest command — the Run button's
   * click handler. No mount effect reaches this.
   *
   * The store refuses a key already in flight before the bus is called, and the
   * backend's latch refuses it again if reached, so a double-click that beats
   * this re-render still starts exactly one run. The walk-forward operation
   * holds the same latch: `busy` covers it. */
  function onRun() {
    if (currentId === null || busy) return;
    operations.start(backtestKey(currentId), () =>
      commands.runBacktestVersion({ versionId: currentId }),
    );
  }

  /**
   * Open a run by id (#331/#316): a backtest run first — `getBacktestRun` opens
   * a walk-forward fold's id as an ordinary run by the server's own contract —
   * then a walk-forward run when the backtest read refuses `not_found`. Both are
   * READS; nothing on this path starts a run. When both refuse, the backtest
   * read's message is the one shown: it is the read that asked first, and its
   * `child_run_id` carries the id the trader typed. Each submit is a selection
   * request of its own: it captures the selection generation, and a newer
   * submit, or a later pick by the trader, makes every remaining apply on its
   * path a no-op (#269/#316).
   */
  async function openById() {
    const runId = runIdDraft.trim();
    if (runId === "") return;
    // Every submit is a selection request of its own: it supersedes earlier
    // submits, and any newer pick supersedes it (see the ref's comment).
    const generation = ++selectionGeneration.current;
    setOpenByIdError(null);
    let backtest: BusResult<BacktestRunDto>;
    try {
      backtest = await commands.getBacktestRun({ runId });
    } catch {
      if (generation !== selectionGeneration.current) return;
      setOpenByIdError(readFailure());
      return;
    }
    if (generation !== selectionGeneration.current) return;
    if (backtest.status === "ok") {
      const dto = backtest.data;
      selectVersion(dto.strategyVersionId);
      // The store holds exactly the answer just read: no second read, and the
      // run command is nowhere near this path.
      operations.start(backtestKey(dto.strategyVersionId), () => Promise.resolve(backtest));
      return;
    }
    if (backtest.error.code !== "not_found") {
      setOpenByIdError(backtest.error);
      return;
    }
    let walkForward: BusResult<WalkForwardRunDto>;
    try {
      walkForward = await readWalkForwardRunOnce(runId);
    } catch {
      if (generation !== selectionGeneration.current) return;
      setOpenByIdError(readFailure());
      return;
    }
    if (generation !== selectionGeneration.current) return;
    if (walkForward.status === "ok") {
      selectVersion(walkForward.data.versionId);
      setRequestedWf(walkForward.data.walkForwardRunId);
      return;
    }
    setOpenByIdError(backtest.error);
  }

  // --- the coach rail (r1.s4.w3) ------------------------------------------
  //
  // Keyed by the persisted RUN, not by the session: one run has one rail, and
  // keying it this way is what lets a remount find the rail again — and read the
  // session id back out of the record — without the screen persisting anything
  // itself.
  const runId = run.kind === "done" ? run.dto.runId : null;
  const rail = railStateOf(runId === null ? undefined : operations.lookup(coachKey(runId)));

  /**
   * "Ask the coach": the DESKTOP mints the session id, once, here.
   *
   * A settled rail is cleared FIRST. Without that the guard below rejects every
   * ask after the first outcome, which made the failure card's own recovery text
   * unactionable — it told the trader to ask again on a rail that could not.
   *
   * Every SETTLED session gets a fresh id, not only an `interrupted` one. W1's
   * claim semantics make a settled row terminal whatever settled it, so asking
   * again under its id meets the recorded outcome instead of starting a turn — the
   * previous id is reusable only while its claim is still PENDING, which is the
   * reload case the backend resolves for itself.
   */
  function onAskCoach() {
    if (runId === null || IN_FLIGHT_KINDS.has(rail.kind)) return;
    // A busy rail is DEFERRING to another turn. Checking again has to reload THAT
    // session — a fresh id would ask a new question and bill for it, which is the
    // opposite of what the button offers. Every other settled state is terminal and
    // takes a fresh id.
    const sessionId =
      rail.kind === "busy" && rail.contested !== null ? rail.contested : crypto.randomUUID();
    if (rail.kind !== "idle") {
      operations.clear(coachKey(runId));
    }
    operations.start(
      coachKey(runId),
      async () => {
        const result = await commands.coachTurn({ runId, sessionId });
        return result.status === "ok"
          ? { status: "ok" as const, data: { kind: "turn" as const, session: result.data } }
          : result;
      },
      "asking",
    );
  }

  /** Modify, reject or accept — always quoting back the session id the record
   * already holds, never a fresh one. */
  function onDecide(action: CoachActionDto) {
    const sessionId = sessionIdOf(rail);
    if (runId === null || sessionId === null) return;
    // The proposal as it stands, captured before the call: a REFUSED decision
    // returns it unchanged rather than replacing the card with the refusal.
    const current = rail.kind === "proposal" ? rail.session : null;
    operations.start(
      coachKey(runId),
      async () => {
        const result = await commands.coachDecide({ sessionId, action });
        if (result.status === "error" && current !== null && result.error.code !== "busy") {
          return {
            status: "ok" as const,
            data: { kind: "refused" as const, session: current, error: result.error },
          };
        }
        return result.status === "ok"
          ? { status: "ok" as const, data: { kind: "decision" as const, decision: result.data } }
          : result;
      },
      action.kind,
    );
  }

  return (
    <div className="bt-lab">
      <div className="ctool">
        <div className="ctool-left">
          <h1 className="ctool-title">Backtest Lab</h1>
          <span className="ctool-count">
            Select a persisted strategy version and run it against the persisted
            BTCUSDT candle snapshots
          </span>
        </div>
      </div>

      {catalog.kind === "loading" && <div className="bt-state">Loading the strategy library…</div>}

      {catalog.kind === "error" && (
        <div className="bt-state bt-error" role="alert">
          <span className="mono">{catalog.code}</span> {catalog.message}
        </div>
      )}

      {catalog.kind === "empty" && (
        // A designed first run, not a void — one line naming the single next
        // action (the Library's G4 pattern).
        <div className="bt-state bt-empty">
          <h2 className="bt-empty-title">No strategies yet</h2>
          <p className="bt-empty-body">
            Describe a strategy in the Strategy Designer — its versions will
            appear here to backtest.
          </p>
        </div>
      )}

      {catalog.kind === "ready" && (
        <div className="bt-toolbar">
          <label className="bt-select-label" htmlFor="bt-version">
            Strategy version
          </label>
          <VersionPicker
            strategies={catalog.overview.strategies}
            value={currentId}
            onChange={selectVersion}
            disabled={busy}
          />
          <button
            type="button"
            className="bt-run btn-prim"
            onClick={onRun}
            disabled={busy || currentId === null}
          >
            {run.kind === "running" ? "Running…" : "Run backtest"}
          </button>
          {/* r2.s3.w5: the acceptance gate on the same selector — K folds of
              rolling out-of-sample windows, K defaulted to the backend's own
              default and bounded to its 2..=12 range. Same latch discipline
              as Run: in flight, everything here is disabled. */}
          <label className="bt-select-label" htmlFor="bt-folds">
            Folds
          </label>
          <input
            id="bt-folds"
            className="bt-folds mono"
            type="number"
            min={2}
            max={12}
            value={wf.folds}
            onChange={(event) => wf.setFolds(event.target.value)}
            disabled={busy}
          />
          <button
            type="button"
            className="bt-walk btn-sec"
            onClick={() => {
              // A fresh walk-forward is the run the pane must show: a request
              // for an older one is done with.
              setRequestedWf(null);
              wf.start();
            }}
            disabled={busy || currentId === null}
          >
            {wf.state.kind === "running" ? "Walking forward…" : "Walk forward"}
          </button>
          {/* No percentage, no cancel — both commands are request/response. */}
        </div>
      )}

      {/* r4.s5.w3 (#331, #316): the version's runs are reachable beside the
          picker — its own backtests and its latest walk-forward run, each row
          opening the persisted run it names (a READ: `getBacktestRun` /
          `getWalkForwardRun`, never a new run). Below the toolbar rather than
          inside it: the toolbar is one horizontal row, and a list needs a
          full-width block of its own (A3's rule read the same way). The field
          opens ANY backtest or walk-forward run id — the Lab selects that run's
          version and shows it. */}
      {catalog.kind === "ready" && selectedVersion !== null && (
        <section className="bt-section bt-runs" aria-label="Version runs">
          <h3 className="bt-h">Runs</h3>
          <VersionRuns
            version={selectedVersion}
            busy={busy}
            folds={pane.kind === "done" ? foldMapOf(pane.dto) : NO_FOLDS}
            onOpenRun={(runId) => {
              supersedeSelectionReads();
              wf.openRun(runId);
            }}
            onOpenWalkForward={(walkForwardRunId) => {
              supersedeSelectionReads();
              setRequestedWf(walkForwardRunId);
            }}
          />
          <div className="bt-open-id">
            <label className="bt-select-label" htmlFor="bt-open-run">
              Open run by id
            </label>
            <input
              id="bt-open-run"
              className="bt-select bt-open-input mono"
              type="text"
              value={runIdDraft}
              placeholder="backtest or walk-forward run id"
              autoComplete="off"
              spellCheck={false}
              disabled={busy}
              onChange={(event) => {
                setRunIdDraft(event.target.value);
                // Editing the draft clears the refusal — it described the old id.
                setOpenByIdError(null);
              }}
            />
            <button
              type="button"
              className="bt-open-go btn-sec"
              onClick={() => void openById()}
              disabled={busy || runIdDraft.trim() === ""}
            >
              Open
            </button>
          </div>
          {openByIdError !== null && (
            <div className="bt-run-error" role="alert">
              <div>
                <span className="mono">{openByIdError.code}</span> {openByIdError.message}
              </div>
            </div>
          )}
        </section>
      )}

      {/* C3 (r2.s1.w4): a child version's latest run beside its parent's — for
          ANY child, not only a coach accept. Shown only when there is a parent
          to compare with and at least one run to compare. */}
      {selectedVersion !== null &&
        selectedVersion.parentId !== null &&
        (run.kind === "done" || selectedVersion.recentRuns.length > 0) && (
          <CompareWithParent version={selectedVersion} run={run} />
        )}

      {/* r2.s3.w5: the walk-forward result pane sits ABOVE the run result —
          a fold row's "open run" lands in the ordinary result view below it.
          #268: it renders this session's own run, or the version's persisted
          one read back by id. */}
      <WalkForwardPane
        state={pane}
        onOpenFold={(runId) => {
          supersedeSelectionReads();
          wf.openRun(runId);
        }}
        onPromote={selectedVersion === null ? null : () => setPromoting(true)}
      />

      {promoting && selectedVersion !== null && catalog.kind === "ready" && (
        <PromoteSheet
          target={{
            version: selectedVersion,
            strategyName: selectedVersion.dsl.name,
            versionLabel: versionLabelOf(catalog.overview, selectedVersion.id),
          }}
          onClose={() => setPromoting(false)}
          onPromoted={openSessionDetail}
        />
      )}

      {/* #212: a refused fold count starts no operation — the refusal renders on
          the same error surface the failed states use, naming the allowed
          range. It clears on the next draft edit or a valid start. */}
      {wf.refusal !== null && (
        <div className="bt-run-error" role="alert">
          <div>{wf.refusal}</div>
        </div>
      )}

      {run.kind === "running" && (
        <div className="bt-state dim" role="status">
          Running the backtest…
        </div>
      )}

      {run.kind === "failed" && (
        <div className="bt-run-error" role="alert">
          <div>
            <span className="mono">{run.error.code}</span> {run.error.message}
          </div>
          {/* The structured saved-run truth: the DTO's `run_id` FIELD, read
              directly — never parsed out of the message. */}
          {run.error.run_id !== null && (
            <p className="bt-saved-run mono">Saved as run {run.error.run_id}</p>
          )}
        </div>
      )}

      {run.kind === "done" && (
        <div className="bt-result" key={run.dto.runId}>
          <ProvenanceBand dto={run.dto} />
          <KpiTiles dto={run.dto} />
          {/* The coach rail sits directly beneath the run it coaches on, and is
              hidden entirely until the trader asks — opt-in, never a pane that
              appears with a result nobody requested. */}
          <CoachRail
            state={rail}
            onAsk={onAskCoach}
            onRun={onRun}
            onDecide={onDecide}
            onSelectChild={(id) => void onSelectChild(id)}
          />
          {/* #417 (PDR-0038 option A): a withheld run has no outcome series to
              draw — the server sends empty `equity`/`regimes` and null
              histograms rather than a curve over the trades that remain. Each
              section below renders only when it has data, so the Lab shows the
              withheld sentence (in KpiTiles) and the trades that cleared the
              fence, and never an empty frame standing in for a total. */}
          {!run.dto.totalsWithheld && (
            <>
              <EquityChart
                points={run.dto.equity}
                startingEquity={run.dto.startingEquity}
              />
              <RegimeBars cells={run.dto.regimes} />
            </>
          )}
          {run.dto.mfe !== null && (
            <Histogram title="MFE (R)" className="bt-mfe" hist={run.dto.mfe} />
          )}
          {run.dto.mae !== null && (
            <Histogram title="MAE (R)" className="bt-mae" hist={run.dto.mae} />
          )}
          <TradeTable trades={run.dto.trades} runId={run.dto.runId} dto={run.dto} />
          <ChartTwins dto={run.dto} />
        </div>
      )}
    </div>
  );
}

// ---------------------------------------------------------------------------
// The version's runs (#331, #316) — the persisted runs the trader can open
// ---------------------------------------------------------------------------
//
// Every row names a run the SERVER holds and opens it by id: an own backtest
// through `getBacktestRun` (the same read the fold table already uses) or the
// version's latest walk-forward through its own persisted read — never a new
// run. The list is the version's `recentRuns` plus the catalogue pointer,
// rendered in the Library's run-row shape, and it renders no row the payload
// does not carry.

function VersionRuns({
  version,
  busy,
  folds,
  onOpenRun,
  onOpenWalkForward,
}: {
  version: LibraryVersion;
  busy: boolean;
  folds: FoldMap;
  onOpenRun: (runId: string) => void;
  onOpenWalkForward: (walkForwardRunId: string) => void;
}) {
  const pointer = version.latestWalkForwardRunId;
  const rows = orderRunsWithFolds(version.recentRuns, folds);
  return (
    <>
      {rows.length === 0 ? (
        <div className="bt-none dim">None yet.</div>
      ) : (
        <div className="bt-list bt-runs-list">
          {rows.map(({ run, fold }) => (
            <div key={run.id} className="bt-row">
              <span className="bt-id mono">{run.id.slice(0, SHORT_ID_LEN)}</span>
              {/* #330: a fold of the walk-forward the pane read wears its own
                  label; every other row is an own run and wears none. */}
              {fold !== null && (
                <span className="bt-fold-label mono">{foldLabel(fold)}</span>
              )}
              <span className="bt-range">
                <Figure exact={run.createdAt}>{dateUtc(run.createdAt)}</Figure>
              </span>
              <span className="bt-spacer" />
              <span className={`mono ${run.expectancy.startsWith("-") ? "bear" : "bull"}`}>
                <Figure exact={run.expectancy}>{rMultiple(run.expectancy)}</Figure>
              </span>
              <span className="mono dim">{run.trades}t</span>
              <button
                type="button"
                className="bt-run-open btn-sec-sm"
                onClick={() => onOpenRun(run.id)}
                disabled={busy}
              >
                open run <span className="mono">{run.id.slice(0, SHORT_ID_LEN)}</span>
              </button>
            </div>
          ))}
        </div>
      )}
      {/* #316: the version's latest walk-forward run — the catalogue's own
          pointer — opens here like any other run: `getWalkForwardRun` reads it
          back with its folds and verdict, and starts nothing. */}
      {pointer !== null && (
        <div className="bt-row bt-runs-wf">
          <span className="bt-id mono">{pointer.slice(0, SHORT_ID_LEN)}</span>
          <span className="bt-wf-label mono">walk-forward</span>
          <span className="bt-spacer" />
          <button
            type="button"
            className="bt-run-open btn-sec-sm"
            onClick={() => onOpenWalkForward(pointer)}
            disabled={busy}
          >
            open walk-forward
          </button>
        </div>
      )}
    </>
  );
}

// ---------------------------------------------------------------------------
// The walk-forward pane (r2.s3.w5) — verdict under the rule the run names,
// one row per fold, each fold opening as an ordinary run
// ---------------------------------------------------------------------------
//
// Every string here is the DTO's own: the scheme and rule render exactly as the
// run recorded them (`rolling-oos/v1`, `wf-v1` today — never a hard-coded
// label, so a `v2` scheme lands without a screen change), bounds are the DTO's
// numbers, and the verdict words are the recorded tallies. The pane computes
// no verdict of its own.

/** The pane's visible states — idle renders nothing at all. `reopening` is the
 * persisted read's own in-flight state, never the run's. */
function WalkForwardPane({
  state,
  onOpenFold,
  onPromote,
}: {
  state: PaneState;
  onOpenFold: (runId: string) => void;
  onPromote: (() => void) | null;
}) {
  if (state.kind === "running") {
    return (
      <div className="bt-state dim" role="status">
        Running the walk-forward…
      </div>
    );
  }
  if (state.kind === "reopening") {
    // A read, not a run: the wording may not claim an engine is working.
    return (
      <div className="bt-state dim" role="status">
        Opening the persisted walk-forward run…
      </div>
    );
  }
  if (state.kind === "failed") {
    // The same shape the run error renders: code family + the backend's own
    // message. Walk-forward errors never carry `run_id` (that field names a
    // backtest run id), so no saved-run line exists here.
    return (
      <div className="bt-run-error" role="alert">
        <div>
          <span className="mono">{state.error.code}</span> {state.error.message}
        </div>
      </div>
    );
  }
  if (state.kind === "done") {
    return (
      <>
        {/* #268: a run READ off the server is provenance the fresh path cannot
            have — say which it is, so no one reads this pane as work they just
            started. The result below is the same component either way. */}
        {state.reopened === true && (
          <p className="bt-wf-origin dim">
            Persisted run, reopened by id — nothing was re-run.
          </p>
        )}
        <WalkForwardResult dto={state.dto} onOpenFold={onOpenFold} onPromote={onPromote} />
      </>
    );
  }
  return null;
}

/** The recorded run: provenance fields, the `wf-v1` verdict line, the fold table. */
function WalkForwardResult({
  dto,
  onOpenFold,
  onPromote,
}: {
  dto: WalkForwardRunDto;
  onOpenFold: (runId: string) => void;
  onPromote: (() => void) | null;
}) {
  const verdict = dto.verdict;
  return (
    <section className="bt-section bt-walkforward" aria-label="Walk-forward result">
      <h3 className="bt-h">Walk-forward</h3>
      <div className="bt-wf-meta">
        {/* #316's own comment: the pane names the VERSION it ran on, the way
            the backtest result band already does — verifying which version a
            walk-forward ran on must not need the Library's certification line. */}
        <Field label="version">{dto.versionId}</Field>
        <Field label="scheme">{dto.scheme}</Field>
        <Field label="rule">{dto.rule}</Field>
        <Field label="folds">{dto.k}</Field>
        <Field label="span">
          <Figure exact={`${dto.spanFrom} → ${dto.spanTo}`}>
            {dateRange(dto.spanFrom, dto.spanTo)}
          </Figure>
        </Field>
        {/* `from_defaulted` is stated either way — an explicit bound and a
            defaulted one are different provenance, both worth seeing. */}
        <Field label="from">
          {dto.fromDefaulted ? "defaulted (first fully-warm bar)" : "requested"}
        </Field>
        <Field label="engine">{dto.engineFingerprint}</Field>
        <Field label="run">{dto.walkForwardRunId}</Field>
      </div>
      <p className={`bt-wf-verdict ${verdict.pass ? "bt-wf-pass" : "bt-wf-fail"}`}>
        <span className="bt-wf-verdict-word">{verdict.pass ? "PASS" : "FAIL"}</span>{" "}
        <span className="mono">{foldsHold(verdict.foldsHolding, dto.k, verdict.foldsRequired)}</span>{" "}
        · pooled lower bound{" "}
        <span className="mono">
          <Figure exact={asText(verdict.pooled.lowerBound)}>
            {decimal(asText(verdict.pooled.lowerBound), 2)}
          </Figure>
        </span>
        {/* r3.s4.w5: spec §2's second entry point — a passing verdict offers
            the promotion right here. The sheet decides the variant from the
            server; this button only opens it. */}
        {verdict.pass && onPromote !== null && (
          <button className="btn-sec-sm bt-wf-promote" onClick={onPromote}>
            Promote to paper
          </button>
        )}
      </p>
      <div className="bt-table-scroll" role="region" aria-label="Fold rows" tabIndex={0}>
        <table className="bt-trades bt-wf-folds">
          <thead>
            <tr>
              <th scope="col">fold</th>
              <th scope="col">window from</th>
              <th scope="col">window to</th>
              <th scope="col">trades</th>
              <th scope="col">lower bound</th>
              <th scope="col">holds</th>
              <th scope="col">expectancy</th>
              <th scope="col">win rate</th>
              <th scope="col">run</th>
            </tr>
          </thead>
          <tbody>
            {dto.folds.map((fold) => (
              <tr key={fold.index}>
                <td className="mono">{fold.index}</td>
                <td className="mono">
                  <Figure exact={fold.windowFrom}>{timeUtc(fold.windowFrom)}</Figure>
                </td>
                <td className="mono">
                  <Figure exact={fold.windowTo}>{timeUtc(fold.windowTo)}</Figure>
                </td>
                {/* The verdict's own count, not the fold run's (`fold.trades`):
                    this is the per-fold VERDICT table, and bindings document `n`
                    as "the trades the `wf-v1` verdict saw" (equal in production). */}
                <td className="mono">{fold.n}</td>
                <td className="mono">
                  <Figure exact={asText(fold.lowerBound)}>
                    {decimal(asText(fold.lowerBound), 2)}
                  </Figure>
                </td>
                <td>{fold.holds ? "yes" : "no"}</td>
                <td className="mono">
                  <Figure exact={fold.expectancy}>
                    {usdt(fold.expectancy, { signed: true })}
                  </Figure>
                </td>
                <td className="mono">
                  <Figure exact={fold.winRate}>{percentFromFraction(fold.winRate)}</Figure>
                </td>
                <td>
                  <button
                    type="button"
                    className="bt-wf-open"
                    onClick={() => onOpenFold(fold.backtestRunId)}
                  >
                    open run <span className="mono">{fold.backtestRunId}</span>
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Provenance band
// ---------------------------------------------------------------------------

/** One label/value pair in the provenance band's grid. */
function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="bt-field">
      <span className="bt-field-label">{label}</span>
      <span className="bt-field-value mono">{children}</span>
    </div>
  );
}

function ProvenanceBand({ dto }: { dto: BacktestRunDto }) {
  return (
    <section className="bt-section bt-provenance" aria-label="Run provenance">
      <Field label="pair">{dto.pair}</Field>
      <Field label="primary">
        {dto.primaryTimeframe} · {dto.primaryDataVersion}
      </Field>
      {/* r3.s2.w4 (#219): the HTF row is gated on the STRATEGY, not on the
          recorded snapshot. Every run records an H4 selection — the resolver
          supplies the application default even for a strategy with no `htf`
          operand — so `htfTimeframe` alone would claim a higher timeframe the
          strategy never read. `usesHtf` is the version's compiled answer. The
          em-dash arm still covers a strategy that reads the HTF while the
          recorded timeframe is absent (a pre-0006 row). */}
      {dto.usesHtf && (
        <Field label="htf">
          {dto.htfTimeframe !== null ? `${dto.htfTimeframe} · ${dto.htfDataVersion}` : EM_DASH}
        </Field>
      )}
      {/* r3.s2.w4: the daily row, gated on the RECORDED snapshot instead — a
          strategy either read D1 (which pins the version it read) or it did
          not. A pre-0017 row records none, so no row is the honest read. The
          timeframe is fixed (`1d`), so the version is the whole value. */}
      {dto.d1DataVersion != null && <Field label="d1">{dto.d1DataVersion}</Field>}
      <Field label="date range">
        <Figure exact={`${dto.firstOpenTimeMs} → ${dto.lastCloseTimeMs}`}>
          {dateRange(dto.firstOpenTimeMs, dto.lastCloseTimeMs)}
        </Figure>
      </Field>
      <Field label="starting equity">
        <Figure exact={dto.startingEquity}>{usdt(dto.startingEquity)}</Figure>
      </Field>
      <Field label="taker fee">{dto.takerFeeBps} bps</Field>
      <Field label="slippage">{dto.slippageBps} bps</Field>
      <Field label="funding">{dto.funding}</Field>
      {/* r3.s1.w4 (#142): the recorded symbol filters, or the honest "not
          recorded" for a pre-0015 run — never a guessed constant. The guard
          reads loose (`!= null`) so a fixture or cached payload that predates
          the field degrades to "not recorded" instead of a TypeError. */}
      <Field label="filters">
        {dto.symbolFilters != null ? (
          <>
            lot_step{" "}
            <Figure exact={dto.symbolFilters.lotStep}>
              {qty(dto.symbolFilters.lotStep)}
            </Figure>{" "}
            · min_qty{" "}
            <Figure exact={dto.symbolFilters.minQty}>{qty(dto.symbolFilters.minQty)}</Figure> ·
            min_notional{" "}
            <Figure exact={dto.symbolFilters.minNotional}>
              {usdt(dto.symbolFilters.minNotional)}
            </Figure>{" "}
            · max_leverage{" "}
            <Figure exact={dto.symbolFilters.maxLeverage}>
              {qty(dto.symbolFilters.maxLeverage)}
            </Figure>
          </>
        ) : (
          "not recorded"
        )}
      </Field>
      <Field label="engine">
        {dto.engineFingerprint} · {dto.engineTarget}
      </Field>
      <Field label="content hash">{dto.resultContentHash}</Field>
      <Field label="run">{dto.runId}</Field>
      <Field label="version">{dto.strategyVersionId}</Field>
      <Field label="schema">{dto.schemaVersion}</Field>
      <Field label="created">
        <Figure exact={dto.createdAt}>{timeUtc(dto.createdAt)}</Figure>
      </Field>
      {/* The DTO's one control-metadata exception: the pre-save comparison's
          own warning, visibly flagged as such — present only when it exists. */}
      {dto.fingerprintWarning !== null && (
        <p className="bt-fp-warning" role="note">
          Fingerprint warning (pre-save comparison): {dto.fingerprintWarning}
        </p>
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// The withheld totals (r5-p1, #417, PDR-0038 option A)
// ---------------------------------------------------------------------------

/** The ONE sentence the Lab shows in place of the run's totals while an open
 *  freeze's holdout start cuts them (#417 step 3).
 *
 *  Wording follows the Chart tab's own holdout notes ("Holdout from <date UTC>
 *  — <n> bars withheld"): the date is the same `dateUtc` rendering, so one
 *  freeze reads the same in both tabs. The server decided the fence and sent
 *  every word of this — the count, the flag and the instant — so the screen
 *  formats and never re-derives.
 *
 *  The count is the trades the READ withheld, never a total the screen computed:
 *  a reader who adds the listed rows to it gets nothing, because the withheld
 *  rows are not in the answer at all. */
function WithheldTotals({ dto }: { dto: BacktestRunDto }) {
  if (!dto.totalsWithheld) return null;
  const count = dto.withheldTradeCount;
  const noun = count === 1 ? "trade" : "trades";
  return (
    <p className="bt-withheld" role="note">
      {`${count} ${noun} and the run totals withheld: holdout from ${dateUtc(dto.holdoutStartMs)}`}
    </p>
  );
}

// ---------------------------------------------------------------------------
// KPI tiles
// ---------------------------------------------------------------------------

function Kpi({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="bt-kpi">
      <span className="bt-kpi-lab">{label}</span>
      <span className="bt-kpi-val mono">{children}</span>
    </div>
  );
}

function KpiTiles({ dto }: { dto: BacktestRunDto }) {
  // #417 (PDR-0038 option A): a withheld run has no totals to render, and the
  // honest screen is the ONE sentence in their place — never a grid of em
  // dashes a reader could mistake for a run that traded nothing. The decision
  // is the server's (`totalsWithheld`); this screen only obeys it.
  if (dto.totalsWithheld) {
    return (
      <section className="bt-section bt-kpis" aria-label="Headline metrics">
        <WithheldTotals dto={dto} />
      </section>
    );
  }
  return (
    <section className="bt-section bt-kpis" aria-label="Headline metrics">
      {/* Every figure is A2-formatted over the DTO's own text, with that exact
          text on the element; null renders an em dash. No deltas, no
          comparisons, no derived percentages — the coach rail that compares
          runs is r1.s4. */}
      <Kpi label="Net P&L">
        <Figure exact={dto.netPnl}>{usdt(dto.netPnl, { signed: true })}</Figure>
      </Kpi>
      <Kpi label="Expectancy">
        <Figure exact={dto.expectancy}>{usdt(dto.expectancy, { signed: true })}</Figure>
      </Kpi>
      <Kpi label="Win rate">
        <Figure exact={dto.winRate}>{percentFromFraction(dto.winRate)}</Figure>
      </Kpi>
      <Kpi label="Profit factor">
        <Figure exact={dto.profitFactor}>{decimal(dto.profitFactor, 2)}</Figure>
      </Kpi>
      <Kpi label="Trades">{dto.tradeCount ?? EM_DASH}</Kpi>
      <Kpi label="Wins">{dto.winCount ?? EM_DASH}</Kpi>
      <Kpi label="Losses">{dto.lossCount ?? EM_DASH}</Kpi>
      <Kpi label="Max win streak">{dto.maxWinStreak ?? EM_DASH}</Kpi>
      <Kpi label="Max loss streak">{dto.maxLossStreak ?? EM_DASH}</Kpi>
      <Kpi label="Max drawdown">
        <Figure exact={dto.maxDrawdown}>{usdt(dto.maxDrawdown)}</Figure>
      </Kpi>
      <Kpi label="Sharpe">
        <Figure exact={asText(dto.sharpe)}>{decimal(asText(dto.sharpe), 2)}</Figure>
      </Kpi>
      <Kpi label="Sortino">
        <Figure exact={asText(dto.sortino)}>{decimal(asText(dto.sortino), 2)}</Figure>
      </Kpi>
      <Kpi label="Skipped sub-lot">{dto.skippedSubLot ?? EM_DASH}</Kpi>
      <Kpi label="Skipped sub-notional">{dto.skippedSubNotional ?? EM_DASH}</Kpi>
      <Kpi label="Skipped leverage-capped">{dto.skippedLeverageCapped ?? EM_DASH}</Kpi>
      <Kpi label="Fees">
        <Figure exact={dto.feesTotal}>{usdt(dto.feesTotal)}</Figure>
      </Kpi>
      <Kpi label="Funding">
        <Figure exact={dto.fundingTotal}>{usdt(dto.fundingTotal, { signed: true })}</Figure>
      </Kpi>
      <Kpi label="Slippage">
        <Figure exact={dto.slippageTotal}>{usdt(dto.slippageTotal)}</Figure>
      </Kpi>
      <Kpi label="Gross profit">
        <Figure exact={dto.grossProfit}>{usdt(dto.grossProfit)}</Figure>
      </Kpi>
      <Kpi label="Gross loss">
        <Figure exact={dto.grossLoss}>{usdt(dto.grossLoss)}</Figure>
      </Kpi>
      <Kpi label="Avg win">
        <Figure exact={dto.avgWin}>{usdt(dto.avgWin)}</Figure>
      </Kpi>
      <Kpi label="Avg loss">
        <Figure exact={dto.avgLoss}>{usdt(dto.avgLoss)}</Figure>
      </Kpi>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Equity chart (one series, focusable, pointer/keyboard parity)
// ---------------------------------------------------------------------------

const EQ_W = 720;
const EQ_H = 240;
const EQ_PAD = 28;
/** The padded plot fraction — keeps the extreme points off the frame. */
const EQ_INSET = 0.08;

function EquityChart({
  points,
  startingEquity,
}: {
  points: BacktestRunDto["equity"];
  startingEquity: string;
}) {
  // Geometry only: the strings themselves are what renders.
  const values = points.map((p) => num(p.equity));
  const refValue = num(startingEquity);
  const lo = Math.min(...values, refValue);
  const hi = Math.max(...values, refValue);
  const span = hi - lo;
  const min = lo - span * EQ_INSET;
  const max = hi + span * EQ_INSET;

  const x = (i: number) => EQ_PAD + (i / Math.max(points.length - 1, 1)) * (EQ_W - 2 * EQ_PAD);
  const y = (v: number) =>
    EQ_H - EQ_PAD - ((v - min) / Math.max(max - min, 1e-9)) * (EQ_H - 2 * EQ_PAD);

  const [active, setActive] = useState(0);
  const clamped = Math.min(active, points.length - 1);
  const point = points[clamped];

  const linePoints = points.map((p, i) => `${x(i)},${y(num(p.equity))}`).join(" ");
  const areaPoints = `${linePoints} ${x(points.length - 1)},${y(lo)} ${x(0)},${y(lo)}`;

  /** Keyboard traversal: Left/Right step point-to-point, Home/End jump. */
  function onKeyDown(event: React.KeyboardEvent<HTMLDivElement>) {
    if (event.key === "ArrowRight") {
      event.preventDefault();
      setActive((i) => Math.min(i + 1, points.length - 1));
    } else if (event.key === "ArrowLeft") {
      event.preventDefault();
      setActive((i) => Math.max(i - 1, 0));
    } else if (event.key === "Home") {
      event.preventDefault();
      setActive(0);
    } else if (event.key === "End") {
      event.preventDefault();
      setActive(points.length - 1);
    }
  }

  return (
    <section className="bt-section bt-equity" aria-label="Equity curve">
      <h3 className="bt-h">Equity curve</h3>
      <div
        className="bt-eq-plot"
        role="img"
        aria-label={`Equity curve, ${points.length} points`}
        tabIndex={0}
        onKeyDown={onKeyDown}
      >
        <svg viewBox={`0 0 ${EQ_W} ${EQ_H}`} preserveAspectRatio="none">
          {/* One series: a 2px line with a quiet area fill beneath it. */}
          <polygon className="bt-eq-area" points={areaPoints} />
          <polyline className="bt-eq-line" points={linePoints} />
          {/* The starting-equity reference line, carrying the DTO's own value:
              formatted below, its exact string on hover — the reference is
              never colour-only. */}
          <line
            className="bt-eq-ref"
            x1={EQ_PAD}
            x2={EQ_W - EQ_PAD}
            y1={y(refValue)}
            y2={y(refValue)}
          />
          {points.map((p, i) => (
            <circle
              key={p.timeMs}
              className="bt-eq-hit"
              cx={x(i)}
              cy={y(num(p.equity))}
              r={9}
              onMouseOver={() => setActive(i)}
            />
          ))}
        </svg>
      </div>
      {/* One readout for both pointer and keyboard — parity is structural:
          the same state drives the same formatted text and exact strings
          either way. */}
      <p className="bt-eq-readout mono" aria-live="polite">
        <span className="bt-eq-readout-time">
          <Figure exact={point.timeMs}>{timeUtc(point.timeMs)}</Figure>
        </span>{" "}
        ·{" "}
        <span className="bt-eq-readout-equity">
          <Figure exact={point.equity}>{usdt(point.equity)}</Figure>
        </span>
      </p>
      <p className="bt-eq-ref-label">
        starting equity{" "}
        <span className="mono">
          <Figure exact={startingEquity}>{usdt(startingEquity)}</Figure>
        </span>
      </p>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Regime bars (one zero baseline, hue fixed by slot)
// ---------------------------------------------------------------------------

function RegimeBars({ cells }: { cells: RegimeCellDto[] }) {
  // Geometry only: the widest |netPnl| sets the scale; sign chooses the side
  // of the one zero baseline — never a second hue.
  const maxAbs = Math.max(...cells.map((c) => Math.abs(num(c.netPnl))), 1e-9);

  return (
    <section className="bt-section bt-regimes" aria-label="Regime breakdown">
      <h3 className="bt-h">Regimes</h3>
      {/* Compact fixed-order legend beside the direct row labels — both, so
          the hue mapping is legible with or without the geometry. */}
      <div className="bt-legend">
        {cells.map((cell, i) => (
          <span key={cell.regime} className="bt-legend-item">
            <span className={`bt-swatch bt-slot-${i + 1}`} />
            {REGIME_LABELS[cell.regime] ?? cell.regime}
          </span>
        ))}
      </div>
      <div className="bt-regime-rows">
        {cells.map((cell, i) => {
          const width = (Math.abs(num(cell.netPnl)) / maxAbs) * 50;
          const positive = num(cell.netPnl) >= 0;
          return (
            <div key={cell.regime} className="bt-regime-row" style={{ minHeight: 24 }}>
              <span className={`bt-swatch bt-slot-${i + 1}`} />
              <span className="bt-regime-name">{REGIME_LABELS[cell.regime] ?? cell.regime}</span>
              <span className="bt-regime-track">
                <span className="bt-regime-baseline" />
                <span
                  className={`bt-regime-bar bt-slot-${i + 1}`}
                  style={positive ? { left: "50%", width: `${width}%` } : { right: "50%", width: `${width}%` }}
                />
              </span>
              <span className="bt-regime-count mono">{cell.tradeCount}</span>
              <span className="bt-regime-pnl mono">
                <Figure exact={cell.netPnl}>{usdt(cell.netPnl)}</Figure>
              </span>
            </div>
          );
        })}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// MFE / MAE histograms (single hue, pinned column order)
// ---------------------------------------------------------------------------

/** One histogram column: underflow | finite bin | overflow, in fixed visual
 * order. The label interpolates the DTO's own bound strings verbatim — the
 * screen computes no binning. */
function histogramColumns(hist: HistogramDto): { label: string; count: number }[] {
  return [
    { label: "< 0 R", count: hist.underflow },
    ...hist.bins.map((b) => ({ label: `[${b.lower}, ${b.upper}) R`, count: b.count })),
    { label: "≥ 3 R", count: hist.overflow },
  ];
}

function Histogram({ title, className, hist }: { title: string; className: string; hist: HistogramDto }) {
  const columns = histogramColumns(hist);
  const maxCount = Math.max(...columns.map((c) => c.count), 1);

  return (
    <section className={`bt-section bt-histogram ${className}`} aria-label={title}>
      <h3 className="bt-h">{title}</h3>
      <div className="bt-hist-cols">
        {columns.map((column, i) => (
          // The interactive dimension (the pointer's horizontal travel) is at
          // least 24px regardless of the painted mark.
          <div key={i} className="bt-col" style={{ minWidth: 24 }}>
            <span className="bt-col-count mono">{column.count}</span>
            <div className="bt-col-stack">
              <div
                className="bt-col-bar bt-slot-1"
                style={{
                  height: `${(column.count / maxCount) * 100}%`,
                  minHeight: column.count > 0 ? 2 : 0,
                }}
              />
            </div>
            <span className="bt-col-label">{column.label}</span>
          </div>
        ))}
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Trade table (all 18 fields, seq order, focusable scroll region)
// ---------------------------------------------------------------------------

/** The per-column formatter for the trade table: every money, price, quantity,
 * R and time column, keyed by the DTO's own field name. A column absent here is
 * a label (`direction`, `exitReason`, `source`, `regime`) and renders as
 * delivered; the header stays the field name itself, so the table remains the
 * DTO's own column list. */
const TRADE_FORMATTERS: Partial<
  Record<(typeof TRADE_FIELDS)[number], (text: string | null) => string>
> = {
  qty,
  entryPrice: price,
  exitPrice: price,
  stopPrice: price,
  entrySignalTime: timeUtc,
  entryFillTime: timeUtc,
  exitSignalTime: timeUtc,
  exitFillTime: timeUtc,
  feesTotal: (text) => usdt(text),
  fundingTotal: (text) => usdt(text, { signed: true }),
  slippageTotal: (text) => usdt(text),
  realizedPnl: (text) => usdt(text, { signed: true }),
  realizedR: rMultiple,
  mfeR: rMultiple,
  maeR: rMultiple,
};

function TradeTable({ trades, runId, dto }: { trades: TradeRowDto[]; runId: string; dto: BacktestRunDto }) {
  return (
    <section className="bt-section bt-trades-section" aria-label="Trades">
      <h3 className="bt-h">Trades</h3>
      {/* #417 step 3: the trades the fence left out are named HERE, at the table
          that would have listed them, so a reader who counts rows is told why the
          count is short. The TOTALS sentence belongs to the totals block above
          (the ruling puts it "in place of the totals"); this is the row-level
          note, in the Chart tab's own shape for the same fact — "n marks hidden:
          inside the holdout".
          The count covers every row the fence rejected, INCLUDING a straddling
          trade (entry before H, exit at or after it), so the wording is "reaching
          the holdout" — the same phrase the Chart tab's own sentence uses — and
          never "inside", which would name only the trades that OPEN there. */}
      {dto.withheldTradeCount > 0 && (
        <p className="bt-withheld-rows" role="note">
          {`${dto.withheldTradeCount} ${
            dto.withheldTradeCount === 1 ? "trade" : "trades"
          } withheld: reaching the holdout`}
        </p>
      )}
      {/* r5.s2.w2: the tab that draws these trades on the run's own candles. The
          handover is the fragment and the run id, never shared state (the #269
          deep-link pattern one screen over). */}
      <a className="ch-link" href={`#/chart?run=${runId}`}>
        Open in Chart →
      </a>
      {/* Keyboard-focusable scroll region: the table scrolls within it rather
          than being clipped by the pane. */}
      <div className="bt-table-scroll" role="region" aria-label="Trade rows" tabIndex={0}>
        <table className="bt-trades">
          <thead>
            <tr>
              {TRADE_FIELDS.map((field) => (
                <th key={field} scope="col">
                  {field}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {trades.map((trade, i) => (
              <tr key={i}>
                {TRADE_FIELDS.map((field) => {
                  const format = TRADE_FORMATTERS[field];
                  const exact = trade[field];
                  return (
                    <td key={field} className="mono">
                      {format === undefined ? (
                        /* `stopPrice` is `string | null` — a pre-0011 row renders `—` (r2.s2.w2). */
                        (exact ?? "—")
                      ) : (
                        <Figure exact={exact}>{format(exact)}</Figure>
                      )}
                    </td>
                  );
                })}
              </tr>
            ))}
            {trades.length === 0 && (
              <tr>
                <td colSpan={TRADE_FIELDS.length}>
                  {/* PDR-0011: the empty state says WHICH empty it is. A fenced read
                      must not read as "this run traded nothing". */}
                  {dto.totalsWithheld
                    ? "No trades in this run fall before the holdout."
                    : "The run recorded no trades."}
                </td>
              </tr>
            )}
          </tbody>
        </table>
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Table twins (no data reachable only through the graphic)
// ---------------------------------------------------------------------------

/** Every chart's in-DOM accessible twin carrying the same exact strings.
 *
 *  A withheld run (#417) has no series to twin: the four tables below render
 *  their headers and no rows, and the withheld sentence lives in the KPI block
 *  above, which is the one place the reading is explained. An empty twin is
 *  therefore not a fabricated "no data" claim — the page has already said why. */
function ChartTwins({ dto }: { dto: BacktestRunDto }) {
  const mfeColumns = useMemo(
    () => (dto.mfe === null ? [] : histogramColumns(dto.mfe)),
    [dto.mfe],
  );
  const maeColumns = useMemo(
    () => (dto.mae === null ? [] : histogramColumns(dto.mae)),
    [dto.mae],
  );

  return (
    <section className="bt-twins" aria-label="Chart data tables">
      <table className="bt-twin bt-twin-equity">
        <caption>Equity curve</caption>
        <thead>
          <tr>
            <th scope="col">time (epoch ms)</th>
            <th scope="col">equity</th>
          </tr>
        </thead>
        <tbody>
          {dto.equity.map((p) => (
            <tr key={p.timeMs}>
              <td className="mono">{p.timeMs}</td>
              <td className="mono">{p.equity}</td>
            </tr>
          ))}
        </tbody>
      </table>

      <table className="bt-twin bt-twin-regimes">
        <caption>Regime breakdown</caption>
        <thead>
          <tr>
            <th scope="col">regime</th>
            <th scope="col">trades</th>
            <th scope="col">net P&L</th>
          </tr>
        </thead>
        <tbody>
          {dto.regimes.map((cell) => (
            <tr key={cell.regime} data-regime={cell.regime}>
              <td>{REGIME_LABELS[cell.regime] ?? cell.regime}</td>
              <td className="mono">{cell.tradeCount}</td>
              <td className="mono">
                <Figure exact={cell.netPnl}>{usdt(cell.netPnl)}</Figure>
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {(
        [
          ["bt-twin-mfe", "MFE (R)", mfeColumns],
          ["bt-twin-mae", "MAE (R)", maeColumns],
        ] as const
      ).map(([twinClass, caption, columns]) => (
        <table key={twinClass} className={`bt-twin ${twinClass}`}>
          <caption>{caption}</caption>
          <thead>
            <tr>
              <th scope="col">column</th>
              <th scope="col">count</th>
            </tr>
          </thead>
          <tbody>
            {columns.map((column) => (
              <tr key={column.label}>
                <td>{column.label}</td>
                <td className="mono">{column.count}</td>
              </tr>
            ))}
          </tbody>
        </table>
      ))}
    </section>
  );
}

// ---------------------------------------------------------------------------
// The coach rail (r1.s4.w3) — one proposal or one recorded failure, never both
// ---------------------------------------------------------------------------
//
// Every number below is a DTO string, rendered as delivered. The rail computes no
// money value, no percentage and no delta: the before/after comparison is two
// columns of the backend's own strings side by side, and the reader does the
// subtraction, because a number this screen invented would be a number nothing
// persisted.
//
// The failure card's `recovery` is the DTO's own field. A `kind → text` mapping
// here would be a second copy of a decision the backend already made, and the one
// that a new failure variant silently slips past.

/** The rail's whole surface. Hidden until the trader presses "Ask the coach". */
function CoachRail({
  state,
  onAsk,
  onRun,
  onDecide,
  onSelectChild,
}: {
  state: RailState;
  onAsk: () => void;
  onRun: () => void;
  onDecide: (action: CoachActionDto) => void;
  onSelectChild: (versionId: string) => void;
}) {
  if (state.kind === "idle") {
    return (
      <section className="bt-section coach-invite" aria-label="Coach">
        <button type="button" className="coach-ask btn-prim" onClick={onAsk}>
          Ask the coach
        </button>
        <span className="coach-invite-note">
          One change at a time, on this run's own recorded results.
        </span>
      </section>
    );
  }

  return (
    <section className="bt-section coach-rail" aria-label="Coach rail">
      <h3 className="bt-h">Coach</h3>
      <CoachBody
        state={state}
        onAsk={onAsk}
        onRun={onRun}
        onDecide={onDecide}
        onSelectChild={onSelectChild}
      />
      <CoachProvenance state={state} />
    </section>
  );
}

/** The in-flight lines, one per state, each naming what is actually happening. */
const IN_FLIGHT: Record<string, string> = {
  running: "Asking the coach…",
  modifying: "Re-validating your change…",
  rejecting: "Recording the rejection…",
  accepting: "Re-backtesting the accepted child…",
};

function CoachBody({
  state,
  onAsk,
  onRun,
  onDecide,
  onSelectChild,
}: {
  state: RailState;
  onAsk: () => void;
  onRun: () => void;
  onDecide: (action: CoachActionDto) => void;
  onSelectChild: (versionId: string) => void;
}) {
  if (
    state.kind === "running" ||
    state.kind === "modifying" ||
    state.kind === "rejecting" ||
    state.kind === "accepting"
  ) {
    return (
      <div className="coach-state dim" role="status">
        {IN_FLIGHT[state.kind]}
        {state.kind === "running" && state.note !== null && (
          <p className="coach-busy">{state.note}</p>
        )}
      </div>
    );
  }
  // A `busy` refusal is not a failure — nothing broke, and the answer is coming
  // from whoever holds the key. It is not `running` either: THIS record has
  // settled, so nothing further will arrive to move it along, and the retry is
  // how the trader picks the result up.
  if (state.kind === "busy") {
    return (
      <div className="coach-state dim" role="status">
        {IN_FLIGHT.running}
        <p className="coach-busy">{state.note}</p>
        <button type="button" className="coach-retry btn-sec" onClick={onAsk}>
          Check again
        </button>
      </div>
    );
  }
  if (state.kind === "failed") {
    return (
      <FailureCard session={state.session} error={state.error} onAsk={onAsk} onRun={onRun} />
    );
  }
  if (state.kind === "proposal") {
    return (
      <ProposalCard session={state.session} refusal={state.refusal} onDecide={onDecide} />
    );
  }
  if (state.kind === "completed") {
    return (
      <AcceptedPanel
        session={state.session}
        accepted={state.accepted}
        onSelectChild={onSelectChild}
      />
    );
  }
  // `idle` is handled by the invitation above and never reaches the body. Stated
  // as a branch rather than a fall-through so the union stays exhaustive for the
  // compiler too — a ninth state would fail to typecheck here.
  return null;
}

/**
 * One recorded failure: the typed kind, its detail, and the BACKEND's recovery.
 *
 * Every failure card carries a recovery AND the action that performs it. A typed
 * coach failure names its own; an operational error has none to name, so the
 * generic one is stated rather than left blank — a failure card with no way
 * forward is a dead end, and the recovery text was previously advice the rail
 * could not act on.
 */
function FailureCard({
  session,
  error,
  onAsk,
  onRun,
}: {
  session: CoachSessionDto | null;
  error: BusError | null;
  onAsk: () => void;
  onRun: () => void;
}) {
  const failure = session?.failure ?? null;
  const recovery = failure?.recovery ?? "try again";
  // `missing_backtest_inputs` is the one failure asking the coach again cannot
  // recover from. The parent run is immutable, so it can never acquire the
  // provenance it lacks, and every re-ask against it records the identical failure
  // for ever. Its recovery says to run the version again, so THAT is the button it
  // gets — an action that performs the stated recovery rather than one that
  // contradicts it.
  const rerun = failure?.kind === "missing_backtest_inputs";
  return (
    <div className="coach-failure" role="alert">
      <p className="coach-failure-kind mono">{failure?.kind ?? error?.code ?? "internal"}</p>
      <p className="coach-failure-detail">{failure?.detail ?? error?.message ?? ""}</p>
      <p className="coach-recovery">
        <span className="coach-recovery-label">What to do</span> {recovery}
      </p>
      {rerun ? (
        <button type="button" className="coach-retry btn-prim" onClick={onRun}>
          Run this version again
        </button>
      ) : (
        <button type="button" className="coach-retry btn-prim" onClick={onAsk}>
          Ask the coach again
        </button>
      )}
    </div>
  );
}

/** The one proposed change, its hypothesis, and the three actions. */
function ProposalCard({
  session,
  refusal,
  onDecide,
}: {
  session: CoachSessionDto;
  refusal: BusError | null;
  onDecide: (action: CoachActionDto) => void;
}) {
  const proposal = session.proposal;
  const [draft, setDraft] = useState<string | null>(null);

  if (proposal === null) {
    return null;
  }
  const settled = proposal.disposition === "rejected" || proposal.disposition === "accepted";

  return (
    <div className="coach-proposal">
      <div className="coach-change">
        <span className="coach-change-path mono">{proposal.mutation.path}</span>
        <span className="coach-change-arrow">→</span>
        <span className="coach-change-value mono">{proposal.mutation.newValue}</span>
      </div>
      <p className="coach-hypothesis">{proposal.hypothesis}</p>
      {/* A refused decision, shown ON the card rather than in place of it: the
          backend rejected the edit, not the turn, so the proposal is still here
          to correct and act on. */}
      {refusal !== null && (
        <p className="coach-refusal" role="alert">
          <span className="mono">{refusal.code}</span> {refusal.message}
        </p>
      )}
      <p className="coach-disposition">
        <span className="coach-disposition-label">Status</span>{" "}
        <span className="mono">{proposal.disposition}</span>
      </p>
      {proposal.acceptFailure !== null && (
        <p className="coach-accept-failure" role="alert">
          The last accept stopped at{" "}
          <span className="mono">{proposal.acceptFailure.stage}</span>:{" "}
          {proposal.acceptFailure.message}
          {proposal.acceptFailure.subject !== null && (
            <>
              {" "}
              (<span className="mono">{proposal.acceptFailure.subject}</span>)
            </>
          )}
        </p>
      )}
      {!settled && draft === null && (
        <div className="coach-actions">
          <button
            type="button"
            className="coach-modify"
            onClick={() => setDraft(proposal.mutation.newValue)}
          >
            Modify
          </button>
          <button
            type="button"
            className="coach-reject"
            onClick={() => onDecide({ kind: "reject" })}
          >
            Reject
          </button>
          <button
            type="button"
            className="coach-accept btn-prim"
            onClick={() => onDecide({ kind: "accept" })}
          >
            Accept
          </button>
        </div>
      )}
      {!settled && draft !== null && (
        <div className="coach-edit">
          <label className="coach-edit-label" htmlFor="coach-new-value">
            New value
          </label>
          <input
            id="coach-new-value"
            className="coach-edit-input mono"
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
          />
          <button
            type="button"
            className="coach-revalidate btn-prim"
            onClick={() => {
              onDecide({
                kind: "modify",
                path: proposal.mutation.path,
                newValue: draft,
              });
              setDraft(null);
            }}
          >
            Re-validate
          </button>
          <button type="button" className="coach-cancel-edit" onClick={() => setDraft(null)}>
            Cancel
          </button>
        </div>
      )}
    </div>
  );
}

/** The `SummaryDto` rows the before/after table compares, in one fixed order. */
const SUMMARY_ROWS: { key: keyof SummaryDto; label: string }[] = [
  { key: "expectancy", label: "Expectancy" },
  { key: "netPnl", label: "Net P&L" },
  { key: "winRate", label: "Win rate" },
  { key: "tradeCount", label: "Trades" },
  { key: "winCount", label: "Wins" },
  { key: "lossCount", label: "Losses" },
  { key: "profitFactor", label: "Profit factor" },
  { key: "grossProfit", label: "Gross profit" },
  { key: "grossLoss", label: "Gross loss" },
  { key: "avgWin", label: "Avg win" },
  { key: "avgLoss", label: "Avg loss" },
  { key: "maxDrawdown", label: "Max drawdown" },
  { key: "maxWinStreak", label: "Max win streak" },
  { key: "maxLossStreak", label: "Max loss streak" },
  { key: "commissionTotal", label: "Fees" },
  { key: "fundingTotal", label: "Funding" },
  { key: "sharpe", label: "Sharpe" },
  { key: "sortino", label: "Sortino" },
];

/** The formatter for one summary row — the same A2 kinds the KPI tiles use, so
 * the before/after table and the tiles cannot disagree. A key absent here is a
 * count and renders as delivered. */
const SUMMARY_FORMATTERS: Partial<
  Record<keyof SummaryDto, (text: string | null) => string>
> = {
  expectancy: (text) => usdt(text, { signed: true }),
  netPnl: (text) => usdt(text, { signed: true }),
  winRate: percentFromFraction,
  profitFactor: (text) => decimal(text, 2),
  grossProfit: (text) => usdt(text),
  grossLoss: (text) => usdt(text),
  avgWin: (text) => usdt(text),
  avgLoss: (text) => usdt(text),
  maxDrawdown: (text) => usdt(text),
  commissionTotal: (text) => usdt(text),
  fundingTotal: (text) => usdt(text, { signed: true }),
  sharpe: (text) => decimal(text, 2),
  sortino: (text) => decimal(text, 2),
};

/** One summary cell: the A2 display text, with the DTO's own string on the
 * element — `null` is an em dash, never a zero. */
function cell(summary: SummaryDto, key: keyof SummaryDto) {
  const value = summary[key];
  const exact = value === null ? null : String(value);
  const format = SUMMARY_FORMATTERS[key];
  if (format === undefined) return <>{exact ?? EM_DASH}</>;
  return <Figure exact={exact}>{format(exact)}</Figure>;
}

/**
 * The before/after summary table — extracted from `AcceptedPanel` (r2.s1.w4
 * C3) so the coach accept and the standalone child-vs-parent compare share ONE
 * cell shape. `badge` is optional and rides the table's caption: the accept
 * never passes one, so its table renders exactly as it did inline.
 */
function CompareTable({
  before,
  after,
  badge,
  beforeRunId,
  afterRunId,
}: {
  before: SummaryDto;
  after: SummaryDto;
  badge?: React.ReactNode;
  /** The compared runs' ids, when the caller knows them (#326). The coach
   * accept's inline table posts no ids and renders exactly as it did. */
  beforeRunId?: string | null;
  afterRunId?: string | null;
}) {
  return (
    <table className="coach-compare" aria-label="Before and after">
      {badge !== undefined && <caption className="compare-caption">{badge}</caption>}
      <thead>
        <tr>
          <th scope="col">metric</th>
          <th scope="col">
            before (parent)
            {beforeRunId != null && (
              <span className="compare-run mono" title={beforeRunId}>
                {beforeRunId.slice(0, SHORT_ID_LEN)}
              </span>
            )}
          </th>
          <th scope="col">
            after (child)
            {afterRunId != null && (
              <span className="compare-run mono" title={afterRunId}>
                {afterRunId.slice(0, SHORT_ID_LEN)}
              </span>
            )}
          </th>
        </tr>
      </thead>
      <tbody>
        {SUMMARY_ROWS.map((row) => (
          <tr key={row.key}>
            <th scope="row">{row.label}</th>
            <td className="mono">{cell(before, row.key)}</td>
            <td className="mono">{cell(after, row.key)}</td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

/** What `CompareWithParent` resolves to — the same explicit-state discipline
 * as the catalog's own union. */
type CompareState =
  | { kind: "loading" }
  | { kind: "error"; error: BusError }
  | { kind: "ready"; dto: CompareChildRunDto };

/**
 * The selected version's latest run beside its parent's latest — for ANY child
 * version, not only a coach accept (r2.s1.w4 C3). The child run named is the
 * freshest one the screen can prove: the run this screen just finished when it
 * is newer than the catalogue's, else the catalogue's latest.
 *
 * The catalogue's latest is `version.latestRun`, NOT `recentRuns[0]` (R2):
 * `recentRuns` is the whole run catalogue and a walk-forward's folds are in it —
 * they are ordinary runs (L8), and the fold table opens them as such — so after a
 * walk-forward completes its folds sit at the head of that list, sharing one
 * `created_at` with each other. Comparing against whichever UUID sorted first
 * would compare a fresh full-span run against a sub-window fold. `latestRun` is
 * the server's answer to "the version's latest run" (the same
 * `latest_run_for_version` read the KPIs use, which excludes folds), so the
 * comparison has a full-span run on both sides. A version with no ordinary run
 * has `latestRun: null` and there is nothing to compare — a state to report
 * rather than a fold to substitute.
 *
 * A typed refusal renders its reason line instead of the table — a child whose
 * parent has no run is a state to report, not an error to raise.
 */
function CompareWithParent({ version, run }: { version: LibraryVersion; run: RunState }) {
  const [outcome, setOutcome] = useState<CompareState>({ kind: "loading" });

  const catalogueLatest = version.latestRun ?? null;
  const persistedRunId = catalogueLatest?.id ?? null;
  const freshRunId =
    run.kind === "done" &&
    run.dto.strategyVersionId === version.id &&
    (catalogueLatest === null ||
      run.dto.createdAt >= catalogueLatest.createdAt)
      ? run.dto.runId
      : null;
  const latestRunId = freshRunId ?? persistedRunId;

  useEffect(() => {
    if (latestRunId === null) return;
    let alive = true;
    setOutcome({ kind: "loading" });
    commands
      .compareChildRun({ childRunId: latestRunId })
      .then((result) => {
        if (!alive) return;
        setOutcome(
          result.status === "ok"
            ? { kind: "ready", dto: result.data }
            : { kind: "error", error: result.error },
        );
      })
      .catch(() => {
        // The IPC call itself failing — the honest reason line, never a
        // fabricated table.
        if (alive) {
          setOutcome({
            kind: "error",
            error: {
              code: "internal",
              message: "The comparison read failed.",
              run_id: null,
              session_id: null,
              child_run_id: null,
            },
          });
        }
      });
    return () => {
      alive = false;
    };
    // `version` (not `version.id`) is a dep on purpose: a C2 refetch hands the
    // section a FRESH object, and re-comparing picks up a parent-side run that
    // arrived while the app was unfocused. `latestRunId` covers the post-Run
    // case the object identity cannot.
  }, [version, latestRunId]);

  return (
    <section className="bt-section compare-parent" aria-label="Compare with parent">
      <h3 className="compare-title">Compare with parent</h3>
      {outcome.kind === "loading" && (
        <p className="compare-reason dim">Comparing with the parent…</p>
      )}
      {outcome.kind === "error" && (
        <p className="compare-reason" role="alert">
          <span className="mono">{outcome.error.code}</span> {outcome.error.message}
        </p>
      )}
      {outcome.kind === "ready" && (
        <CompareTable
          before={outcome.dto.before}
          after={outcome.dto.after}
          // #326: which two runs the columns are — the DTO names both, so the
          // panel says so.
          beforeRunId={outcome.dto.parentRunId}
          afterRunId={outcome.dto.childRunId}
          badge={
            outcome.dto.inputsDiffer ? (
              <>
                <span
                  className="badge-inputs-differ"
                  title={outcome.dto.inputsNote ?? undefined}
                >
                  inputs differ
                </span>
                {/* #326: WHY they cannot be proven equal, in words beside the
                    badge — the hover keeps the exact text as well. */}
                {outcome.dto.inputsNote !== null && (
                  <span className="compare-inputs-note"> {outcome.dto.inputsNote}</span>
                )}
              </>
            ) : undefined
          }
        />
      )}
    </section>
  );
}

/** The committed accept: the child beside its parent, and both links. */
function AcceptedPanel({
  session,
  accepted,
  onSelectChild,
}: {
  session: CoachSessionDto;
  accepted: AcceptedCoachDto;
  onSelectChild: (versionId: string) => void;
}) {
  const unreadable = accepted.readBack !== "ok" ? accepted.readBack.failure : null;
  return (
    <div className="coach-accepted">
      <p className="coach-accepted-line">
        Accepted — one coach-attributed child version and one re-backtest.
      </p>
      <div className="coach-links">
        <a className="coach-link" href="#/library">
          Child version <span className="mono">{accepted.childVersionId}</span>
        </a>
        <button
          type="button"
          className="coach-link-run"
          onClick={() => onSelectChild(accepted.childVersionId)}
        >
          Select the child in the Lab — child run{" "}
          <span className="mono">{accepted.acceptedRunId}</span>
        </button>
      </div>
      {unreadable !== null && (
        <p className="coach-unreadable" role="note">
          The child run was saved but could not be read back, so there is no “after”
          column to show: {unreadable}
        </p>
      )}
      {accepted.after !== null && (
        <CompareTable before={accepted.before} after={accepted.after} />
      )}
      <p className="coach-session-line mono">session {session.sessionId}</p>
    </div>
  );
}

/** Cost, currency and prompt version — visible with every outcome that had a call. */
function CoachProvenance({ state }: { state: RailState }) {
  const session =
    state.kind === "proposal" || state.kind === "completed"
      ? state.session
      : state.kind === "failed"
        ? state.session
        : null;
  if (session === null || session.llmCallId === null) {
    return null;
  }
  return (
    <p className="coach-provenance mono">
      cost {session.cost === null ? EM_DASH : `${session.cost.amount} ${session.cost.currency}`} ·
      prompt {session.promptVersion ?? EM_DASH} · call {session.llmCallId}
    </p>
  );
}
