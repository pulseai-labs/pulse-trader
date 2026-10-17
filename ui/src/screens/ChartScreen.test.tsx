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

import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => ({
  adapter: {
    setCandles: vi.fn(),
    setOverlays: vi.fn(),
    setTrades: vi.fn(),
    setMarks: vi.fn(),
    onCandleClick: vi.fn(),
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
    listMarks: vi.fn(),
    writeMark: vi.fn(),
    withdrawMark: vi.fn(),
  },
}));

import { App } from "../App";
import { commands } from "../bindings";
import type {
  BacktestRunDto,
  BusErrorCode,
  ChartCandleDto,
  ChartDataDto,
  ChartOverlayDto,
  ChartRunDto,
  ChartTradeDto,
  MarkDto,
  MarkListDto,
  ServerStatus,
  TradeRowDto,
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
/** The higher timeframe's own bar, and the plan's half-width on it. */
const TF_MS_4H = 14_400_000;
/** The window's half-width: ±500 bars, the plan's own reading. */
const HALF = 500 * TF_MS_15M;
const HALF_4H = 500 * TF_MS_4H;

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

/** One stored trade in the RUN read's own shape (`TradeRowDto`) — what
 *  `getBacktestRun` answers with, which is the tab's first source of trades. */
function tradeRow(over: Partial<TradeRowDto> = {}): TradeRowDto {
  return {
    direction: "long",
    qty: "0.1",
    entryPrice: "90000",
    exitPrice: "92000",
    entrySignalTime: String(ENTRY),
    entryFillTime: String(ENTRY),
    exitSignalTime: String(ENTRY + DAY),
    exitFillTime: String(ENTRY + DAY),
    feesTotal: "1.8",
    fundingTotal: "0",
    slippageTotal: "0.9",
    realizedPnl: "180",
    realizedR: "2",
    mfeR: "2.4",
    maeR: "-0.3",
    exitReason: "take_profit",
    source: "signal",
    regime: "trend_up",
    stopPrice: "89000",
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

function failure(message: string, code: BusErrorCode = "validation") {
  return {
    status: "error" as const,
    error: {
      code,
      message,
      run_id: null,
      session_id: null,
      child_run_id: null,
    },
  };
}

/** One example-trade mark, as `listMarks` reads it (r5.s3.w1). Times are ms strings. */
function mark(over: Partial<MarkDto> = {}): MarkDto {
  return {
    markId: "mark-1",
    pair: "BTCUSDT",
    timeframe: "4h",
    barTime: String(FIRST),
    side: "long",
    setup: "support-rejection longs",
    note: null,
    writer: { kind: "trader", name: "Alice", selfReported: false },
    dataVersion: "9a1dcf67cf4ea260",
    createdAt: String(FIRST + TF_MS_4H),
    ...over,
  };
}

/** The list read's DTO, wrapped as the command's `typedError` union answers it. */
function markList(over: Partial<MarkListDto> = {}): MarkListDto {
  return { marks: [], hiddenByHoldout: null, ...over };
}

/** A promise the test settles itself, so that two reads' answers can be made to arrive
 *  in an order the server would never be asked for (CR5). */
function deferred<T>() {
  let resolve!: (answer: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { resolve, promise };
}

/** Open run-old on its 4h leg with that chart read left in flight, then move the
 *  fragment to run-new while it is still pending. The new run's own read is the
 *  caller's to settle, so each test below fixes the order of the two answers. */
async function switchRunWhileChartPending(
  oldChart: ReturnType<typeof deferred<unknown>>,
  newRunRead: ReturnType<typeof deferred<unknown>>,
) {
  vi.mocked(commands.getBacktestRun)
    .mockResolvedValueOnce(ok(runDto({ runId: "run-old", trades: [tradeRow()] })) as never)
    .mockReturnValueOnce(newRunRead.promise as never);
  vi.mocked(commands.chartData).mockReturnValue(oldChart.promise as never);

  await open("#/chart?run=run-old");
  await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));

  window.location.hash = "#/chart?run=run-new";
  fireEvent(window, new HashChangeEvent("hashchange"));
  await waitFor(() => expect(commands.getBacktestRun).toHaveBeenCalledTimes(2));
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
    // The run read always carries the run's stored trades (the chart tab reads them
    // before any chart read: CR1), so the fixture carries them too, empty by default.
    trades: [],
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
  // No marks by default: every existing case below is about the chart, not the marks,
  // and a hanging or failing marks read would put a state message on its screen.
  vi.mocked(commands.listMarks).mockReset();
  vi.mocked(commands.listMarks).mockResolvedValue(ok(markList()));
  vi.mocked(commands.writeMark).mockReset();
  vi.mocked(commands.withdrawMark).mockReset();
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

  it("clears the failed leg's error when the leg the trader picks is only asking for a trade", async () => {
    // The 4h leg's read fails. The trader then switches to the 15m leg, which is over
    // the cap: no read for 15m is attempted at all, so the 4h read's error is not this
    // leg's and must not stand under the 15m notice — the notice is the whole truth,
    // and what it asks for (a trade of the run's own) is still there to pick.
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(runDto({ trades: [tradeRow()] })) as never,
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      failure("the chart read refused", "validation") as never,
    );
    await open("#/chart?run=run-1");
    expect(await screen.findByRole("alert")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /^15m/ }));

    expect(await screen.findByText(/Pick a trade to load 15m around it/)).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
    // The run's own stored trades are still the selector the notice asks about.
    const table = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(table).getAllByRole("button")).toHaveLength(1);
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });

  it("reads a window around the selected trade once one is picked", async () => {
    await open("#/chart?run=run-1");
    await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));
    fireEvent.click(screen.getByRole("button", { name: /^15m/ }));
    await screen.findByText(/more than one read/);

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));

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
    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));
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

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));

    await waitFor(() => expect(h.adapter.centerOn).toHaveBeenCalledWith(String(ENTRY)));
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });
});

