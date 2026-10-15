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
//  * winners are bull, losers bear, open trades amber (design §1.3).

import { describe, expect, it } from "vitest";

import type { ChartCandleDto, ChartOverlayDto, ChartTradeDto } from "../bindings";
import type { ChartPalette } from "./palette";
import { buildLayers, toCandles, toMarkers, toOverlayLayers } from "./layers";

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
};

/** Two 4h candles: 2025-01-01T00:00Z and 04:00Z. */
function candles(): ChartCandleDto[] {
  return [
    { t: "1735689600000", o: "93548.8", h: "94449.2", l: "93460.2", c: "93810", v: "13962.019" },
    { t: "1735704000000", o: "93810", h: "93914.7", l: "93502", c: "93660.7", v: "7852.483" },
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
});
