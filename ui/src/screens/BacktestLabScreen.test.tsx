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

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { StrictMode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

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
  LibraryVersion,
  SummaryDto,
  WalkForwardRunDto,
} from "../bindings";
import BacktestLabScreen from "./BacktestLabScreen";
import { RouteContent } from "../App";
import { resolveRoute } from "../routes";

const catalogMock = vi.mocked(commands.libraryOverview);
const runMock = vi.mocked(commands.runBacktestVersion);
const compareMock = vi.mocked(commands.compareChildRun);
const walkForwardMock = vi.mocked(commands.runWalkForwardVersion);
const getRunMock = vi.mocked(commands.getBacktestRun);

// ---------------------------------------------------------------------------
// Fixtures — shaped exactly like the generated types, values chosen to be
// distinct so a verbatim assertion can only pass on the real payload path.
// ---------------------------------------------------------------------------

function catalogVersion(id: string, parentId: string | null): LibraryVersion {
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
    recentRuns: [],
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

function bins(counts: readonly number[]): HistogramBinDto[] {
  return counts.map((count, i) => ({
    lower: (i * 0.25).toFixed(2),
    upper: ((i + 1) * 0.25).toFixed(2),
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
  firstOpenTimeMs: "1735689600000",
  lastCloseTimeMs: "1756684800000",
  startingEquity: "10000.00",
  takerFeeBps: "4.0000",
  slippageBps: "2.0000",
  funding: "snapshot_rates",
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
  fingerprintWarning: "engine fingerprint changed since the prior run",
  profitFactor: "0.681",
  sharpe: -0.42,
  sortino: -0.31,
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

beforeEach(() => {
  catalogMock.mockReset();
  runMock.mockReset();
  compareMock.mockReset();
  walkForwardMock.mockReset();
  getRunMock.mockReset();
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

  it("lists exactly the catalog's real (strategy, version) pairs with version ids as values", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });

    render(<BacktestLabScreen />);

    const select = await screen.findByRole("combobox");
    const options = within(select).getAllByRole("option");
    expect(options.map((o) => o.textContent)).toEqual([
      "Alpha Wave · v1",
      "Alpha Wave · v2",
      "Beta Break · v1",
    ]);
    expect(options.map((o) => o.getAttribute("value"))).toEqual([
      "v-alpha-1",
      "v-alpha-2",
      "v-beta-1",
    ]);
    expect((select as HTMLSelectElement).value).toBe("v-alpha-1");
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
    fireEvent.change(screen.getByRole("combobox"), { target: { value: "v-beta-1" } });
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

    const select = (await screen.findByRole("combobox")) as HTMLSelectElement;
    expect(select.disabled).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: /run backtest/i }));

    expect(
      (screen.getByRole("button", { name: /running…/i }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(select.disabled).toBe(true);

    resolveRun({ status: "ok", data: SEEDED_RUN });
    await screen.findByText("run-9f2c41ab");
    // Settled: the selector unlocks alongside the button.
    expect(select.disabled).toBe(false);
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

    fireEvent.change(screen.getByRole("combobox"), { target: { value: "v-alpha-2" } });

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
      "1735689600000",
      "1756684800000",
      "10000.00",
      "4.0000",
      "2.0000",
      "snapshot_rates",
      "sha256:abc123def456",
      "aarch64-apple-darwin",
      "sha256:9876fedcba",
      "run-9f2c41ab",
      "v-alpha-1",
      "3",
      "2026-09-02T10:15:30.123Z",
    ]) {
      expect(text).toContain(exact);
    }
    // The null fingerprintWarning renders no warning UI and no warning text.
    expect(container.querySelector(".bt-fp-warning")).toBeNull();
    expect(text).not.toContain("fingerprint changed");
  });

  it("renders every KPI tile from the DTO, with an em dash for null ratio fields", async () => {
    const container = await renderRun(SEEDED_RUN);
    const tiles = container.querySelector(".bt-kpis");
    expect(tiles).not.toBeNull();
    const text = (tiles as HTMLElement).textContent ?? "";

    for (const exact of [
      "-84.125",
      "-4.20625",
      "0.4375",
      "180.000",
      "264.125",
      "22.500",
      "33.016",
      "412.750",
      "26.400",
      "-3.115",
      "13.200",
    ]) {
      expect(text).toContain(exact);
    }
    // tradeCount / winCount / lossCount / streaks / skipped counters.
    for (const label of ["Trades", "Wins", "Losses", "Max win streak", "Max loss streak",
      "Skipped sub-lot", "Skipped sub-notional", "Skipped leverage-capped"]) {
      expect(within(tiles as HTMLElement).getByText(label)).toBeTruthy();
    }
    // profitFactor / sharpe / sortino are null in this run → em dash, never 0.
    const tileFor = (label: string) =>
      within(tiles as HTMLElement)
        .getByText(label)
        .closest(".bt-kpi") as HTMLElement;
    for (const label of ["Profit factor", "Sharpe", "Sortino"]) {
      expect(tileFor(label).textContent).toContain("—");
      expect(tileFor(label).textContent).not.toMatch(/\d/);
    }
  });

  it("renders the equity chart: one line over the DTO's points plus the starting-equity reference", async () => {
    const container = await renderRun(SEEDED_RUN);
    const chart = container.querySelector(".bt-equity");
    expect(chart).not.toBeNull();
    const inChart = within(chart as HTMLElement);

    // One polyline series (not two), a quiet area fill, and the reference line.
    expect((chart as HTMLElement).querySelectorAll("polyline")).toHaveLength(1);
    expect((chart as HTMLElement).querySelectorAll("polygon")).toHaveLength(1);
    const ref = (chart as HTMLElement).querySelector(".bt-eq-ref");
    expect(ref).not.toBeNull();
    // The reference is labelled with the DTO's own exact string, and the
    // readout shows the traversed point's exact strings (point[0] before any
    // traversal; the full point-to-point path is the parity test below).
    expect(inChart.getAllByText("10000.00").length).toBeGreaterThan(0);
    expect(inChart.getByText("1735689600000")).toBeTruthy();
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
    // Each row carries its own tradeCount and exact netPnl string.
    const trendingDown = within(rows[1] as HTMLElement);
    expect(trendingDown.getByText("4")).toBeTruthy();
    expect(trendingDown.getByText("-294.250")).toBeTruthy();

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

    for (const [cls, hist] of [
      [".bt-mfe", SEEDED_RUN.mfe],
      [".bt-mae", SEEDED_RUN.mae],
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
    // Seq order, exact strings.
    expect(within(rows[0] as HTMLElement).getAllByText("stop_loss").length).toBeGreaterThan(0);
    expect(within(rows[0] as HTMLElement).getByText("-166.396250")).toBeTruthy();
    expect(within(rows[0] as HTMLElement).getByText("0.01500000")).toBeTruthy();
    // The recorded stop renders its exact decimal string…
    expect(within(rows[0] as HTMLElement).getByText("61500.00")).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText("take_profit")).toBeTruthy();
    expect(within(rows[1] as HTMLElement).getByText("129.545000")).toBeTruthy();
    // …while a null stop (the pre-0011 row shape) renders the em dash.
    expect(within(rows[1] as HTMLElement).getByText("—")).toBeTruthy();
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
    // And the non-null ratio fields render their values, not em dashes.
    const tiles = container.querySelector(".bt-kpis") as HTMLElement;
    expect(within(tiles).getByText("0.681")).toBeTruthy();
    expect(within(tiles).getByText("-0.42")).toBeTruthy();
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

    // Regime twin: four rows, label + tradeCount + netPnl.
    const rgTwin = container.querySelector(".bt-twin-regimes") as HTMLElement;
    expect(rgTwin).not.toBeNull();
    for (const cell of SEEDED_RUN.regimes) {
      const row = rgTwin.querySelector(`[data-regime="${cell.regime}"]`);
      expect(row).not.toBeNull();
      expect((row as HTMLElement).textContent).toContain(String(cell.tradeCount));
      expect((row as HTMLElement).textContent).toContain(cell.netPnl);
    }

    // Histogram twins: underflow + 12 bins + overflow with their counts.
    for (const [cls, hist] of [
      [".bt-twin-mfe", SEEDED_RUN.mfe],
      [".bt-twin-mae", SEEDED_RUN.mae],
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

  it("exposes identical exact-string readouts for keyboard and pointer on the equity plot", async () => {
    const container = await renderRun(SEEDED_RUN);
    const chart = container.querySelector(".bt-equity") as HTMLElement;
    const plot = chart.querySelector(".bt-eq-plot") as HTMLElement;
    const readout = chart.querySelector(".bt-eq-readout") as HTMLElement;
    expect(plot).not.toBeNull();
    expect(readout).not.toBeNull();

    // Keyboard: Left/Right step point-to-point, Home/End jump to first/last.
    plot.focus();
    fireEvent.keyDown(plot, { key: "ArrowRight" });
    expect(readout.textContent).toContain("1736294400000");
    expect(readout.textContent).toContain("10042.75");
    fireEvent.keyDown(plot, { key: "End" });
    expect(readout.textContent).toContain("1737504000000");
    fireEvent.keyDown(plot, { key: "Home" });
    expect(readout.textContent).toContain("1735689600000");
    fireEvent.keyDown(plot, { key: "ArrowRight" });
    fireEvent.keyDown(plot, { key: "ArrowLeft" });
    expect(readout.textContent).toContain("1735689600000");

    // Pointer: hovering the third point's hit marker drives the SAME readout
    // element with the same exact strings — parity is structural.
    const hits = chart.querySelectorAll(".bt-eq-hit");
    expect(hits).toHaveLength(SEEDED_RUN.equity.length);
    fireEvent.mouseOver(hits[2]);
    expect(readout.textContent).toContain("1736899200000");
    expect(readout.textContent).toContain("9910.50");
  });

  it("makes the equity plot and the trade-table scroll region focus stops in visual order", async () => {
    const container = await renderRun(SEEDED_RUN);

    const select = screen.getByRole("combobox");
    const run = screen.getByRole("button", { name: /run backtest/i });
    const plot = container.querySelector(".bt-eq-plot") as HTMLElement;
    const region = container.querySelector(".bt-table-scroll") as HTMLElement;

    for (const el of [plot, region]) {
      expect(el.tabIndex).toBe(0);
    }
    // Focus order follows visual order: selector → Run → plot → table region.
    const follows = (a: HTMLElement, b: HTMLElement) =>
      (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
    expect(follows(select, run)).toBe(true);
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
      expect(parseFloat((col as HTMLElement).style.minWidth)).toBeGreaterThanOrEqual(24);
    }
    const rows = container.querySelectorAll(".bt-regime-row");
    expect(rows.length).toBeGreaterThan(0);
    for (const row of rows) {
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
    // Both expectancies, exactly as the DTO delivered them — no delta, no percent.
    expect(comparison.textContent).toContain("-10.515625");
    expect(comparison.textContent).toContain("20.250");
    expect(comparison.textContent).toContain("0.375");
    expect(comparison.textContent).toContain("0.666");

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
      expect((screen.getByRole("combobox") as HTMLSelectElement).value).toBe("v-alpha-2");
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

    fireEvent.change(screen.getByRole("combobox"), { target: { value: "v-beta-1" } });

    expect(screen.queryByRole("region", { name: /coach/i })).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// r2.s1.w4 — C2 refetch on focus + C3 compare with parent
// ---------------------------------------------------------------------------

/** A catalog where the child version carries one persisted run — the shape the
 * Library serves after `pulse mcp`'s `run_backtest` writes it. */
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
          recentRuns: [
            {
              id: "run-child-1",
              createdAt: "2026-08-22T09:00:00.000Z",
              expectancy: "20.250",
              trades: 6,
            },
          ],
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

    fireEvent.change(await screen.findByRole("combobox"), { target: { value: "v-alpha-2" } });

    await waitFor(() => {
      expect(compareMock).toHaveBeenCalledWith({ childRunId: "run-child-1" });
    });
    const table = await screen.findByRole("table", { name: /before and after/i });
    expect(table.textContent).toContain("-10.515625");
    expect(table.textContent).toContain("20.250");

    const badge = screen.getByText("inputs differ");
    expect(badge.className).toContain("badge-inputs-differ");
    expect(badge.getAttribute("title")).toBe(COMPARE_DTO.inputsNote);
  });

  it("renders the same table with NO badge when the runs' inputs match", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CHILD_CATALOG });
    compareMock.mockResolvedValue({
      status: "ok",
      data: { ...COMPARE_DTO, inputsDiffer: false, inputsNote: null },
    });
    render(<BacktestLabScreen />);

    fireEvent.change(await screen.findByRole("combobox"), { target: { value: "v-alpha-2" } });
    const table = await screen.findByRole("table", { name: /before and after/i });
    expect(table.textContent).toContain("20.250");
    expect(screen.queryByText("inputs differ")).toBeNull();
  });

  it("shows the typed refusal's reason instead of the table", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CHILD_CATALOG });
    compareMock.mockResolvedValue({ status: "error", error: COMPARE_REFUSAL });
    render(<BacktestLabScreen />);

    fireEvent.change(await screen.findByRole("combobox"), { target: { value: "v-alpha-2" } });

    await screen.findByText(/has no run to compare against/i);
    expect(screen.queryByRole("table", { name: /before and after/i })).toBeNull();
  });

  it("offers no comparison for a root version, nor for a child with no run", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    render(<BacktestLabScreen />);

    // v-alpha-1 is selected by default and is a ROOT — nothing to compare with.
    // v-alpha-2 has a parent but no recent run in this catalog.
    fireEvent.change(await screen.findByRole("combobox"), { target: { value: "v-alpha-2" } });

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

    fireEvent.change(await screen.findByRole("combobox"), { target: { value: "v-alpha-2" } });
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
    const select = (await screen.findByRole("combobox")) as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "v-alpha-2" } });
    const callsOnMount = catalogMock.mock.calls.length;

    fireEvent(window, new Event("focus"));

    await waitFor(() => {
      expect(catalogMock.mock.calls.length).toBeGreaterThan(callsOnMount);
    });
    expect(select.value).toBe("v-alpha-2");
  });

  it("falls back to the first option when the selected version is gone after a refetch", async () => {
    catalogMock.mockResolvedValue({ status: "ok", data: CATALOG });
    render(<BacktestLabScreen />);
    const select = (await screen.findByRole("combobox")) as HTMLSelectElement;
    fireEvent.change(select, { target: { value: "v-alpha-2" } });

    // The refetched catalog no longer carries the selected version.
    catalogMock.mockResolvedValue({
      status: "ok",
      data: {
        strategies: [{ ...CATALOG.strategies[0], versions: [catalogVersion("v-alpha-1", null)] }],
      },
    });
    fireEvent(window, new Event("focus"));

    await waitFor(() => {
      expect(select.value).toBe("v-alpha-1");
    });
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

  it("renders the scheme and rule as the DTO names them, the span, and the verdict line", async () => {
    const container = await renderWalkForward(WF_DTO);
    const pane = container.querySelector(".bt-walkforward") as HTMLElement;
    expect(pane).not.toBeNull();
    const text = pane.textContent ?? "";

    // Sentinel values — renderable only off the DTO, never a baked-in label.
    expect(text).toContain("scheme-sentinel/v9");
    expect(text).toContain("rule-sentinel");
    expect(text).toContain("2026-08-01T00:00:00.000Z");
    expect(text).toContain("2026-08-31T23:59:59.000Z");
    expect(text).toContain("wf-44aa19c2");
    // The verdict line: the word, the holding/required tally, the pooled bound.
    expect(text).toContain("FAIL");
    expect(text).toContain("1/2");
    expect(text).toContain("-0.8123");
    // `from_defaulted` is surfaced as provenance, not silently applied.
    expect(text).toContain("defaulted");
  });

  it("renders one row per fold with window, trades, lower bound and holds", async () => {
    const container = await renderWalkForward(WF_DTO);
    const region = within(container).getByRole("region", { name: /fold rows/i });
    const rows = Array.from(region.querySelectorAll("tbody tr"));
    expect(rows).toHaveLength(2);

    const first = rows[0].textContent ?? "";
    expect(first).toContain("2026-08-01T00:00:00.000Z");
    expect(first).toContain("2026-08-15T12:00:00.000Z");
    expect(first).toContain("2");
    expect(first).toContain("-0.4");
    expect(first).toContain("yes");
    // The fold run's own summary fields ride the same row.
    expect(first).toContain("0.250");
    expect(first).toContain("0.500");

    const second = rows[1].textContent ?? "";
    expect(second).toContain("3");
    expect(second).toContain("-1.7");
    expect(second).toContain("no");
    expect(second).toContain("-1.000");
    expect(second).toContain("0.333");
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

    const select = (await screen.findByRole("combobox")) as HTMLSelectElement;
    fireEvent.click(screen.getByRole("button", { name: /walk forward/i }));

    await screen.findByRole("button", { name: /walking forward/i });
    expect(select.disabled).toBe(true);
    expect(
      (screen.getByRole("button", { name: /run backtest/i }) as HTMLButtonElement).disabled,
    ).toBe(true);
    expect(
      (screen.getByRole("button", { name: /walking forward/i }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);

    resolveWf({ status: "ok", data: WF_DTO });
    await screen.findByText("wf-44aa19c2");
    expect(select.disabled).toBe(false);
    expect(
      (screen.getByRole("button", { name: /walk forward/i }) as HTMLButtonElement).disabled,
    ).toBe(false);
  });

  it("clears a rendered walk-forward result when the selector changes", async () => {
    const container = await renderWalkForward(WF_DTO);
    expect(container.querySelector(".bt-walkforward")).not.toBeNull();

    fireEvent.change(screen.getByRole("combobox"), { target: { value: "v-beta-1" } });

    expect(container.querySelector(".bt-walkforward")).toBeNull();
    expect(screen.queryByText("wf-44aa19c2")).toBeNull();
  });
});