describe("a leg over the cap before any chart read (CR1)", () => {
  it("offers the run's own stored trades when the leg it opens on is over the cap", async () => {
    // A primary-only 15m run (no higher timeframe at all, so no leg switch exists) whose
    // whole span is 38,401 bars: the tab can only ask for a trade. The run read already
    // carries that run's trades, so there is something to ask WITH.
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          primaryTimeframe: "15m",
          htfTimeframe: null,
          usesHtf: false,
          trades: [
            tradeRow({}),
            tradeRow({
              direction: "short",
              entryFillTime: String(EARLIER_ENTRY),
              exitFillTime: String(EARLIER_ENTRY + DAY),
              exitPrice: "88000",
              realizedR: "-1",
            }),
          ],
        }),
      ),
    );
    await open("#/chart?run=run-1");

    expect(await screen.findByText(/Pick a trade to load 15m around it/)).toBeTruthy();
    // Nothing is read: a window over the cap is never sent to be refused.
    expect(commands.chartData).not.toHaveBeenCalled();
    // The rows are the run's own trades, and the table says which trades it is showing.
    const table = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(table).getAllByRole("button")).toHaveLength(2);
    expect(screen.getByText("Backtest run run-1 · 15m")).toBeTruthy();

    // Picking one plans the ±500-bar window around its entry, exactly as amendment B
    // does for a trade picked after a read: exactly one bounded chart read.
    fireEvent.click(within(table).getByRole("button", { name: /^Trade 1, long/ }));
    await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));
    expect(commands.chartData).toHaveBeenCalledWith({
      pair: "BTCUSDT",
      timeframe: "15m",
      fromMs: String(ENTRY - HALF),
      toMs: String(ENTRY + HALF),
      runId: "run-1",
    });
  });
});

describe("a switch to another run (CR3)", () => {
  it("clears the previous run's chart and trades before the new run's context applies", async () => {
    // run-old is an ordinary under-cap 4h leg, drawn with its own trade; run-new is a
    // primary-only 15m run that is over the cap, so it starts by asking for a trade.
    // Both runs carry a trade, at different entries — the rows must never be the other
    // run's, and the chart must never keep the other run's candles.
    vi.mocked(commands.getBacktestRun).mockImplementation((request) =>
      Promise.resolve({
        status: "ok",
        data:
          request.runId === "run-old"
            ? runDto({ runId: "run-old", trades: [tradeRow({})] })
            : runDto({
                runId: "run-new",
                primaryTimeframe: "15m",
                htfTimeframe: null,
                usesHtf: false,
                trades: [
                  tradeRow({
                    entryFillTime: String(EARLIER_ENTRY),
                    exitFillTime: String(EARLIER_ENTRY + DAY),
                  }),
                ],
              }),
      } as never),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(chartDto({ run: runLayer({ runId: "run-old", series: "htf" }) })),
    );

    await open("#/chart?run=run-old");
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenCalledTimes(1));
    const before = await screen.findByRole("table", { name: "Trades in the run" });
    expect(within(before).getByText("2025-04-11 00:00 UTC")).toBeTruthy();

    // The fragment names another run while the tab stays mounted.
    window.location.hash = "#/chart?run=run-new";
    fireEvent(window, new HashChangeEvent("hashchange"));

    // The new run's own ask, its own rows — and none of the old run's.
    expect(await screen.findByText(/Pick a trade to load 15m around it/)).toBeTruthy();
    expect(screen.getByText("Backtest run run-new · 15m")).toBeTruthy();
    // The previous run's drawing is gone, not kept under the new run's name.
    expect(h.adapter.setCandles).toHaveBeenCalledTimes(2);
    expect(h.adapter.setCandles).toHaveBeenLastCalledWith([]);
    expect(h.adapter.setOverlays).toHaveBeenLastCalledWith([]);
    expect(h.adapter.setTrades).toHaveBeenLastCalledWith([]);
    expect(h.adapter.fitAll).toHaveBeenCalledTimes(1);
    // The rows are run-new's own stored trades, and the table says so.
    const after = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(after).queryByText("2025-04-11 00:00 UTC")).toBeNull();
    expect(within(after).getByText("2025-03-27 00:00 UTC")).toBeTruthy();
    // The old run's read is not repeated for the new run: it is over the cap.
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });
});

