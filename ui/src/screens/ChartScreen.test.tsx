// The Chart tab's rendered tests (r5.s2.w2) — the ADAPTER IS MOCKED, because jsdom has
// no canvas and this file is about what the screen decides, not about what the library
// paints. The adapter's own test (`../chart/adapter.test.ts`) is the one that touches
// `lightweight-charts`, also mocked.
//
// `App` is rendered with the fragment under test, the way `SettingsScreen.test.tsx`
// and `BacktestLabScreen.test.tsx` do: the ROUTES row, the shell, the deep link and the
// screen are then all under test together, and a screen mounted without the shell it
// lives in would throw.
//
// Every case here is one of the spec's own: the free open, the run deep link, the leg
// switch and the over-cap window (with the amendment that a trade outside the loaded
// window re-reads around itself rather than scrolling to a bar that is not drawn), the
// holdout note, `notDrawn`, the empty/loading/failed states, and the About credit line.

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
  adapter: {
    setCandles: vi.fn(),
    setOverlays: vi.fn(),
    setTrades: vi.fn(),
    fitAll: vi.fn(),
    centerOn: vi.fn(),
    destroy: vi.fn(),
  },
}));

vi.mock("../chart/adapter", () => ({
  createChartAdapter: vi.fn(() => h.adapter),
}));

vi.mock("@tauri-apps/api/app", () => ({
  getVersion: vi.fn(() => Promise.resolve("0.1.5")),
}));

const statusMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    serverStatus: () => statusMock(),
    serverConnect: vi.fn(),
    serverAddress: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    serverDisconnect: vi.fn().mockResolvedValue({ status: "ok", data: null }),
    credentialStatus: vi.fn().mockResolvedValue("env"),
    paperSessions: vi.fn().mockResolvedValue({ status: "ok", data: [] }),
    getBacktestRun: vi.fn(),
    chartData: vi.fn(),
  },
}));

import { App } from "../App";
import { commands } from "../bindings";
import type {
  BacktestRunDto,
  ChartCandleDto,
  ChartDataDto,
  ChartOverlayDto,
  ChartRunDto,
  ChartTradeDto,
  ServerStatus,
} from "../bindings";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const DAY = 86_400_000;
/** 2025-01-01T00:00Z — the run's own first bar, and the fixtures' anchor. */
const FIRST = 1_735_689_600_000;
/** The run's own last bar: 400 days later, so its 15m leg is far over the cap
 *  (38,401 bars) while its 4h leg is not (2,401 bars). */
const LAST = FIRST + 400 * DAY;
/** 2025-04-11T00:00Z — the first trade's entry, inside the primary span. */
const ENTRY = FIRST + 100 * DAY;
/** 2025-03-27T00:00Z — the second trade's entry, 15 days before the first. */
const EARLIER_ENTRY = FIRST + 85 * DAY;
const TF_MS_15M = 900_000;
/** The window's half-width: ±500 bars, the plan's own reading. */
const HALF = 500 * TF_MS_15M;

const SERIES_OVERLAYS: ChartOverlayDto[] = [
  { kind: "level", label: "support:3-3-0.005-2-100", values: [null, "93460.2"] },
  { kind: "line", label: "ema:20", values: ["90000", "90100"] },
  {
    kind: "pivot",
    label: "pivot_low:2-2",
    events: [{ confirmTime: String(FIRST + DAY), peakTime: String(FIRST), price: "92796.5" }],
  },
  { kind: "pattern", label: "cdl:engulfing", events: [{ time: String(FIRST), value: "100" }] },
];

function candle(t: number): ChartCandleDto {
  return { t: String(t), o: "93548.8", h: "94449.2", l: "93460.2", c: "93810", v: "13962.019" };
}

function trade(over: Partial<ChartTradeDto>): ChartTradeDto {
  return {
    side: "long",
    entryTime: String(ENTRY),
    entryPrice: "90000",
    exitTime: String(ENTRY + DAY),
    exitPrice: "92000",
    exitReason: "take_profit",
    realizedR: "2",
    ...over,
  };
}

function runLayer(over: Partial<ChartRunDto> = {}): ChartRunDto {
  return {
    runId: "run-1",
    versionId: "v-1",
    series: "htf",
    trades: [trade({})],
    overlays: SERIES_OVERLAYS,
    notDrawn: ["rsi:14"],
    ...over,
  };
}

