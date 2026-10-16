// The chart adapter (r5.s2.w2) — the ONLY file in the app that imports
// `lightweight-charts` (ADR-0030, PDR-0004).
//
// Everything the library needs is decided here, and everything it needs is already in
// the read. The mapping itself lives in `layers.ts` (pure, no library, tested without
// mocks); this file owns the library's calls and the chart's lifecycle, and nothing
// else: no market data, no strategy, no run is decided here.
//
// Four decisions worth naming, because they are the ones a reader will question:
//
//  * **The attribution logo stays on.** `attributionLogo: true` is passed EXPLICITLY
//    rather than left to the library's default, so a future version cannot drop
//    TradingView's notice from the chart by changing that default (grill Q6). The
//    library draws it in the container's bottom-left corner, and nothing in this
//    screen overlays the canvas.
//  * **The palette is read once, off the element the chart is mounted in.** The
//    aliases live in `ui/src/styles/chart-tab.css`, so the chart follows the app's
//    theme without this file naming a colour.
//  * **The markers are one list.** Pivots, patterns and trades all ride the candle
//    series through ONE `createSeriesMarkers` plugin, merged and time-sorted in
//    `layers.ts` — the order the library requires and the only one it accepts. The list
//    handed to the plugin holds only markers the loaded bars can hold, each carrying the
//    open of the bar that contains it (`markersInWindow`, the close review's CR2 and CR4):
//    the library places a marker by its time against the candle series, snapping a time it
//    cannot place onto the first or last loaded candle and resolving an interior time to
//    the NEXT loaded bar.
//  * **The layers are redrawn, not stacked.** A new read removes the previous run's
//    level and line series before creating its own; only the candle series, and the
//    marker plugin hanging off it, outlive a read.

import {
  CandlestickSeries,
  ColorType,
  LineSeries,
  LineStyle,
  LineType,
  createChart,
  createSeriesMarkers,
} from "lightweight-charts";
import type {
  CandlestickData,
  IChartApi,
  ISeriesApi,
  ISeriesMarkersPluginApi,
  LineData,
  MouseEventParams,
  SeriesMarker,
  SeriesMarkerPricePosition,
  Time,
  UTCTimestamp,
  WhitespaceData,
} from "lightweight-charts";

import type { ChartCandleDto, ChartOverlayDto, ChartTradeDto, MarkDto } from "../bindings";
import type { CandlePoint, ChartMarker, OverlayLayer } from "./layers";
import {
  containingIndex,
  markersInWindow,
  toCandles,
  toMarkers,
  toOverlayLayers,
} from "./layers";
import { chartPalette } from "./palette";

/** The chart, as the screen sees it. No library type crosses this interface, so the
 *  screen could be mounted against a second renderer unchanged. */
export interface ChartAdapter {
  /** Draw this window's candles. The first call also creates the series. */
  setCandles(candles: readonly ChartCandleDto[]): void;
  /** Draw these overlays' level and line series, and their pivot and pattern
   *  markers. The previous read's layer series are removed first. */
  setOverlays(overlays: readonly ChartOverlayDto[]): void;
  /** Draw these trades' markers. */
  setTrades(trades: readonly ChartTradeDto[]): void;
  /** Draw these example marks' markers (r5.s3.w3). */
  setMarks(marks: readonly MarkDto[]): void;
  /** Call `handler` with the open of the loaded candle a click landed on. A click
   *  outside the loaded candles, or before any read has drawn a bar, calls nothing.
   *  One handler: a second call replaces the first. */
  onCandleClick(handler: (openTime: string) => void): void;
  /** Fit the whole loaded window into view. */
  fitAll(): void;
  /** Scroll so the bar nearest `timeMs` sits in the middle of the pane. */
  centerOn(timeMs: string): void;
  /** Remove the chart and everything it owns. Irreversible: unmount only. */
  destroy(): void;
}

/** How many bars either side of the centred bar stay in view. */
const CENTRE_BARS = 60;