describe("an answer that outlived its run (CR5)", () => {
  it("keeps the new run's not-found refusal when the old run's chart answer lands after it", async () => {
    const oldChart = deferred<unknown>();
    const newRun = deferred<unknown>();
    await switchRunWhileChartPending(oldChart, newRun);

    // The new run is not a run at all: its read refuses with not_found.
    await act(async () => newRun.resolve(failure("no backtest run run-new", "not_found")));
    expect((await screen.findByRole("alert")).textContent).toContain("no backtest run run-new");

    // The old run's chart now succeeds. It was asked for under an identity the fragment
    // no longer names, so it may draw nothing and erase nothing: the refusal stands.
    await act(async () =>
      oldChart.resolve(ok(chartDto({ run: runLayer({ runId: "run-old" }) }))),
    );

    expect(screen.getByRole("alert").textContent).toContain("no backtest run run-new");
    expect(h.adapter.setCandles).not.toHaveBeenCalled();
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });

  it("drops the old run's refusal when the new run is a valid over-cap run", async () => {
    const oldChart = deferred<unknown>();
    const newRun = deferred<unknown>();
    await switchRunWhileChartPending(oldChart, newRun);

    // The old run's failure lands while the new run's read is still in flight: it
    // belongs to a run the fragment has left, so nothing is shown for it.
    await act(async () =>
      oldChart.resolve(failure("run-old chart cannot read old snapshot", "not_found")),
    );
    expect(screen.queryByRole("alert")).toBeNull();

    // The new run is a valid primary-only 15m run whose 400-day span is over the cap:
    // it asks for a trade and shows its OWN stored rows.
    await act(async () =>
      newRun.resolve(
        ok(
          runDto({
            runId: "run-new",
            primaryTimeframe: "15m",
            htfTimeframe: null,
            usesHtf: false,
            trades: [
              tradeRow({
                entryFillTime: String(EARLIER_ENTRY),
                exitFillTime: String(EARLIER_ENTRY + DAY),
              }),
            ],
          }),
        ),
      ),
    );

    expect(await screen.findByText(/Pick a trade to load 15m around it/)).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByText("Backtest run run-new · 15m")).toBeTruthy();
    const table = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(table).getByText("2025-03-27 00:00 UTC")).toBeTruthy();
    expect(commands.chartData).toHaveBeenCalledTimes(1);
    expect(h.adapter.setCandles).not.toHaveBeenCalled();
  });

  it("keeps a leg switch inside one run's D4 behaviour while a chart read is in flight", async () => {
    const firstRead = deferred<unknown>();
    vi.mocked(commands.chartData).mockReturnValue(firstRead.promise as never);

    await open("#/chart?run=run-1");
    await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));

    // Same run, same fragment: switching to the over-cap 15m leg asks for a trade. It
    // sends no read and clears nothing (D4) — the tab has not even drawn the 4h read.
    fireEvent.click(screen.getByRole("button", { name: /^15m/ }));
    expect(await screen.findByText(/Pick a trade to load 15m around it/)).toBeTruthy();
    expect(commands.chartData).toHaveBeenCalledTimes(1);
    expect(h.adapter.setCandles).not.toHaveBeenCalled();

    // The superseded 4h read answers afterwards: it writes nothing — no window under the
    // 15m chip, no refusal — and the notice it was switched away from stands.
    await act(async () => firstRead.resolve(ok(chartDto({ run: runLayer() }))));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(h.adapter.setCandles).not.toHaveBeenCalled();
    expect(screen.getByText(/Pick a trade to load 15m around it/)).toBeTruthy();
    expect(screen.getByText("Backtest run run-1 · 15m")).toBeTruthy();
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

    const table = await screen.findByRole("table", { name: "Trades in the run" });
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

// ---------------------------------------------------------------------------
// The fence over every trade the tab lists (holdout)
// ---------------------------------------------------------------------------

describe("the holdout fence over the tab's trades", () => {
  /** The fence: two days after the first trade's exit, before the second trade. */
  const HOLDOUT = ENTRY + 2 * DAY;
  /** A fenced trade's entry — at or after the fence, so it may never be listed. */
  const FENCED_ENTRY = ENTRY + 3 * DAY;

  /** The run read's own answer: its boundary plus one safe and one fenced trade. */
  function fencedRunDto(over: Partial<BacktestRunDto> = {}): BacktestRunDto {
    return {
      ...runDto({
        primaryTimeframe: "15m",
        htfTimeframe: null,
        usesHtf: false,
        trades: [
          tradeRow({}),
          tradeRow({
            entryFillTime: String(FENCED_ENTRY),
            exitFillTime: String(FENCED_ENTRY + DAY),
          }),
        ],
        ...over,
      }),
      holdoutStartMs: String(HOLDOUT),
    } as unknown as BacktestRunDto;
  }

  it("hides a stored trade that reaches the fence before any chart answer arrives", async () => {
    // The over-cap leg sends no read at all (CR1), so the run read is the ONLY
    // source of these rows: the fence has to be applied there, or it leaks.
    vi.mocked(commands.getBacktestRun).mockResolvedValue(ok(fencedRunDto()));
    await open("#/chart?run=run-1");

    expect(await screen.findByText(/Pick a trade to load 15m around it/)).toBeTruthy();
    expect(commands.chartData).not.toHaveBeenCalled();
    const table = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(table).getAllByRole("button")).toHaveLength(1);
    expect(within(table).getByText("2025-04-11 00:00 UTC")).toBeTruthy();
    expect(within(table).queryByText("2025-04-14 00:00 UTC")).toBeNull();
  });

  it("hides a stored trade that reaches the fence a drawn window brings back", async () => {
    // A window read answers with its own rows, and the tab lists the run's: the
    // fence has to hold for the run's list too, not only the chart answer's.
    vi.mocked(commands.getBacktestRun).mockResolvedValue(ok(fencedRunDto()));
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(chartDto({ run: runLayer({ trades: [trade({})] }) })),
    );
    await open("#/chart?run=run-1");

    expect(await screen.findByText("Trades in the run")).toBeTruthy();
    const table = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(table).queryByText("2025-04-14 00:00 UTC")).toBeNull();
    expect(within(table).getByText("2025-04-11 00:00 UTC")).toBeTruthy();
  });

  it("uses the chart answer's own fence when the run read carried none", async () => {    // A freeze opened after the run read: the chart read is the fresher word, and
    // the tab must not keep listing a trade the fence now covers.
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          trades: [
            tradeRow({}),
            tradeRow({
              entryFillTime: String(FENCED_ENTRY),
              exitFillTime: String(FENCED_ENTRY + DAY),
            }),
          ],
        }),
      ),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(
        chartDto({
          holdout: { holdoutStartMs: String(HOLDOUT), barsWithheld: 3 },
          run: runLayer({ trades: [trade({})] }),
        }),
      ),
    );
    await open("#/chart?run=run-1");

    expect(await screen.findByText(/bars withheld/)).toBeTruthy();
    const table = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(table).getAllByRole("button")).toHaveLength(1);
    expect(within(table).queryByText("2025-04-14 00:00 UTC")).toBeNull();
  });

  it("lets a chart answer with no holdout clear the run read's older boundary", async () => {
    // The freeze was closed between the two reads. The chart answer is the fresher
    // word, and a `null` holdout there means no freeze was open at that read — so the
    // tab must not keep hiding a trade the fence no longer covers.
    // A two-leg run whose 4h leg fits in one read, so a chart answer really arrives.
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(fencedRunDto({ htfTimeframe: "4h", usesHtf: true })),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(chartDto({ run: runLayer({ trades: [trade({})] }) })),
    );
    await open("#/chart?run=run-1");

    expect(await screen.findByText(/of these trades/)).toBeTruthy();
    const table = screen.getByRole("table", { name: "Trades in the run" });
    expect(within(table).getAllByRole("button")).toHaveLength(2);
    expect(within(table).getByText("2025-04-14 00:00 UTC")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// The run-wide selector (navigation must never be erased by a window)
// ---------------------------------------------------------------------------

describe("the run-wide trade selector", () => {
  it("keeps a distant run trade selectable after a bounded window is drawn", async () => {
    // The run recorded two trades 15 days apart. The bounded window read returns
    // ONLY the first: the second is not in the window, so it cannot be drawn or
    // centred — it is still the trader's next navigation move.
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          trades: [
            tradeRow({}),
            tradeRow({
              entryFillTime: String(EARLIER_ENTRY),
              exitFillTime: String(EARLIER_ENTRY + DAY),
            }),
          ],
        }),
      ),
    );
    vi.mocked(commands.chartData)
      .mockResolvedValueOnce(ok(chartDto({ run: runLayer({ trades: [trade({})] }) })))
      .mockResolvedValue(
        ok(
          chartDto({
            timeframe: "15m",
            candles: [candle(ENTRY - HALF), candle(ENTRY + HALF)],
            run: runLayer({ series: "primary", trades: [trade({})] }),
          }),
        ),
      );
    await open("#/chart?run=run-1");
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenCalledTimes(1));

    // The 15m leg is over the cap: the tab asks for a trade, then reads around it.
    fireEvent.click(screen.getByRole("button", { name: /^15m/ }));
    await screen.findByText(/more than one read/);
    const table = await screen.findByRole("table", { name: "Trades in the run" });
    fireEvent.click(within(table).getByRole("button", { name: /^Trade 1, long/ }));
    await waitFor(() =>
      expect(commands.chartData).toHaveBeenLastCalledWith({
        pair: "BTCUSDT",
        timeframe: "15m",
        fromMs: String(ENTRY - HALF),
        toMs: String(ENTRY + HALF),
        runId: "run-1",
      }),
    );

    // The window holds only the first trade; the distant one is still here, and
    // picking it reads a window around ITS entry rather than scrolling nowhere.
    expect(within(table).getAllByRole("button")).toHaveLength(2);
    fireEvent.click(within(table).getByRole("button", { name: /^Trade 2, long/ }));
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
});

