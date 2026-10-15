// The adapter's own tests (r5.s2.w2) — the ONLY test that touches
// `lightweight-charts`, and it touches it MOCKED. jsdom has no canvas, and the point
// here is not to prove the library draws; it is to prove OUR wiring:
//
//  * the TradingView attribution logo is asked for EXPLICITLY (`attributionLogo:
//    true`), so a library default flip cannot silently drop it (grill Q6);
//  * the read's values reach the library in its own units and shape — candles as
//    seconds/numbers, a level as a stepped dashed series, an EMA as a plain dashed
//    one, a warm-up bar as whitespace;
//  * every marker (pivots, patterns, trades) lands in ONE time-sorted list on the
//    candle series, because that is the only order the library accepts;
//  * a marker whose time is outside the loaded candles is never handed over at all
//    (the close review's CR2): the library would snap it onto a boundary candle;
//  * a second draw replaces the layer series instead of stacking a second copy on
//    top of the first;
//  * fit, centre and destroy reach the chart's own API.

import { beforeEach, describe, expect, it, vi } from "vitest";

const h = vi.hoisted(() => {
  const timeScale = { fitContent: vi.fn(), setVisibleLogicalRange: vi.fn() };
  const series: {
    definition: unknown;
    options: unknown;
    setData: ReturnType<typeof vi.fn>;
  }[] = [];
  const chart = {
    addSeries: vi.fn((definition: unknown, options: unknown) => {
      const created = { definition, options, setData: vi.fn() };
      series.push(created);
      return created;
    }),
    removeSeries: vi.fn(),
    applyOptions: vi.fn(),
    timeScale: vi.fn(() => timeScale),
    remove: vi.fn(),
  };
  const markersApi = { setMarkers: vi.fn() };
  return { timeScale, chart, series, markersApi };
});

vi.mock("lightweight-charts", () => ({
  createChart: vi.fn(() => h.chart),
  createSeriesMarkers: vi.fn(() => h.markersApi),
  CandlestickSeries: { type: "Candlestick" },
  LineSeries: { type: "Line" },
  LineStyle: { Solid: 0, Dashed: 2 },
  LineType: { Simple: 0, WithSteps: 1 },
  ColorType: { Solid: "solid" },
}));

import { createChart, createSeriesMarkers } from "lightweight-charts";

import type { ChartCandleDto, ChartOverlayDto, ChartTradeDto } from "../bindings";
import { createChartAdapter } from "./adapter";
import { PALETTE_TOKENS } from "./palette";

/** A container carrying the chart's ten aliases, as the loaded sheet leaves it. */
function container(): HTMLElement {
  const el = document.createElement("div");
  el.style.setProperty(PALETTE_TOKENS.bull, "bull");
  el.style.setProperty(PALETTE_TOKENS.bear, "bear");
  el.style.setProperty(PALETTE_TOKENS.open, "open");
  el.style.setProperty(PALETTE_TOKENS.serverLine, "line");
  el.style.setProperty(PALETTE_TOKENS.serverLevel, "level");
  el.style.setProperty(PALETTE_TOKENS.pivot, "pivot");
  el.style.setProperty(PALETTE_TOKENS.pivotGhost, "ghost");
  el.style.setProperty(PALETTE_TOKENS.background, "bg");
  el.style.setProperty(PALETTE_TOKENS.text, "text");
  el.style.setProperty(PALETTE_TOKENS.grid, "grid");
  document.body.append(el);
  return el;
}

/** The candle series the adapter created (the first `addSeries` call). */
function candleSeries() {
  return h.series[0];
}

/** The layer series created after the candle series. */
function layerSeries() {
  return h.series.slice(1);
}

const CANDLES: ChartCandleDto[] = [
  { t: "1735689600000", o: "93548.8", h: "94449.2", l: "93460.2", c: "93810", v: "13962.019" },
  { t: "1735704000000", o: "93810", h: "93914.7", l: "93502", c: "93660.7", v: "7852.483" },
];

/** Four 4h bars — 2025-01-01T00:00Z through 12:00Z — for the one test that draws a
 *  pivot two bars past the trade: every marker under test has to sit on a loaded bar,
 *  or `markersInWindow` correctly refuses to hand it over (CR2). */
const SPANNED_CANDLES: ChartCandleDto[] = [
  ...CANDLES,
  { t: "1735718400000", o: "93660.7", h: "94100", l: "93600.2", c: "94050", v: "6001.19" },
  { t: "1735747200000", o: "94050", h: "94500", l: "93900", c: "94380.4", v: "5102.7" },
];

