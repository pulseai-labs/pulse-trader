// The Backtest Lab screen's focused suite (r1.s3.w4, AC-1) — authored BEFORE
// the screen exists and driven RED, mirroring `LibraryScreen.test.tsx`'s
// `vi.mock("../bindings")` pattern: fixtures mirror the generated types in
// `ui/src/bindings.ts` exactly, and no command answers except through the
// mock. Every value asserted below is a fixture string, so a pass means the
// screen rendered the DTO's own exact strings — never a number the frontend
// computed, formatted or invented.
//
// Covers the spec's focused-suite list: catalog loading/error/empty/success,
// zero `runBacktestVersion` invocations from mount (StrictMode included),
// the indeterminate running label, `BusError` rendering incl. the structured
// `run_id`, selector-change clearing, the fresh-DTO render (provenance, KPIs,
// equity, regimes, both histograms, all 19 trade columns), table twins,
// pointer/keyboard tooltip parity with Left/Right/Home/End traversal, the
// focusable trade-table scroll region, and the ≥24px inline hit targets.

import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { StrictMode, useState } from "react";
import { flushSync } from "react-dom";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../bindings", () => ({
  commands: {
    libraryOverview: vi.fn(),
    runBacktestVersion: vi.fn(),
    // r1.s4.w3: the rail's two commands, mocked on the same terms — no command
    // answers except through this mock.
    coachTurn: vi.fn(),
    coachDecide: vi.fn(),
    // r2.s1.w4 C3: the child-vs-parent comparison, mocked on the same terms.
    compareChildRun: vi.fn(),
    // r2.s3.w5: the walk-forward gate and the fold-run read, same terms.
    runWalkForwardVersion: vi.fn(),
    getBacktestRun: vi.fn(),
    // #268: the persisted walk-forward run's own read, same terms again.
    getWalkForwardRun: vi.fn(),
  },
}));

import { commands } from "../bindings";
import type {
  BacktestRunDto,
  BusError,
  CoachDecisionDto,
  CoachSessionDto,
  CompareChildRunDto,
  HistogramBinDto,
  LibraryOverview,
  LibraryRunSummary,
  LibraryVersion,
  SummaryDto,
  WalkForwardFoldDto,
  WalkForwardRunDto,
} from "../bindings";
import BacktestLabScreen, { parseFoldCount } from "./BacktestLabScreen";
import { clearBacktestSelection } from "./backtestSelection";
import { ActiveOperationsProvider } from "../hooks/useActiveOperations";
import { RouteContent } from "../App";
import { decimal, price, rMultiple } from "../format/numbers";
import { timeUtc } from "../format/time";
import { resolveRoute } from "../routes";

const catalogMock = vi.mocked(commands.libraryOverview);
const runMock = vi.mocked(commands.runBacktestVersion);
const compareMock = vi.mocked(commands.compareChildRun);
const walkForwardMock = vi.mocked(commands.runWalkForwardVersion);
const getRunMock = vi.mocked(commands.getBacktestRun);
const reopenMock = vi.mocked(commands.getWalkForwardRun);

// ---------------------------------------------------------------------------
// The figure helper (r4.s5.w1): every formatted value renders the A2 display
// text with the DTO's own exact string as `title`/`data-exact`, so the two are
// asserted together — a formatting change cannot quietly drop the server's
// text, which is what keeps "the screen renders server text" proven.
// ---------------------------------------------------------------------------

function expectFigure(scope: Element, display: string, exact: string): void {
  const element = within(scope as HTMLElement).getByText(display);
  expect(element.getAttribute("data-exact")).toBe(exact);
  expect(element.getAttribute("title")).toBe(exact);
}

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like the generated types, values chosen to be
// distinct so a verbatim assertion can only pass on the real payload path.
// ---------------------------------------------------------------------------

function catalogVersion(
  id: string,
  parentId: string | null,
  latestRun: LibraryRunSummary | null = null,
  recentRuns: LibraryRunSummary[] = [],
): LibraryVersion {
  return {
    id,
    parentId,
    createdAt: "2026-08-20T10:00:00.000Z",
    createdBy: "human",
    agentName: null,
    hypothesis: null,
    dsl: {
      name: "RSI Oversold",
      direction: "long",
      entry: ["rsi(14) < 30"],
      filters: [],
      exits: ["stop 5%", "take profit 2R"],
      risk: ["risk 1% per trade", "max leverage 3x"],
    },
    stats: null,
    deltaVsParent: null,
    recentRuns,
    latestRun,
    certified: false,
    latestWalkForwardRunId: null,
  };
}

const CATALOG: LibraryOverview = {
  strategies: [
    {
      id: "strat-alpha",
      name: "Alpha Wave",
      createdAt: "2026-08-01T09:00:00.000Z",
      pinnedVersionId: null,
      versions: [catalogVersion("v-alpha-1", null), catalogVersion("v-alpha-2", "v-alpha-1")],
    },
    {
      id: "strat-beta",
      name: "Beta Break",
      createdAt: "2026-08-10T09:00:00.000Z",
      pinnedVersionId: null,
      versions: [catalogVersion("v-beta-1", null)],
    },
  ],
};

const MFE_COUNTS = [0, 1, 0, 2, 1, 3, 1, 0, 1, 0, 0, 0];
const MAE_COUNTS = [1, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0];

/** The 0.25-wide bin bounds as the DTO carries them — literal display strings,
 * the same text a two-decimal float format would produce, so the fixture needs
 * no float call (AC-5's own census reads this file). */
const BIN_BOUNDS = [
  "0.00", "0.25", "0.50", "0.75", "1.00", "1.25",
  "1.50", "1.75", "2.00", "2.25", "2.50", "2.75", "3.00",
] as const;

function bins(counts: readonly number[]): HistogramBinDto[] {
  return counts.map((count, i) => ({
    lower: BIN_BOUNDS[i],
    upper: BIN_BOUNDS[i + 1],
    count,
  }));
}

const SEEDED_RUN: BacktestRunDto = {
  runId: "run-9f2c41ab",
  strategyVersionId: "v-alpha-1",
  schemaVersion: 3,
  createdAt: "2026-09-02T10:15:30.123Z",
  pair: "BTCUSDT",
  primaryTimeframe: "15m",
  primaryDataVersion: "btcusdt-15m-v7",
  htfTimeframe: "4h",
  htfDataVersion: "btcusdt-4h-v3",
  // r3.s2.w4: the recorded daily snapshot, and the strategy-level flag the band
  // gates the HTF row on. This fixture is the "both series" shape.
  d1DataVersion: "btcusdt-1d-v1",
  usesHtf: true,
  firstOpenTimeMs: "1735689600000",
  lastCloseTimeMs: "1756684800000",
  holdoutStartMs: null,
  // #417 (PDR-0038 option A): this fixture is the UNFENCED shape — no freeze
  // was open at its read, so nothing was withheld and every total below is its
  // stored value. `WITHHELD_RUN` below is the fenced shape.
  withheldTradeCount: 0,
  totalsWithheld: false,
  startingEquity: "10000.00",
  takerFeeBps: "4.0000",
  slippageBps: "2.0000",
  funding: "snapshot_rates",
  // r3.s1.w4 (#142): the recorded symbol filters, exact decimal strings as
  // persisted. The not-recorded case overrides this to null below.
  symbolFilters: {
    lotStep: "0.001",
    minQty: "0.005",
    minNotional: "100",
    maxLeverage: "20",
  },
  // Unwindowed run — no recorded lead-in (r2.s3.w2).
  leadInFrom: null,
  engineFingerprint: "sha256:abc123def456",
  engineTarget: "aarch64-apple-darwin",
  resultContentHash: "sha256:9876fedcba",
  fingerprintWarning: null,
  netPnl: "-84.125",
  feesTotal: "26.400",
  fundingTotal: "-3.115",
  slippageTotal: "13.200",
  expectancy: "-4.20625",
  winRate: "0.4375",
  profitFactor: null,
  grossProfit: "180.000",
  grossLoss: "264.125",
  avgWin: "22.500",
  avgLoss: "33.016",
  maxDrawdown: "412.750",
  tradeCount: 8,
  winCount: 3,
  lossCount: 5,
  maxWinStreak: 2,
  maxLossStreak: 3,
  sharpe: null,
  sortino: null,
  skippedSubLot: 1,
  skippedSubNotional: 2,
  skippedLeverageCapped: 0,
  equity: [
    { timeMs: "1735689600000", equity: "10000.00" },
    { timeMs: "1736294400000", equity: "10042.75" },
    { timeMs: "1736899200000", equity: "9910.50" },
    { timeMs: "1737504000000", equity: "9915.875" },
  ],
  regimes: [
    { regime: "trending_up", tradeCount: 3, netPnl: "180.000" },
    { regime: "trending_down", tradeCount: 4, netPnl: "-294.250" },
    { regime: "ranging", tradeCount: 1, netPnl: "30.125" },
    { regime: "unknown", tradeCount: 0, netPnl: "0.000" },
  ],
  mfe: { binWidth: "0.25", bins: bins(MFE_COUNTS), underflow: 1, overflow: 2 },
  mae: { binWidth: "0.25", bins: bins(MAE_COUNTS), underflow: 2, overflow: 0 },
  trades: [
    {
      direction: "long",
      qty: "0.01500000",
      entryPrice: "64250.50",
      exitPrice: "63100.25",
      entrySignalTime: "1735690500000",
      entryFillTime: "1735691400000",
      exitSignalTime: "1735950300000",
      exitFillTime: "1735951200000",
      feesTotal: "6.600",
      fundingTotal: "-0.780",
      slippageTotal: "3.300",
      realizedPnl: "-166.396250",
      realizedR: "-1.658",
      mfeR: "0.6215",
      maeR: "-1.7025",
      exitReason: "stop_loss",
      source: "signal",
      regime: "trending_down",
      stopPrice: "61500.00",
    },
    {
      direction: "short",
      qty: "0.01000000",
      entryPrice: "64000.00",
      exitPrice: "62680.00",
      entrySignalTime: "1736980500000",
      entryFillTime: "1736981400000",
      exitSignalTime: "1737240300000",
      exitFillTime: "1737241200000",
      feesTotal: "6.330",
      fundingTotal: "-0.455",
      slippageTotal: "3.165",
      realizedPnl: "129.545000",
      realizedR: "1.290",
      mfeR: "1.4515",
      maeR: "-0.4025",
      exitReason: "take_profit",
      source: "signal",
      regime: "ranging",
      // A pre-0011-shaped row carries no recorded stop — the cell renders `—`.
      stopPrice: null,
    },
  ],
  // A standalone run carries no walk-forward membership (pre-0013 shape).
  walkForward: null,
};

/** A second fresh run, differing exactly where the null/warning paths need
 * coverage: htf pair null, a non-null `fingerprintWarning`, and the ratio
 * fields non-null. */
const WARN_RUN: BacktestRunDto = {
  ...SEEDED_RUN,
  runId: "run-77aa88bb",
  strategyVersionId: "v-alpha-2",
  htfTimeframe: null,
  htfDataVersion: null,
  // A strategy that reads the HTF over a row that records no HTF timeframe
  // (the pre-0006 shape): the row still renders, with the em dash.
  usesHtf: true,
  d1DataVersion: null,
  fingerprintWarning: "engine fingerprint changed since the prior run",
  profitFactor: "0.681",
  sharpe: -0.42,
  sortino: -0.31,
};

/** The FENCED shape (#417, PDR-0038 option A): an open freeze's holdout start
 *  cuts this run, so the server sent only the trades that cleared the fence and
 *  withheld every outcome aggregate and series.
 *
 *  Every withheld field is `null` — NOT a number the screen could recompute —
 *  so this fixture is also the proof that the screen renders the server's
 *  decision rather than deriving its own. The counts are the shape the Rust
 *  core produces: `Option<u32>` → `null`, `Vec` → `[]`. */
const WITHHELD_RUN: BacktestRunDto = {
  ...SEEDED_RUN,
  runId: "run-fenced-0001",
  holdoutStartMs: "1756684800000",
  withheldTradeCount: 2,
  totalsWithheld: true,
  netPnl: null,
  feesTotal: null,
  fundingTotal: null,
  slippageTotal: null,
  expectancy: null,
  winRate: null,
  profitFactor: null,
  grossProfit: null,
  grossLoss: null,
  avgWin: null,
  avgLoss: null,
  maxDrawdown: null,
  tradeCount: null,
  winCount: null,
  lossCount: null,
  maxWinStreak: null,
  maxLossStreak: null,
  sharpe: null,
  sortino: null,
  skippedSubLot: null,
  skippedSubNotional: null,
  skippedLeverageCapped: null,
  equity: [],
  regimes: [],
  mfe: null,
  mae: null,
  // Only the pre-fence trades cross — the run's stored rows that clear the fence.
  trades: [SEEDED_RUN.trades[0]],
};

const RUN_ERROR: BusError = {
  // The message deliberately does NOT contain the run id — the saved-as line
  // can only pass through the structured `run_id` field, never prose parsing.
  code: "data",
  message: "The saved run could not be read back.",
  run_id: "run-51234", session_id: null, child_run_id: null,
};

const CATALOG_ERROR: BusError = {
  code: "internal",
  message: "The library read failed.",
  run_id: null, session_id: null, child_run_id: null,
};

/** Render + wait for the catalog to land, then click Run on the seeded
 * payload. Returns the container for scoped class queries. */
async function renderRun(
  run: BacktestRunDto | BusError,
  catalog: LibraryOverview = CATALOG,
) {
  catalogMock.mockResolvedValue({ status: "ok", data: catalog });
  const isDto = "netPnl" in run;
  const settles: { status: "ok"; data: BacktestRunDto } | { status: "error"; error: BusError } =
    isDto ? { status: "ok", data: run } : { status: "error", error: run };
  const awaitText = isDto ? run.runId : run.message;
  runMock.mockResolvedValue(settles);
  const { container } = render(<BacktestLabScreen />);
  fireEvent.click(await screen.findByRole("button", { name: /run backtest/i }));
  await screen.findByText(awaitText);
  return container;
}

/** Choose a version through the picker the way a trader does — open it, type a
 * query, and click the `offset`-th option the filter leaves (0-based, in the
 * order the list shows). Every call site's own assertions pin WHICH version
 * that turned out to be; a query with fewer matches than `offset + 1` is a
 * broken test, not a silent no-op. */
async function chooseVersion(query: string, offset = 0) {
  const box = (await screen.findByRole("combobox")) as HTMLInputElement;
  fireEvent.focus(box);
  fireEvent.change(box, { target: { value: query } });
  const options = screen.getAllByRole("option");
  const option = options[offset] as HTMLElement | undefined;
  if (option === undefined) {
    throw new Error(
      `chooseVersion("${query}", ${offset}): the filter left ${options.length} option(s)`,
    );
  }
  // The picker keeps focus in the input, so the click lands without a blur.
  fireEvent.mouseDown(option);
  fireEvent.click(option);
}

beforeEach(() => {
  catalogMock.mockReset();
  runMock.mockReset();
  compareMock.mockReset();
  walkForwardMock.mockReset();
  getRunMock.mockReset();
  reopenMock.mockReset();
  // The remembered selection is a module singleton that outlives a screen by
  // design (#315); each test starts from no selection.
  clearBacktestSelection();
});

// ---------------------------------------------------------------------------
// Catalog states
// ---------------------------------------------------------------------------

describe("BacktestLabScreen (catalog states)", () => {
  it("renders a loading state while the catalog read is in flight", () => {
    catalogMock.mockImplementation(() => new Promise(() => {}));

    render(<BacktestLabScreen />);

    expect(screen.getByText(/loading/i)).toBeTruthy();
    // No option exists in any state except a real persisted version.
    expect(screen.queryByRole("option")).toBeNull();
  });

  it("renders the catalog BusError with its code family and message", async () => {
    catalogMock.mockResolvedValue({ status: "error", error: CATALOG_ERROR });

    render(<BacktestLabScreen />);

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("internal");
    expect(alert.textContent).toContain(CATALOG_ERROR.message);
    expect(screen.queryByRole("option")).toBeNull();
  });

  it("renders an honest empty state naming the Strategy Designer, and no sample option", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: { strategies: [] } });

    render(<BacktestLabScreen />);

    expect(await screen.findByText(/no strategies yet/i)).toBeTruthy();
    expect(screen.getByText(/strategy designer/i)).toBeTruthy();
    expect(screen.queryByRole("option")).toBeNull();
    expect(screen.queryByText(/sample/i)).toBeNull();
  });

  it("lists exactly the catalog's real (strategy, version) pairs, grouped, with the first version selected", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });

    const { container } = render(<BacktestLabScreen />);

    const box = (await screen.findByRole("combobox")) as HTMLInputElement;
    // The closed picker shows the current selection's own text.
    expect(box.value).toBe("v1 · 2026-08-20 UTC · v-alpha- · human");

    fireEvent.focus(box);
    expect(
      [...container.querySelectorAll(".vp-group")].map((header) => header.textContent),
    ).toEqual([
      "Alpha Wave",
      "Beta Break",
    ]);
    expect(
      screen.getAllByRole("option").map((option) => option.textContent),
    ).toEqual([
      "v1 · 2026-08-20 UTC · v-alpha- · human",
      "v2 · 2026-08-20 UTC · v-alpha- · human",
      "v1 · 2026-08-20 UTC · v-beta-1 · human",
    ]);
  });
});