// ---------------------------------------------------------------------------
// The selection the tab is drawing (a free choice must not be lost)
// ---------------------------------------------------------------------------

describe("the selection the tab owns", () => {
  it("draws a free pair and timeframe chosen after the fragment's run read refused", async () => {
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      failure("no backtest run gone", "not_found") as never,
    );
    const free = chartDto({ candles: [candle(FIRST), candle(FIRST + 4 * 3_600_000)] });
    vi.mocked(commands.chartData).mockResolvedValue(ok(free));
    await open("#/chart?run=gone");

    // The refusal is shown, the free controls are the only controls, and the trader
    // picks one: the tab must draw what it just read.
    expect(await screen.findByText(/no backtest run gone/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /^ETHUSDT/ }));
    await waitFor(() =>
      expect(commands.chartData).toHaveBeenLastCalledWith({
        pair: "ETHUSDT",
        timeframe: "4h",
        fromMs: null,
        toMs: null,
        runId: null,
      }),
    );
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenLastCalledWith(free.candles));
  });

  it("keeps a free choice made while the run read is still in flight", async () => {
    const runRead = deferred<unknown>();
    vi.mocked(commands.getBacktestRun).mockReturnValue(runRead.promise as never);
    const free = chartDto({ candles: [candle(FIRST), candle(FIRST + 4 * 3_600_000)] });
    vi.mocked(commands.chartData).mockResolvedValue(ok(free));
    await open("#/chart?run=run-1");

    fireEvent.click(await screen.findByRole("button", { name: /^SOLUSDT/ }));
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenLastCalledWith(free.candles));

    // The run read lands afterwards: it must write nothing for this chart, and the
    // free controls must stay the trader's.
    await act(async () => runRead.resolve(ok(runDto())));
    expect(screen.getByText("No run")).toBeTruthy();
    expect(screen.queryByText(/Backtest run run-1/)).toBeNull();
    expect(h.adapter.setCandles).toHaveBeenLastCalledWith(free.candles);
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });

  it("keeps the free chart when the in-flight run read refuses afterwards", async () => {
    const runRead = deferred<unknown>();
    vi.mocked(commands.getBacktestRun).mockReturnValue(runRead.promise as never);
    const free = chartDto({ candles: [candle(FIRST), candle(FIRST + 4 * 3_600_000)] });
    vi.mocked(commands.chartData).mockResolvedValue(ok(free));
    await open("#/chart?run=run-1");

    fireEvent.click(await screen.findByRole("button", { name: /^XRPUSDT/ }));
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenLastCalledWith(free.candles));

    await act(async () => runRead.resolve(failure("no backtest run run-1", "not_found")));
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.getByText("No run")).toBeTruthy();
    expect(h.adapter.setCandles).toHaveBeenLastCalledWith(free.candles);
  });
});

