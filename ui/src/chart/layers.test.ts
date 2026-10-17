// The chart layer mapping's own tests (r5.s2.w2).
//
// This module is where a DTO string becomes the library's number, and it is the only
// place in the app that does it (ADR-0030). So the tests here are about exactly that
// edge and about the four rules the spine's traceability names:
//
//  * a level draws as it stood at each bar — stepped, from the per-bar values, with
//    the engine's warm-up as whitespace rather than a line across it (G3);
//  * a pivot's marker sits on its confirmation bar, with the faint ghost on the peak
//    bar `right` bars earlier (G3, design §0 rule 2);
//  * a pattern draws below the bar in bull and above it in bear (design §1.3);
//  * winners are bull, losers bear, open trades amber (design §1.3);
//  * a marker whose bar is not in the loaded window is not drawn at all (`markersInWindow`,
//    the close review's CR2): the library would snap it onto the first or last loaded bar.
//  * a marker whose bar IS in the window carries that bar's open (`markersInWindow`, the
//    close review's CR4): the library resolves an interior time to the NEXT loaded bar —
//    its lookup is the first open at or after the time — so an in-bar fill handed over with
//    its exact time would draw on the bar AFTER the one it happened in.

import { describe, expect, it } from "vitest";

import type { ChartCandleDto, ChartOverlayDto, ChartTradeDto, MarkDto } from "../bindings";
import type { ChartMarker } from "./layers";
import type { ChartPalette } from "./palette";
import { buildLayers, markersInWindow, toCandles, toMarkers, toOverlayLayers } from "./layers";

/** The palette as `chartPalette` returns it, with values a test can name. */
const PALETTE: ChartPalette = {
  background: "bg",
  text: "text",
  grid: "grid",
  bull: "bull",
  bear: "bear",
  open: "open",
  serverLine: "line",
  serverLevel: "level",
  pivot: "pivot",
  pivotGhost: "ghost",
  markTrader: "mark-trader",
  markAgent: "mark-agent",
};

/** One example-trade mark, as `listMarks` reads it (r5.s3.w1). Times are ms strings. */
function mark(over: Partial<MarkDto> = {}): MarkDto {
  return {
    markId: "mark-1",
    pair: "BTCUSDT",
    timeframe: "4h",
    barTime: "1735689600000",
    side: "long",
    setup: "support-rejection longs",
    note: null,
    writer: { kind: "trader", name: "Alice", selfReported: false },
    dataVersion: "9a1dcf67cf4ea260",
    createdAt: "1735693200000",
    ...over,
  };
}

/** Two 4h candles: 2025-01-01T00:00Z and 04:00Z. */
function candles(): ChartCandleDto[] {
  return [
    { t: "1735689600000", o: "93548.8", h: "94449.2", l: "93460.2", c: "93810", v: "13962.019" },
    { t: "1735704000000", o: "93810", h: "93914.7", l: "93502", c: "93660.7", v: "7852.483" },
  ];
}

/** The same series one bar longer — 2025-01-01T00:00Z, 04:00Z and 08:00Z — so an in-bar fill
 *  has a wrong bar it could be placed on (CR4). */
function threeCandles(): ChartCandleDto[] {
  return [
    ...candles(),
    { t: "1735718400000", o: "93660.7", h: "94100", l: "93600.2", c: "94050", v: "6001.19" },
  ];
}

describe("toCandles", () => {
  it("turns the DTO's exact strings into the library's seconds and numbers", () => {
    expect(toCandles(candles())).toEqual([
      { time: 1735689600, open: 93548.8, high: 94449.2, low: 93460.2, close: 93810 },
      { time: 1735704000, open: 93810, high: 93914.7, low: 93502, close: 93660.7 },
    ]);
  });
});