// ---------------------------------------------------------------------------
// Run state machine
// ---------------------------------------------------------------------------

describe("BacktestLabScreen (run state machine)", () => {
  it("never invokes runBacktestVersion from mount — including a StrictMode double mount — before the Run click", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    runMock.mockResolvedValue({ status: "ok", data: SEEDED_RUN });

    render(
      <StrictMode>
        <BacktestLabScreen />
      </StrictMode>,
    );
    await screen.findByRole("combobox");
    // Flush microtasks so an erroneous mount-effect run would have landed.
    await Promise.resolve();

    expect(runMock).not.toHaveBeenCalled();
  });

  it("invokes runBacktestVersion exactly once per click, with the currently selected version id", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    runMock
      .mockResolvedValueOnce({ status: "ok", data: SEEDED_RUN })
      .mockResolvedValueOnce({
        status: "ok",
        data: { ...SEEDED_RUN, runId: "run-beta-001", strategyVersionId: "v-beta-1" },
      });
    render(<BacktestLabScreen />);

    fireEvent.click(await screen.findByRole("button", { name: /run backtest/i }));
    await screen.findByText("run-9f2c41ab");
    expect(runMock).toHaveBeenCalledTimes(1);
    expect(runMock).toHaveBeenLastCalledWith({ versionId: "v-alpha-1" });

    // A different version selected, then Run: the call uses the new id, and
    // the fresh DTO that lands is the new run's.
    await chooseVersion("beta");
    fireEvent.click(screen.getByRole("button", { name: /run backtest/i }));
    await screen.findByText("run-beta-001");
    expect(screen.queryByText("run-9f2c41ab")).toBeNull();
    expect(runMock).toHaveBeenCalledTimes(2);
    expect(runMock).toHaveBeenLastCalledWith({ versionId: "v-beta-1" });
  });

  it("shows an indeterminate running label with the button disabled — no percentage, no cancel", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    let resolveRun: (value: { status: "ok"; data: BacktestRunDto }) => void = () => {};
    runMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveRun = resolve;
        }),
    );
    render(<BacktestLabScreen />);

    fireEvent.click(await screen.findByRole("button", { name: /run backtest/i }));

    const button = screen.getByRole("button", { name: /running…/i }) as HTMLButtonElement;
    expect(button.disabled).toBe(true);
    expect(screen.queryByText(/cancel/i)).toBeNull();
    expect(screen.queryByText(/\d+\s*%/)).toBeNull();

    resolveRun({ status: "ok", data: SEEDED_RUN });
    await screen.findByText("run-9f2c41ab");
    expect(screen.queryByRole("button", { name: /running…/i })).toBeNull();
  });

  it("locks the selector while a run is in flight — a mid-run selection cannot re-enable Run", async () => {
    // The race this pins: a selector change resets the run state to idle, which
    // re-enables the Run button while the original uncancellable request is
    // still executing — a second click would launch an overlapping engine run
    // whose persisted result the token check silently drops. Locking the
    // selector for the duration removes the reachable path.
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    let resolveRun: (value: { status: "ok"; data: BacktestRunDto }) => void = () => {};
    runMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveRun = resolve;
        }),
    );
    render(<BacktestLabScreen />);

    const box = (await screen.findByRole("combobox")) as HTMLInputElement;
    expect(box.disabled).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: /run backtest/i }));

    expect(
      (screen.getByRole("button", { name: /running…/i }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(box.disabled).toBe(true);

    resolveRun({ status: "ok", data: SEEDED_RUN });
    await screen.findByText("run-9f2c41ab");
    // Settled: the selector unlocks alongside the button.
    expect(box.disabled).toBe(false);
  });

  it("renders a run BusError's code and message, and the structured run_id as a saved-run id — never parsed prose", async () => {
    await renderRun(RUN_ERROR);

    const alerts = screen.getAllByRole("alert");
    const runAlert = alerts.find((a) => a.textContent?.includes(RUN_ERROR.message));
    expect(runAlert).toBeDefined();
    expect(runAlert?.textContent).toContain("data");
    // The saved-run truth reads the `run_id` FIELD: the id renders even though
    // the message does not contain it, and names it as a run id.
    expect(screen.getByText("Saved as run run-51234")).toBeTruthy();
  });

  it("clears a rendered result when the selector changes — a stale result is not attributable to the new version", async () => {
    await renderRun(SEEDED_RUN);

    await chooseVersion("v2");

    expect(screen.queryByText("run-9f2c41ab")).toBeNull();
    expect(screen.queryByText("BTCUSDT")).toBeNull();
    expect(screen.queryByText("sha256:abc123def456")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// The fresh-DTO render
// ---------------------------------------------------------------------------

describe("BacktestLabScreen (fresh result render)", () => {
  it("renders the provenance band verbatim — pair, timeframes + data versions, date range, costs, engine, identity", async () => {
    const container = await renderRun(SEEDED_RUN);
    const band = container.querySelector(".bt-provenance");
    expect(band).not.toBeNull();
    const text = (band as HTMLElement).textContent ?? "";

    for (const exact of [
      "BTCUSDT",
      "15m",
      "btcusdt-15m-v7",
      "4h",
      "btcusdt-4h-v3",
      // r3.s2.w4: the daily row, when the run recorded one.
      "btcusdt-1d-v1",
      "snapshot_rates",
      "sha256:abc123def456",
      "aarch64-apple-darwin",
      "sha256:9876fedcba",
      "run-9f2c41ab",
      "v-alpha-1",
      "3",
    ]) {
      expect(text).toContain(exact);
    }
    // The A2-formatted values, each with the DTO's own string on the element —
    // the date range and created instant say UTC (r4.s5.w1).
    expectFigure(band as HTMLElement, "2025-01-01 → 2025-09-01 UTC", "1735689600000 → 1756684800000");
    expectFigure(band as HTMLElement, "10,000.00 USDT", "10000.00");
    expectFigure(band as HTMLElement, "2026-09-02 10:15 UTC", "2026-09-02T10:15:30.123Z");
    // r3.s1.w4 (#142): the recorded filters render their four values, formatted.
    // lot_step and min_qty are quantities, max_leverage leverage — min_notional
    // is the block's one MONEY amount, so it reads as USDT.
    expectFigure(band as HTMLElement, "0.001", "0.001");
    expectFigure(band as HTMLElement, "0.005", "0.005");
    expectFigure(band as HTMLElement, "100.00 USDT", "100");
    expectFigure(band as HTMLElement, "20", "20");
    expect(text).toContain("lot_step");
    expect(text).toContain("min_qty");
    expect(text).toContain("min_notional");
    expect(text).toContain("max_leverage");
    expect(text).toContain("4.0000 bps");
    expect(text).toContain("2.0000 bps");
    // The null fingerprintWarning renders no warning UI and no warning text.
    expect(container.querySelector(".bt-fp-warning")).toBeNull();
    expect(text).not.toContain("fingerprint changed");
  });

  it("renders 'not recorded' for a pre-0015 run whose filters the DTO lacks (r3.s1.w4)", async () => {
    const container = await renderRun({ ...SEEDED_RUN, symbolFilters: null });
    const band = container.querySelector(".bt-provenance") as HTMLElement;
    const text = band.textContent ?? "";
    expect(text).toContain("not recorded");
    expect(text).not.toContain("lot_step");
  });

  // -------------------------------------------------------------------------
  // r3.s2.w4 (#219) — the two series rows are gated independently
  // -------------------------------------------------------------------------
  //
  // The band's HTF row is gated on the STRATEGY (`usesHtf`), because every run
  // records an H4 selection even when its DSL has no `htf` operand; the D1 row
  // is gated on the RECORDED snapshot (`d1DataVersion`), because a run either
  // read a daily series — and pinned the version it read — or it did not. The
  // three shapes below are the ones the spec names.

  it("shows neither series row for a run with no HTF operand and no daily snapshot", async () => {
    const container = await renderRun({
      ...SEEDED_RUN,
      usesHtf: false,
      d1DataVersion: null,
    });
    const text =
      (container.querySelector(".bt-provenance") as HTMLElement).textContent ?? "";

    // The recorded H4 selection is still in the DTO — the row is what is gone.
    expect(text).not.toContain("4h");
    expect(text).not.toContain("btcusdt-4h-v3");
    expect(text).not.toContain("btcusdt-1d-v1");
    // The primary row is untouched by either gate.
    expect(text).toContain("15m");
    expect(text).toContain("btcusdt-15m-v7");
    // The em dash belongs to the HTF row alone: with the row gone it must not
    // appear as a stray value elsewhere in the band.
    expect(text.match(/—/g) ?? []).toHaveLength(0);
  });

  it("shows only the HTF row for an H4-only run that recorded no daily snapshot", async () => {
    const container = await renderRun({
      ...SEEDED_RUN,
      usesHtf: true,
      d1DataVersion: null,
    });
    const text =
      (container.querySelector(".bt-provenance") as HTMLElement).textContent ?? "";

    expect(text).toContain("4h");
    expect(text).toContain("btcusdt-4h-v3");
    expect(text).not.toContain("btcusdt-1d-v1");
    expect(text).not.toContain("—");
  });

  it("shows the HTF row and the daily row for a run that read both", async () => {
    const container = await renderRun(SEEDED_RUN);
    const text =
      (container.querySelector(".bt-provenance") as HTMLElement).textContent ?? "";

    expect(text).toContain("4h");
    expect(text).toContain("btcusdt-4h-v3");
    // The daily row names the version the run read; the timeframe is fixed at
    // `1d`, so the version is the whole value and no `1d` label is added.
    expect(text).toContain("btcusdt-1d-v1");
    expect(text).not.toContain("—");
  });

  it("renders every KPI tile from the DTO, with an em dash for null ratio fields", async () => {
    const container = await renderRun(SEEDED_RUN);
    const tiles = container.querySelector(".bt-kpis");
    expect(tiles).not.toBeNull();

    // Every money, percent and ratio tile is A2-formatted, the DTO's own string
    // on the element.
    const tileFor = (label: string) =>
      within(tiles as HTMLElement)
        .getByText(label)
        .closest(".bt-kpi") as HTMLElement;
    expectFigure(tileFor("Net P&L"), "−84.13 USDT", "-84.125");
    expectFigure(tileFor("Expectancy"), "−4.21 USDT", "-4.20625");
    expectFigure(tileFor("Win rate"), "43.8%", "0.4375");
    expectFigure(tileFor("Max drawdown"), "412.75 USDT", "412.750");
    expectFigure(tileFor("Fees"), "26.40 USDT", "26.400");
    expectFigure(tileFor("Funding"), "−3.12 USDT", "-3.115");
    expectFigure(tileFor("Slippage"), "13.20 USDT", "13.200");
    expectFigure(tileFor("Gross profit"), "180.00 USDT", "180.000");
    expectFigure(tileFor("Gross loss"), "264.13 USDT", "264.125");
    expectFigure(tileFor("Avg win"), "22.50 USDT", "22.500");
    expectFigure(tileFor("Avg loss"), "33.02 USDT", "33.016");

    // tradeCount / winCount / lossCount / streaks / skipped counters.
    for (const label of ["Trades", "Wins", "Losses", "Max win streak", "Max loss streak",
      "Skipped sub-lot", "Skipped sub-notional", "Skipped leverage-capped"]) {
      expect(within(tiles as HTMLElement).getByText(label)).toBeTruthy();
    }
    // profitFactor / sharpe / sortino are null in this run → em dash, never 0.
    for (const label of ["Profit factor", "Sharpe", "Sortino"]) {
      expect(tileFor(label).textContent).toContain("—");
      expect(tileFor(label).textContent).not.toMatch(/\d/);
    }
  });

  it("renders the equity chart: one line over the DTO's points plus the starting-equity reference", async () => {
    const container = await renderRun(SEEDED_RUN);
    const chart = container.querySelector(".bt-equity");
    expect(chart).not.toBeNull();

    // One polyline series (not two), a quiet area fill, and the reference line.
    expect((chart as HTMLElement).querySelectorAll("polyline")).toHaveLength(1);
    expect((chart as HTMLElement).querySelectorAll("polygon")).toHaveLength(1);
    const ref = (chart as HTMLElement).querySelector(".bt-eq-ref");
    expect(ref).not.toBeNull();
    // The reference reads in human units with the DTO's own exact string on
    // the title, and the readout shows the traversed point the same way
    // (point[0] before any traversal; the full point-to-point path is the
    // parity test below).
    const refLabel = (chart as HTMLElement).querySelector(".bt-eq-ref-label") as HTMLElement;
    expectFigure(refLabel, "10,000.00 USDT", "10000.00");
    const readout = (chart as HTMLElement).querySelector(".bt-eq-readout") as HTMLElement;
    expectFigure(readout, "2025-01-01 00:00 UTC", "1735689600000");
    expectFigure(readout, "10,000.00 USDT", "10000.00");
  });

  it("renders the four regime rows in the DTO's fixed order with fixed human labels, legend and direct labels", async () => {
    const container = await renderRun(SEEDED_RUN);
    const section = container.querySelector(".bt-regimes");
    expect(section).not.toBeNull();

    const rows = Array.from((section as HTMLElement).querySelectorAll(".bt-regime-row"));
    expect(rows.map((r) => r.querySelector(".bt-regime-name")?.textContent)).toEqual([
      "Trending up",
      "Trending down",
      "Ranging",
      "Unknown",
    ]);
    // Each row carries its own tradeCount and its A2-formatted netPnl, the DTO
    // string on the element.
    const trendingDown = within(rows[1] as HTMLElement);
    expect(trendingDown.getByText("4")).toBeTruthy();
    expectFigure(rows[1] as HTMLElement, "−294.25 USDT", "-294.250");

    // A compact 4-swatch legend, same fixed labels.
    const legend = (section as HTMLElement).querySelector(".bt-legend");
    expect(legend).not.toBeNull();
    expect(
      Array.from((legend as HTMLElement).querySelectorAll(".bt-legend-item")).map(
        (i) => i.textContent,
      ),
    ).toEqual(["Trending up", "Trending down", "Ranging", "Unknown"]);
  });

  it("renders MFE and MAE as separate single-hue histograms with the pinned column order and labels", async () => {
    const container = await renderRun(SEEDED_RUN);

    // #417: the histogram fields are `Option` on the wire, so an unfenced
    // fixture must prove them present before the columns can be measured.
    const mfe = SEEDED_RUN.mfe;
    const mae = SEEDED_RUN.mae;
    expect(mfe).not.toBeNull();
    expect(mae).not.toBeNull();
    for (const [cls, hist] of [
      [".bt-mfe", mfe!],
      [".bt-mae", mae!],
    ] as const) {
      const chart = container.querySelector(cls);
      expect(chart).not.toBeNull();
      const columns = Array.from((chart as HTMLElement).querySelectorAll(".bt-col"));
      // underflow + 12 finite bins + overflow = 14 columns.
      expect(columns).toHaveLength(14);

      const labels = columns.map((c) => c.querySelector(".bt-col-label")?.textContent);
      expect(labels[0]).toBe("< 0 R");
      expect(labels[13]).toBe("≥ 3 R");
      // The finite labels interpolate the DTO's own bound strings, verbatim.
      expect(labels[1]).toBe(`[${hist.bins[0].lower}, ${hist.bins[0].upper}) R`);
      expect(labels[12]).toBe(`[${hist.bins[11].lower}, ${hist.bins[11].upper}) R`);

      // Every column is the same hue (slot 1) — never cycled by rank.
      for (const bar of (chart as HTMLElement).querySelectorAll(".bt-col-bar")) {
        expect(bar.classList.contains("bt-slot-1")).toBe(true);
      }

      // Heights are the DTO's counts, shown as direct labels: the underflow
      // and overflow counts appear in their own columns.
      const under = within(columns[0] as HTMLElement).getByText(String(hist.underflow));
      const over = within(columns[13] as HTMLElement).getByText(String(hist.overflow));
      expect(under).toBeTruthy();
      expect(over).toBeTruthy();
    }

    // The two charts are separate — never one overlaid chart.
    expect(container.querySelectorAll(".bt-histogram")).toHaveLength(2);
  });

  it("renders the trade table: one row per trade in seq order with all 19 DTO columns, exact strings", async () => {
    const container = await renderRun(SEEDED_RUN);
    const region = container.querySelector(".bt-table-scroll");
    expect(region).not.toBeNull();

    const table = (region as HTMLElement).querySelector("table");
    expect(table).not.toBeNull();
    const headers = Array.from((table as HTMLElement).querySelectorAll("th")).map(
      (h) => h.textContent,
    );
    expect(headers).toEqual([
      "direction", "qty", "entryPrice", "exitPrice", "entrySignalTime", "entryFillTime",
      "exitSignalTime", "exitFillTime", "feesTotal", "fundingTotal", "slippageTotal",
      "realizedPnl", "realizedR", "mfeR", "maeR", "exitReason", "source", "regime",
      "stopPrice",
    ]);

    const rows = Array.from((table as HTMLElement).querySelectorAll("tbody tr"));
    expect(rows).toHaveLength(2);
    // Seq order: the formatted figure, the DTO's own string on the element.
    expect(within(rows[0] as HTMLElement).getAllByText("stop_loss").length).toBeGreaterThan(0);
    expectFigure(rows[0] as HTMLElement, "−166.40 USDT", "-166.396250");
    expectFigure(rows[0] as HTMLElement, "0.015", "0.01500000");
    // The recorded stop renders its A2 price, exact string on the element…
    expectFigure(rows[0] as HTMLElement, "61,500.00", "61500.00");
    expect(within(rows[1] as HTMLElement).getByText("take_profit")).toBeTruthy();
    expectFigure(rows[1] as HTMLElement, "+129.55 USDT", "129.545000");
    // …while a null stop (the pre-0011 row shape) renders the em dash.
    expect(within(rows[1] as HTMLElement).getByText("—")).toBeTruthy();
    // The four time columns render A2's `YYYY-MM-DD HH:mm` UTC, the epoch-ms
    // text on the element (r4.s5.w1's Dispatch-2 clarification).
    expectFigure(rows[0] as HTMLElement, "2025-01-01 00:15 UTC", "1735690500000");
    expectFigure(rows[0] as HTMLElement, "2025-01-01 00:30 UTC", "1735691400000");
  });

  it("renders an em dash for a null HTF pair and visibly flags a non-null fingerprintWarning", async () => {
    const container = await renderRun(WARN_RUN);
    const band = container.querySelector(".bt-provenance") as HTMLElement;
    const text = band.textContent ?? "";

    // htfTimeframe/htfDataVersion null → em dash beside the primary pair,
    // never a guessed timeframe. The primary pair still renders.
    expect(text).toContain("15m");
    expect(text).toContain("—");
    expect(text).not.toContain("4h");

    // The warning is the pre-save comparison's, flagged as such.
    const warning = container.querySelector(".bt-fp-warning");
    expect(warning).not.toBeNull();
    expect((warning as HTMLElement).textContent).toContain(
      "engine fingerprint changed since the prior run",
    );
    // And the non-null ratio fields render their A2 values, not em dashes, the
    // DTO's own text on the element.
    const tiles = container.querySelector(".bt-kpis") as HTMLElement;
    expectFigure(tiles, "0.68", "0.681");
    expectFigure(tiles, "−0.42", "-0.42");
  });
});

// ---------------------------------------------------------------------------
// #417 (PDR-0038 option A) — the withheld totals and the fenced rows
// ---------------------------------------------------------------------------

describe("BacktestLabScreen (#417 — a withheld run says so)", () => {
  it("shows one sentence in place of the totals, naming the count and the holdout date", async () => {
    const container = await renderRun(WITHHELD_RUN);

    // The ONE sentence, verbatim from the ruling's own example, with the date
    // rendered the way the Chart tab renders it (`dateUtc` -> `YYYY-MM-DD`).
    const notes = container.querySelectorAll(".bt-withheld");
    expect(notes).toHaveLength(1);
    expect((notes[0] as HTMLElement).textContent).toBe(
      "2 trades and the run totals withheld: holdout from 2025-09-01 UTC",
    );

    // No total renders at all: not a zero, not an em dash grid, not a value the
    // frontend derived. The KPI section holds the sentence and nothing else.
    const tiles = container.querySelector(".bt-kpis") as HTMLElement;
    expect(tiles.textContent).toContain("2 trades and the run totals withheld");
    expect(tiles.querySelectorAll(".bt-kpi")).toHaveLength(0);
    // The stored figures the fixture still carries nowhere appear.
    for (const withheld of ["−84.13", "-84.125", "43.8%", "0.4375", "412.75"]) {
      expect(tiles.textContent).not.toContain(withheld);
    }
  });

  it("draws no equity curve, no regimes and no histograms for a withheld run", async () => {
    const container = await renderRun(WITHHELD_RUN);

    expect(container.querySelector(".bt-equity")).toBeNull();
    expect(container.querySelector(".bt-histogram")).toBeNull();
    // The twins carry the same emptiness — no table body stands in for a total.
    expect(container.querySelector(".bt-twin-equity tbody")?.children.length ?? 0).toBe(0);
    expect(container.querySelector(".bt-twin-regimes tbody")?.children.length ?? 0).toBe(0);
  });

  it("still lists the trades that cleared the fence, with their exact values", async () => {
    const container = await renderRun(WITHHELD_RUN);

    const rows = container.querySelectorAll(".bt-trades tbody tr");
    expect(rows).toHaveLength(1);
    // The row is the DTO's own stored trade, unaltered: the entry price and the
    // realized R render as the A2 display text, with the DTO's exact string on
    // the element (the `Figure` contract) — never a value the screen derived.
    const row = rows[0] as HTMLElement;
    const entry = within(row).getByText(price(WITHHELD_RUN.trades[0].entryPrice));
    expect(entry.getAttribute("data-exact")).toBe(WITHHELD_RUN.trades[0].entryPrice);
    const r = within(row).getByText(rMultiple(WITHHELD_RUN.trades[0].realizedR));
    expect(r.getAttribute("data-exact")).toBe(WITHHELD_RUN.trades[0].realizedR);
    // The row-level note names the withheld rows, in the Chart tab's own shape.
    const note = container.querySelector(".bt-withheld-rows") as HTMLElement;
    expect(note.textContent).toBe("2 trades withheld: inside the holdout");
  });

  it("names the fence at the trade table rather than reading as a run with no trades", async () => {
    // A fenced run whose EVERY trade is withheld: the empty table must say which
    // empty it is (PDR-0011) — never "the run recorded no trades".
    const container = await renderRun({ ...WITHHELD_RUN, trades: [] });
    const table = container.querySelector(".bt-trades tbody") as HTMLElement;
    expect(table.textContent).toContain("No trades in this run fall before the holdout.");
    expect(table.textContent).not.toContain("The run recorded no trades.");
  });

  it("renders the unfenced run exactly as before — no sentence, every total present", async () => {
    // The control: `totalsWithheld: false` with no freeze open. This is the
    // byte-for-byte path the ruling's step 4 protects.
    const container = await renderRun(SEEDED_RUN);
    expect(container.querySelector(".bt-withheld")).toBeNull();
    const tiles = container.querySelector(".bt-kpis") as HTMLElement;
    expect(tiles.querySelectorAll(".bt-kpi").length).toBeGreaterThan(0);
    expect(within(tiles).getByText("−84.13 USDT").getAttribute("data-exact")).toBe("-84.125");
    expect(container.querySelector(".bt-equity")).not.toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Table twins, parity, focus, hit targets
// ---------------------------------------------------------------------------

describe("BacktestLabScreen (twins, parity, focus, hit targets)", () => {
  it("carries an in-DOM table twin for every chart with the same exact values", async () => {
    const container = await renderRun(SEEDED_RUN);

    // Equity twin: one row per point, exact strings.
    const eqTwin = container.querySelector(".bt-twin-equity") as HTMLElement;
    expect(eqTwin).not.toBeNull();
    for (const p of SEEDED_RUN.equity) {
      expect(within(eqTwin).getAllByText(p.timeMs).length).toBeGreaterThan(0);
      expect(within(eqTwin).getAllByText(p.equity).length).toBeGreaterThan(0);
    }

    // Regime twin: four rows, label + tradeCount + the A2 netPnl with the DTO's
    // own string on the element.
    const rgTwin = container.querySelector(".bt-twin-regimes") as HTMLElement;
    expect(rgTwin).not.toBeNull();
    const regimeNetPnl: Record<string, string> = {
      "180.000": "180.00 USDT",
      "-294.250": "−294.25 USDT",
      "30.125": "30.13 USDT",
      "0.000": "0.00 USDT",
    };
    for (const cell of SEEDED_RUN.regimes) {
      const row = rgTwin.querySelector(`[data-regime="${cell.regime}"]`);
      expect(row).not.toBeNull();
      expect((row as HTMLElement).textContent).toContain(String(cell.tradeCount));
      expectFigure(row as HTMLElement, regimeNetPnl[cell.netPnl], cell.netPnl);
    }

    // Histogram twins: underflow + 12 bins + overflow with their counts.
    const twinMfe = SEEDED_RUN.mfe;
    const twinMae = SEEDED_RUN.mae;
    expect(twinMfe).not.toBeNull();
    expect(twinMae).not.toBeNull();
    for (const [cls, hist] of [
      [".bt-twin-mfe", twinMfe!],
      [".bt-twin-mae", twinMae!],
    ] as const) {
      const twin = container.querySelector(cls) as HTMLElement;
      expect(twin).not.toBeNull();
      const rows = twin.querySelectorAll("tbody tr");
      expect(rows).toHaveLength(14);
      expect(rows[0].textContent).toContain(String(hist.underflow));
      expect(rows[13].textContent).toContain(String(hist.overflow));
      expect(rows[1].textContent).toContain(`[${hist.bins[0].lower}, ${hist.bins[0].upper}) R`);
      expect(rows[1].textContent).toContain(String(hist.bins[0].count));
    }
  });

  it("exposes identical human-unit readouts for keyboard and pointer, exact strings on the title", async () => {
    const container = await renderRun(SEEDED_RUN);
    const chart = container.querySelector(".bt-equity") as HTMLElement;
    const plot = chart.querySelector(".bt-eq-plot") as HTMLElement;
    const readout = chart.querySelector(".bt-eq-readout") as HTMLElement;
    expect(plot).not.toBeNull();
    expect(readout).not.toBeNull();

    // Keyboard: Left/Right step point-to-point, Home/End jump to first/last.
    plot.focus();
    fireEvent.keyDown(plot, { key: "ArrowRight" });
    expectFigure(readout, "2025-01-08 00:00 UTC", "1736294400000");
    expectFigure(readout, "10,042.75 USDT", "10042.75");
    fireEvent.keyDown(plot, { key: "End" });
    expectFigure(readout, "2025-01-22 00:00 UTC", "1737504000000");
    fireEvent.keyDown(plot, { key: "Home" });
    expectFigure(readout, "2025-01-01 00:00 UTC", "1735689600000");
    fireEvent.keyDown(plot, { key: "ArrowRight" });
    fireEvent.keyDown(plot, { key: "ArrowLeft" });
    expectFigure(readout, "2025-01-01 00:00 UTC", "1735689600000");

    // Pointer: hovering the third point's hit marker drives the SAME readout
    // element with the same formatted text and exact strings — parity is
    // structural.
    const hits = chart.querySelectorAll(".bt-eq-hit");
    expect(hits).toHaveLength(SEEDED_RUN.equity.length);
    fireEvent.mouseOver(hits[2]);
    expectFigure(readout, "2025-01-15 00:00 UTC", "1736899200000");
    expectFigure(readout, "9,910.50 USDT", "9910.50");

    // The starting-equity reference line's label reads in USDT too, with the
    // DTO's own string one hover away.
    const refLabel = chart.querySelector(".bt-eq-ref-label") as HTMLElement;
    expect(refLabel).not.toBeNull();
    expectFigure(refLabel, "10,000.00 USDT", "10000.00");
  });

  it("makes the equity plot and the trade-table scroll region focus stops in visual order", async () => {
    const container = await renderRun(SEEDED_RUN);

    const picker = screen.getByRole("combobox");
    const run = screen.getByRole("button", { name: /run backtest/i });
    const plot = container.querySelector(".bt-eq-plot") as HTMLElement;
    const region = container.querySelector(".bt-table-scroll") as HTMLElement;

    for (const el of [plot, region]) {
      expect(el.tabIndex).toBe(0);
    }
    // Focus order follows visual order: selector → Run → plot → table region.
    const follows = (a: HTMLElement, b: HTMLElement) =>
      (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
    expect(follows(picker, run)).toBe(true);
    expect(follows(run, plot)).toBe(true);
    expect(follows(plot, region)).toBe(true);

    // The scroll region is named, and scrolls rather than clips.
    expect(region.getAttribute("role")).toBe("region");
    expect(region.getAttribute("aria-label")).toBeTruthy();
    expect(region.className).toContain("bt-table-scroll");
  });

  it("gives every histogram column and regime row an interactive-dimension hit target of at least 24px", async () => {
    const container = await renderRun(SEEDED_RUN);

    const columns = container.querySelectorAll(".bt-col");
    expect(columns.length).toBeGreaterThan(0);
    for (const col of columns) {
      // chart geometry: the painted hit target's own pixels, not a displayed figure
      expect(parseFloat((col as HTMLElement).style.minWidth)).toBeGreaterThanOrEqual(24);
    }
    const rows = container.querySelectorAll(".bt-regime-row");
    expect(rows.length).toBeGreaterThan(0);
    for (const row of rows) {
      // chart geometry: the painted hit target's own pixels, not a displayed figure
      expect(parseFloat((row as HTMLElement).style.minHeight)).toBeGreaterThanOrEqual(24);
    }
  });
});

// ---------------------------------------------------------------------------
// Route registration (the real ROUTES table)
// ---------------------------------------------------------------------------

describe("the backtest route entry (the real ROUTES table)", () => {
  it("mounts the screen through RouteContent", async () => {
    const route = resolveRoute("/backtest");
    expect(route).toBeDefined();
    expect(route?.element).toBeDefined();

    catalogMock.mockResolvedValue({ status: "ok", data: { strategies: [] } });
    render(<RouteContent route={route} />);
    expect(await screen.findByText(/no strategies yet/i)).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// The coach rail (r1.s4.w3) — opt-in, one proposal or one recorded failure
// ---------------------------------------------------------------------------
//
// Same discipline as the run tests above: every value asserted is a fixture
// string, so a pass means the rail rendered the DTO's own text. The `recovery`
// fixtures are deliberately NOT the real recovery sentences — a rail that mapped
// failure kind to recovery text in TypeScript would render its own copy and pass
// a laxer assertion; these can only pass by rendering the DTO's field.

const PARENT_SUMMARY: SummaryDto = {
  tradeCount: 8,
  winCount: 3,
  lossCount: 5,
  winRate: "0.375",
  grossProfit: "180.000",
  grossLoss: "264.125",
  netPnl: "-84.125",
  profitFactor: null,
  avgWin: "60.000",
  avgLoss: "52.825",
  expectancy: "-10.515625",
  maxDrawdown: "412.750",
  maxWinStreak: 2,
  maxLossStreak: 3,
  commissionTotal: "26.400",
  fundingTotal: "-3.115",
  sharpe: null,
  sortino: null,
};

const CHILD_SUMMARY: SummaryDto = {
  ...PARENT_SUMMARY,
  tradeCount: 6,
  winCount: 4,
  lossCount: 2,
  winRate: "0.666",
  netPnl: "121.500",
  expectancy: "20.250",
};

function proposedSession(overrides: Partial<CoachSessionDto> = {}): CoachSessionDto {
  return {
    sessionId: "sess-77",
    runId: SEEDED_RUN.runId,
    versionId: "v-alpha-1",
    outcome: "proposed",
    proposal: {
      mutation: { path: "entry.lhs.indicator.rsi.period", newValue: "21" },
      hypothesis: "a slower RSI trades less often on this chop",
      disposition: "proposed",
      childVersionId: null,
      acceptedRunId: null,
      acceptFailure: null,
    },
    failure: null,
    llmCallId: "call-abc",
    cost: { amount: "0.0184", currency: "CNY" },
    promptVersion: "b7f3ac91",
    createdAt: "2026-09-05T10:15:00.000Z",
    ...overrides,
  };
}

function failedSession(kind: string, detail: string, recovery: string): CoachSessionDto {
  return {
    ...proposedSession(),
    outcome: "failed",
    proposal: null,
    failure: { kind, detail, recovery },
  };
}

const coachTurnMock = vi.mocked(commands.coachTurn);
const coachDecideMock = vi.mocked(commands.coachDecide);

/** Render, run, then open the rail — the trader's actual sequence. */
async function openRail(session: CoachSessionDto) {
  const container = await renderRun(SEEDED_RUN);
  coachTurnMock.mockResolvedValue({ status: "ok", data: session });
  fireEvent.click(screen.getByRole("button", { name: /ask the coach/i }));
  await screen.findByRole("region", { name: /coach/i });
  return container;
}

describe("BacktestLabScreen (the coach rail)", () => {
  beforeEach(() => {
    coachTurnMock.mockReset();
    coachDecideMock.mockReset();
  });

  it("is opt-in: no coach command is invoked from mount, StrictMode included, and no rail is shown", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    runMock.mockResolvedValue({ status: "ok", data: SEEDED_RUN });

    render(
      <StrictMode>
        <BacktestLabScreen />
      </StrictMode>,
    );
    await screen.findByRole("button", { name: /run backtest/i });

    expect(coachTurnMock).not.toHaveBeenCalled();
    expect(coachDecideMock).not.toHaveBeenCalled();
    expect(screen.queryByRole("region", { name: /coach/i })).toBeNull();
  });

  it("offers the coach only beneath a persisted run, and mints the session id itself", async () => {
    const container = await renderRun(SEEDED_RUN);
    coachTurnMock.mockResolvedValue({ status: "ok", data: proposedSession() });

    fireEvent.click(screen.getByRole("button", { name: /ask the coach/i }));
    await screen.findByRole("region", { name: /coach/i });

    expect(coachTurnMock).toHaveBeenCalledTimes(1);
    const request = coachTurnMock.mock.calls[0][0];
    expect(request.runId).toBe(SEEDED_RUN.runId);
    // A UUID minted by the DESKTOP — the backend never mints one.
    expect(request.sessionId).toMatch(
      /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i,
    );
    expect(container).toBeTruthy();
  });

  it("shows exactly ONE proposed change with its hypothesis and the three actions", async () => {
    await openRail(proposedSession());

    const rail = screen.getByRole("region", { name: /coach/i });
    expect(within(rail).getByText("entry.lhs.indicator.rsi.period")).toBeTruthy();
    expect(within(rail).getByText("21")).toBeTruthy();
    expect(within(rail).getByText(/a slower RSI trades less often/i)).toBeTruthy();
    // One change, not a list of them.
    expect(rail.querySelectorAll(".coach-change").length).toBe(1);
    expect(within(rail).getByRole("button", { name: /^modify$/i })).toBeTruthy();
    expect(within(rail).getByRole("button", { name: /^reject$/i })).toBeTruthy();
    expect(within(rail).getByRole("button", { name: /^accept$/i })).toBeTruthy();
  });

  it("keeps the call's cost, currency and prompt version visible", async () => {
    await openRail(proposedSession());

    const rail = screen.getByRole("region", { name: /coach/i });
    expect(rail.textContent).toContain("0.0184");
    expect(rail.textContent).toContain("CNY");
    expect(rail.textContent).toContain("b7f3ac91");
  });

  it("renders a typed failure with the BACKEND's recovery, never one it derived itself", async () => {
    // A recovery string no frontend mapping would ever produce.
    await openRail(
      failedSession(
        "inapplicable_advice",
        "the coach asked for a volume filter",
        "RECOVERY-FROM-THE-DTO-ONLY",
      ),
    );

    const rail = screen.getByRole("region", { name: /coach/i });
    expect(rail.textContent).toContain("inapplicable_advice");
    expect(rail.textContent).toContain("the coach asked for a volume filter");
    expect(rail.textContent).toContain("RECOVERY-FROM-THE-DTO-ONLY");
    // No proposal card on a failed turn — one outcome, never both.
    expect(rail.querySelector(".coach-change")).toBeNull();
  });

  it("re-validates a modification through coach_decide on the SAME session id, and shows the stored value", async () => {
    await openRail(proposedSession());
    const modified: CoachDecisionDto = {
      session: {
        ...proposedSession(),
        proposal: {
          mutation: { path: "entry.lhs.indicator.rsi.period", newValue: "9" },
          hypothesis: "a slower RSI trades less often on this chop",
          disposition: "modified",
          childVersionId: null,
          acceptedRunId: null,
          acceptFailure: null,
        },
      },
      accepted: null,
    };
    coachDecideMock.mockResolvedValue({ status: "ok", data: modified });

    fireEvent.click(screen.getByRole("button", { name: /^modify$/i }));
    const input = screen.getByLabelText(/new value/i);
    fireEvent.change(input, { target: { value: "9" } });
    fireEvent.click(screen.getByRole("button", { name: /re-validate|apply/i }));

    await screen.findByText("9");
    expect(coachDecideMock).toHaveBeenCalledTimes(1);
    const request = coachDecideMock.mock.calls[0][0];
    expect(request.sessionId).toBe("sess-77");
    expect(request.action).toEqual({
      kind: "modify",
      path: "entry.lhs.indicator.rsi.period",
      newValue: "9",
    });
  });

  it("records a rejection and stops offering the proposal's actions", async () => {
    await openRail(proposedSession());
    coachDecideMock.mockResolvedValue({
      status: "ok",
      data: {
        session: {
          ...proposedSession(),
          proposal: {
            mutation: { path: "entry.lhs.indicator.rsi.period", newValue: "21" },
            hypothesis: "a slower RSI trades less often on this chop",
            disposition: "rejected",
            childVersionId: null,
            acceptedRunId: null,
            acceptFailure: null,
          },
        },
        accepted: null,
      },
    });

    fireEvent.click(screen.getByRole("button", { name: /^reject$/i }));

    await screen.findByText(/rejected/i);
    expect(coachDecideMock.mock.calls[0][0].action).toEqual({ kind: "reject" });
    expect(screen.queryByRole("button", { name: /^accept$/i })).toBeNull();
  });

  it("shows the accepted child beside its parent, with both expectancies and both links", async () => {
    await openRail(proposedSession());
    coachDecideMock.mockResolvedValue({
      status: "ok",
      data: {
        session: {
          ...proposedSession(),
          proposal: {
            mutation: { path: "entry.lhs.indicator.rsi.period", newValue: "21" },
            hypothesis: "a slower RSI trades less often on this chop",
            disposition: "accepted",
            childVersionId: "v-child-9",
            acceptedRunId: "run-child-9",
            acceptFailure: null,
          },
        },
        accepted: {
          childVersionId: "v-child-9",
          acceptedRunId: "run-child-9",
          before: PARENT_SUMMARY,
          after: CHILD_SUMMARY,
          readBack: "ok",
          walkForwardRunId: null,
        },
      },
    });

    fireEvent.click(screen.getByRole("button", { name: /^accept$/i }));

    const comparison = await screen.findByRole("table", { name: /before and after/i });
    // Both sides, A2-formatted, each cell carrying the DTO's own string — no
    // delta, no invented percent. The counts render as delivered.
    expectFigure(comparison, "−10.52 USDT", "-10.515625");
    expectFigure(comparison, "+20.25 USDT", "20.250");
    expectFigure(comparison, "37.5%", "0.375");
    expectFigure(comparison, "66.6%", "0.666");

    const rail = screen.getByRole("region", { name: /coach/i });
    expect(within(rail).getByRole("link", { name: /child version/i }).getAttribute("href")).toBe(
      "#/library",
    );
    expect(rail.textContent).toContain("v-child-9");
    expect(rail.textContent).toContain("run-child-9");
    expect(coachDecideMock.mock.calls[0][0].action).toEqual({ kind: "accept" });
  });

  it("selects the accepted child in the Lab, so the child run is one click from being re-run", async () => {
    await openRail(proposedSession());
    coachDecideMock.mockResolvedValue({
      status: "ok",
      data: {
        session: {
          ...proposedSession(),
          proposal: {
            mutation: { path: "entry.lhs.indicator.rsi.period", newValue: "21" },
            hypothesis: "a slower RSI trades less often on this chop",
            disposition: "accepted",
            childVersionId: "v-alpha-2",
            acceptedRunId: "run-child-9",
            acceptFailure: null,
          },
        },
        accepted: {
          childVersionId: "v-alpha-2",
          acceptedRunId: "run-child-9",
          before: PARENT_SUMMARY,
          after: CHILD_SUMMARY,
          readBack: "ok",
          walkForwardRunId: null,
        },
      },
    });

    fireEvent.click(screen.getByRole("button", { name: /^accept$/i }));
    await screen.findByRole("table", { name: /before and after/i });

    // The Lab's own selection moves to the child — the rail's second link is a
    // real affordance in THIS screen, not a second route.
    //
    // Awaited, because selecting the child REFETCHES the catalog first: the child
    // was minted after the catalog was read, so selecting it against the stale
    // list would set the selector to an id no option carries.
    fireEvent.click(screen.getByRole("button", { name: /select the child/i }));
    await waitFor(() => {
      expect((screen.getByRole("combobox") as HTMLInputElement).value).toBe(
        "v2 · 2026-08-20 UTC · v-alpha- · human",
      );
    });
  });

  it("lets a failed turn be asked again — the recovery the card states is one the rail can perform", async () => {
    await openRail(failedSession("interrupted", "the turn never settled", "start a new coaching session"));
    await screen.findByText(/start a new coaching session/i);

    // The card's recovery is an ACTION, not advice the rail cannot act on: the
    // settled record is cleared and a second turn actually starts.
    coachTurnMock.mockResolvedValue({ status: "ok", data: proposedSession() });
    fireEvent.click(screen.getByRole("button", { name: /ask the coach again/i }));

    await screen.findByText(/a slower RSI trades less often on this chop/i);
    expect(coachTurnMock).toHaveBeenCalledTimes(2);
    // A settled session is terminal whatever settled it, so the retry carries a NEW
    // id rather than asking again under one the backend has already recorded an
    // outcome for.
    expect(coachTurnMock.mock.calls[1][0].sessionId).not.toBe(
      coachTurnMock.mock.calls[0][0].sessionId,
    );
  });

  it("checks the CONTESTED session when busy, never a fresh one", async () => {
    await renderRun(SEEDED_RUN);
    coachTurnMock.mockResolvedValue({
      status: "error",
      error: {
        code: "busy",
        message: "coach turn sess-elsewhere for this run has not settled yet; check again to see where it got to",
        run_id: null,
        session_id: "sess-elsewhere",
        child_run_id: null,
      },
    });
    fireEvent.click(screen.getByRole("button", { name: /ask the coach/i }));
    await screen.findByText(/has not settled yet/i);

    // The button offers the OTHER turn's result. Asking under a fresh id would ask a
    // new question and bill for it instead.
    coachTurnMock.mockResolvedValue({ status: "ok", data: proposedSession() });
    fireEvent.click(screen.getByRole("button", { name: /check again/i }));
    await screen.findByText(/a slower RSI trades less often on this chop/i);
    expect(coachTurnMock.mock.calls[1][0].sessionId).toBe("sess-elsewhere");
  });

  it("offers a re-run, not another ask, when the parent run has no input provenance", async () => {
    await openRail(
      failedSession(
        "missing_backtest_inputs",
        "this run predates input provenance",
        "run this version again, then ask the coach",
      ),
    );
    await screen.findByRole("button", { name: /run this version again/i });

    // Asking again cannot recover this: the parent run is immutable, so it can never
    // acquire the provenance it lacks and every re-ask records the same failure. The
    // card offers the action its own recovery names.
    expect(screen.queryByRole("button", { name: /ask the coach again/i })).toBeNull();
    runMock.mockResolvedValue({ status: "ok", data: SEEDED_RUN });
    fireEvent.click(screen.getByRole("button", { name: /run this version again/i }));
    expect(runMock).toHaveBeenCalledTimes(2);
  });

  it("gives an operational failure a recovery too, so no failure card is a dead end", async () => {
    await renderRun(SEEDED_RUN);
    coachTurnMock.mockResolvedValue({
      status: "error",
      error: { code: "internal", message: "the provider is unreachable", run_id: null, session_id: null, child_run_id: null },
    });
    fireEvent.click(screen.getByRole("button", { name: /ask the coach/i }));

    // No typed coach failure exists to name its own recovery — the generic one is
    // stated rather than left blank.
    await screen.findByText(/the provider is unreachable/i);
    expect(screen.getByText(/what to do/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /ask the coach again/i })).toBeTruthy();
  });

  it("keeps the proposal when a decision is refused, and shows the reason on the card", async () => {
    await openRail(proposedSession());
    coachDecideMock.mockResolvedValue({
      status: "error",
      error: { code: "validation", message: "`abc` is not a whole-number period", run_id: null, session_id: null, child_run_id: null },
    });

    fireEvent.click(screen.getByRole("button", { name: /^modify$/i }));
    fireEvent.change(screen.getByLabelText(/new value/i), { target: { value: "abc" } });
    fireEvent.click(screen.getByRole("button", { name: /re-validate|apply/i }));

    // The refusal appears ON the card. The proposal it refused is still there to
    // correct — replacing it with the message would throw away what was edited.
    await screen.findByText(/is not a whole-number period/i);
    expect(screen.getByText(/a slower RSI trades less often on this chop/i)).toBeTruthy();
    expect(screen.getByRole("button", { name: /^accept$/i })).toBeTruthy();
  });

  it("shows a busy refusal as its own transient state with a way to check again", async () => {
    await renderRun(SEEDED_RUN);
    coachTurnMock.mockResolvedValue({
      status: "error",
      error: {
        code: "busy",
        message: "coach turn sess-other for this run has not settled yet; check again",
        run_id: null,
        session_id: null,
        child_run_id: null,
      },
    });
    fireEvent.click(screen.getByRole("button", { name: /ask the coach/i }));

    // Not a failure — nothing broke. Not `running` either: THIS record settled, so
    // nothing further will arrive to move it along and the trader needs a way to
    // pick the other invocation's result up.
    await screen.findByText(/has not settled yet/i);
    expect(screen.queryByText(/what to do/i)).toBeNull();

    // And the button DOES something: a busy record has settled, so re-asking is
    // the only way the trader picks the other invocation's result up.
    coachTurnMock.mockResolvedValue({ status: "ok", data: proposedSession() });
    fireEvent.click(screen.getByRole("button", { name: /check again/i }));
    await screen.findByText(/a slower RSI trades less often on this chop/i);
    expect(coachTurnMock).toHaveBeenCalledTimes(2);
  });

  it("names each decision in flight — modifying, rejecting, accepting", async () => {
    await openRail(proposedSession());
    coachDecideMock.mockImplementation(() => new Promise(() => {}));

    fireEvent.click(screen.getByRole("button", { name: /^reject$/i }));
    await screen.findByText(/recording the rejection/i);
  });

  it("says so when an accepted child run could not be read back, and still names both ids", async () => {
    await openRail(proposedSession());
    coachDecideMock.mockResolvedValue({
      status: "ok",
      data: {
        session: {
          ...proposedSession(),
          proposal: {
            mutation: { path: "entry.lhs.indicator.rsi.period", newValue: "21" },
            hypothesis: "a slower RSI trades less often on this chop",
            disposition: "accepted",
            childVersionId: "v-child-9",
            acceptedRunId: "run-child-9",
            acceptFailure: null,
          },
        },
        accepted: {
          childVersionId: "v-child-9",
          acceptedRunId: "run-child-9",
          before: PARENT_SUMMARY,
          after: null,
          readBack: { failure: "the saved child run could not be re-read" },
          walkForwardRunId: null,
        },
      },
    });

    fireEvent.click(screen.getByRole("button", { name: /^accept$/i }));

    const rail = await screen.findByRole("region", { name: /coach/i });
    await screen.findByText(/could not be read back/i);
    expect(rail.textContent).toContain("v-child-9");
    expect(rail.textContent).toContain("run-child-9");
    expect(rail.textContent).toContain("the saved child run could not be re-read");
  });

  it("renders a refused overlapping coach call as already-running, not as a failure", async () => {
    await renderRun(SEEDED_RUN);
    coachTurnMock.mockResolvedValue({
      status: "error",
      error: {
        code: "busy",
        message: "a coach operation for session `sess-77` is already running",
        run_id: null, session_id: null, child_run_id: null,
      },
    });

    fireEvent.click(screen.getByRole("button", { name: /ask the coach/i }));

    const rail = await screen.findByRole("region", { name: /coach/i });
    await screen.findByText(/already running/i);
    // Not an alert: nothing failed.
    expect(within(rail).queryByRole("alert")).toBeNull();
  });

  it("closes the rail when the selection moves to another version", async () => {
    await openRail(proposedSession());
    expect(screen.getByRole("region", { name: /coach/i })).toBeTruthy();

    await chooseVersion("beta");

    expect(screen.queryByRole("region", { name: /coach/i })).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// r2.s1.w4 — C2 refetch on focus + C3 compare with parent
// ---------------------------------------------------------------------------

/** A catalog where the child version carries one persisted run — the shape the
 * Library serves after `pulse mcp`'s `run_backtest` writes it. */
/** The child version's one persisted run — both the catalogue's row and the
 * version's latest run, which is the shape when nothing but an ordinary backtest
 * has run. */
const CHILD_RUN: LibraryRunSummary = {
  id: "run-child-1",
  createdAt: "2026-08-22T09:00:00.000Z",
  expectancy: "20.250",
  trades: 6,
};

const CHILD_CATALOG: LibraryOverview = {
  strategies: [
    {
      id: "strat-alpha",
      name: "Alpha Wave",
      createdAt: "2026-08-01T09:00:00.000Z",
      pinnedVersionId: null,
      versions: [
        catalogVersion("v-alpha-1", null),
        {
          ...catalogVersion("v-alpha-2", "v-alpha-1"),
          recentRuns: [CHILD_RUN],
          latestRun: CHILD_RUN,
        },
      ],
    },
  ],
};

const COMPARE_DTO: CompareChildRunDto = {
  childVersionId: "v-alpha-2",
  parentVersionId: "v-alpha-1",
  childRunId: "run-child-1",
  parentRunId: "run-parent-1",
  before: PARENT_SUMMARY,
  after: CHILD_SUMMARY,
  inputsDiffer: true,
  inputsNote: "child run predates recorded inputs — inputs cannot be compared",
};

const COMPARE_REFUSAL: BusError = {
  code: "not_found",
  message: "parent version v-alpha-1 has no run to compare against",
  run_id: null,
  session_id: null,
  child_run_id: null,
};

describe("BacktestLabScreen (C3 — compare with parent, r2.s1.w4)", () => {
  it("compares the selected child's latest run with its parent's, badge on when inputs differ", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CHILD_CATALOG });
    compareMock.mockResolvedValue({ status: "ok", data: COMPARE_DTO });
    render(<BacktestLabScreen />);

    await chooseVersion("v2");

    await waitFor(() => {
      expect(compareMock).toHaveBeenCalledWith({ childRunId: "run-child-1" });
    });
    const table = await screen.findByRole("table", { name: /before and after/i });
    expectFigure(table, "−10.52 USDT", "-10.515625");
    expectFigure(table, "+20.25 USDT", "20.250");

    const badge = screen.getByText("inputs differ");
    expect(badge.className).toContain("badge-inputs-differ");
    expect(badge.getAttribute("title")).toBe(COMPARE_DTO.inputsNote);
  });

  it("compares against the latest run, not a fold that heads the catalogue (R2)", async () => {
    // A walk-forward's folds share ONE `created_at` and are newer than the
    // version's ordinary run, so the catalogue (newest first) is headed by an
    // arbitrary fold. `latestRun` is the server's non-fold answer — the same
    // `latest_run_for_version` read the KPIs use — so the comparison must name it,
    // not `recentRuns[0]`, which is what the screen read before.
    const foldRow: LibraryRunSummary = {
      id: "run-fold-0",
      createdAt: "2026-08-23T09:00:00.000Z",
      expectancy: "3.000",
      trades: 4,
    };
    const foldHeaded: LibraryOverview = {
      strategies: [
        {
          id: "strat-alpha",
          name: "Alpha Wave",
          createdAt: "2026-08-01T09:00:00.000Z",
          pinnedVersionId: null,
          versions: [
            catalogVersion("v-alpha-1", null),
            {
              ...catalogVersion("v-alpha-2", "v-alpha-1"),
              recentRuns: [foldRow, CHILD_RUN],
              latestRun: CHILD_RUN,
              certified: true,
              latestWalkForwardRunId: "wf-1",
            },
          ],
        },
      ],
    };
    catalogMock.mockResolvedValue({ status: "ok", data: foldHeaded });
    compareMock.mockResolvedValue({ status: "ok", data: COMPARE_DTO });
    // #268: this version carries a walk-forward pointer, so selecting it ALSO
    // reopens that persisted run. Answered here so the claim under test stays
    // the compare read's own — which run id it names.
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, walkForwardRunId: "wf-1", versionId: "v-alpha-2" },
    });
    render(<BacktestLabScreen />);

    await chooseVersion("v2");

    await waitFor(() => {
      expect(compareMock).toHaveBeenCalledWith({ childRunId: "run-child-1" });
    });
    expect(compareMock).not.toHaveBeenCalledWith({ childRunId: "run-fold-0" });
  });

  it("renders the same table with NO badge when the runs' inputs match", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CHILD_CATALOG });
    compareMock.mockResolvedValue({
      status: "ok",
      data: { ...COMPARE_DTO, inputsDiffer: false, inputsNote: null },
    });
    render(<BacktestLabScreen />);

    await chooseVersion("v2");
    const table = await screen.findByRole("table", { name: /before and after/i });
    expectFigure(table, "+20.25 USDT", "20.250");
    expect(screen.queryByText("inputs differ")).toBeNull();
  });

  it("shows the typed refusal's reason instead of the table", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CHILD_CATALOG });
    compareMock.mockResolvedValue({ status: "error", error: COMPARE_REFUSAL });
    render(<BacktestLabScreen />);

    await chooseVersion("v2");

    await screen.findByText(/has no run to compare against/i);
    expect(screen.queryByRole("table", { name: /before and after/i })).toBeNull();
  });

  it("offers no comparison for a root version, nor for a child with no run", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    render(<BacktestLabScreen />);

    // v-alpha-1 is selected by default and is a ROOT — nothing to compare with.
    // v-alpha-2 has a parent but no recent run in this catalog.
    await chooseVersion("v2");

    await waitFor(() => {
      expect(screen.queryByRole("table", { name: /before and after/i })).toBeNull();
    });
    expect(compareMock).not.toHaveBeenCalled();
  });

  it("re-compares on the fresh run after Run completes", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CHILD_CATALOG });
    compareMock.mockResolvedValue({ status: "ok", data: COMPARE_DTO });
    runMock.mockResolvedValue({
      status: "ok",
      data: { ...SEEDED_RUN, runId: "run-fresh", strategyVersionId: "v-alpha-2" },
    });
    render(<BacktestLabScreen />);

    await chooseVersion("v2");
    await waitFor(() => {
      expect(compareMock).toHaveBeenCalledWith({ childRunId: "run-child-1" });
    });

    fireEvent.click(screen.getByRole("button", { name: /run backtest/i }));
    await screen.findByText("run-fresh");

    await waitFor(() => {
      expect(compareMock).toHaveBeenCalledWith({ childRunId: "run-fresh" });
    });
  });
});