// ---------------------------------------------------------------------------
// Centring: the containing bar, and the clamped window's anchor
// ---------------------------------------------------------------------------

describe("centring on the trade under the pointer", () => {
  it("centres a fill inside the final loaded bar without another read", async () => {
    // The last loaded bar is ENTRY's bar; the fill happens INSIDE it, after its open.
    // The marker is drawn on that bar (CR4), so the click centres rather than reading.
    const fill = String(ENTRY + TF_MS_15M / 2);
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          primaryTimeframe: "15m",
          htfTimeframe: null,
          usesHtf: false,
          firstOpenTimeMs: String(ENTRY - TF_MS_15M),
          lastCloseTimeMs: String(ENTRY + TF_MS_15M - 1),
          trades: [tradeRow({ entryFillTime: fill, exitFillTime: String(ENTRY + DAY) })],
        }),
      ),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(
        chartDto({
          timeframe: "15m",
          candles: [candle(ENTRY - TF_MS_15M), candle(ENTRY)],
          run: runLayer({
            series: "primary",
            trades: [trade({ entryTime: fill, exitTime: String(ENTRY + DAY) })],
          }),
        }),
      ),
    );
    await open("#/chart?run=run-1");
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenCalledTimes(1));

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));
    await waitFor(() => expect(h.adapter.centerOn).toHaveBeenCalledWith(fill));
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });

  it("centres a clamped window on the trade the trader picked", async () => {
    // The trade entered one bar after the run's first, so its window is clamped to
    // the run's start: fitting the whole window would leave the marker at the edge.
    const entry = FIRST + TF_MS_15M;
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          primaryTimeframe: "15m",
          htfTimeframe: null,
          usesHtf: false,
          trades: [tradeRow({ entryFillTime: String(entry), exitFillTime: String(entry + DAY) })],
        }),
      ),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(
        chartDto({
          timeframe: "15m",
          candles: [candle(FIRST), candle(entry + HALF)],
          run: runLayer({ series: "primary" }),
        }),
      ),
    );
    await open("#/chart?run=run-1");
    await screen.findByText(/Pick a trade to load 15m around it/);

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));
    await waitFor(() =>
      expect(commands.chartData).toHaveBeenLastCalledWith({
        pair: "BTCUSDT",
        timeframe: "15m",
        fromMs: String(FIRST),
        toMs: String(entry + HALF),
        runId: "run-1",
      }),
    );
    await waitFor(() => expect(h.adapter.centerOn).toHaveBeenCalledWith(String(entry)));
  });

  it("names a trade no window on this leg can draw instead of re-reading it", async () => {
    // The 4h leg fits one read, so the tab asks for the run's own span — the widest
    // window this leg can make. Its answer stops before the trade's own candle (the run's
    // last 4h candle had not closed by its last 15m bar), so no narrower window holds
    // that bar and asking again can only repeat the answer.
    const entry = FIRST + 10 * DAY;
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          trades: [
            tradeRow({ entryFillTime: String(entry), exitFillTime: String(entry + DAY) }),
          ],
        }),
      ),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(
        chartDto({
          candles: [candle(FIRST), candle(FIRST + 4 * TF_MS_4H)],
          run: runLayer({ trades: [] }),
        }),
      ),
    );
    await open("#/chart?run=run-1");
    await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));

    expect(commands.chartData).toHaveBeenCalledTimes(1);
    await screen.findByText("The trade's entry is not drawn in this 4h window.");
  });

  it("does not repeat the read for a trade the window it asked for cannot hold", async () => {
    // A leg over the cap reads a bounded window around the trade the trader picks. When
    // that window answers without the trade's bar, the same row's next click asks the
    // very same question: it is named, not read a third time.
    const entry = FIRST + 100 * DAY;
    const windows = [candle(FIRST), candle(FIRST + TF_MS_4H)];
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          lastCloseTimeMs: String(FIRST + 4_000 * DAY),
          trades: [
            tradeRow({ entryFillTime: String(entry), exitFillTime: String(entry + DAY) }),
          ],
        }),
      ),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(chartDto({ candles: windows, run: runLayer({ trades: [] }) })),
    );
    await open("#/chart?run=run-1");
    await screen.findByText(/Pick a trade to load 4h around it/);

    const row = await screen.findByRole("button", { name: /^Trade 1, long/ });
    fireEvent.click(row);
    await waitFor(() =>
      expect(commands.chartData).toHaveBeenLastCalledWith({
        pair: "BTCUSDT",
        timeframe: "4h",
        fromMs: String(entry - HALF_4H),
        toMs: String(entry + HALF_4H),
        runId: "run-1",
      }),
    );
    await waitFor(() => expect(h.adapter.setCandles).toHaveBeenLastCalledWith(windows));

    // That window has been read and does not hold the bar, so the row's next click asks
    // the very same question: it is named, and no further read goes out — not then, and
    // not on the click after that either.
    fireEvent.click(row);
    expect(commands.chartData).toHaveBeenCalledTimes(1);
    await screen.findByText("The trade's entry is not drawn in this 4h window.");

    fireEvent.click(row);
    expect(commands.chartData).toHaveBeenCalledTimes(1);
  });

  it("names the window, not a cause, when the run starts inside the candle holding the entry", async () => {
    // A fold whose own window starts INSIDE an H4 period: the candle that holds its first
    // bars opened before that window, so the read never asks for it and the returned bars
    // start at the NEXT H4 open. The bar is missing for a reason that has nothing to do
    // with the run's end — the candle closed hours after the fold began — so the note must
    // name the window and assert no cause at all.
    const entry = FIRST + TF_MS_15M;
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          firstOpenTimeMs: String(entry),
          lastCloseTimeMs: String(FIRST + 30 * DAY),
          trades: [
            tradeRow({ entryFillTime: String(entry), exitFillTime: String(entry + DAY) }),
          ],
        }),
      ),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(
        chartDto({
          candles: [candle(FIRST + TF_MS_4H), candle(FIRST + 2 * TF_MS_4H)],
          run: runLayer({ trades: [] }),
        }),
      ),
    );
    await open("#/chart?run=run-1");
    await waitFor(() => expect(commands.chartData).toHaveBeenCalledTimes(1));

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));

    // The bar is named, no further read goes out, and no cause is claimed.
    expect(commands.chartData).toHaveBeenCalledTimes(1);
    await screen.findByText("The trade's entry is not drawn in this 4h window.");
    expect(screen.queryByText(/had not closed by the run's last/)).toBeNull();
  });

  it("still reads a window for an ordinary trade the drawn window does not hold", async () => {
    // A preservation guard on the same leg (it passes on de919fc too): a far-away trade
    // is off-window, not undrawable, so its click reads a window at its OWN anchor. The
    // run-wide selector must keep working.
    const near = FIRST + 100 * DAY;
    const far = FIRST + 2_000 * DAY;
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(
        runDto({
          lastCloseTimeMs: String(FIRST + 4_000 * DAY),
          trades: [
            tradeRow({ entryFillTime: String(near), exitFillTime: String(near + DAY) }),
            tradeRow({ entryFillTime: String(far), exitFillTime: String(far + DAY) }),
          ],
        }),
      ),
    );
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(
        chartDto({
          candles: [candle(near), candle(near + TF_MS_4H)],
          run: runLayer({ trades: [] }),
        }),
      ),
    );
    await open("#/chart?run=run-1");
    await screen.findByText(/Pick a trade to load 4h around it/);

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 1, long/ }));
    await waitFor(() =>
      expect(h.adapter.setCandles).toHaveBeenLastCalledWith([
        candle(near),
        candle(near + TF_MS_4H),
      ]),
    );

    fireEvent.click(await screen.findByRole("button", { name: /^Trade 2, long/ }));
    await waitFor(() =>
      expect(commands.chartData).toHaveBeenLastCalledWith({
        pair: "BTCUSDT",
        timeframe: "4h",
        fromMs: String(far - HALF_4H),
        toMs: String(far + HALF_4H),
        runId: "run-1",
      }),
    );
  });
});

