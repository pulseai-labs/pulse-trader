// The Backtest Lab (r1.s3.w4) — the rendering half of the spine's journey.
//
// A trader selects a persisted strategy version from the REAL library catalog
// (`commands.libraryOverview` — the Library's own read, no second catalog) and
// presses Run: `commands.runBacktestVersion({ versionId })` executes W3's
// fixed request/response command and this screen renders only the fresh
// `BacktestRunDto` it returns. Every decimal and epoch value is the DTO's own
// exact string — display truth is the string itself. The ONLY numeric
// conversion in this file is `parseFloat` for chart geometry (pixel
// positions), never for display, derivation or comparison; the screen does no
// binning, no money math, no deltas, no re-backtest, and invents no value the
// DTO does not carry. No sample option, no mock run, no progress percentage,
// no cancel affordance — the command is request/response and r1's target is
// <5s.
//
// Convention (UnbuiltScreen.tsx is the worked example): one file per screen,
// default export, zero props — the shell mounts it from the route table and
// all state is its own.

import { useCallback, useEffect, useMemo, useState } from "react";

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
} from "../bindings";
import { backtestKey, coachKey, useActiveOperations } from "../hooks/useActiveOperations";
import type { BusResult, OperationRecord } from "../hooks/useActiveOperations";
import { useRefetchOnFocus } from "../hooks/useRefetchOnFocus";

/** The em dash every null renders — a statement that no value exists, never
 * a zero dressed up as data (the Library's grill A1 rule, applied here). */
const EM_DASH = "—";

/** The one window into the chart geometry exception: a DTO string becomes a
 * number ONLY to place a pixel. Never displayed, never compared, never
 * re-formatted. */
function num(value: string): number {
  return Number.parseFloat(value);
}

// ---------------------------------------------------------------------------
// Catalog state (the selector's four explicit states)
// ---------------------------------------------------------------------------

/** One selectable option: a real (strategy, version) pair from the payload. */
interface Option {
  /** The version's id — the run request's `versionId`. */
  value: string;
  /** `Strategy name · vN`, N the version's position in its parent-ordered list. */
  label: string;
}

type CatalogState =
  | { kind: "loading" }
  | { kind: "error"; code: string; message: string }
  | { kind: "empty" }
  // The overview rides along (r2.s1.w4 C3): the compare section needs the
  // selected version's `parentId` and `recentRuns`, which the flattened
  // options do not carry.
  | { kind: "ready"; options: Option[]; overview: LibraryOverview };

/** Flatten the catalog's strategies into the selector's options — the real
 * versions only; a version not in the payload cannot appear here. */