describe("BacktestLabScreen (C2 — refetch on focus, r2.s1.w4)", () => {
  it("refetches the catalog when the window regains focus, and keeps the selection", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    render(<BacktestLabScreen />);
    await chooseVersion("v2");
    const box = screen.getByRole("combobox") as HTMLInputElement;
    const callsOnMount = catalogMock.mock.calls.length;

    fireEvent(window, new Event("focus"));

    await waitFor(() => {
      expect(catalogMock.mock.calls.length).toBeGreaterThan(callsOnMount);
    });
    expect(box.value).toBe("v2 · 2026-08-20 UTC · v-alpha- · human");
  });

  it("falls back to the default when the selected version is gone after a refetch", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    render(<BacktestLabScreen />);
    await chooseVersion("v2");
    const box = screen.getByRole("combobox") as HTMLInputElement;

    // The refetched catalog no longer carries the selected version.
    catalogMock.mockResolvedValue({
      status: "ok",
      data: {
        strategies: [{ ...CATALOG.strategies[0], versions: [catalogVersion("v-alpha-1", null)] }],
      },
    });
    fireEvent(window, new Event("focus"));

    await waitFor(() => {
      expect(box.value).toBe("v1 · 2026-08-20 UTC · v-alpha- · human");
    });
  });
});