function chartDto(over: Partial<ChartDataDto> = {}): ChartDataDto {
  return {
    pair: "BTCUSDT",
    timeframe: "4h",
    dataVersion: "9a1dcf67cf4ea260",
    // Two bars: the run's own first and last, so a trade inside the run's span is a
    // bar this window already holds (which is what makes "centre, don't re-read" the
    // right answer for it).
    candles: [candle(FIRST), candle(LAST)],
    holdout: null,
    run: null,
    ...over,
  };
}

/** The read's DTO, wrapped as the command's `typedError` union answers it. */
function ok<T>(data: T): { status: "ok"; data: T } {
  return { status: "ok", data };
}

function failure(message: string) {
  return {
    status: "error" as const,
    error: {
      code: "validation" as const,
      message,
      run_id: null,
      session_id: null,
      child_run_id: null,
    },
  };
}

/** The run DTO the screen reads its pair, both timeframes and its span from. Only the
 *  fields the chart reads are populated: it is a read of the run, not the Lab's whole
 *  result. */
function runDto(over: Partial<BacktestRunDto> = {}): BacktestRunDto {
  return {
    runId: "run-1",
    strategyVersionId: "v-1",
    pair: "BTCUSDT",
    primaryTimeframe: "15m",
    primaryDataVersion: "dv-primary",
    htfTimeframe: "4h",
    htfDataVersion: "dv-htf",
    d1DataVersion: null,
    usesHtf: true,
    firstOpenTimeMs: String(FIRST),
    lastCloseTimeMs: String(LAST),
    ...over,
  } as unknown as BacktestRunDto;
}

function okStatus(): { status: "ok"; data: ServerStatus } {
  return {
    status: "ok",
    data: {
      state: "up",
      binary_version: "0.1.0",
      engine_fingerprint: "f0dbdf5748d38d5c",
      reason: null,
      role: null,
    },
  };
}

/** Render the app on a fragment and wait for the screen to exist. */
async function open(hash: string) {
  window.location.hash = hash;
  render(<App />);
  await screen.findByRole("region", { name: "Chart" });
}

beforeEach(() => {
  statusMock.mockReset();
  statusMock.mockResolvedValue(okStatus());
  for (const fn of Object.values(h.adapter)) fn.mockClear();
  vi.mocked(commands.getBacktestRun).mockReset();
  vi.mocked(commands.getBacktestRun).mockResolvedValue(ok(runDto()));
  vi.mocked(commands.chartData).mockReset();
  // The common case is a run open, so that is the standing answer; the free-chart case
  // sets its own.
  vi.mocked(commands.chartData).mockResolvedValue(ok(chartDto({ run: runLayer() })));
});

afterEach(() => {
  window.location.hash = "";
});

// ---------------------------------------------------------------------------

describe("opening the Chart tab", () => {
  it("opens on BTCUSDT 4h with no run, and draws the candles it read", async () => {
    vi.mocked(commands.chartData).mockResolvedValue(ok(chartDto()));
    await open("#/chart");

    await waitFor(() =>
      expect(commands.chartData).toHaveBeenCalledWith({
        pair: "BTCUSDT",
        timeframe: "4h",
        fromMs: null,
        toMs: null,
        runId: null,
      }),
    );
    expect(commands.getBacktestRun).not.toHaveBeenCalled();
    await waitFor(() => expect(h.adapter.fitAll).toHaveBeenCalled());
    expect(h.adapter.setCandles).toHaveBeenCalledWith(chartDto().candles);
    expect(h.adapter.setOverlays).toHaveBeenCalledWith([]);
    expect(
      screen.getByText("No run is open, so there are no trades to draw."),
    ).toBeTruthy();
  });

  it("opens the run the fragment names, reading it for its pair and both timeframes", async () => {
    await open("#/chart?run=run-1");

    await waitFor(() =>
      expect(commands.getBacktestRun).toHaveBeenCalledWith({ runId: "run-1" }),
    );
    // The run's rule was compiled on its higher timeframe (`usesHtf`), so that is
    // the leg whose overlays exist: the tab opens where the rule's values live.
    await waitFor(() =>
      expect(commands.chartData).toHaveBeenCalledWith({
        pair: "BTCUSDT",
        timeframe: "4h",
        fromMs: null,
        toMs: null,
        runId: "run-1",
      }),
    );
    expect(await screen.findByText("Backtest run run-1 · 4h")).toBeTruthy();
  });

  it("draws a run's overlays and trades through the adapter", async () => {
    await open("#/chart?run=run-1");

    await waitFor(() => expect(h.adapter.setOverlays).toHaveBeenCalled());
    expect(h.adapter.setOverlays).toHaveBeenCalledWith(SERIES_OVERLAYS);
    expect(h.adapter.setTrades).toHaveBeenCalledWith([trade({})]);
    expect(await screen.findByText("Not drawn on the chart: rsi:14")).toBeTruthy();
  });
});

