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
//    handed to the plugin holds only markers the loaded bars can hold (`markersInWindow`,
//    the close review's CR2): the library places a marker by its time against the candle
//    series and snaps a time it cannot place onto the first or last loaded candle.
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
  SeriesMarker,
  SeriesMarkerPricePosition,
  Time,
  UTCTimestamp,
  WhitespaceData,
} from "lightweight-charts";

import type { ChartCandleDto, ChartOverlayDto, ChartTradeDto } from "../bindings";
import type { CandlePoint, ChartMarker, OverlayLayer } from "./layers";
import { markersInWindow, toCandles, toMarkers, toOverlayLayers } from "./layers";
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

  let candleSeries: ISeriesApi<"Candlestick"> | null = null;
  let markerPlugin: ISeriesMarkersPluginApi<Time> | null = null;
  let layerSeries: ISeriesApi<"Line">[] = [];

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
   *  hand the library ONLY the markers the loaded bars can hold (CR2): a marker time
   *  the series cannot place is snapped onto the first or last loaded candle. */
  const drawMarkers = (): void => {
    if (markerPlugin === null) return;
    markerPlugin.setMarkers(
      markersInWindow(toMarkers(overlayDtos, tradeDtos, palette), candlePoints).map(
        toSeriesMarker,
      ),
    );
  };

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

    fitAll() {
      chart.timeScale().fitContent();
    },

    centerOn(timeMs) {
      const target = Math.floor(Number(timeMs) / 1000);
      let index = Math.max(0, candleDtos.length - 1);
      for (let i = 0; i < candleDtos.length; i += 1) {
        if (Math.floor(Number(candleDtos[i].t) / 1000) >= target) {
          index = i;
          break;
        }
      }
      chart.timeScale().setVisibleLogicalRange({
        from: index - CENTRE_BARS,
        to: index + CENTRE_BARS,
      });
    },

    destroy() {
      chart.remove();
      candleSeries = null;
      markerPlugin = null;
      layerSeries = [];
      candleDtos = [];
      candlePoints = [];
      overlayDtos = [];
      tradeDtos = [];
    },
  };
}