// ---------------------------------------------------------------------------
// #315 — the selection and its shown result survive a reconnect
// ---------------------------------------------------------------------------

/** The App's own nesting for this item's purposes: `ActiveOperationsProvider`
 * above the route (`App.tsx`), and a screen the gate can unmount when the
 * server goes down — exactly what rendering Connect in the app's place does. */
function GatedLab() {
  const [up, setUp] = useState(true);
  return (
    <ActiveOperationsProvider>
      <button type="button" onClick={() => setUp((current) => !current)}>
        {up ? "take the server down" : "bring the server back"}
      </button>
      {up ? <BacktestLabScreen /> : null}
    </ActiveOperationsProvider>
  );
}

describe("BacktestLabScreen (#315 — a reconnect keeps the selection and its result)", () => {
  it("shows the same version and its run after the screen unmounts and comes back", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    runMock.mockResolvedValue({
      status: "ok",
      data: { ...SEEDED_RUN, runId: "run-beta-001", strategyVersionId: "v-beta-1" },
    });

    render(<GatedLab />);

    await chooseVersion("beta");
    fireEvent.click(screen.getByRole("button", { name: /run backtest/i }));
    await screen.findByText("run-beta-001");

    // The server drops: the gate takes the screen down, and only the screen.
    fireEvent.click(screen.getByRole("button", { name: /take the server down/i }));
    expect(screen.queryByRole("combobox")).toBeNull();

    // It comes back: the Lab remounts and reads the catalog again. The version
    // the trader had chosen is still the selected one…
    fireEvent.click(screen.getByRole("button", { name: /bring the server back/i }));
    const box = (await screen.findByRole("combobox")) as HTMLInputElement;
    await waitFor(() => {
      expect(box.value).toBe("v1 · 2026-08-20 UTC · v-beta-1 · human");
    });

    // …and the run the trader was reading is still shown for it: the record
    // outlived the screen, and the restored selection addresses it again.
    expect(screen.getByText("run-beta-001")).toBeTruthy();
  });

  it("applies nothing from an unmounted Lab: a pending read cannot select or start a run later (#269/#331)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3Catalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-own-1" },
    });
    // The stale read's version has a parent to compare with; the refusal is the
    // compare read's own answer — this test is about the read not applying.
    compareMock.mockResolvedValue({
      status: "error",
      error: {
        code: "not_found",
        message: "no parent run for run-late-0001",
        run_id: null,
        session_id: null,
        child_run_id: "run-late-0001",
      },
    });
    // The backtest read is held open, settled by hand after the unmount.
    const pending = new Map<
      string,
      (answer: { status: "ok"; data: BacktestRunDto }) => void
    >();
    // The executor form, not `Promise.withResolvers`: this tree's `lib` is
    // ES2022 (tsconfig.json), and `npm run typecheck` is a gate.
    getRunMock.mockImplementation(
      (request) =>
        new Promise((resolve) => {
          pending.set(request.runId, resolve);
        }),
    );

    render(<GatedLab />);

    const input = (await screen.findByLabelText(/open run by id/i)) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "run-late-0001" } });
    fireEvent.click(screen.getByRole("button", { name: /^open$/i }));
    expect(pending.has("run-late-0001")).toBe(true);

    // The server drops: the gate takes the screen down while the read is out.
    fireEvent.click(screen.getByRole("button", { name: /take the server down/i }));
    expect(screen.queryByLabelText(/open run by id/i)).toBeNull();

    // The read lands with the Lab gone: nothing of it may survive.
    await act(async () => {
      pending.get("run-late-0001")?.({
        status: "ok",
        data: { ...SEEDED_RUN, runId: "run-late-0001", strategyVersionId: "ver-second" },
      });
    });

    // Back up: the remembered selection is untouched — the default version, not
    // the read's — and the run the read carried was never started.
    fireEvent.click(screen.getByRole("button", { name: /bring the server back/i }));
    const box = (await screen.findByRole("combobox")) as HTMLInputElement;
    await waitFor(() => {
      expect(box.value).toContain("ver-own-");
    });
    expect(screen.queryByText("run-late-0001")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// r2.s3.w5 — the walk-forward gate: Walk forward action, per-fold table,
// fold rows opening as ordinary runs
// ---------------------------------------------------------------------------
//
// Same discipline as the rest of this file: every value asserted below is a
// fixture value, so a pass means the pane rendered the DTO's own payload. The
// scheme and rule fixtures are SENTINELS — a pane that hard-coded
// "rolling-oos/v1"/"wf-v1" instead of rendering the DTO's fields fails these
// tests, which is the difference between pinning the surface and pinning the
// current backend constants.

/** A two-fold walk-forward result with sentinel scheme/rule names and values
 * distinct per fold, so only the real payload path can satisfy the asserts. */
const WF_DTO: WalkForwardRunDto = {
  walkForwardRunId: "wf-44aa19c2",
  versionId: "v-alpha-1",
  scheme: "scheme-sentinel/v9",
  k: 2,
  rule: "rule-sentinel",
  spanFrom: "2026-08-01T00:00:00.000Z",
  spanTo: "2026-08-31T23:59:59.000Z",
  fromDefaulted: true,
  engineFingerprint: "sha256:wfengine9",
  verdict: {
    pass: false,
    foldsHolding: 1,
    foldsRequired: 2,
    pooled: { n: 5, meanR: "-0.500", lowerBound: -0.8123, holds: false },
  },
  folds: [
    {
      index: 0,
      windowFrom: "2026-08-01T00:00:00.000Z",
      windowTo: "2026-08-15T12:00:00.000Z",
      backtestRunId: "run-fold-0",
      n: 2,
      meanR: "0.250",
      lowerBound: -0.4,
      holds: true,
      trades: 2,
      expectancy: "0.250",
      winRate: "0.500",
    },
    {
      index: 1,
      windowFrom: "2026-08-15T12:00:00.000Z",
      windowTo: "2026-08-31T23:59:59.000Z",
      backtestRunId: "run-fold-1",
      n: 3,
      meanR: "-1.000",
      lowerBound: -1.7,
      holds: false,
      trades: 3,
      expectancy: "-1.000",
      winRate: "0.333",
    },
  ],
};

/** The ordinary persisted run fold 0 ran as — its DTO carries the membership. */
const FOLD_RUN: BacktestRunDto = {
  ...SEEDED_RUN,
  runId: "run-fold-0",
  walkForward: { walkForwardRunId: "wf-44aa19c2", foldIndex: 0 },
};

const WF_ERROR: BusError = {
  code: "validation",
  message: "k must be in 2..=12, got 13",
  run_id: null, session_id: null, child_run_id: null,
};

/** Render + catalog, then click Walk forward and wait for `awaitText`. */
async function renderWalkForward(
  outcome: WalkForwardRunDto | BusError,
  catalog: LibraryOverview = CATALOG,
) {
  catalogMock.mockResolvedValue({ status: "ok", data: catalog });
  const isDto = "walkForwardRunId" in outcome;
  walkForwardMock.mockResolvedValue(
    isDto ? { status: "ok", data: outcome } : { status: "error", error: outcome },
  );
  const { container } = render(<BacktestLabScreen />);
  fireEvent.click(await screen.findByRole("button", { name: /walk forward/i }));
  await screen.findByText(isDto ? outcome.walkForwardRunId : outcome.message);
  return container;
}

describe("BacktestLabScreen (walk-forward, r2.s3.w5)", () => {
  it("never invokes runWalkForwardVersion from mount — StrictMode included", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    walkForwardMock.mockResolvedValue({ status: "ok", data: WF_DTO });

    render(
      <StrictMode>
        <BacktestLabScreen />
      </StrictMode>,
    );
    await screen.findByRole("button", { name: /walk forward/i });
    // Flush microtasks so an erroneous mount-effect call would have landed.
    await Promise.resolve();

    expect(walkForwardMock).not.toHaveBeenCalled();
    expect(getRunMock).not.toHaveBeenCalled();
  });

  it("invokes runWalkForwardVersion exactly once per click with the selected version and k", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    walkForwardMock.mockResolvedValue({ status: "ok", data: WF_DTO });
    render(<BacktestLabScreen />);

    // The folds input defaults to the backend's own default — 6 — and that is
    // what the request carries.
    fireEvent.click(await screen.findByRole("button", { name: /walk forward/i }));
    await screen.findByText("wf-44aa19c2");
    expect(walkForwardMock).toHaveBeenCalledTimes(1);
    expect(walkForwardMock).toHaveBeenLastCalledWith({ versionId: "v-alpha-1", k: 6 });
  });

  it("sends the folds input's value as k when the trader sets it", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    walkForwardMock.mockResolvedValue({ status: "ok", data: WF_DTO });
    render(<BacktestLabScreen />);

    fireEvent.change(await screen.findByLabelText(/folds/i), { target: { value: "3" } });
    fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));

    await screen.findByText("wf-44aa19c2");
    expect(walkForwardMock).toHaveBeenLastCalledWith({ versionId: "v-alpha-1", k: 3 });
  });

  // #212 — the fold-count guard: a count the backend would refuse (or silently
  // truncate) never starts an operation; the refusal renders on the pane's
  // error surface naming the allowed range.
  it("refuses a fractional fold count (6.9) with the range message and starts no operation", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    walkForwardMock.mockResolvedValue({ status: "ok", data: WF_DTO });
    render(<BacktestLabScreen />);

    const folds = await screen.findByLabelText(/folds/i);
    fireEvent.change(folds, { target: { value: "6.9" } });
    fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));

    expect(
      await screen.findByText("Folds must be a whole number from 2 to 12."),
    ).not.toBeNull();
    expect(walkForwardMock).not.toHaveBeenCalled();

    // A valid count after a refusal starts normally — the refusal cleared.
    fireEvent.change(folds, { target: { value: "6" } });
    fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));
    await screen.findByText("wf-44aa19c2");
    expect(walkForwardMock).toHaveBeenLastCalledWith({ versionId: "v-alpha-1", k: 6 });
  });

  it("refuses out-of-range fold counts (1 and 13) with the range message and starts no operation", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    walkForwardMock.mockResolvedValue({ status: "ok", data: WF_DTO });
    render(<BacktestLabScreen />);

    const folds = await screen.findByLabelText(/folds/i);
    for (const bad of ["1", "13"]) {
      fireEvent.change(folds, { target: { value: bad } });
      fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));
      expect(
        await screen.findByText("Folds must be a whole number from 2 to 12."),
      ).not.toBeNull();
      expect(walkForwardMock).not.toHaveBeenCalled();
    }
  });

  it("refuses a non-numeric fold count at the parser, which the number input cannot carry", () => {
    // The folds input is `type="number"`: the browser's value sanitization
    // turns a non-numeric string into "" before React ever sees it, so "abc"
    // is unreachable through the DOM. The guard refuses it at the parser
    // anyway — defence in depth, and the honest way to pin the case.
    expect(parseFoldCount("abc")).toEqual({
      kind: "refused",
      message: "Folds must be a whole number from 2 to 12.",
    });
    // The truncation trap the parser exists for: parseInt would say 6.
    expect(parseFoldCount("6.9").kind).toBe("refused");
  });

  it("keeps today's meaning for an emptied folds field — the backend default k", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    walkForwardMock.mockResolvedValue({ status: "ok", data: WF_DTO });
    render(<BacktestLabScreen />);

    fireEvent.change(await screen.findByLabelText(/folds/i), { target: { value: "" } });
    fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));

    await screen.findByText("wf-44aa19c2");
    expect(walkForwardMock).toHaveBeenLastCalledWith({ versionId: "v-alpha-1", k: undefined });
  });

  it("renders the scheme and rule as the DTO names them, the span, and the verdict line", async () => {
    const container = await renderWalkForward(WF_DTO);
    const pane = container.querySelector(".bt-walkforward") as HTMLElement;
    expect(pane).not.toBeNull();
    const text = pane.textContent ?? "";

    // Sentinel values — renderable only off the DTO, never a baked-in label.
    expect(text).toContain("scheme-sentinel/v9");
    expect(text).toContain("rule-sentinel");
    expect(text).toContain("wf-44aa19c2");
    // The span is a date range, A2-formatted, saying UTC, the DTO's own pair on
    // the element.
    expectFigure(
      pane,
      "2026-08-01 → 2026-08-31 UTC",
      "2026-08-01T00:00:00.000Z → 2026-08-31T23:59:59.000Z",
    );
    // The verdict line: the word, the folds sentence (#328/#305), the pooled
    // bound — never a bare ratio.
    expect(text).toContain("FAIL");
    expect(text).toContain("1 of 2 folds hold (2 required)");
    expect(text).not.toContain("1/2");
    expectFigure(pane, "−0.81", "-0.8123");
    // `from_defaulted` is surfaced as provenance, not silently applied.
    expect(text).toContain("defaulted");
  });

  it("renders one row per fold with window, trades, lower bound and holds", async () => {
    const container = await renderWalkForward(WF_DTO);
    const region = within(container).getByRole("region", { name: /fold rows/i });
    const rows = Array.from(region.querySelectorAll("tbody tr"));
    expect(rows).toHaveLength(2);

    // Window cells: `YYYY-MM-DD HH:mm` UTC, the DTO's own RFC 3339 on the
    // element (the fold table is one of the two tables the Dispatch-2
    // clarification names).
    expectFigure(rows[0], "2026-08-01 00:00 UTC", "2026-08-01T00:00:00.000Z");
    expectFigure(rows[0], "2026-08-15 12:00 UTC", "2026-08-15T12:00:00.000Z");
    const first = rows[0].textContent ?? "";
    expect(first).toContain("2");
    expect(first).toContain("yes");
    // The fold run's own summary fields ride the same row, A2-formatted.
    expectFigure(rows[0], "−0.40", "-0.4");
    expectFigure(rows[0], "+0.25 USDT", "0.250");
    expectFigure(rows[0], "50.0%", "0.500");

    const second = rows[1].textContent ?? "";
    expect(second).toContain("3");
    expect(second).toContain("no");
    expectFigure(rows[1], "−1.70", "-1.7");
    expectFigure(rows[1], "−1.00 USDT", "-1.000");
    expectFigure(rows[1], "33.3%", "0.333");
  });

  it("opens a fold as an ordinary run: getBacktestRun on the fold's id lands in the existing result view", async () => {
    const container = await renderWalkForward(WF_DTO);
    getRunMock.mockResolvedValue({ status: "ok", data: FOLD_RUN });

    const region = within(container).getByRole("region", { name: /fold rows/i });
    fireEvent.click(within(region).getByRole("button", { name: /open run run-fold-0/i }));

    await waitFor(() => {
      expect(getRunMock).toHaveBeenCalledWith({ runId: "run-fold-0" });
    });
    // The existing result view answers with the fold run: provenance band, KPI
    // tiles, equity chart — the same sections a Run render carries. (The id
    // also sits in the fold row's own button, so the wait is on the band.)
    await waitFor(() => {
      expect(container.querySelector(".bt-provenance")).not.toBeNull();
    });
    expect(within(container.querySelector(".bt-provenance") as HTMLElement)
      .getByText("run-fold-0")).toBeTruthy();
    expect(container.querySelector(".bt-kpis")).not.toBeNull();
    expect(container.querySelector(".bt-equity")).not.toBeNull();
    // And the walk-forward pane stays put — the run opened beneath it.
    expect(container.querySelector(".bt-walkforward")).not.toBeNull();
  });

  it("renders a walk-forward BusError's code and message like the run error does", async () => {
    await renderWalkForward(WF_ERROR);

    const alert = screen.getAllByRole("alert").find((a) =>
      a.textContent?.includes(WF_ERROR.message),
    );
    expect(alert).toBeDefined();
    expect(alert?.textContent).toContain("validation");
  });

  it("locks the selector and both actions while a walk-forward is in flight", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    let resolveWf: (value: { status: "ok"; data: WalkForwardRunDto }) => void = () => {};
    walkForwardMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveWf = resolve;
        }),
    );
    render(<BacktestLabScreen />);

    const box = (await screen.findByRole("combobox")) as HTMLInputElement;
    fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));

    await screen.findByRole("button", { name: /walking forward/i });
    expect(box.disabled).toBe(true);
    expect(
      (screen.getByRole("button", { name: /run backtest/i }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("button", { name: /walking forward/i }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);

    resolveWf({ status: "ok", data: WF_DTO });
    await screen.findByText("wf-44aa19c2");
    expect(box.disabled).toBe(false);
    expect(
      (screen.getByRole("button", { name: /walk forward/i }) as HTMLButtonElement).disabled,
    ).toBe(false);
  });

  it("clears a rendered walk-forward result when the selector changes", async () => {
    const container = await renderWalkForward(WF_DTO);
    expect(container.querySelector(".bt-walkforward")).not.toBeNull();

    await chooseVersion("beta");

    expect(container.querySelector(".bt-walkforward")).toBeNull();
    expect(screen.queryByText("wf-44aa19c2")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// #268 — reopening a FINISHED walk-forward run (r3.s3 d29)
// ---------------------------------------------------------------------------
//
// d29's outcome, verbatim: "the run finished on the server while the Mac slept:
// all six fold rows and the verdict are shown on the run that was started, not
// a restarted one". The server keeps the run, the version's catalogue row
// carries `latestWalkForwardRunId`, and `getWalkForwardRun` reads that run back
// in the SAME DTO `runWalkForwardVersion` answers with — so the Lab renders a
// run it never started, through the same `WalkForwardResult`, and starts
// nothing.
//
// These fixtures use the REAL pinned names (`rolling-oos/v1`, `wf-v1`) and the
// backend's default six folds, because that is the shape d29's walk reads off
// the screen. The verdict is a FAIL (`3/4` folds holding) — an uncertified run,
// exactly the state #268 was walked in.

/** One distinct window and tally per fold, so a row can only pass on its OWN
 * fold's numbers. Fold 3 has no lower bound — the em-dash path.
 *
 * Fold 2 deliberately carries `n` ≠ `trades` (4 vs 9): two separate fields on
 * the wire, the count the `wf-v1` verdict saw and the fold run's persisted
 * count, equal in production but not necessarily so. It is what pins the trades
 * column to `n`, the field the column header names. */
const REOPENED_FOLDS: WalkForwardFoldDto[] = [
  {
    index: 0, windowFrom: "2026-07-01T00:00:00.000Z", windowTo: "2026-07-02T00:00:00.000Z",
    backtestRunId: "run-fold-reopen-0", n: 3, meanR: "0.410", lowerBound: -0.4,
    holds: true, trades: 3, expectancy: "0.410", winRate: "0.667",
  },
  {
    index: 1, windowFrom: "2026-07-02T00:00:00.000Z", windowTo: "2026-07-03T00:00:00.000Z",
    backtestRunId: "run-fold-reopen-1", n: 2, meanR: "-0.510", lowerBound: -0.72,
    holds: false, trades: 2, expectancy: "-0.510", winRate: "0.500",
  },
  {
    index: 2, windowFrom: "2026-07-03T00:00:00.000Z", windowTo: "2026-07-04T00:00:00.000Z",
    backtestRunId: "run-fold-reopen-2", n: 4, meanR: "0.620", lowerBound: 0.16,
    holds: true, trades: 9, expectancy: "0.620", winRate: "0.750",
  },
  {
    index: 3, windowFrom: "2026-07-04T00:00:00.000Z", windowTo: "2026-07-05T00:00:00.000Z",
    backtestRunId: "run-fold-reopen-3", n: 1, meanR: "-1.230", lowerBound: null,
    holds: false, trades: 1, expectancy: "-1.230", winRate: "0.000",
  },
  {
    index: 4, windowFrom: "2026-07-05T00:00:00.000Z", windowTo: "2026-07-06T00:00:00.000Z",
    backtestRunId: "run-fold-reopen-4", n: 5, meanR: "0.330", lowerBound: 0.05,
    holds: true, trades: 5, expectancy: "0.330", winRate: "0.600",
  },
  {
    index: 5, windowFrom: "2026-07-06T00:00:00.000Z", windowTo: "2026-07-07T00:00:00.000Z",
    backtestRunId: "run-fold-reopen-5", n: 6, meanR: "-0.070", lowerBound: -0.11,
    holds: false, trades: 6, expectancy: "-0.070", winRate: "0.500",
  },
];

/** The persisted run as the server answers it: six folds, a FAIL verdict. */
const REOPENED_DTO: WalkForwardRunDto = {
  walkForwardRunId: "wf-6c1d9f38",
  versionId: "v-alpha-1",
  scheme: "rolling-oos/v1",
  k: 6,
  rule: "wf-v1",
  spanFrom: "2026-07-01T00:00:00.000Z",
  spanTo: "2026-07-07T00:00:00.000Z",
  fromDefaulted: true,
  engineFingerprint: "sha256:reopened9",
  verdict: {
    pass: false,
    foldsHolding: 3,
    foldsRequired: 4,
    pooled: { n: 21, meanR: "-0.120", lowerBound: -0.4321, holds: false },
  },
  folds: REOPENED_FOLDS,
};

/** A second version's own persisted run — the per-version keying's evidence. */
const OTHER_WF_DTO: WalkForwardRunDto = {
  ...REOPENED_DTO,
  walkForwardRunId: "wf-9a8b7c6d",
  versionId: "v-alpha-2",
};

/** The run a live Walk forward click answers with in the precedence test. */
const FRESH_WF_DTO: WalkForwardRunDto = {
  ...REOPENED_DTO,
  walkForwardRunId: "wf-fresh-1a2b3c4d",
};

/** The d29 shape: a version whose catalogue row ALREADY points at a run the
 * server finished while the client was away — `uncertified · <run id>`, exactly
 * as the Library renders it. */
function reopenableCatalog(
  alphaLatest: string | null,
  secondLatest: string | null,
): LibraryOverview {
  return {
    strategies: [
      {
        id: "strat-alpha",
        name: "Alpha Wave",
        createdAt: "2026-08-01T09:00:00.000Z",
        pinnedVersionId: null,
        versions: [
          { ...catalogVersion("v-alpha-1", null), latestWalkForwardRunId: alphaLatest },
          { ...catalogVersion("v-alpha-2", "v-alpha-1"), latestWalkForwardRunId: secondLatest },
        ],
      },
      {
        id: "strat-beta",
        name: "Beta Break",
        createdAt: "2026-08-10T09:00:00.000Z",
        pinnedVersionId: null,
        versions: [catalogVersion("v-beta-1", null)],
      },
    ],
  };
}

describe("BacktestLabScreen (#268 — reopening a finished walk-forward run)", () => {
  it("reopens the version's persisted run by id: one getWalkForwardRun call, the whole view, nothing re-run", async () => {
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", null),
    });
    reopenMock.mockResolvedValue({ status: "ok", data: REOPENED_DTO });

    // StrictMode, this file's convention for the mount claims: the double-invoked
    // effect must still spend exactly ONE read on the pointer.
    const { container } = render(
      <StrictMode>
        <BacktestLabScreen />
      </StrictMode>,
    );

    // The run id is on screen — the d29 walker ties the view to the run that was
    // STARTED, and this read is what put it there.
    await screen.findByText("wf-6c1d9f38");
    expect(reopenMock).toHaveBeenCalledTimes(1);
    expect(reopenMock).toHaveBeenCalledWith({ walkForwardRunId: "wf-6c1d9f38" });
    // A READ: neither the walk-forward command nor the backtest command runs.
    expect(walkForwardMock).not.toHaveBeenCalled();
    expect(runMock).not.toHaveBeenCalled();

    const pane = container.querySelector(".bt-walkforward") as HTMLElement;
    expect(pane).not.toBeNull();
    const text = pane.textContent ?? "";
    // The naming scheme, the rule, the span and the verdict — the DTO's own,
    // the span and the folds sentence A2-formatted.
    expect(text).toContain("rolling-oos/v1");
    expect(text).toContain("wf-v1");
    expect(text).toContain("wf-6c1d9f38");
    expect(text).toContain("FAIL");
    expectFigure(
      pane,
      "2026-07-01 → 2026-07-07 UTC",
      "2026-07-01T00:00:00.000Z → 2026-07-07T00:00:00.000Z",
    );
    // The sentence counts the SCHEME's folds (`k = 6` on this fixture), not the
    // six rows' holding count.
    expect(text).toContain("3 of 6 folds hold (4 required)");
    expect(text).not.toContain("3/6");
    expectFigure(pane, "−0.43", "-0.4321");
    // The pane says WHERE this run came from — and says it OUTSIDE the result
    // section, which is the same component on both paths.
    expect(container.querySelector(".bt-wf-origin")?.textContent).toContain("reopened by id");

    // All SIX fold rows, CELL BY CELL: window from/to, the trades column, the
    // lower bound and holds — each cell asserted against its own field, so a
    // column rendering the wrong field cannot pass on a coincidence. Fold 2's
    // `n` and `trades` differ (4 vs 9), which pins the trades column to `n`:
    // the count the verdict saw, the field that column names.
    const region = within(container).getByRole("region", { name: /fold rows/i });
    const rows = Array.from(region.querySelectorAll("tbody tr"));
    expect(rows).toHaveLength(6);
    REOPENED_FOLDS.forEach((fold, index) => {
      const cells = Array.from(rows[index].querySelectorAll("td"));
      // The window cells render A2's UTC text with the DTO's own RFC 3339 on
      // the element; the rest are asserted against their own field.
      expect(cells[1].textContent).toBe(timeUtc(fold.windowFrom));
      expect(cells[1].querySelector("[data-exact]")?.getAttribute("data-exact")).toBe(
        fold.windowFrom,
      );
      expect(cells[2].textContent).toBe(timeUtc(fold.windowTo));
      expect(cells[2].querySelector("[data-exact]")?.getAttribute("data-exact")).toBe(
        fold.windowTo,
      );
      expect(cells[3].textContent).toBe(String(fold.n));
      expect(cells[4].textContent).toBe(
        fold.lowerBound === null ? "—" : decimal(String(fold.lowerBound), 2),
      );
      expect(cells[5].textContent).toBe(fold.holds ? "yes" : "no");
    });
    // The fold whose two counts differ proves which one the column reads.
    expect(String(REOPENED_FOLDS[2].n)).not.toBe(String(REOPENED_FOLDS[2].trades));
  });

  it("lets this session's own run win: a fresh result stays, and the persisted read is never consulted again", async () => {
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", null),
    });
    reopenMock.mockResolvedValue({ status: "ok", data: REOPENED_DTO });

    const { container } = render(<BacktestLabScreen />);
    await screen.findByText("wf-6c1d9f38");
    expect(reopenMock).toHaveBeenCalledTimes(1);

    // Now the trader runs it themselves: the fresh DTO is what the pane shows.
    walkForwardMock.mockResolvedValue({ status: "ok", data: FRESH_WF_DTO });
    fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));
    await screen.findByText("wf-fresh-1a2b3c4d");

    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
    const pane = container.querySelector(".bt-walkforward") as HTMLElement;
    expect(pane.textContent).toContain("wf-fresh-1a2b3c4d");
    // A run this session produced carries no reopen marker, and the read behind
    // it is not re-fired.
    expect(container.querySelector(".bt-wf-origin")).toBeNull();
    expect(reopenMock).toHaveBeenCalledTimes(1);
  });

  it("follows the selection: each version's own pointer is read, and a version with no persisted run shows no pane", async () => {
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", "wf-9a8b7c6d"),
    });
    reopenMock.mockImplementation(async (request) =>
      request.walkForwardRunId === "wf-6c1d9f38"
        ? { status: "ok" as const, data: REOPENED_DTO }
        : { status: "ok" as const, data: OTHER_WF_DTO },
    );

    const { container } = render(<BacktestLabScreen />);
    await screen.findByText("wf-6c1d9f38");
    expect(reopenMock).toHaveBeenLastCalledWith({ walkForwardRunId: "wf-6c1d9f38" });

    // v-alpha-2 carries a DIFFERENT persisted run — the pane follows the
    // selection, and the previous version's run id goes with it.
    await chooseVersion("v2");
    await screen.findByText("wf-9a8b7c6d");
    expect(reopenMock).toHaveBeenLastCalledWith({ walkForwardRunId: "wf-9a8b7c6d" });
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();

    // v-beta-1 has no walk-forward run at all: no read, and no pane to fill.
    await chooseVersion("beta");
    await waitFor(() => {
      expect(container.querySelector(".bt-walkforward")).toBeNull();
    });
    expect(reopenMock).toHaveBeenCalledTimes(2);
    expect(screen.queryByText("wf-9a8b7c6d")).toBeNull();
  });

  it("renders a failed reopen as its own error state — never the previous version's pane", async () => {
    const notFound: BusError = {
      code: "not_found",
      message: "no walk-forward run wf-gone-00000000",
      run_id: null, session_id: null, child_run_id: null,
    };
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", "wf-gone-00000000"),
    });
    reopenMock.mockImplementation(async (request) =>
      request.walkForwardRunId === "wf-6c1d9f38"
        ? { status: "ok" as const, data: REOPENED_DTO }
        : { status: "error" as const, error: notFound },
    );

    const { container } = render(<BacktestLabScreen />);
    await screen.findByText("wf-6c1d9f38");

    await chooseVersion("v2");

    // The backend's own typed refusal, rendered like every other failure card.
    const failure = (await screen.findAllByRole("alert")).find((alert) =>
      alert.textContent?.includes(notFound.message),
    );
    expect(failure?.textContent).toContain("not_found");
    // And the last good table is GONE: a failed read never wears the previous
    // version's pane.
    expect(container.querySelector(".bt-walkforward")).toBeNull();
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
  });

  it("keys the read to its target: the render that lands on a new version shows neither the old run nor its failure", async () => {
    const dropped: BusError = {
      code: "internal",
      message: "the connection dropped",
      run_id: null, session_id: null, child_run_id: null,
    };
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", "wf-9a8b7c6d"),
    });
    // v-alpha-1's read fails; v-alpha-2's answers with its own run.
    reopenMock.mockImplementation(async (request) =>
      request.walkForwardRunId === "wf-6c1d9f38"
        ? { status: "error" as const, error: dropped }
        : { status: "ok" as const, data: OTHER_WF_DTO },
    );

    render(<BacktestLabScreen />);
    const failure = (
      await screen.findAllByRole("alert")
    ).find((alert) => alert.textContent?.includes(dropped.message));
    expect(failure).toBeTruthy();

    // Change the selector OUTSIDE `act`: the render commits while the effect
    // that re-reads has not run yet. That one frame is what review F1 is about —
    // it must show the NEW target's in-flight state, never the old version's
    // failure or run.
    switchVersionOutsideAct("v2");

    expect(screen.queryByText(dropped.message)).toBeNull();
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
    expect(screen.getByText("Opening the persisted walk-forward run…")).toBeTruthy();

    // The new version's own read then lands, and the pane follows it.
    await screen.findByText("wf-9a8b7c6d");
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
  });

  it("keys the read to its target: a run already rendered for one version is not shown under the next one", async () => {
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", "wf-9a8b7c6d"),
    });
    reopenMock.mockImplementation(async (request) =>
      request.walkForwardRunId === "wf-6c1d9f38"
        ? { status: "ok" as const, data: REOPENED_DTO }
        : { status: "ok" as const, data: OTHER_WF_DTO },
    );

    const { container } = render(<BacktestLabScreen />);
    await screen.findByText("wf-6c1d9f38");
    expect(container.querySelector(".bt-walkforward")).not.toBeNull();

    switchVersionOutsideAct("v2");

    // One frame, un-act-ed: v-alpha-1's six fold rows may not stand in for
    // v-alpha-2 — not even for a paint.
    expect(container.querySelector(".bt-walkforward")).toBeNull();
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
    expect(screen.getByText("Opening the persisted walk-forward run…")).toBeTruthy();

    await screen.findByText("wf-9a8b7c6d");
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
  });

  it("retries a failed read when the catalog refetches with the same pointer", async () => {
    const offline: BusError = {
      code: "internal",
      message: "the server did not answer",
      run_id: null, session_id: null, child_run_id: null,
    };
    // A FRESH payload object per call, exactly as a real refetch parses one —
    // that new identity is what the read's effect keys on.
    catalogMock.mockImplementation(async () => ({
      status: "ok" as const,
      data: reopenableCatalog("wf-6c1d9f38", null),
    }));
    reopenMock.mockResolvedValueOnce({ status: "error", error: offline });
    reopenMock.mockResolvedValue({ status: "ok", data: REOPENED_DTO });

    const { container } = render(<BacktestLabScreen />);
    const failure = (
      await screen.findAllByRole("alert")
    ).find((alert) => alert.textContent?.includes(offline.message));
    expect(failure).toBeTruthy();
    expect(container.querySelector(".bt-walkforward")).toBeNull();
    expect(reopenMock).toHaveBeenCalledTimes(1);

    // The window regains focus: the catalogue is re-read — same version, same
    // pointer — and the pane's read gets another go instead of leaving a dead
    // error card behind.
    const callsOnMount = catalogMock.mock.calls.length;
    fireEvent(window, new Event("focus"));
    await waitFor(() => {
      expect(catalogMock.mock.calls.length).toBeGreaterThan(callsOnMount);
    });

    await screen.findByText("wf-6c1d9f38");
    expect(reopenMock).toHaveBeenCalledTimes(2);
    expect(reopenMock).toHaveBeenLastCalledWith({ walkForwardRunId: "wf-6c1d9f38" });
    expect(screen.queryByText(offline.message)).toBeNull();
  });

  it("drops a superseded read's answer: the version on screen keeps its own run, and the pane never sticks on loading (N1)", async () => {
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", "wf-9a8b7c6d"),
    });
    // Both reads are held open and settled by hand, so the ORDER of the answers
    // is the test's to choose: v-alpha-1's answer is the last one in.
    const pending = new Map<string, (answer: ReopenAnswer) => void>();
    reopenMock.mockImplementation(
      (request) =>
        new Promise<ReopenAnswer>((resolve) => {
          pending.set(request.walkForwardRunId, resolve);
        }),
    );

    render(<BacktestLabScreen />);
    await waitFor(() => {
      expect(pending.has("wf-6c1d9f38")).toBe(true);
    });

    // The trader moves on while v-alpha-1's read is still out.
    await chooseVersion("v2");
    await waitFor(() => {
      expect(pending.has("wf-9a8b7c6d")).toBe(true);
    });

    // The version now on screen answers first...
    await act(async () => {
      pending.get("wf-9a8b7c6d")?.({ status: "ok", data: OTHER_WF_DTO });
    });
    expect(screen.getByText("wf-9a8b7c6d")).toBeTruthy();

    // ...and the version the trader left answers LAST. Its run belongs to a pane
    // that is gone: the screen may not swap to it, and may not fall back to
    // "Opening…" with no effect left to run.
    await act(async () => {
      pending.get("wf-6c1d9f38")?.({ status: "ok", data: REOPENED_DTO });
    });
    expect(screen.getByText("wf-9a8b7c6d")).toBeTruthy();
    expect(screen.queryByText("Opening the persisted walk-forward run…")).toBeNull();
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
  });

  it("drops a superseded read's answer for the SAME id: an older failure cannot overwrite a newer success (N2)", async () => {
    const offline: BusError = {
      code: "internal",
      message: "the server did not answer",
      run_id: null, session_id: null, child_run_id: null,
    };
    catalogMock.mockResolvedValue({
      status: "ok",
      data: reopenableCatalog("wf-6c1d9f38", "wf-9a8b7c6d"),
    });
    const pending: Array<{ id: string; answer: (answer: ReopenAnswer) => void }> = [];
    reopenMock.mockImplementation(
      (request) =>
        new Promise<ReopenAnswer>((resolve) => {
          pending.push({ id: request.walkForwardRunId, answer: resolve });
        }),
    );

    render(<BacktestLabScreen />);
    await waitFor(() => {
      expect(pending).toHaveLength(1); // v-alpha-1's first read
    });

    // Away and back while that read is still out: the SAME id is read twice.
    await chooseVersion("v2");
    await waitFor(() => {
      expect(pending).toHaveLength(2); // v-alpha-2's read
    });
    // Back to v-alpha-1: the same id is read again.
    await chooseVersion("alpha");
    await waitFor(() => {
      expect(pending).toHaveLength(3); // v-alpha-1 again
    });
    const sameId = pending.filter((entry) => entry.id === "wf-6c1d9f38");
    expect(sameId).toHaveLength(2);

    // The NEWER read of that id succeeds...
    await act(async () => {
      sameId[1].answer({ status: "ok", data: REOPENED_DTO });
    });
    expect(screen.getByText("wf-6c1d9f38")).toBeTruthy();

    // ...and the older one fails LAST, for the very same id. The id matches, but
    // the read is still superseded: the run stays, the stale failure is dropped.
    await act(async () => {
      sameId[0].answer({ status: "error", error: offline });
    });
    expect(screen.getByText("wf-6c1d9f38")).toBeTruthy();
    expect(screen.queryByText(offline.message)).toBeNull();
  });

  it("never revalidates a run it already shows: a refetch with the same pointer spends no read and evicts nothing (P2-B)", async () => {
    // The refetch's payload is HELD OPEN and handed over by the test, so the
    // render it causes and the passive effect that render arms both run before
    // the read count below is taken. Counting while the refetch was still in the
    // air let the done-skip's own mutation pass (verifier re-check, claim 5b).
    let landRefetch: (() => void) | null = null;
    let calls = 0;
    catalogMock.mockImplementation(async () => {
      calls += 1;
      if (calls === 1) {
        return { status: "ok" as const, data: reopenableCatalog("wf-6c1d9f38", null) };
      }
      // A fresh payload object, as a real refetch parses one.
      return new Promise((resolve) => {
        landRefetch = () =>
          resolve({ status: "ok" as const, data: reopenableCatalog("wf-6c1d9f38", null) });
      });
    });
    reopenMock.mockResolvedValue({ status: "ok", data: REOPENED_DTO });

    render(<BacktestLabScreen />);
    await screen.findByText("wf-6c1d9f38");
    expect(catalogMock).toHaveBeenCalledTimes(1);
    expect(reopenMock).toHaveBeenCalledTimes(1);

    // The window regains focus: the catalogue is asked for again, and the test
    // decides when that answer lands.
    fireEvent(window, new Event("focus"));
    await waitFor(() => {
      expect(landRefetch).not.toBeNull();
    });

    // Land it inside `act`: the refetch's render and the effect it arms have both
    // run by the time the count is read. The run is immutable and already on
    // screen, so nothing may be read a second time.
    await act(async () => {
      landRefetch?.();
      await Promise.resolve();
    });

    expect(reopenMock).toHaveBeenCalledTimes(1);
    expect(screen.getByText("wf-6c1d9f38")).toBeTruthy();
    expect(screen.queryByText("Opening the persisted walk-forward run…")).toBeNull();
  });
});