describe("the leg switch", () => {
  it("asks for a trade before it reads a leg whose whole span is over the bar cap", async () => {
    await open("#/chart?run=run-1");
    await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));

    fireEvent.click(screen.getByRole("button", { name: /^15m/ }));

    expect(
      await screen.findByText(
        "The whole run at 15m is 38,401 bars — more than one read (the cap is 20,000). Pick a trade to load 15m around it.",
      ),
    ).toBeTruthy();
    // Nothing was read: a read the server would refuse with the cap is not sent.
    expect(commands.chartData).toHaveBeenCalledTimes(1);
    expect(h.adapter.setCandles).toHaveBeenCalledTimes(1);
  });

  it("reads a window around the selected trade once one is picked", async () => {
    await open("#/chart?run=run-1");
    await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole("button", { name: /^15m/ }));
    await screen.findByText(/more than one read/);

    fireEvent.click(screen.getByRole("button", { name: /^Trade 1, long/ }));

    await waitFor(() =>
      expect(commands.chartData).toHaveBeenLastCalledWith({
        pair: "BTCUSDT",
        timeframe: "15m",
        fromMs: String(ENTRY - HALF),
        toMs: String(ENTRY + HALF),
        runId: "run-1",
      }),
    );
  });

  it("re-reads around a trade whose entry bar is not in the loaded window (amendment B)", async () => {
    // The flow the amendment is about: switch to the 15m leg, pick a trade to load a
    // window around it, and then pick the OTHER trade — whose entry bar is not in that
    // window. Its row is in the table (the read returned it: it entered before the
    // window and exited inside it), so the tab must read around that bar, never scroll
    // to a bar that was never loaded.
    vi.mocked(commands.chartData)
      // the mount read, on the run's 4h leg, whose span fits in one read
      .mockResolvedValueOnce(ok(chartDto({ run: runLayer() })))
      // every read after it: the 15m window, holding both trades
      .mockResolvedValue(
        ok(
          chartDto({
            timeframe: "15m",
            candles: [candle(ENTRY - HALF), candle(ENTRY + HALF)],
            run: runLayer({
              series: "primary",
              trades: [
                trade({}),
                trade({
                  entryTime: String(EARLIER_ENTRY),
                  exitTime: String(ENTRY),
                  exitPrice: "88000",
                  realizedR: "-1",
                }),
              ],
            }),
          }),
        ),
      );
    await open("#/chart?run=run-1");
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenCalledTimes(1));

    // The 15m leg is over the cap, so the tab asks for a trade first; picking one
    // loads the window around its entry.
    fireEvent.click(screen.getByRole("button", { name: /^15m/ }));
    await screen.findByText(/more than one read/);
    fireEvent.click(screen.getByRole("button", { name: /^Trade 1, long/ }));
    await waitFor(() =>
      expect(commands.chartData).toHaveBeenNthCalledWith(2, {
        pair: "BTCUSDT",
        timeframe: "15m",
        fromMs: String(ENTRY - HALF),
        toMs: String(ENTRY + HALF),
        runId: "run-1",
      }),
    );

    // Now the other trade, whose entry bar the loaded window does not hold.
    fireEvent.click(await screen.findByRole("button", { name: /^Trade 2, long/ }));

    await waitFor(() =>
      expect(commands.chartData).toHaveBeenLastCalledWith({
        pair: "BTCUSDT",
        timeframe: "15m",
        fromMs: String(EARLIER_ENTRY - HALF),
        toMs: String(EARLIER_ENTRY + HALF),
        runId: "run-1",
      }),
    );
  });

  it("centres the chart on a trade that is already drawn, without re-reading", async () => {
    await open("#/chart?run=run-1");
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenCalledTimes(1));

    fireEvent.click(screen.getByRole("button", { name: /^Trade 1, long/ }));

    await waitFor(() => expect(h.adapter.centerOn).toHaveBeenCalledWith(String(ENTRY)));
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });
});