describe("toOverlayLayers", () => {
  it("draws a level stepped, dashed, from the engine's per-bar values", () => {
    const overlays: ChartOverlayDto[] = [
      { kind: "level", label: "support:3-3-0.005-2-100", values: [null, "93460.2"] },
    ];
    expect(toOverlayLayers(overlays, candles(), PALETTE)).toEqual([
      {
        label: "support:3-3-0.005-2-100",
        stepped: true,
        color: "level",
        // The warm-up is whitespace, not a value: the level draws nothing there
        // rather than a line the engine never computed.
        data: [{ time: 1735689600 }, { time: 1735704000, value: 93460.2 }],
      },
    ]);
  });

  it("draws an EMA line unstepped, dashed, in the indicator-line colour", () => {
    const overlays: ChartOverlayDto[] = [
      { kind: "line", label: "ema:20", values: ["90000", "90100"] },
    ];
    expect(toOverlayLayers(overlays, candles(), PALETTE)).toEqual([
      {
        label: "ema:20",
        stepped: false,
        color: "line",
        data: [
          { time: 1735689600, value: 90000 },
          { time: 1735704000, value: 90100 },
        ],
      },
    ]);
  });

  it("pairs the values with the candle times, and ignores a pivot or pattern overlay", () => {
    const overlays: ChartOverlayDto[] = [
      { kind: "pivot", label: "pivot_high:2-2", events: [] },
      { kind: "pattern", label: "cdl:engulfing", events: [] },
    ];
    expect(toOverlayLayers(overlays, candles(), PALETTE)).toEqual([]);
  });
});

describe("toMarkers", () => {
  it("puts a pivot's marker on its confirmation bar and a faint ghost on the peak bar", () => {
    const overlays: ChartOverlayDto[] = [
      {
        kind: "pivot",
        label: "pivot_low:2-2",
        events: [{ confirmTime: "1735747200000", peakTime: "1735718400000", price: "92796.5" }],
      },
    ];
    expect(toMarkers(overlays, [], PALETTE)).toEqual([
      // Ascending by time: the ghost (04:00) precedes its confirmation (12:00).
      {
        time: 1735718400,
        position: "atPriceMiddle",
        price: 92796.5,
        shape: "circle",
        color: "ghost",
        size: 0.6,
      },
      {
        time: 1735747200,
        position: "atPriceMiddle",
        price: 92796.5,
        shape: "circle",
        color: "pivot",
      },
    ]);
  });

  it("draws a bullish pattern below the bar in bull and a bearish one above it in bear", () => {
    const overlays: ChartOverlayDto[] = [
      {
        kind: "pattern",
        label: "cdl:engulfing",
        events: [
          { time: "1735689600000", value: "100" },
          { time: "1735704000000", value: "-100" },
        ],
      },
    ];
    expect(toMarkers(overlays, [], PALETTE)).toEqual([
      { time: 1735689600, position: "belowBar", shape: "circle", color: "bull" },
      { time: 1735704000, position: "aboveBar", shape: "circle", color: "bear" },
    ]);
  });

  it("draws a zero-valued pattern in amber above the bar (the design's neutral)", () => {
    // The read filters zeroes out, so this branch is unreachable from today's
    // server; it exists so a server change renders sanely rather than as a bull.
    const overlays: ChartOverlayDto[] = [
      { kind: "pattern", label: "cdl:doji", events: [{ time: "1735689600000", value: "0" }] },
    ];
    expect(toMarkers(overlays, [], PALETTE)).toEqual([
      { time: 1735689600, position: "aboveBar", shape: "circle", color: "open" },
    ]);
  });

  it("brackets a winning trade with its entry and exit, and names the R on the exit", () => {
    const trades: ChartTradeDto[] = [
      {
        side: "long",
        entryTime: "1735689600000",
        entryPrice: "90000",
        exitTime: "1735704000000",
        exitPrice: "92000",
        exitReason: "take_profit",
        realizedR: "2",
      },
    ];
    expect(toMarkers([], trades, PALETTE)).toEqual([
      { time: 1735689600, position: "belowBar", shape: "arrowUp", color: "bull" },
      { time: 1735704000, position: "aboveBar", shape: "circle", color: "bull", text: "+2.00 R" },
    ]);
  });

  it("draws a losing trade in bear, and a short one mirrored", () => {
    const trades: ChartTradeDto[] = [
      {
        side: "short",
        entryTime: "1735689600000",
        entryPrice: "90000",
        exitTime: "1735704000000",
        exitPrice: "92000",
        exitReason: "stop_loss",
        realizedR: "-1",
      },
    ];
    expect(toMarkers([], trades, PALETTE)).toEqual([
      { time: 1735689600, position: "aboveBar", shape: "arrowDown", color: "bear" },
      { time: 1735704000, position: "belowBar", shape: "circle", color: "bear", text: "−1.00 R" },
    ]);
  });

  it("draws an open trade in amber, with no exit marker", () => {
    // Every stored trade is closed today (w1's D3); this is the shape the DTO's
    // `null` exit fields would arrive in, drawn rather than guessed.
    const trades: ChartTradeDto[] = [
      {
        side: "long",
        entryTime: "1735689600000",
        entryPrice: "90000",
        exitTime: null,
        exitPrice: null,
        exitReason: null,
        realizedR: "0",
      },
    ];
    expect(toMarkers([], trades, PALETTE)).toEqual([
      { time: 1735689600, position: "belowBar", shape: "arrowUp", color: "open" },
    ]);
  });

  it("returns every marker in ascending time, whatever order the layers arrived in", () => {
    const overlays: ChartOverlayDto[] = [
      { kind: "pattern", label: "cdl:hammer", events: [{ time: "1735704000000", value: "100" }] },
    ];
    const trades: ChartTradeDto[] = [
      {
        side: "long",
        entryTime: "1735689600000",
        entryPrice: "90000",
        exitTime: "1735718400000",
        exitPrice: "92000",
        exitReason: "take_profit",
        realizedR: "2",
      },
    ];
    const times = toMarkers(overlays, trades, PALETTE).map((marker) => marker.time);
    expect(times).toEqual([...times].sort((a, b) => a - b));
    expect(times).toHaveLength(3);
  });
});