/** Pick the first option matching `query` through the DOM rather than
 * `fireEvent`, so the render commits while the passive effect that re-reads has
 * NOT run: the frame review F1 found, made deterministic here by `flushSync`
 * instead of relying on React's discrete-event flushing.
 *
 * Opening and filtering go through `fireEvent` (act-wrapped — that frame is not
 * the one under test); only the COMMIT is dispatched raw, inside `flushSync`. */
function switchVersionOutsideAct(query: string): void {
  const box = screen.getByRole("combobox") as HTMLInputElement;
  fireEvent.focus(box);
  fireEvent.change(box, { target: { value: query } });
  const option = screen.getAllByRole("option")[0];
  if (option === undefined) {
    throw new Error(`switchVersionOutsideAct: no option matches ${query}`);
  }
  flushSync(() => {
    option.dispatchEvent(new MouseEvent("click", { bubbles: true }));
  });
}

/** One hold-open read's answer, settled by a test that chooses the order. */
type ReopenAnswer =
  | { status: "ok"; data: WalkForwardRunDto }
  | { status: "error"; error: BusError };

// ---------------------------------------------------------------------------
// r4.s5.w3 — the Library/Lab/Designer walk-filed defects
// ---------------------------------------------------------------------------
//
// #331/#316: a version's runs are reachable and openable — its own backtests and
// any persisted walk-forward — WITHOUT starting anything. The only commands on
// those paths are the reads (`getBacktestRun`, `getWalkForwardRun`); the run
// commands are asserted untouched.