// ---------------------------------------------------------------------------
// The example-trade marks (r5.s3.w3)
// ---------------------------------------------------------------------------

/** The candle click the screen registered with the adapter, so a case can fire one.
 *  The adapter is mocked; this is the seam the screen's own arm → click → write path
 *  runs through. */
function fireCandleClick(openTime: string) {
  const handler = h.adapter.onCandleClick.mock.calls.at(-1)?.[0] as
    | ((openTime: string) => void)
    | undefined;
  expect(handler, "the screen never subscribed a candle click").toBeDefined();
  handler?.(openTime);
}

/** Arm "Mark example" with a setup label, the way the trader does. */
async function arm(setup = "support-rejection longs", side?: "Long" | "Short") {
  if (side !== undefined) {
    fireEvent.click(await screen.findByRole("button", { name: side }));
  }
  fireEvent.change(await screen.findByLabelText(/Setup label/i), {
    target: { value: setup },
  });
  fireEvent.click(await screen.findByRole("button", { name: /Mark example/ }));
}

describe("the marks read", () => {
  // These cases are about the marks, so the chart behind them is the FREE chart the
  // standing mock does not answer: a case that opened `#/chart` while `chartData`
  // answered with a run's layer would have its answer rejected by the drawn-answer gate,
  // and the tab would draw no leg at all.
  beforeEach(() => {
    vi.mocked(commands.chartData).mockResolvedValue(ok(chartDto()));
  });

  it("reads the marks for the pair on screen, with no timeframe filter", async () => {
    await open("#/chart");
    await waitFor(() =>
      expect(commands.listMarks).toHaveBeenCalledWith({ pair: "BTCUSDT" }),
    );
  });

  it("draws the on-screen timeframe's marks and leaves the others off the chart", async () => {
    vi.mocked(commands.listMarks).mockResolvedValue(
      ok(
        markList({
          marks: [
            mark({ markId: "m-4h", timeframe: "4h" }),
            mark({ markId: "m-15m", timeframe: "15m", barTime: String(FIRST + TF_MS_15M) }),
          ],
        }),
      ),
    );
    await open("#/chart");
    await waitFor(() =>
      expect(h.adapter.setMarks).toHaveBeenLastCalledWith([mark({ markId: "m-4h" })]),
    );
  });

  it("says plainly how many marks live on another timeframe", async () => {
    vi.mocked(commands.listMarks).mockResolvedValue(
      ok(
        markList({
          marks: [
            mark({ markId: "m-4h", timeframe: "4h" }),
            mark({ markId: "m-15a", timeframe: "15m" }),
            mark({ markId: "m-15b", timeframe: "15m" }),
          ],
        }),
      ),
    );
    await open("#/chart");
    await expect(screen.findByText("2 marks on 15m")).resolves.toBeTruthy();
  });

  it("says plainly how many marks the holdout hid", async () => {
    vi.mocked(commands.listMarks).mockResolvedValue(
      ok(
        markList({
          hiddenByHoldout: { holdoutStartMs: String(FIRST + 50 * DAY), count: 1 },
        }),
      ),
    );
    await open("#/chart");
    await expect(
      screen.findByText("1 mark hidden: inside the holdout"),
    ).resolves.toBeTruthy();
  });

  it("says the marks are loading, then says there are none yet", async () => {
    const pending = deferred<unknown>();
    vi.mocked(commands.listMarks).mockReturnValue(pending.promise as never);
    await open("#/chart");
    await expect(screen.findByText("Loading example marks…")).resolves.toBeTruthy();

    await act(async () => {
      pending.resolve(ok(markList()));
    });
    await expect(
      screen.findByText('No example marks yet — arm "Mark example", then click a candle.'),
    ).resolves.toBeTruthy();
  });

  it("shows a failed marks read plainly, never as an empty list", async () => {
    vi.mocked(commands.listMarks).mockResolvedValue(failure("the store is unreadable", "data"));
    await open("#/chart");
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("the store is unreadable");
    expect(screen.queryByText(/No example marks yet/)).toBeNull();
  });
});