/** One mapped marker, in the library's two marker shapes. */
function toSeriesMarker(marker: ChartMarker): SeriesMarker<Time> {
  const time = marker.time as UTCTimestamp;
  const optional = {
    ...(marker.text === undefined ? {} : { text: marker.text }),
    ...(marker.size === undefined ? {} : { size: marker.size }),
  };
  if (marker.price !== undefined) {
    // A pivot sits AT its confirmed price, not above or below the bar: the price is
    // what the engine confirmed, and the bar is where it became knowable (G3).
    return {
      time,
      shape: marker.shape,
      color: marker.color,
      position: marker.position as SeriesMarkerPricePosition,
      price: marker.price,
      ...optional,
    };
  }
  return {
    time,
    shape: marker.shape,
    color: marker.color,
    // Anything not price-positioned is a bar position; `layers.ts` only ever
    // produces the three bar positions for the non-price markers.
    position: marker.position as "aboveBar" | "belowBar" | "inBar",
    ...optional,
  };
}

/** A level or line layer as the library's line-series data. A point without a value
 *  is whitespace: the library draws nothing there. */
function toLineData(layer: OverlayLayer): (LineData<Time> | WhitespaceData<Time>)[] {
  return layer.data.map((point) =>
    point.value === undefined
      ? { time: point.time as UTCTimestamp }
      : { time: point.time as UTCTimestamp, value: point.value },
  );
}