describe("example marks as markers (r5.s3.w3)", () => {
  it("draws a long mark below the bar and a short mark above it, in a square", () => {
    // The square is the third shape the library's marker union allows, so a mark is
    // distinguishable from a trade's entry (an arrow) and its exit (a circle).
    expect(toMarkers([], [], PALETTE, [mark()])).toEqual([
      {
        time: 1735689600,
        position: "belowBar",
        shape: "square",
        color: "mark-trader",
      },
    ]);
    expect(toMarkers([], [], PALETTE, [mark({ side: "short" })])).toEqual([
      {
        time: 1735689600,
        position: "aboveBar",
        shape: "square",
        color: "mark-trader",
      },
    ]);
  });

  it("draws an agent's mark in the AI colour and a trader's in the neutral one (G5)", () => {
    const marks = [
      mark({ markId: "m-trader" }),
      mark({
        markId: "m-agent",
        writer: { kind: "agent", name: "claude-code", selfReported: true },
      }),
    ];
    expect(toMarkers([], [], PALETTE, marks).map((entry) => entry.color)).toEqual([
      "mark-trader",
      "mark-agent",
    ]);
  });

  it("merges the marks into the ONE list beside trades and overlays, ascending", () => {
    const overlays: ChartOverlayDto[] = [
      { kind: "pattern", label: "cdl:engulfing", events: [{ time: "1735747200000", value: "100" }] },
    ];
    const trades: ChartTradeDto[] = [
      {
        side: "long",
        entryTime: "1735704000000",
        entryPrice: "90000",
        exitTime: "1735704000000",
        exitPrice: "92000",
        exitReason: "take_profit",
        realizedR: "2",
      },
    ];
    const merged = toMarkers(overlays, trades, PALETTE, [mark()]);
    expect(merged.map((entry) => entry.time)).toEqual([1735689600, 1735704000, 1735704000, 1735747200]);
    expect(merged.map((entry) => entry.shape)).toEqual([
      "square",
      "arrowUp",
      "circle",
      "circle",
    ]);
  });

  it("clips a mark whose bar is not loaded, and places one inside a loaded bar on that bar", () => {
    // A 15m mark inside the 00:00–04:00 4h bar, and one a bar past the window: the first
    // draws on the bar that CONTAINS it (CR4), the second draws nowhere (CR2).
    const loaded = toCandles(candles());
    const inside = mark({ barTime: "1735693200000" });
    const past = mark({ markId: "m-past", barTime: "1735718400000" });
    expect(markersInWindow(toMarkers([], [], PALETTE, [inside]), loaded)).toEqual([
      { time: 1735689600, position: "belowBar", shape: "square", color: "mark-trader" },
    ]);
    expect(markersInWindow(toMarkers([], [], PALETTE, [past]), loaded)).toEqual([]);
  });

  it("keeps the existing three-argument call working, with no marks drawn", () => {
    // The parameter is additive and defaulted: every caller written before this item
    // reads the same list it did before.
    expect(toMarkers([], [], PALETTE)).toEqual([]);
  });
});