const TRADE: ChartTradeDto = {
  side: "long",
  entryTime: "1735689600000",
  entryPrice: "90000",
  exitTime: "1735704000000",
  exitPrice: "92000",
  exitReason: "take_profit",
  realizedR: "2",
};

beforeEach(() => {
  h.series.length = 0;
  h.chart.addSeries.mockClear();
  h.chart.removeSeries.mockClear();
  h.chart.remove.mockClear();
  h.markersApi.setMarkers.mockClear();
  h.timeScale.fitContent.mockClear();
  h.timeScale.setVisibleLogicalRange.mockClear();
  vi.mocked(createChart).mockClear();
  vi.mocked(createSeriesMarkers).mockClear();
});

describe("createChartAdapter", () => {
  it("asks for the attribution logo and the library's own auto-size", () => {
    createChartAdapter(container());
    expect(vi.mocked(createChart)).toHaveBeenCalledTimes(1);
    const [target, options] = vi.mocked(createChart).mock.calls[0];
    expect(target).toBeInstanceOf(HTMLElement);
    expect(options).toMatchObject({ autoSize: true });
    // The logo is a LAYOUT option, and it is passed explicitly rather than left to the
    // library's default, so a default flip cannot silently drop TradingView's notice.
    expect(options?.layout).toMatchObject({ attributionLogo: true });
  });

  it("draws the candles in the library's own units", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    expect(h.series).toHaveLength(1);
    expect(candleSeries().options).toMatchObject({ upColor: "bull", downColor: "bear" });
    expect(candleSeries().setData).toHaveBeenCalledWith([
      { time: 1735689600, open: 93548.8, high: 94449.2, low: 93460.2, close: 93810 },
      { time: 1735704000, open: 93810, high: 93914.7, low: 93502, close: 93660.7 },
    ]);
  });

  it("draws a level dashed and stepped, with the engine's warm-up as whitespace", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    const overlays: ChartOverlayDto[] = [
      { kind: "level", label: "support:3-3-0.005-2-100", values: [null, "93460.2"] },
    ];
    adapter.setOverlays(overlays);
    expect(layerSeries()).toHaveLength(1);
    expect(layerSeries()[0].options).toMatchObject({
      color: "level",
      lineStyle: 2,
      lineType: 1,
    });
    expect(layerSeries()[0].setData).toHaveBeenCalledWith([
      { time: 1735689600 },
      { time: 1735704000, value: 93460.2 },
    ]);
  });

  it("draws an EMA dashed and unstepped", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    adapter.setOverlays([{ kind: "line", label: "ema:20", values: ["90000", "90100"] }]);
    expect(layerSeries()[0].options).toMatchObject({
      color: "line",
      lineStyle: 2,
      lineType: 0,
    });
  });

  it("replaces the layer series on a second draw instead of stacking them", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    adapter.setOverlays([{ kind: "line", label: "ema:20", values: ["90000", "90100"] }]);
    adapter.setOverlays([{ kind: "line", label: "ema:50", values: ["80000", "80100"] }]);
    expect(h.chart.removeSeries).toHaveBeenCalledTimes(1);
    expect(layerSeries()).toHaveLength(2);
    // The candle series is never removed: it is the chart's floor.
    expect(h.chart.removeSeries).not.toHaveBeenCalledWith(candleSeries());
  });

  it("puts every marker on the candle series, in ONE ascending list", () => {
    const adapter = createChartAdapter(container());
    // The bars span both the trade (00:00 → 04:00) and the pivot's peak (08:00) and
    // confirmation (12:00), so all four markers are the window's own.
    adapter.setCandles(SPANNED_CANDLES);
    adapter.setOverlays([
      {
        kind: "pivot",
        label: "pivot_low:2-2",
        events: [{ confirmTime: "1735747200000", peakTime: "1735718400000", price: "92796.5" }],
      },
    ]);
    adapter.setTrades([TRADE]);
    expect(vi.mocked(createSeriesMarkers)).toHaveBeenCalledWith(candleSeries(), []);
    const last = h.markersApi.setMarkers.mock.calls.at(-1)?.[0] as { time: number }[];
    expect(last.map((marker) => marker.time)).toEqual([1735689600, 1735704000, 1735718400, 1735747200]);
    expect(last[0]).toMatchObject({ shape: "arrowUp", color: "bull", position: "belowBar" });
    expect(last[1]).toMatchObject({ shape: "circle", color: "bull", text: "+2.00 R" });
    expect(last[2]).toMatchObject({ color: "ghost", size: 0.6 });
    expect(last[3]).toMatchObject({ color: "pivot" });
  });

  it("hands the library no marker outside the loaded bars (CR2)", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    adapter.setOverlays([
      {
        kind: "pivot",
        label: "pivot_low:2-2",
        // The peak is two days before the loaded window: the ghost is not drawn, the
        // confirmation (a loaded bar) is.
        events: [{ confirmTime: "1735689600000", peakTime: "1735516800000", price: "92796.5" }],
      },
    ]);
    // The read returns a trade when EITHER endpoint is in the window: this one entered
    // two days earlier, so only its exit may be handed over.
    adapter.setTrades([{ ...TRADE, entryTime: "1735516800000" }]);

    // Nothing this draw ever hands the library is outside the loaded range — the first
    // bar's open up to the END of the last loaded bar: a marker the library cannot place
    // is snapped onto the first or last loaded candle.
    for (const [markers] of h.markersApi.setMarkers.mock.calls) {
      for (const marker of markers as { time: number }[]) {
        expect(marker.time).toBeGreaterThanOrEqual(1735689600);
        expect(marker.time).toBeLessThan(1735718400);
      }
    }
    const last = h.markersApi.setMarkers.mock.calls.at(-1)?.[0] as { time: number }[];
    expect(last.map((marker) => marker.time)).toEqual([1735689600, 1735704000]);
    expect(last[0]).toMatchObject({ color: "pivot" });
    expect(last[1]).toMatchObject({ color: "bull", text: "+2.00 R" });
  });

  it("draws an exit inside the last loaded bar's own period, and nothing past its end", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    adapter.setOverlays([
      {
        kind: "pivot",
        label: "pivot_low:2-2",
        // The peak is the first loaded bar; the confirmation, at 09:00Z, belongs to the
        // bar AFTER the last loaded one (04:00Z–08:00Z) and is never handed over.
        events: [{ confirmTime: "1735722000000", peakTime: "1735689600000", price: "92796.5" }],
      },
    ]);
    // 05:30Z is inside the last loaded bar's period — the run's final exit, on the 4h
    // chart this walk shows — so it is drawn at its own time and lands on that bar.
    adapter.setTrades([{ ...TRADE, entryTime: "1735689600000", exitTime: "1735709400000" }]);

    const last = h.markersApi.setMarkers.mock.calls.at(-1)?.[0] as { time: number }[];
    expect(last.map((marker) => marker.time)).toEqual([1735689600, 1735689600, 1735709400]);
    for (const [markers] of h.markersApi.setMarkers.mock.calls) {
      for (const marker of markers as { time: number }[]) {
        // Nothing at or past the end of the last loaded bar (04:00Z + one 4h bar).
        expect(marker.time).toBeLessThan(1735718400);
      }
    }
  });

  it("keeps markers from overlays and trades when only one of the two is redrawn", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    adapter.setTrades([TRADE]);
    adapter.setOverlays([
      { kind: "pattern", label: "cdl:engulfing", events: [{ time: "1735689600000", value: "100" }] },
    ]);
    const last = h.markersApi.setMarkers.mock.calls.at(-1)?.[0] as { color: string }[];
    expect(last.map((marker) => marker.color)).toEqual(["bull", "bull", "bull"]);
    expect(last).toHaveLength(3);
  });

  it("fits the window, centres on a bar, and removes itself", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    adapter.fitAll();
    expect(h.timeScale.fitContent).toHaveBeenCalledTimes(1);

    adapter.centerOn("1735704000000");
    expect(h.timeScale.setVisibleLogicalRange).toHaveBeenCalledWith({ from: -59, to: 61 });

    adapter.destroy();
    expect(h.chart.remove).toHaveBeenCalledTimes(1);
  });

  it("centres on the last bar when the time asked for is past the window", () => {
    const adapter = createChartAdapter(container());
    adapter.setCandles(CANDLES);
    adapter.centerOn("1799999999000");
    expect(h.timeScale.setVisibleLogicalRange).toHaveBeenCalledWith({ from: -59, to: 61 });
  });

  it("draws nothing at all before a read lands", () => {
    createChartAdapter(container());
    expect(h.chart.addSeries).not.toHaveBeenCalled();
    expect(h.markersApi.setMarkers).not.toHaveBeenCalled();
  });
});
