// The chart layer mapping (r5.s2.w2) — DTO in, plain shapes out.
//
// This module holds the whole translation the adapter boundary exists for
// (ADR-0030): the read's exact strings become the library's seconds and numbers HERE
// and nowhere else in the app. `ui/src/chart/` sits outside the no-float guard's
// scanned directories precisely so this can be the one place that happens.
//
// It imports no chart library and holds no chart handle, so the rules it encodes are
// testable without mocking anything. The rules are the spine's own:
//
//  * **G3, no later bar.** A level arrives as one value per window bar — the level as
//    it stood at that bar — and is drawn STEPPED from those values, never as one line
//    across the history. A `null` is the engine's warm-up (or a bar it read no value
//    for) and becomes whitespace, so nothing is drawn there rather than a line the
//    engine never computed.
//  * **G3, pivots.** A pivot's marker sits on its CONFIRMATION bar, at the confirmed
//    price, with a faint ghost on the peak bar `right` bars earlier. The peak alone is
//    never marked: that would make the chart look smarter than the strategy was.
//  * **The design brief's colours.** Server-computed layers are dashed and neutral;
//    patterns draw below the bar in bull and above it in bear; winners are bull,
//    losers bear, open trades amber. Not one colour is chosen here — every one is a
//    key of the palette the screen's sheet owns.
//  * **PDR-0011.** What is not drawn is never faked: an overlay with no value at a bar
//    draws nothing at that bar.
//  * **G3, no false bar.** A marker is drawn only when its own time falls inside the loaded
//    window (`markersInWindow`, the close review's CR2) — the first bar's open up to the
//    end of the last loaded bar, exclusive. A read hands back a trade when EITHER of its
//    endpoints is in the window, and a pivot's peak bar can be behind the window's first
//    bar: the library maps a time it cannot place onto the nearest loaded CANDLE, which
//    would mark a bar the trade or the pivot never touched.

import type { ChartCandleDto, ChartDataDto, ChartOverlayDto, ChartTradeDto } from "../bindings";
import { rMultiple } from "../format/numbers";
import type { ChartPalette } from "./palette";

/** One candle, as the library's candlestick series takes it. */
export interface CandlePoint {
  /** Epoch SECONDS — the library's own time unit; the read crosses in ms. */
  readonly time: number;
  readonly open: number;
  readonly high: number;
  readonly low: number;
  readonly close: number;
}

/** One point of a level or line layer. A point with no `value` is whitespace: the
 *  library draws nothing at that bar (the engine's warm-up, or no value read). */
export interface LinePoint {
  readonly time: number;
  readonly value?: number;
}

/** One server-computed series. `stepped` is what makes a level draw as it stood at
 *  each bar; an EMA moves every bar and is drawn unstepped. */
export interface OverlayLayer {
  readonly label: string;
  readonly stepped: boolean;
  readonly color: string;
  readonly data: readonly LinePoint[];
}

/** One marker, in the library's own vocabulary. */
export interface ChartMarker {
  readonly time: number;
  readonly position:
    | "aboveBar"
    | "belowBar"
    | "inBar"
    | "atPriceTop"
    | "atPriceBottom"
    | "atPriceMiddle";
  readonly shape: "circle" | "square" | "arrowUp" | "arrowDown";
  readonly color: string;
  /** Required by the library for the `atPrice*` positions (pivots). */
  readonly price?: number;
  readonly text?: string;
  readonly size?: number;
}

/** Everything one read draws. */
export interface ChartLayers {
  readonly candles: readonly CandlePoint[];
  readonly overlays: readonly OverlayLayer[];
  readonly markers: readonly ChartMarker[];
}

/** The ghost's marker size — the library's default is 1. */
const GHOST_SIZE = 0.6;

/** The read's exact ms text as the library's seconds. */
function seconds(ms: string): number {
  return Math.floor(Number(ms) / 1000);
}

/** The read's exact decimal text as a chart coordinate. */
function decimal(text: string): number {
  return Number(text);
}

/** The window's candles, in the library's units. */
export function toCandles(candles: readonly ChartCandleDto[]): CandlePoint[] {
  return candles.map((candle) => ({
    time: seconds(candle.t),
    open: decimal(candle.o),
    high: decimal(candle.h),
    low: decimal(candle.l),
    close: decimal(candle.c),
  }));
}

/** The level and line overlays, as drawable series.
 *
 *  `values[i]` belongs to `candles[i]` — the read returns one value per WINDOW bar —
 *  so the two are zipped positionally. A read that ever disagreed with its own candle
 *  count is truncated to the shorter of the two rather than drawn against the wrong
 *  bars. */
export function toOverlayLayers(
  overlays: readonly ChartOverlayDto[],
  candles: readonly ChartCandleDto[],
  palette: ChartPalette,
): OverlayLayer[] {
  const layers: OverlayLayer[] = [];
  for (const overlay of overlays) {
    if (overlay.kind !== "level" && overlay.kind !== "line") continue;
    const count = Math.min(overlay.values.length, candles.length);
    const data: LinePoint[] = [];
    for (let i = 0; i < count; i += 1) {
      const time = seconds(candles[i].t);
      const value = overlay.values[i];
      data.push(value === null ? { time } : { time, value: decimal(value) });
    }
    layers.push({
      label: overlay.label,
      // A level is the same price until the engine moves it; an EMA moves every bar.
      stepped: overlay.kind === "level",
      color: overlay.kind === "level" ? palette.serverLevel : palette.serverLine,
      data,
    });
  }
  return layers;
}