describe("markersInWindow", () => {
  /** The two candles above, as the series is handed them: 2025-01-01T00:00Z and 04:00Z. */
  const loaded = toCandles(candles());

  it("keeps a trade's exit and drops its entry when the entry predates the window", () => {
    // The read returns a trade when EITHER endpoint falls in the window (w1), so this
    // is a trade the table must list and the canvas may only half-mark.
    const trades: ChartTradeDto[] = [
      {
        side: "long",
        entryTime: "1735603200000",
        entryPrice: "90000",
        exitTime: "1735704000000",
        exitPrice: "92000",
        exitReason: "take_profit",
        realizedR: "2",
      },
    ];
    expect(markersInWindow(toMarkers([], trades, PALETTE), loaded)).toEqual([
      { time: 1735704000, position: "aboveBar", shape: "circle", color: "bull", text: "+2.00 R" },
    ]);
  });

  it("keeps a pivot's confirmation and drops its ghost when the peak predates the window", () => {
    const overlays: ChartOverlayDto[] = [
      {
        kind: "pivot",
        label: "pivot_low:2-2",
        // The peak is a week before the window; the confirmation is its first bar.
        events: [{ confirmTime: "1735689600000", peakTime: "1735084800000", price: "92796.5" }],
      },
    ];
    expect(markersInWindow(toMarkers(overlays, [], PALETTE), loaded)).toEqual([
      {
        time: 1735689600,
        position: "atPriceMiddle",
        price: 92796.5,
        shape: "circle",
        color: "pivot",
      },
    ]);
  });

  it("places a primary-timeframe time that falls between two loaded bars on its containing bar (CR4)", () => {
    // 2025-01-01T01:00Z on a 4h chart: not a bar open, but inside the loaded range. Its
    // containing bar is the 00:00Z–04:00Z bar, so its time becomes that bar's open. The
    // library's own lookup is a lower bound — the first loaded open at or after 01:00Z is
    // 04:00Z — so handing over 01:00Z would draw on the 04:00Z bar, not the one the trade
    // happened in. That placement happens here, before the marker reaches the library.
    const trades: ChartTradeDto[] = [
      {
        side: "long",
        entryTime: "1735693200000",
        entryPrice: "90000",
        exitTime: null,
        exitPrice: null,
        exitReason: null,
        realizedR: "0",
      },
    ];
    expect(markersInWindow(toMarkers([], trades, PALETTE), loaded)).toEqual([
      { time: 1735689600, position: "belowBar", shape: "arrowUp", color: "open" },
    ]);
  });

  it("keeps both loaded boundaries and drops a marker past the last loaded bar", () => {
    // 08:00Z is the NEXT 4h bar: not loaded, so a marker there would be snapped back
    // onto the 04:00 candle and would mark a bar the trade did not happen on.
    const markers: ChartMarker[] = [
      { time: 1735689600, position: "belowBar", shape: "circle", color: "bull" },
      { time: 1735704000, position: "belowBar", shape: "circle", color: "bull" },
      { time: 1735718400, position: "belowBar", shape: "circle", color: "bull" },
      { time: 1735603200, position: "belowBar", shape: "circle", color: "bull" },
    ];
    expect(markersInWindow(markers, loaded).map((marker) => marker.time)).toEqual([
      1735689600, 1735704000,
    ]);
  });

  it("places an exit inside the last loaded bar's own period on that bar, and drops a time at its end (CR4)", () => {
    // The last loaded bar opens at 04:00Z and, as a 4h bar, owns everything up to 08:00Z.
    // An exit that fills at 05:30Z inside that period belongs to that bar, so its time
    // becomes 04:00Z — the bar's own open, the one the library resolves to that bar.
    const trades: ChartTradeDto[] = [
      {
        side: "long",
        entryTime: "1735689600000",
        entryPrice: "90000",
        exitTime: "1735709400000",
        exitPrice: "92000",
        exitReason: "take_profit",
        realizedR: "2",
      },
    ];
    const markers = toMarkers([], trades, PALETTE);
    expect(markersInWindow(markers, loaded).map((marker) => marker.time)).toEqual([
      1735689600, 1735704000,
    ]);

    // The end of the last bar is exclusive: 08:00Z is the NEXT bar's open, not this one's.
    // It is dropped, not placed — and the same for 09:00Z. Were either kept, it would
    // project onto 04:00Z and this list would carry a third marker.
    const withNextBarTimes: ChartMarker[] = [
      ...markers,
      { time: 1735718400, position: "belowBar", shape: "circle", color: "bull" },
      { time: 1735722000, position: "belowBar", shape: "circle", color: "bull" },
    ];
    expect(markersInWindow(withNextBarTimes, loaded).map((marker) => marker.time)).toEqual([
      1735689600, 1735704000,
    ]);
  });

  it("draws nothing at all when the window holds no bar", () => {
    const markers: ChartMarker[] = [
      { time: 1735689600, position: "belowBar", shape: "circle", color: "bull" },
    ];
    expect(markersInWindow(markers, [])).toEqual([]);
  });
});