/** A version's own run: the catalogue row AND the run the Lab opens by id. */
const W3_OWN_RUN: LibraryRunSummary = {
  id: "ownrun-0001",
  createdAt: "2026-08-24T09:00:00.000Z",
  expectancy: "+0.42R",
  trades: 12,
};

/** The catalogue the #331/#316 tests read: `ver-own-1` carries one own run and
 * the walk-forward pointer; `ver-second` carries nothing. */
function w3Catalog(): LibraryOverview {
  return {
    strategies: [
      {
        id: "strat-alpha",
        name: "Alpha Wave",
        createdAt: "2026-08-01T09:00:00.000Z",
        pinnedVersionId: null,
        versions: [
          {
            ...catalogVersion("ver-own-1", null, W3_OWN_RUN, [W3_OWN_RUN]),
            latestWalkForwardRunId: "wf-6c1d9f38",
          },
          catalogVersion("ver-second", "ver-own-1"),
        ],
      },
    ],
  };
}

describe("BacktestLabScreen (#331 — a version's own runs are reachable)", () => {
  it("opens a version's own backtest run from the Runs list without starting a new run (#331)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3Catalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-own-1" },
    });
    getRunMock.mockResolvedValue({
      status: "ok",
      data: { ...SEEDED_RUN, runId: "ownrun-0001", strategyVersionId: "ver-own-1" },
    });

    render(<BacktestLabScreen />);

    const runs = await screen.findByRole("region", { name: /version runs/i });
    fireEvent.click(within(runs).getByRole("button", { name: /open run ownrun-0/i }));

    const provenance = await screen.findByRole("region", { name: /run provenance/i });
    expect(within(provenance).getByText("ownrun-0001")).toBeTruthy();
    expect(getRunMock).toHaveBeenCalledWith({ runId: "ownrun-0001" });
    // A READ: neither run command is reached on this path.
    expect(runMock).not.toHaveBeenCalled();
    expect(walkForwardMock).not.toHaveBeenCalled();
  });
});