describe("the plain states", () => {
  it("names the withheld bars when the read was cut at a holdout", async () => {
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(
        chartDto({
          holdout: { holdoutStartMs: String(ENTRY), barsWithheld: 12 },
        }),
      ),
    );
    await open("#/chart");

    expect(
      await screen.findByText("Holdout from 2025-04-11 UTC — 12 bars withheld"),
    ).toBeTruthy();
  });

  it("says the window is empty rather than drawing an empty chart", async () => {
    vi.mocked(commands.chartData).mockResolvedValue(ok(chartDto({ candles: [] })));
    await open("#/chart");

    expect(
      await screen.findByText("No 4h bars for BTCUSDT in this window."),
    ).toBeTruthy();
    expect(h.adapter.setCandles).not.toHaveBeenCalled();
  });

  it("shows the read's own refusal and draws nothing", async () => {
    vi.mocked(commands.chartData).mockResolvedValue(
      failure("a window of 21001 candles is over the chart read's 20000-candle cap"),
    );
    await open("#/chart");

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("20000-candle cap");
    expect(h.adapter.setCandles).not.toHaveBeenCalled();
    expect(h.adapter.setOverlays).not.toHaveBeenCalled();
  });

  it("says what it is reading while the read is in flight", async () => {
    vi.mocked(commands.chartData).mockReturnValue(new Promise(() => {}));
    await open("#/chart");

    expect(screen.getByText("Loading BTCUSDT 4h…")).toBeTruthy();
    expect(h.adapter.setCandles).not.toHaveBeenCalled();
  });
});

describe("the table copy", () => {
  it("carries the run's trades with their exact strings beside every figure", async () => {
    await open("#/chart?run=run-1");

    const table = await screen.findByRole("table", { name: "Trades in the window" });
    const rows = within(table).getAllByRole("cell");
    const text = rows.map((cell) => cell.textContent).join(" | ");
    expect(text).toContain("long");
    expect(text).toContain("2025-04-11 00:00 UTC");
    expect(text).toContain("90,000.00");
    expect(text).toContain("2025-04-12 00:00 UTC");
    expect(text).toContain("+2.00 R");
    // The server's exact text stays reachable on the element, never only as display
    // text (the `Figure` contract).
    expect(within(table).getByText("+2.00 R").getAttribute("data-exact")).toBe("2");
  });

  it("carries every pivot and pattern event, ghost bar included", async () => {
    await open("#/chart?run=run-1");

    const table = await screen.findByRole("table", { name: "Structure and patterns" });
    const text = within(table)
      .getAllByRole("cell")
      .map((cell) => cell.textContent)
      .join(" | ");
    expect(text).toContain("pivot_low:2-2");
    expect(text).toContain("cdl:engulfing");
    // The ghost's peak bar is data too: an unfilled marker still has to be readable.
    expect(text).toContain("peak (ghost)");
    expect(text).toContain("2025-01-02 00:00 UTC");
    // A pivot's value is a price; a pattern's is the score the read sent, exactly as
    // sent — "100.0000" would read as a price of 100.
    expect(text).toContain("92,796.50");
    expect(text).toContain("100");
    expect(text).not.toContain("100.0000");
  });

  it("carries one row per window bar for the level and line layers, on demand", async () => {
    await open("#/chart?run=run-1");

    const details = await screen.findByText(/Bars, levels and lines/);
    fireEvent.click(details);
    const table = await screen.findByRole("table", { name: "Bars, levels and lines" });
    const text = within(table)
      .getAllByRole("cell")
      .map((cell) => cell.textContent)
      .join(" | ");
    // The warm-up bar draws nothing, so the table says so with the em dash rather
    // than with a zero (PDR-0011).
    expect(text).toContain("—");
    expect(text).toContain("93,460.20");
    expect(text).toContain("90,000.00");
  });
});

describe("Settings → About", () => {
  it("carries the TradingView credit, opening in a new tab and never a same-window navigation", async () => {
    window.location.hash = "#/settings";
    render(<App />);

    expect(
      await screen.findByText(/Charts by TradingView Lightweight Charts™/),
    ).toBeTruthy();
    const link = screen.getByRole("link", { name: "tradingview.com" });
    expect(link.getAttribute("href")).toBe("https://www.tradingview.com/");
    // The app has no opener plugin and no navigation handler of its own: a
    // same-window link would replace the whole app view with tradingview.com.
    expect(link.getAttribute("target")).toBe("_blank");
    expect(link.getAttribute("rel")).toContain("noopener");
    expect(link.getAttribute("rel")).toContain("noreferrer");
    // The app's own version, read from the app — never a literal that goes stale.
    expect(await screen.findByText("PulseTrader 0.1.5")).toBeTruthy();
  });
});