describe("marking a candle", () => {
  // The free chart, for the same reason as the marks-read cases above.
  beforeEach(() => {
    vi.mocked(commands.chartData).mockResolvedValue(ok(chartDto()));
  });

  it("writes the mark on the clicked candle, naming the pair and timeframe on screen", async () => {
    vi.mocked(commands.writeMark).mockResolvedValue(ok(mark()));
    await open("#/chart");
    await arm("breakout retest", "Long");
    await act(async () => fireCandleClick(String(FIRST + TF_MS_4H)));

    expect(commands.writeMark).toHaveBeenCalledWith({
      pair: "BTCUSDT",
      timeframe: "4h",
      barTime: String(FIRST + TF_MS_4H),
      side: "long",
      setup: "breakout retest",
      note: null,
    });
  });

  it("writes ONE mark when two clicks land in the same render pass", async () => {
    // The write is left in flight, so the second click lands before the first settles.
    // `form.submitting` reaches the click handler's ref only after a render, so both
    // clicks observe the old `false` — the handler's own synchronous guard is what
    // refuses the second one. A mark is immutable, so a duplicate would stand until
    // the trader withdrew it by hand (CR3).
    const pending = deferred<unknown>();
    vi.mocked(commands.writeMark).mockReturnValue(pending.promise as never);
    await open("#/chart");
    await arm("breakout retest", "Long");
    await act(async () => {
      fireCandleClick(String(FIRST + TF_MS_4H));
      fireCandleClick(String(FIRST + TF_MS_4H));
    });

    expect(commands.writeMark).toHaveBeenCalledTimes(1);

    // Once the write settles, the next click writes again.
    await act(async () => {
      pending.resolve(ok(mark()));
    });
    vi.mocked(commands.writeMark).mockResolvedValue(ok(mark()));
    await act(async () => fireCandleClick(String(FIRST + TF_MS_4H)));
    expect(commands.writeMark).toHaveBeenCalledTimes(2);
  });

  it("sends no write at all while the control is disarmed", async () => {
    await open("#/chart");
    await screen.findByText('No example marks yet — arm "Mark example", then click a candle.');
    await act(async () => fireCandleClick(String(FIRST)));
    expect(commands.writeMark).not.toHaveBeenCalled();
  });

  it("refuses a blank setup label locally, naming the field and sending no write", async () => {
    await open("#/chart");
    fireEvent.click(await screen.findByRole("button", { name: /Mark example/ }));
    await act(async () => fireCandleClick(String(FIRST)));
    expect(commands.writeMark).not.toHaveBeenCalled();
    await expect(
      screen.findByText("Type a setup label before marking a candle."),
    ).resolves.toBeTruthy();
  });

  it("shows the new mark in the table and on the chart without a reload", async () => {
    const written = mark({ markId: "m-new", setup: "breakout retest" });
    // The tab re-reads the list after a write (Approach 3), so the mock models the
    // server's NEW state: the mark is there from the second call on. The trader never
    // reloads anything.
    vi.mocked(commands.listMarks)
      .mockResolvedValueOnce(ok(markList()))
      .mockResolvedValue(ok(markList({ marks: [written] })));
    vi.mocked(commands.writeMark).mockResolvedValue(ok(written));
    await open("#/chart");
    await arm("breakout retest");
    await act(async () => fireCandleClick(String(FIRST)));

    await expect(screen.findByText("breakout retest")).resolves.toBeTruthy();
    await waitFor(() =>
      expect(h.adapter.setMarks).toHaveBeenLastCalledWith([written]),
    );
  });

  it("shows a holdout refusal in plain words, with the date, and adds no mark", async () => {
    // The bus folds the route's own `holdout` token onto `validation`
    // (`tauri/error.rs::from_server_token` keeps the message and drops the token), so this
    // is the refusal the screen actually receives. What identifies it is the tab's own
    // knowledge of the fence plus the bar it sent: the fence is the write's first step
    // (G2), so a bar at or after the known start is refused for the fence.
    const holdoutMs = FIRST + 50 * DAY;
    vi.mocked(commands.listMarks).mockResolvedValue(
      ok(markList({ hiddenByHoldout: { holdoutStartMs: String(holdoutMs), count: 0 } })),
    );
    vi.mocked(commands.writeMark).mockResolvedValue(
      failure(
        "the holdout freeze refuses this run on BTCUSDT: `barTime` reaches into the holdout, which starts at 2025-02-20T00:00:00Z",
        "validation",
      ),
    );
    await open("#/chart");
    await arm();
    await act(async () => fireCandleClick(String(FIRST + 60 * DAY)));

    const alert = await screen.findByRole("alert");
    // `dateUtc` is the app's ONE date spelling and carries its UTC suffix, exactly as the
    // holdout note above the table does; the sentence uses it, not a bare date.
    expect(alert.textContent).toContain(
      "This candle is inside the holdout from 2025-02-20 UTC; marks there are refused for every writer",
    );
    expect(screen.queryAllByRole("row")).toHaveLength(1);
  });

  it("shows the server's own words when the freeze opened after the tab's last read", async () => {
    // The fence's start is a fact this tab learns from a read, and a freeze can open
    // between that read and a write. Then the tab cannot name the date, and it must not
    // invent one: the server's message — which names it itself — is what the trader reads.
    vi.mocked(commands.listMarks).mockResolvedValue(ok(markList()));
    vi.mocked(commands.writeMark).mockResolvedValue(
      failure(
        "the holdout freeze refuses this run on BTCUSDT: `barTime` reaches into the holdout, which starts at 2025-10-10T00:00:00Z",
        "validation",
      ),
    );
    await open("#/chart");
    await arm();
    await act(async () => fireCandleClick(String(FIRST + 60 * DAY)));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("starts at 2025-10-10T00:00:00Z");
    expect(alert.textContent).not.toContain("This candle is inside the holdout from");
  });

  it("names the time, the pair and the timeframe when no candle opens there", async () => {
    vi.mocked(commands.writeMark).mockResolvedValue(
      failure("no BTCUSDT 4h candle opens at 1735776000000 ms in the HEAD snapshot"),
    );
    await open("#/chart");
    await arm();
    await act(async () => fireCandleClick(String(FIRST + 3 * TF_MS_4H)));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("no BTCUSDT 4h candle opens at 1735776000000 ms");
  });
});