describe("BacktestLabScreen (#316 — any persisted walk-forward run opens)", () => {
  it("opens the persisted walk-forward run from the Runs list, with its version on the pane, without starting a new one (#316)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3Catalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-own-1" },
    });

    const { container } = render(<BacktestLabScreen />);

    const runs = await screen.findByRole("region", { name: /version runs/i });
    fireEvent.click(within(runs).getByRole("button", { name: /open walk-forward/i }));

    const pane = (await screen.findByRole("region", {
      name: /walk-forward result/i,
    })) as HTMLElement;
    expect(pane.textContent).toContain("wf-6c1d9f38");
    expect(pane.textContent).toContain("3 of 6 folds hold (4 required)");
    // #316's own comment: the pane names the VERSION it ran on, so verifying
    // that needs no trip to the Library.
    const meta = container.querySelector(".bt-wf-meta") as HTMLElement;
    expect(meta).not.toBeNull();
    expect(within(meta).getByText("version")).toBeTruthy();
    expect(within(meta).getByText("ver-own-1")).toBeTruthy();
    // A READ, twice over: neither run command is reached.
    expect(walkForwardMock).not.toHaveBeenCalled();
    expect(runMock).not.toHaveBeenCalled();
  });

  it("opens a walk-forward run by id from the field, selecting that run's version (#316)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3Catalog() });
    // The field asks the backtest read first (a fold id opens as an ordinary
    // run); a walk-forward id refuses it, and the walk-forward read answers.
    getRunMock.mockResolvedValue({
      status: "error",
      error: {
        code: "not_found",
        message: "no backtest run wf-9a8b7c6d",
        run_id: null,
        session_id: null,
        child_run_id: "wf-9a8b7c6d",
      },
    });
    reopenMock.mockImplementation(async (request) =>
      request.walkForwardRunId === "wf-9a8b7c6d"
        ? { status: "ok" as const, data: { ...OTHER_WF_DTO, versionId: "ver-second" } }
        : { status: "ok" as const, data: { ...REOPENED_DTO, versionId: "ver-own-1" } },
    );

    render(<BacktestLabScreen />);

    const input = (await screen.findByLabelText(/open run by id/i)) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "wf-9a8b7c6d" } });
    fireEvent.click(screen.getByRole("button", { name: /^open$/i }));

    // The run the id names is on screen — the DTO carries the strategy version,
    // and the picker moves to it.
    await screen.findByText("wf-9a8b7c6d");
    expect(reopenMock).toHaveBeenCalledWith({ walkForwardRunId: "wf-9a8b7c6d" });
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-seco");
    expect(walkForwardMock).not.toHaveBeenCalled();
    expect(runMock).not.toHaveBeenCalled();
  });

  it("shows the server's own message when an id names nothing at all (#316)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3Catalog() });
    const noBacktestRun: BusError = {
      code: "not_found",
      message: "no backtest run nope-0001",
      run_id: null,
      session_id: null,
      child_run_id: "nope-0001",
    };
    getRunMock.mockResolvedValue({ status: "error", error: noBacktestRun });
    reopenMock.mockImplementation(async (request) =>
      request.walkForwardRunId === "wf-6c1d9f38"
        ? { status: "ok" as const, data: { ...REOPENED_DTO, versionId: "ver-own-1" } }
        : {
            status: "error" as const,
            error: {
              code: "not_found",
              message: "no walk-forward run nope-0001",
              run_id: null,
              session_id: null,
              child_run_id: null,
            },
          },
    );

    render(<BacktestLabScreen />);

    const input = (await screen.findByLabelText(/open run by id/i)) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "nope-0001" } });
    fireEvent.click(screen.getByRole("button", { name: /^open$/i }));

    // The BACKTEST read's refusal is the one rendered — it is the read that
    // asked first, and its `child_run_id` carries the id the trader typed.
    const failure = (await screen.findAllByRole("alert")).find((alert) =>
      alert.textContent?.includes(noBacktestRun.message),
    );
    expect(failure).toBeTruthy();
    expect(failure?.textContent).toContain("nope-0001");
    expect(walkForwardMock).not.toHaveBeenCalled();
    expect(runMock).not.toHaveBeenCalled();
  });

  it("drops a superseded open-by-id read: an older answer never replaces a newer run (#269/#331)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3Catalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-own-1" },
    });
    // The child version's compare panel mounts once a run lands on it; the
    // refusal is the read's own answer — this test is about which run shows.
    compareMock.mockResolvedValue({
      status: "error",
      error: {
        code: "not_found",
        message: "no parent run for run-second-1",
        run_id: null,
        session_id: null,
        child_run_id: "run-second-1",
      },
    });
    // Both backtest reads are held open, settled by hand in the order the test
    // chooses: the FIRST submit's answer is the last one in.
    const pending = new Map<
      string,
      (answer: { status: "ok"; data: BacktestRunDto }) => void
    >();
    // The executor form, not `Promise.withResolvers`: this tree's `lib` is
    // ES2022 (tsconfig.json), and `npm run typecheck` is a gate.
    getRunMock.mockImplementation(
      (request) =>
        new Promise((resolve) => {
          pending.set(request.runId, resolve);
        }),
    );

    render(<BacktestLabScreen />);

    const input = (await screen.findByLabelText(/open run by id/i)) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "run-first-01" } });
    fireEvent.click(screen.getByRole("button", { name: /^open$/i }));
    expect(pending.has("run-first-01")).toBe(true);

    fireEvent.change(input, { target: { value: "run-second-1" } });
    fireEvent.click(screen.getByRole("button", { name: /^open$/i }));
    expect(pending.has("run-second-1")).toBe(true);

    // The second submit answers first: its run lands on its own version.
    await act(async () => {
      pending.get("run-second-1")?.({
        status: "ok",
        data: { ...SEEDED_RUN, runId: "run-second-1", strategyVersionId: "ver-second" },
      });
    });
    const provenance = await screen.findByRole("region", { name: /run provenance/i });
    expect(within(provenance).getByText("run-second-1")).toBeTruthy();
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-seco");

    // The first submit lands LAST: superseded, it applies nothing — the newer
    // run stays, and no older one replaces it.
    await act(async () => {
      pending.get("run-first-01")?.({
        status: "ok",
        data: { ...SEEDED_RUN, runId: "run-first-01", strategyVersionId: "ver-own-1" },
      });
    });
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-seco");
    expect(screen.queryByText("run-first-01")).toBeNull();
  });

  it("keeps a listed run when an abandoned open-by-id read lands later (#331)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3Catalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-own-1" },
    });
    // The abandoned read's version has a parent to compare with; the refusal is
    // the compare read's own answer — this test is about which run shows.
    compareMock.mockResolvedValue({
      status: "error",
      error: {
        code: "not_found",
        message: "no parent run for run-first-01",
        run_id: null,
        session_id: null,
        child_run_id: "run-first-01",
      },
    });
    // Every backtest read is held open, settled by hand in the order the test
    // chooses: the abandoned id's answer is the last one in.
    const pending = new Map<
      string,
      (answer: { status: "ok"; data: BacktestRunDto }) => void
    >();
    // The executor form, not `Promise.withResolvers`: this tree's `lib` is
    // ES2022 (tsconfig.json), and `npm run typecheck` is a gate.
    getRunMock.mockImplementation(
      (request) =>
        new Promise((resolve) => {
          pending.set(request.runId, resolve);
        }),
    );

    render(<BacktestLabScreen />);
    const runs = await screen.findByRole("region", { name: /version runs/i });

    // The trader submits an id it never comes back to…
    const input = (await screen.findByLabelText(/open run by id/i)) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "run-first-01" } });
    fireEvent.click(screen.getByRole("button", { name: /^open$/i }));
    expect(pending.has("run-first-01")).toBe(true);

    // …then opens a listed run instead: it answers, and it lands.
    fireEvent.click(within(runs).getByRole("button", { name: /open run ownrun-0/i }));
    expect(pending.has("ownrun-0001")).toBe(true);
    await act(async () => {
      pending.get("ownrun-0001")?.({
        status: "ok",
        data: { ...SEEDED_RUN, runId: "ownrun-0001", strategyVersionId: "ver-own-1" },
      });
    });
    expect(
      within(await screen.findByRole("region", { name: /run provenance/i })).getByText(
        "ownrun-0001",
      ),
    ).toBeTruthy();

    // The abandoned id lands LAST: superseded, it applies nothing — the listed
    // run stays, on the version the trader was looking at.
    await act(async () => {
      pending.get("run-first-01")?.({
        status: "ok",
        data: { ...SEEDED_RUN, runId: "run-first-01", strategyVersionId: "ver-second" },
      });
    });
    expect(
      within(screen.getByRole("region", { name: /run provenance/i })).getByText("ownrun-0001"),
    ).toBeTruthy();
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-own-");
    expect(screen.queryByText("run-first-01")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// #330 — a walk-forward's fold runs are marked and grouped
// ---------------------------------------------------------------------------

/** Two folds of the latest walk-forward (`REOPENED_DTO`'s index 2 and 0) and a
 * fold of an OLDER walk-forward, which no pointer on the version can name. */
const W3_FOLD_LATEST: LibraryRunSummary = {
  id: "run-fold-reopen-2",
  createdAt: "2026-08-23T09:00:00.000Z",
  expectancy: "+0.62R",
  trades: 4,
};

const W3_FOLD_LATEST_FIRST: LibraryRunSummary = {
  id: "run-fold-reopen-0",
  createdAt: "2026-08-23T09:00:00.000Z",
  expectancy: "+0.41R",
  trades: 3,
};

const W3_FOLD_OLDER: LibraryRunSummary = {
  id: "oldwf-fold-1",
  createdAt: "2026-08-21T09:00:00.000Z",
  expectancy: "+0.10R",
  trades: 6,
};

/** The catalogue #330 reads: the version's own run, the latest walk-forward's
 * folds (payload order: fold 3, own, fold 1) and — in the second fixture — a
 * fold of an older walk-forward. */
function w3FoldCatalog(olderFold: LibraryRunSummary | null): LibraryOverview {
  const runs = [W3_FOLD_LATEST, W3_OWN_RUN, W3_FOLD_LATEST_FIRST];
  return {
    strategies: [
      {
        id: "strat-alpha",
        name: "Alpha Wave",
        createdAt: "2026-08-01T09:00:00.000Z",
        pinnedVersionId: null,
        versions: [
          {
            ...catalogVersion(
              "ver-own-1",
              null,
              W3_OWN_RUN,
              olderFold === null ? runs : [olderFold, ...runs],
            ),
            latestWalkForwardRunId: "wf-6c1d9f38",
          },
        ],
      },
    ],
  };
}

describe("BacktestLabScreen (#330 — fold runs are marked and grouped)", () => {
  it("labels the latest walk-forward's folds and groups them after the version's own runs (#330)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3FoldCatalog(null) });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-own-1" },
    });

    const { container } = render(<BacktestLabScreen />);

    // The fold map rides the pane's own read of the version's pointer.
    await screen.findByRole("region", { name: /walk-forward result/i });
    const rows = await waitFor(() => {
      const found = Array.from(container.querySelectorAll(".bt-runs-list .bt-row"));
      expect(found).toHaveLength(3);
      return found;
    });

    // The version's own run leads; the two folds follow, each carrying its own
    // label (`index + 1`, the walk-forward's short id) — never a mark the
    // payload cannot justify.
    expect(rows.map((row) => row.querySelector(".bt-id")?.textContent)).toEqual([
      "ownrun-0",
      "run-fold",
      "run-fold",
    ]);
    expect(rows[0]?.querySelector(".bt-fold-label")).toBeNull();
    expect(rows[1]?.querySelector(".bt-fold-label")?.textContent).toBe("fold 3 of WF wf-6c1d9");
    expect(rows[2]?.querySelector(".bt-fold-label")?.textContent).toBe("fold 1 of WF wf-6c1d9");
  });

  it("leaves an older walk-forward's fold unmarked — the known limit (#330)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3FoldCatalog(W3_FOLD_OLDER) });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-own-1" },
    });

    const { container } = render(<BacktestLabScreen />);
    await screen.findByRole("region", { name: /walk-forward result/i });

    const rows = Array.from(container.querySelectorAll(".bt-runs-list .bt-row"));
    const older = rows.find(
      (row) => row.querySelector(".bt-id")?.textContent === "oldwf-fo",
    );
    expect(older).toBeDefined();
    // `LibraryRunSummary` carries no `walk_forward_run_id`, and the version names
    // only its LATEST walk-forward — so the screen cannot know this run is a fold,
    // and must not guess. It reads exactly one walk-forward: the pointer.
    expect(older?.querySelector(".bt-fold-label")).toBeNull();
    // Non-vacuous: the latest walk-forward's folds in the SAME render do wear
    // their labels — the absence above is a limit, not a broken feature.
    expect(
      rows
        .filter((row) => row.querySelector(".bt-fold-label") !== null)
        .map((row) => row.querySelector(".bt-id")?.textContent),
    ).toEqual(["run-fold", "run-fold"]);
    expect(reopenMock).toHaveBeenCalledTimes(1);
    expect(reopenMock).toHaveBeenCalledWith({ walkForwardRunId: "wf-6c1d9f38" });
  });
});