describe("buildLayers", () => {
  it("returns the candles, the drawn overlays and the markers for one read", () => {
    const layers = buildLayers(
      {
        pair: "BTCUSDT",
        timeframe: "4h",
        dataVersion: "9a1dcf67cf4ea260",
        candles: candles(),
        holdout: null,
        run: {
          runId: "run-1",
          versionId: "v-1",
          series: "htf",
          trades: [],
          overlays: [{ kind: "line", label: "ema:20", values: ["90000", null] }],
          notDrawn: ["rsi:14"],
        },
      },
      PALETTE,
    );
    expect(layers.candles).toHaveLength(2);
    expect(layers.overlays).toHaveLength(1);
    expect(layers.markers).toEqual([]);
  });

  it("clips a trade's unloaded entry, the same rule the adapter draws by", () => {
    const layers = buildLayers(
      {
        pair: "BTCUSDT",
        timeframe: "4h",
        dataVersion: "9a1dcf67cf4ea260",
        candles: candles(),
        holdout: null,
        run: {
          runId: "run-1",
          versionId: "v-1",
          series: "htf",
          trades: [
            {
              side: "short",
              entryTime: "1735603200000",
              entryPrice: "90000",
              exitTime: "1735704000000",
              exitPrice: "92000",
              exitReason: "take_profit",
              realizedR: "2",
            },
          ],
          overlays: [],
          notDrawn: [],
        },
      },
      PALETTE,
    );
    expect(layers.markers).toEqual([
      { time: 1735704000, position: "belowBar", shape: "circle", color: "bull", text: "+2.00 R" },
    ]);
  });

  it("places an in-bar trade's entry and exit on their containing bar, keeping order and the table's exact times (CR4)", () => {
    // A stored primary trade: 01:00Z entry, 02:00Z exit. The chart is 4h with loaded opens
    // at 00:00Z, 04:00Z and 08:00Z, so both fills belong to the FIRST loaded bar. The
    // library's lookup would put 01:00Z on 04:00Z (the first open at or after it); both
    // markers must instead carry the containing bar's open — and the entry must stay
    // before the exit, on that one bar.
    const trade: ChartTradeDto = {
      side: "long",
      entryTime: "1735693200000",
      entryPrice: "90000",
      exitTime: "1735696800000",
      exitPrice: "92000",
      exitReason: "take_profit",
      realizedR: "1",
    };
    const layers = buildLayers(
      {
        pair: "BTCUSDT",
        timeframe: "4h",
        dataVersion: "9a1dcf67cf4ea260",
        candles: threeCandles(),
        holdout: null,
        run: {
          runId: "run-1",
          versionId: "v-1",
          series: "htf",
          trades: [trade],
          overlays: [],
          notDrawn: [],
        },
      },
      PALETTE,
    );
    expect(layers.markers).toEqual([
      { time: 1735689600, position: "belowBar", shape: "arrowUp", color: "bull" },
      { time: 1735689600, position: "aboveBar", shape: "circle", color: "bull", text: "+1.00 R" },
    ]);
    // The placement is the canvas's alone: the trade itself — what the table reads — keeps
    // its exact 01:00Z and 02:00Z fill times.
    expect(trade.entryTime).toBe("1735693200000");
    expect(trade.exitTime).toBe("1735696800000");
  });
});