/** Every marker the window's overlays and trades draw, in ascending time — the order
 *  the library requires. */
export function toMarkers(
  overlays: readonly ChartOverlayDto[],
  trades: readonly ChartTradeDto[],
  palette: ChartPalette,
): ChartMarker[] {
  const markers: ChartMarker[] = [];

  for (const overlay of overlays) {
    if (overlay.kind === "pivot") {
      for (const event of overlay.events) {
        const price = decimal(event.price);
        // The ghost first: the peak bar is `right` bars BEFORE the confirmation, so
        // this is also the order the sort below would produce.
        markers.push({
          time: seconds(event.peakTime),
          position: "atPriceMiddle",
          price,
          shape: "circle",
          color: palette.pivotGhost,
          size: GHOST_SIZE,
        });
        markers.push({
          time: seconds(event.confirmTime),
          position: "atPriceMiddle",
          price,
          shape: "circle",
          color: palette.pivot,
        });
      }
    } else if (overlay.kind === "pattern") {
      for (const event of overlay.events) {
        const value = decimal(event.value);
        markers.push({
          time: seconds(event.time),
          // A bullish pattern draws below the bar, a bearish one above it.
          position: value > 0 ? "belowBar" : "aboveBar",
          shape: "circle",
          color: value > 0 ? palette.bull : value < 0 ? palette.bear : palette.open,
        });
      }
    }
  }

  for (const trade of trades) {
    const long = trade.side === "long";
    // Winners bull, losers bear, open trades amber (design §1.3). The stored trades
    // are all closed today; the open branch is what the DTO's nulls would mean.
    const color =
      trade.exitTime === null
        ? palette.open
        : decimal(trade.realizedR) >= 0
          ? palette.bull
          : palette.bear;
    // The entry and exit bracket the trade: a long enters below the bar and leaves
    // above it, a short the other way round.
    markers.push({
      time: seconds(trade.entryTime),
      position: long ? "belowBar" : "aboveBar",
      shape: long ? "arrowUp" : "arrowDown",
      color,
    });
    if (trade.exitTime !== null) {
      markers.push({
        time: seconds(trade.exitTime),
        position: long ? "aboveBar" : "belowBar",
        shape: "circle",
        color,
        // The result the run recorded, in the app's one R spelling.
        text: rMultiple(trade.realizedR),
      });
    }
  }

  // Ascending time, stably: a marker's own layer order is preserved among equals.
  return markers
    .map((marker, index) => ({ marker, index }))
    .sort((a, b) => a.marker.time - b.marker.time || a.index - b.index)
    .map((entry) => entry.marker);
}

/** One bar's own duration, in the library's seconds, read off the loaded series: the
 *  smallest positive gap between two consecutive bars. A missing bar (a holdout cut, a
 *  gap in the snapshot) leaves a gap WIDER than a bar, never a narrower one, so the
 *  smallest gap is the interval the series is drawn at. `0` when the series is a single
 *  bar: there is no spacing to read, and the caller says so rather than guessing one. */
function barSeconds(candles: readonly Pick<CandlePoint, "time">[]): number {
  let bar = 0;
  for (let i = 1; i < candles.length; i += 1) {
    const gap = candles[i].time - candles[i - 1].time;
    // The read returns its candles ascending; a non-positive gap would be malformed.
    if (gap <= 0) continue;
    if (bar === 0 || gap < bar) bar = gap;
  }
  return bar;
}

/** The markers a loaded window may draw (the close review's CR2).
 *
 *  A marker whose time is outside the loaded bars' own span is dropped here, before the
 *  library ever sees it: the library resolves every marker time against the candle series
 *  and snaps a time it cannot place onto the nearest loaded bar, which would draw a mark
 *  on the first or last candle instead of nowhere.
 *
 *  The span runs from the FIRST bar's open, inclusive, to the END of the last bar —
 *  its open plus one bar's own duration, exclusive. A time inside the range that is not
 *  itself a bar open (a primary-timeframe trade time on the higher-timeframe chart, or an
 *  exit filing inside the last loaded bar) is kept: the library's containing-bar mapping
 *  for it is the intended one, which is why this is a range test and not a membership
 *  test. A time at or past the last bar's end belongs to a bar the window does not hold
 *  and is dropped, so the snapping the clip exists for cannot come back. The table below
 *  the chart still carries every exact time and value, clipped or not (PDR-0011). */
export function markersInWindow(
  markers: readonly ChartMarker[],
  candles: readonly Pick<CandlePoint, "time">[],
): ChartMarker[] {
  if (candles.length === 0) return [];
  // The read returns its candles ascending, so the ends are the span's ends.
  const open = candles[0].time;
  const lastOpen = candles[candles.length - 1].time;
  const bar = barSeconds(candles);
  // A single loaded bar has no spacing to read: that bar's own open is the whole span.
  if (bar === 0) return markers.filter((marker) => marker.time === open);
  return markers.filter(
    (marker) => marker.time >= open && marker.time < lastOpen + bar,
  );
}

/** Everything one chart read draws. */
export function buildLayers(dto: ChartDataDto, palette: ChartPalette): ChartLayers {
  const overlays = dto.run?.overlays ?? [];
  const candles = toCandles(dto.candles);
  return {
    candles,
    overlays: toOverlayLayers(overlays, dto.candles, palette),
    // The same clip the adapter draws by: a marker whose bar is not loaded draws nothing.
    markers: markersInWindow(toMarkers(overlays, dto.run?.trades ?? [], palette), candles),
  };
}