/** Create the chart in `container` and return the handle the screen drives. */
export function createChartAdapter(container: HTMLElement): ChartAdapter {
  const palette = chartPalette(container);
  const chart: IChartApi = createChart(container, {
    autoSize: true,
    layout: {
      background: { type: ColorType.Solid, color: palette.background },
      textColor: palette.text,
      // Explicit, never inherited from the library's default (see this file's header).
      // It is a LAYOUT option: the library draws the notice into the chart's corner.
      attributionLogo: true,
    },
    grid: {
      vertLines: { color: palette.grid },
      horzLines: { color: palette.grid },
    },
    rightPriceScale: { borderColor: palette.grid },
    timeScale: { borderColor: palette.grid },
  });

  // What the read last handed in. The candle times are kept as the read spelled
  // them, because the overlays' values are positional against them.
  let candleDtos: readonly ChartCandleDto[] = [];
  // The same bars in the library's units — the window a marker has to fall inside.
  let candlePoints: readonly CandlePoint[] = [];
  let overlayDtos: readonly ChartOverlayDto[] = [];
  let tradeDtos: readonly ChartTradeDto[] = [];
  let markDtos: readonly MarkDto[] = [];

  let candleSeries: ISeriesApi<"Candlestick"> | null = null;
  let markerPlugin: ISeriesMarkersPluginApi<Time> | null = null;
  let layerSeries: ISeriesApi<"Line">[] = [];
  /** The one click handler the screen subscribed, kept so `destroy` unsubscribes the
   *  EXACT function it subscribed rather than every handler on the chart. */
  let clickHandler: ((param: MouseEventParams<Time>) => void) | null = null;

  /** The candle series, created on the first candle draw. */
  const ensureCandles = (): ISeriesApi<"Candlestick"> => {
    if (candleSeries !== null) return candleSeries;
    candleSeries = chart.addSeries(CandlestickSeries, {
      upColor: palette.bull,
      downColor: palette.bear,
      borderUpColor: palette.bull,
      borderDownColor: palette.bear,
      wickUpColor: palette.bull,
      wickDownColor: palette.bear,
    });
    // Created with the series and fed on every draw: one plugin, one ascending list.
    markerPlugin = createSeriesMarkers(candleSeries, []);
    return candleSeries;
  };

  /** Rebuild the merged marker list from the overlays and trades last handed in, and
   *  hand the library ONLY the markers the loaded bars can hold, each on its containing
   *  bar (CR2, CR4): a marker time the series cannot place is snapped onto the first or
   *  last loaded candle, and a time inside a bar would otherwise resolve to the NEXT one. */
  const drawMarkers = (): void => {
    if (markerPlugin === null) return;
    markerPlugin.setMarkers(
      markersInWindow(
        toMarkers(overlayDtos, tradeDtos, palette, markDtos),
        candlePoints,
      ).map(toSeriesMarker),
    );
  };

  /** Whether `logical` names a loaded bar. The library resolves a click's position with
   *  `Math.ceil` and no clamp, so a click in the pane past the last bar yields an index
   *  one past it, and a click before the first yields a negative one; a position it
   *  could not resolve at all is absent. Its `time` cannot carry this: it is already the
   *  loaded open a time-scale point maps to, so a click right of the last candle comes
   *  back carrying the LAST open (the r5.s2 retro §8 trap, on the write path). */
  const loadedLogical = (logical: number | undefined): logical is number =>
    candlePoints.length > 0 &&
    logical !== undefined &&
    Number.isFinite(logical) &&
    logical >= 0 &&
    logical <= candlePoints.length - 1;

  return {
    setCandles(next) {
      candleDtos = [...next];
      candlePoints = toCandles(candleDtos);
      ensureCandles().setData(candlePoints as CandlestickData<Time>[]);
    },

    setOverlays(next) {
      overlayDtos = [...next];
      ensureCandles();
      for (const stale of layerSeries) {
        chart.removeSeries(stale);
      }
      layerSeries = [];
      for (const layer of toOverlayLayers(overlayDtos, candleDtos, palette)) {
        const line = chart.addSeries(LineSeries, {
          color: layer.color,
          // Both are dashed: they are the server's, not the trader's (design §0
          // rule 5). A level holds its value between the bars the engine moved it,
          // an EMA moves every bar — that is the only difference between them.
          lineStyle: LineStyle.Dashed,
          lineType: layer.stepped ? LineType.WithSteps : LineType.Simple,
          lineWidth: 1,
          priceLineVisible: false,
          lastValueVisible: false,
          crosshairMarkerVisible: false,
        });
        line.setData(toLineData(layer));
        layerSeries.push(line);
      }
      drawMarkers();
    },

    setTrades(next) {
      tradeDtos = [...next];
      ensureCandles();
      drawMarkers();
    },

    setMarks(next) {
      markDtos = [...next];
      ensureCandles();
      drawMarkers();
    },

    onCandleClick(handler) {
      if (clickHandler !== null) chart.unsubscribeClick(clickHandler);
      clickHandler = (param: MouseEventParams<Time>): void => {
        // The CLICK POSITION is what decides, never the resolved time: `logical` is the
        // raw place the click fell, so a click past the last loaded bar is told apart
        // from a click on it, which its `time` alone cannot do.
        if (!loadedLogical(param.logical as number | undefined)) return;
        const raw = param.logical as number;
        // The bar that CONTAINS the click — the same the library would draw a marker on,
        // and never the next loaded bar its own lower-bound lookup would pick.
        const index = containingIndex(candlePoints, candlePoints[Math.round(raw)].time);
        handler(String(candlePoints[index].time * 1000));
      };
      chart.subscribeClick(clickHandler);
    },

    fitAll() {
      chart.timeScale().fitContent();
    },

    centerOn(timeMs) {
      if (candlePoints.length === 0) return;
      const target = Math.floor(Number(timeMs) / 1000);
      // The bar that CONTAINS the time — exactly the bar `markersInWindow` draws the
      // marker on, and never the next loaded bar the library's own lower-bound
      // lookup would pick for a time inside a bar's period (CR4).
      const index = containingIndex(candlePoints, target);
      chart.timeScale().setVisibleLogicalRange({
        from: index - CENTRE_BARS,
        to: index + CENTRE_BARS,
      });
    },

    destroy() {
      // Unsubscribe the exact handler this adapter put on, before the chart it belongs to
      // is taken down. A handler left subscribed would fire against a destroyed chart.
      if (clickHandler !== null) {
        chart.unsubscribeClick(clickHandler);
        clickHandler = null;
      }
      chart.remove();
      candleSeries = null;
      markerPlugin = null;
      layerSeries = [];
      candleDtos = [];
      candlePoints = [];
      overlayDtos = [];
      tradeDtos = [];
      markDtos = [];
    },
  };
}