describe("the marks table", () => {
  const TRADER_MARK = mark({
    markId: "m-trader",
    setup: "support-rejection longs",
    note: "third touch",
    writer: { kind: "trader", name: "Alice", selfReported: false },
  });
  const AGENT_MARK = mark({
    markId: "m-agent",
    timeframe: "4h",
    barTime: String(FIRST + TF_MS_4H),
    side: "short",
    setup: "failed breakout",
    writer: { kind: "agent", name: "claude-code", selfReported: true },
  });

  beforeEach(() => {
    // The free chart: these cases are about the table, and the standing mock's run
    // answer would be rejected by the drawn-answer gate for a free read.
    vi.mocked(commands.chartData).mockResolvedValue(ok(chartDto()));
    vi.mocked(commands.listMarks).mockResolvedValue(
      ok(markList({ marks: [TRADER_MARK, AGENT_MARK] })),
    );
  });

  it("names a trader's writer by the app token's label, and an agent's as self-reported", async () => {
    await open("#/chart");
    await expect(screen.findByText("trader · Alice")).resolves.toBeTruthy();
    await expect(
      screen.findByText("agent · claude-code (self-reported)"),
    ).resolves.toBeTruthy();
  });

  it("lists each mark's time, side, setup and note", async () => {
    await open("#/chart");
    const table = await screen.findByRole("table", { name: /Example marks/ });
    const rows = within(table).getAllByRole("row");
    // The header plus one row per mark.
    expect(rows).toHaveLength(3);
    expect(within(table).getByText("support-rejection longs")).toBeTruthy();
    expect(within(table).getByText("third touch")).toBeTruthy();
    expect(within(table).getByText("failed breakout")).toBeTruthy();
  });

  it("centres the chart on a mark whose row the trader clicks", async () => {
    await open("#/chart");
    await screen.findByText("support-rejection longs");
    fireEvent.click(
      await screen.findByRole("button", { name: /^Mark 1, long,/ }),
    );
    expect(h.adapter.centerOn).toHaveBeenCalledWith(String(FIRST));
  });

  it("labels the table and the controls Reference — not an order", async () => {
    await open("#/chart");
    const table = await screen.findByRole("table", { name: /Example marks/ });
    expect(table.textContent).toContain("Reference — not an order");
    expect((await screen.findByRole("button", { name: /Mark example/ })).textContent)
      .toContain("Reference — not an order");
  });

  it("withdraws a mark: the row and the layer both lose it", async () => {
    // The re-read after the withdraw answers with the withdrawn mark gone, as the
    // server's own list would (a withdrawn mark is hidden by the read).
    vi.mocked(commands.listMarks)
      .mockResolvedValueOnce(ok(markList({ marks: [TRADER_MARK, AGENT_MARK] })))
      .mockResolvedValue(ok(markList({ marks: [TRADER_MARK] })));
    vi.mocked(commands.withdrawMark).mockResolvedValue(ok(AGENT_MARK));
    await open("#/chart");
    await screen.findByText("failed breakout");
    fireEvent.click(await screen.findByRole("button", { name: /Withdraw mark 2/ }));

    await waitFor(() =>
      expect(commands.withdrawMark).toHaveBeenCalledWith({ markId: "m-agent" }),
    );
    await waitFor(() => expect(screen.queryByText("failed breakout")).toBeNull());
    await waitFor(() =>
      expect(h.adapter.setMarks).toHaveBeenLastCalledWith([TRADER_MARK]),
    );
  });

  it("shows a refused withdraw's reason", async () => {
    vi.mocked(commands.withdrawMark).mockResolvedValue(
      failure("no example mark with id m-agent exists", "not_found"),
    );
    await open("#/chart");
    await screen.findByText("failed breakout");
    fireEvent.click(await screen.findByRole("button", { name: /Withdraw mark 2/ }));

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("no example mark with id m-agent exists");
    expect(screen.getByText("failed breakout")).toBeTruthy();
  });

  it("shows the marks and the run's trades together", async () => {
    // This one DOES open a run, so the chart answers with that run's own layer.
    vi.mocked(commands.chartData).mockResolvedValue(
      ok(chartDto({ run: runLayer({ trades: [trade({})] }) })),
    );
    vi.mocked(commands.getBacktestRun).mockResolvedValue(
      ok(runDto({ trades: [tradeRow()] })),
    );
    await open("#/chart?run=run-1");
    await expect(screen.findByText("support-rejection longs")).resolves.toBeTruthy();
    await expect(screen.findByText("Trades in the run")).resolves.toBeTruthy();
    await waitFor(() => expect(h.adapter.setMarks).toHaveBeenCalled());
    expect(h.adapter.setTrades).toHaveBeenCalled();
  });
});