function catalogOptions(overview: LibraryOverview): Option[] {
  const options: Option[] = [];
  for (const strategy of overview.strategies) {
    strategy.versions.forEach((version: LibraryVersion, index: number) => {
      options.push({ value: version.id, label: `${strategy.name} · v${index + 1}` });
    });
  }
  return options;
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
  const [selectedId, setSelectedId] = useState<string | null>(null);
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
          const options = catalogOptions(result.data);
          setCatalog(
            options.length === 0
              ? { kind: "empty" }
              : { kind: "ready", options, overview: result.data },
          );
          const known = new Set(options.map((option) => option.value));
          // C2 (r2.s1.w4): a refetch keeps the selection ONLY while the
          // refreshed catalog still carries it — a vanished version falls back
          // to the first option rather than rendering a blank select.
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

  // C2 (r2.s1.w4): versions and runs written by `pulse mcp` surface on window
  // focus — DOM events only, throttled inside the hook. The refetch is the
  // same `loadCatalog` the mount and the accept paths use, so the selection
  // repair above applies to it too.
  useRefetchOnFocus(
    useCallback(() => {
      void loadCatalog();
    }, [loadCatalog]),
  );

  const options = catalog.kind === "ready" ? catalog.options : [];
  const currentId = selectedId ?? options[0]?.value ?? null;
  /** This version's operation, whoever started it and whenever. Keyed by version,
   * so a result can never be attributed to a version it was not run for — the
   * property the old bumped token approximated. */
  const run = runStateOf(
    currentId === null ? undefined : operations.lookup(backtestKey(currentId)),
  );

  /** The selected version's own record — the compare section needs its
   * `parentId` and `recentRuns`, which the selector's flattened options drop. */
  const selectedVersion =
    catalog.kind === "ready"
      ? (catalog.overview.strategies
          .flatMap((strategy) => strategy.versions)
          .find((version) => version.id === currentId) ?? null)
      : null;

  /** Selecting a version shows THAT version's operation. Nothing is cleared and
   * nothing is cancelled: each version's record is its own, so a run still going
   * for the previous selection stays going, and coming back to it shows it. The
   * coach rail is per-run, so it closes with the selection. */
  function onSelectorChange(id: string) {
    setSelectedId(id);
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
    const known = await loadCatalog();
    if (known.has(versionId)) {
      setSelectedId(versionId);
    }
  }

  /** The screen's ONLY invocation of the backtest command — the Run button's
   * click handler. No mount effect reaches this.
   *
   * The store refuses a key already in flight before the bus is called, and the
   * backend's latch refuses it again if reached, so a double-click that beats
   * this re-render still starts exactly one run. */
  function onRun() {
    if (currentId === null || run.kind === "running") return;
    operations.start(backtestKey(currentId), () =>
      commands.runBacktestVersion({ versionId: currentId }),
    );
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
          <select
            id="bt-version"
            className="bt-select"
            value={currentId ?? ""}
            onChange={(event) => onSelectorChange(event.target.value)}
            disabled={run.kind === "running"}
          >
            {options.map((option) => (
              <option key={option.value} value={option.value}>
                {option.label}
              </option>
            ))}
          </select>
          <button
            type="button"
            className="bt-run btn-prim"
            onClick={onRun}
            disabled={run.kind === "running" || currentId === null}
          >
            {run.kind === "running" ? "Running…" : "Run backtest"}
          </button>
          {/* No percentage, no cancel — the run is request/response. */}
        </div>
      )}

      {/* C3 (r2.s1.w4): a child version's latest run beside its parent's — for
          ANY child, not only a coach accept. Shown only when there is a parent
          to compare with and at least one run to compare. */}
      {selectedVersion !== null &&
        selectedVersion.parentId !== null &&
        (run.kind === "done" || selectedVersion.recentRuns.length > 0) && (
          <CompareWithParent version={selectedVersion} run={run} />
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
          <EquityChart
            points={run.dto.equity}
            startingEquity={run.dto.startingEquity}
          />
          <RegimeBars cells={run.dto.regimes} />
          <Histogram title="MFE (R)" className="bt-mfe" hist={run.dto.mfe} />
          <Histogram title="MAE (R)" className="bt-mae" hist={run.dto.mae} />
          <TradeTable trades={run.dto.trades} />
          <ChartTwins dto={run.dto} />
        </div>
      )}
    </div>
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
      <Field label="htf">
        {dto.htfTimeframe !== null ? `${dto.htfTimeframe} · ${dto.htfDataVersion}` : EM_DASH}
      </Field>
      <Field label="date range">
        {dto.firstOpenTimeMs} → {dto.lastCloseTimeMs}
      </Field>
      <Field label="starting equity">{dto.startingEquity}</Field>
      <Field label="taker fee">{dto.takerFeeBps} bps</Field>
      <Field label="slippage">{dto.slippageBps} bps</Field>
      <Field label="funding">{dto.funding}</Field>
      <Field label="engine">
        {dto.engineFingerprint} · {dto.engineTarget}
      </Field>
      <Field label="content hash">{dto.resultContentHash}</Field>
      <Field label="run">{dto.runId}</Field>
      <Field label="version">{dto.strategyVersionId}</Field>
      <Field label="schema">{dto.schemaVersion}</Field>
      <Field label="created">{dto.createdAt}</Field>
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
  return (
    <section className="bt-section bt-kpis" aria-label="Headline metrics">
      {/* Exact strings; null renders an em dash. No deltas, no comparisons,
          no derived percentages — the coach rail that compares runs is r1.s4. */}
      <Kpi label="Net P&L">{dto.netPnl}</Kpi>
      <Kpi label="Expectancy">{dto.expectancy}</Kpi>
      <Kpi label="Win rate">{dto.winRate}</Kpi>
      <Kpi label="Profit factor">{dto.profitFactor ?? EM_DASH}</Kpi>
      <Kpi label="Trades">{dto.tradeCount}</Kpi>
      <Kpi label="Wins">{dto.winCount}</Kpi>
      <Kpi label="Losses">{dto.lossCount}</Kpi>
      <Kpi label="Max win streak">{dto.maxWinStreak}</Kpi>
      <Kpi label="Max loss streak">{dto.maxLossStreak}</Kpi>
      <Kpi label="Max drawdown">{dto.maxDrawdown}</Kpi>
      <Kpi label="Sharpe">{dto.sharpe ?? EM_DASH}</Kpi>
      <Kpi label="Sortino">{dto.sortino ?? EM_DASH}</Kpi>
      <Kpi label="Skipped sub-lot">{dto.skippedSubLot}</Kpi>
      <Kpi label="Skipped sub-notional">{dto.skippedSubNotional}</Kpi>
      <Kpi label="Skipped leverage-capped">{dto.skippedLeverageCapped}</Kpi>
      <Kpi label="Fees">{dto.feesTotal}</Kpi>
      <Kpi label="Funding">{dto.fundingTotal}</Kpi>
      <Kpi label="Slippage">{dto.slippageTotal}</Kpi>
      <Kpi label="Gross profit">{dto.grossProfit}</Kpi>
      <Kpi label="Gross loss">{dto.grossLoss}</Kpi>
      <Kpi label="Avg win">{dto.avgWin}</Kpi>
      <Kpi label="Avg loss">{dto.avgLoss}</Kpi>
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
          {/* The starting-equity reference line, labelled with the DTO's own
              string — the reference is never colour-only. */}
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
          the same state drives the same exact strings either way. */}
      <p className="bt-eq-readout mono" aria-live="polite">
        <span className="bt-eq-readout-time">{point.timeMs}</span> ·{" "}
        <span className="bt-eq-readout-equity">{point.equity}</span>
      </p>
      <p className="bt-eq-ref-label">
        starting equity <span className="mono">{startingEquity}</span>
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
              <span className="bt-regime-pnl mono">{cell.netPnl}</span>
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