// ---------------------------------------------------------------------------
// #326 — the compare panel names its runs and explains the inputs
// ---------------------------------------------------------------------------

describe("BacktestLabScreen (#326 — the compare names its runs)", () => {
  it("names both compared runs and shows the inputs note beside the inputs-differ badge (#326)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CHILD_CATALOG });
    compareMock.mockResolvedValue({ status: "ok", data: COMPARE_DTO });
    render(<BacktestLabScreen />);

    await chooseVersion("v2");
    const section = await screen.findByRole("region", { name: /compare with parent/i });

    // Both columns carry their run: the 8-character short id on screen, the
    // DTO's full id on the element.
    const child = within(section).getByText("run-chil");
    expect(child.getAttribute("title")).toBe(COMPARE_DTO.childRunId);
    const parent = within(section).getByText("run-pare");
    expect(parent.getAttribute("title")).toBe(COMPARE_DTO.parentRunId);

    // The reason the inputs cannot be proven equal rides VISIBLY beside the
    // badge — the hover keeps the exact text too.
    const badge = within(section).getByText("inputs differ");
    expect(badge.getAttribute("title")).toBe(COMPARE_DTO.inputsNote);
    expect(within(section).getByText(COMPARE_DTO.inputsNote as string)).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// #269 — the certification deep link (`#/backtest?wf=<id>`)
// ---------------------------------------------------------------------------

/** Two versions, neither with a pointer or a run: the ONLY walk-forward the Lab
 * can read here is the one the fragment names. */
function w3LinkCatalog(): LibraryOverview {
  return {
    strategies: [
      {
        id: "strat-alpha",
        name: "Alpha Wave",
        createdAt: "2026-08-01T09:00:00.000Z",
        pinnedVersionId: null,
        versions: [
          catalogVersion("ver-own-1", null),
          catalogVersion("ver-second", "ver-own-1"),
        ],
      },
    ],
  };
}

describe("BacktestLabScreen (#269 — the certification deep link)", () => {
  afterEach(() => {
    window.location.hash = "";
  });

  it("opens the run a #/backtest?wf= fragment names, selecting that run's version (#269)", async () => {
    window.location.hash = "#/backtest?wf=wf-6c1d9f38";
    catalogMock.mockResolvedValue({ status: "ok", data: w3LinkCatalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-second" },
    });

    render(<BacktestLabScreen />);

    // The run the fragment names is on screen, and the picker moved to the
    // version the DTO attributes it to.
    await screen.findByText("wf-6c1d9f38");
    expect(reopenMock).toHaveBeenCalledWith({ walkForwardRunId: "wf-6c1d9f38" });
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-seco");
    // A READ, exactly like the Runs rows: nothing is started.
    expect(walkForwardMock).not.toHaveBeenCalled();
    expect(runMock).not.toHaveBeenCalled();
  });

  it("opens the run a fragment names while the Lab is already open (#269)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3LinkCatalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-second" },
    });

    render(<BacktestLabScreen />);
    // Mounted and idle: neither version carries a pointer, so nothing has been
    // read yet.
    await screen.findByRole("region", { name: /version runs/i });
    expect(reopenMock).not.toHaveBeenCalled();

    window.location.hash = "#/backtest?wf=wf-6c1d9f38";
    window.dispatchEvent(new HashChangeEvent("hashchange"));

    await screen.findByText("wf-6c1d9f38");
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-seco");
  });

  it("consumes the fragment once, so a remount never replays the link (#269, #315)", async () => {
    window.location.hash = "#/backtest?wf=wf-6c1d9f38";
    catalogMock.mockResolvedValue({ status: "ok", data: w3LinkCatalog() });
    reopenMock.mockResolvedValue({
      status: "ok",
      data: { ...REOPENED_DTO, versionId: "ver-second" },
    });

    const first = render(<BacktestLabScreen />);
    await screen.findByText("wf-6c1d9f38");
    expect(reopenMock).toHaveBeenCalledTimes(1);
    // The fragment is consumed as soon as it is taken…
    expect(window.location.hash).toBe("#/backtest");

    // …so the screen coming back (a reconnect remounts it) neither reads the
    // run again nor re-opens it from a replayed link.
    first.unmount();
    render(<BacktestLabScreen />);
    await screen.findByRole("region", { name: /version runs/i });
    await act(async () => {});
    expect(reopenMock).toHaveBeenCalledTimes(1);
    expect(screen.queryByText("wf-6c1d9f38")).toBeNull();
  });

  it("keeps the trader's later pick when a pending deep-link read resolves (#269)", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: w3LinkCatalog() });
    // A run id no other case reads, so the shared read cache cannot stand in
    // for the link's own read. The read is held open, settled by hand after the
    // pick.
    const LINK_WF = "wf-g1-stale01";
    const pending = new Map<string, (answer: ReopenAnswer) => void>();
    // The executor form, not `Promise.withResolvers`: this tree's `lib` is
    // ES2022 (tsconfig.json), and `npm run typecheck` is a gate.
    reopenMock.mockImplementation(
      (request) =>
        new Promise<ReopenAnswer>((resolve) => {
          pending.set(request.walkForwardRunId, resolve);
        }),
    );

    render(<BacktestLabScreen />);
    await screen.findByRole("region", { name: /version runs/i });

    // The link arrives while the screen is open; its read is still out when the
    // trader picks another version.
    window.location.hash = `#/backtest?wf=${LINK_WF}`;
    window.dispatchEvent(new HashChangeEvent("hashchange"));
    expect(pending.has(LINK_WF)).toBe(true);
    expect(window.location.hash).toBe("#/backtest");

    await chooseVersion("ver-seco");
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-seco");

    // The superseded read resolves — and applies nothing: the pick stands, and
    // no run pane opens from the link.
    await act(async () => {
      pending.get(LINK_WF)?.({
        status: "ok",
        data: { ...REOPENED_DTO, walkForwardRunId: LINK_WF, versionId: "ver-own-1" },
      });
    });
    expect((screen.getByRole("combobox") as HTMLInputElement).value).toContain("ver-seco");
    expect(screen.queryByText(LINK_WF)).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// r5.s2.w2 — the run detail opens in the Chart tab
// ---------------------------------------------------------------------------

describe("BacktestLabScreen (r5.s2.w2 — the run detail's Open in Chart link)", () => {
  it("links the run's own id to the Chart tab's deep link", async () => {
    // The Lab hands the run over by ID, the way the Library hands a walk-forward run
    // to the Lab (#269): a fragment the Chart tab reads, never shared state. The tab
    // then reads the run for its pair, both timeframes and its span.
    const container = await renderRun(SEEDED_RUN);

    const link = within(container).getByRole("link", { name: /open in chart/i });
    expect(link.getAttribute("href")).toBe(`#/chart?run=${SEEDED_RUN.runId}`);
  });
});