function TradeTable({ trades }: { trades: TradeRowDto[] }) {
  return (
    <section className="bt-section bt-trades-section" aria-label="Trades">
      <h3 className="bt-h">Trades</h3>
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
                {TRADE_FIELDS.map((field) => (
                  <td key={field} className="mono">
                    {/* `stopPrice` is `string | null` — a pre-0011 row renders `—` (r2.s2.w2). */}
                    {trade[field] ?? "—"}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Table twins (no data reachable only through the graphic)
// ---------------------------------------------------------------------------

/** Every chart's in-DOM accessible twin carrying the same exact strings. */
function ChartTwins({ dto }: { dto: BacktestRunDto }) {
  const mfeColumns = useMemo(() => histogramColumns(dto.mfe), [dto.mfe]);
  const maeColumns = useMemo(() => histogramColumns(dto.mae), [dto.mae]);

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
              <td className="mono">{cell.netPnl}</td>
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

/** One summary cell, exactly as delivered — `null` is an em dash, never a zero. */
function cell(summary: SummaryDto, key: keyof SummaryDto) {
  const value = summary[key];
  return value === null ? EM_DASH : String(value);
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
}: {
  before: SummaryDto;
  after: SummaryDto;
  badge?: React.ReactNode;
}) {
  return (
    <table className="coach-compare" aria-label="Before and after">
      {badge !== undefined && <caption className="compare-caption">{badge}</caption>}
      <thead>
        <tr>
          <th scope="col">metric</th>
          <th scope="col">before (parent)</th>
          <th scope="col">after (child)</th>
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
 * is newer than the catalog's, else the catalog's latest (`recentRuns` arrives
 * `created_at DESC`). A typed refusal renders its reason line instead of the
 * table — a child whose parent has no run is a state to report, not an error
 * to raise.
 */
function CompareWithParent({ version, run }: { version: LibraryVersion; run: RunState }) {
  const [outcome, setOutcome] = useState<CompareState>({ kind: "loading" });

  const persistedRunId = version.recentRuns[0]?.id ?? null;
  const freshRunId =
    run.kind === "done" &&
    run.dto.strategyVersionId === version.id &&
    (version.recentRuns[0] === undefined ||
      run.dto.createdAt >= version.recentRuns[0].createdAt)
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
          badge={
            outcome.dto.inputsDiffer ? (
              <span
                className="badge-inputs-differ"
                title={outcome.dto.inputsNote ?? undefined}
              >
                inputs differ
              </span>
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
