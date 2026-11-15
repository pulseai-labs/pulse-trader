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
//    layer series, and every pane but the candles', before creating its own; only the
//    candle series, and the marker plugin hanging off it, outlive a read.
//  * **WT1 has its own pane (r6.s4.w4).** An oscillator is not a price, so it is created in
//    pane 1 and the candles keep three parts of the height to its one. Its guide at 0 is a
//    price line, not data: the read sends no value for it.
//  * **The selected trade's setup rides the same marker list (r6.s4.w4).** Its arm, dip and
//    trigger markers join the one list `markersInWindow` clips and places, so a setup bar
//    outside the loaded window draws nothing. Its stop is a price line on the candle series.
//  * **The holdout band (r7.s2.w1).** A read that names no run carries the bars from the
//    holdout start onward; a series primitive hatches them behind the candles, from halfway
//    between the last bar before the start and the first bar inside it to the pane's right
//    edge, with a dashed edge line. It is drawn on the candle pane only.

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
  IPriceLine,
  IPrimitivePaneRenderer,
  IPrimitivePaneView,
  ISeriesApi,
  ISeriesMarkersPluginApi,
  ISeriesPrimitive,
  LineData,
  MouseEventParams,
  SeriesAttachedParameter,
  SeriesMarker,
  SeriesMarkerPricePosition,
  Time,
  UTCTimestamp,
  WhitespaceData,
} from "lightweight-charts";

import type { ChartCandleDto, ChartOverlayDto, ChartTradeDto, MarkDto } from "../bindings";
import type { CandlePoint, ChartMarker, HoldoutBandBars, OverlayLayer } from "./layers";
import {
  containingIndex,
  holdoutBandBars,
  markersInWindow,
  setupMarkers,
  stopLine,
  toCandles,
  toMarkers,
  toOverlayLayers,
} from "./layers";
import type { ChartPalette } from "./palette";
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
  /** Draw this trade's setup markers and stop line on the leg the read drew (`null` clears
   *  both). */
  setSelectedTrade(trade: ChartTradeDto | null, series: "primary" | "htf"): void;
  /** Hatch the loaded bars that open at or after `holdoutStartMs` — a run-less read's holdout
   *  band (ADR-0030 amendment, 2026-11-14). `null` clears it. */
  setHoldout(holdoutStartMs: string | null): void;
  /** Call `handler` with the open of the loaded candle a click landed on. A click
   *  outside the loaded candles, or before any read has drawn a bar, calls nothing.
   *  One handler: a second call replaces the first. */
  onCandleClick(handler: (openTime: string) => void): void;
  /** Fit the whole loaded window into view. */
  fitAll(): void;
  /** Scroll so the bar nearest `timeMs` sits in the middle of the pane. */
  centerOn(timeMs: string): void;
  /** Halve ("in") or double ("out") the visible bar range around its middle. */
  zoom(direction: "in" | "out"): void;
  /** Remove the chart and everything it owns. Irreversible: unmount only. */
  destroy(): void;
}

/** How many bars either side of the centred bar stay in view. */
const CENTRE_BARS = 60;

/** One zoom step halves or doubles the visible range. */
const ZOOM_FACTOR = 2;

/** The least half-range a zoom-in leaves: ten bars in view, so every bar stays clickable. */
const MIN_ZOOM_HALF_BARS = 5;

/** The holdout band's fill opacity: canvas v9's hatch fill (`fillOpacity="0.55"`). */
const HOLDOUT_FILL_ALPHA = 0.55;

/** The hatch lines' spacing and width, in CSS pixels (canvas v9's 6 px pattern, 1.2 px line). */
const HOLDOUT_HATCH_STEP = 6;
const HOLDOUT_HATCH_WIDTH = 1.2;

/** The band edge's dash, in CSS pixels (canvas v9's `4 3`). */
const HOLDOUT_EDGE_DASH = [4, 3];

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

/** The holdout band as the adapter drives it: a series primitive plus the one setter it calls. */
interface HoldoutBand extends ISeriesPrimitive<Time> {
  /** Hatch from these loaded bars; `null` draws no band. */
  setBars(bars: HoldoutBandBars | null): void;
}

/** The holdout band primitive. Its left edge is halfway between the last bar before the start and
 *  the first bar inside it (the pane's left edge when the first loaded bar is inside), so the hatch
 *  covers whole bars; it is drawn in the background, under the candles. */
function createHoldoutBand(palette: ChartPalette): HoldoutBand {
  let bars: HoldoutBandBars | null = null;
  let chart: SeriesAttachedParameter<Time>["chart"] | null = null;
  let requestUpdate: (() => void) | null = null;
  let left: number | null = null;
  const renderer: IPrimitivePaneRenderer = {
    draw: () => {
      // The band is background only.
    },
    drawBackground: (target) => {
      const edge = left;
      if (edge === null) return;
      target.useBitmapCoordinateSpace(({ context, bitmapSize, horizontalPixelRatio: ratio }) => {
        const x = Math.max(0, Math.round(edge * ratio));
        const width = bitmapSize.width - x;
        if (width <= 0) return;
        const height = bitmapSize.height;
        context.save();
        context.beginPath();
        context.rect(x, 0, width, height);
        context.clip();
        context.globalAlpha = HOLDOUT_FILL_ALPHA;
        context.fillStyle = palette.holdoutFill;
        context.fillRect(x, 0, width, height);
        context.globalAlpha = 1;
        context.strokeStyle = palette.holdoutHatch;
        context.lineWidth = HOLDOUT_HATCH_WIDTH * ratio;
        context.beginPath();
        for (let offset = x - height; offset < bitmapSize.width; offset += HOLDOUT_HATCH_STEP * ratio) {
          context.moveTo(offset, height);
          context.lineTo(offset + height, 0);
        }
        context.stroke();
        context.strokeStyle = palette.holdoutEdge;
        context.lineWidth = Math.max(1, Math.round(ratio));
        context.setLineDash(HOLDOUT_EDGE_DASH.map((dash) => dash * ratio));
        context.beginPath();
        context.moveTo(x, 0);
        context.lineTo(x, height);
        context.stroke();
        context.restore();
      });
    },
  };
  const view: IPrimitivePaneView = { renderer: () => renderer, zOrder: () => "bottom" };
  return {
    attached(param) {
      chart = param.chart;
      requestUpdate = param.requestUpdate;
    },
    detached() {
      chart = null;
      requestUpdate = null;
    },
    updateAllViews() {
      left = null;
      if (bars === null || chart === null) return;
      const scale = chart.timeScale();
      const first = scale.timeToCoordinate(bars.first as UTCTimestamp);
      if (first === null) return;
      if (bars.before === null) {
        left = 0;
        return;
      }
      const before = scale.timeToCoordinate(bars.before as UTCTimestamp);
      left = before === null ? first : (before + first) / 2;
    },
    paneViews() {
      return [view];
    },
    setBars(next) {
      bars = next;
      requestUpdate?.();
    },
  };
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
    // The library shows a `UTCTimestamp` in UTC, so with the time of day on, the axis and
    // the crosshair label read the bar's UTC open (#456). On `1d` that is `00:00`.
    timeScale: { borderColor: palette.grid, timeVisible: true, secondsVisible: false },
  });

  // What the read last handed in. The candle times are kept as the read spelled
  // them, because the overlays' values are positional against them.
  let candleDtos: readonly ChartCandleDto[] = [];
  // The same bars in the library's units — the window a marker has to fall inside.
  let candlePoints: readonly CandlePoint[] = [];
  let overlayDtos: readonly ChartOverlayDto[] = [];
  let tradeDtos: readonly ChartTradeDto[] = [];
  let markDtos: readonly MarkDto[] = [];
  let selectedTrade: ChartTradeDto | null = null;
  let selectedSeries: "primary" | "htf" = "primary";
  let holdoutStartMs: string | null = null;

  let candleSeries: ISeriesApi<"Candlestick"> | null = null;
  let markerPlugin: ISeriesMarkersPluginApi<Time> | null = null;
  let holdoutBand: HoldoutBand | null = null;
  let layerSeries: ISeriesApi<"Line">[] = [];
  /** The selected trade's stop line on the candle series, kept so it can be taken off. */
  let stopPriceLine: IPriceLine | null = null;
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
    holdoutBand = createHoldoutBand(palette);
    candleSeries.attachPrimitive(holdoutBand);
    return candleSeries;
  };

  /** Rebuild the merged marker list from the overlays, trades, marks and the selected
   *  trade's setup last handed in, and
   *  hand the library ONLY the markers the loaded bars can hold, each on its containing
   *  bar (CR2, CR4): a marker time the series cannot place is snapped onto the first or
   *  last loaded candle, and a time inside a bar would otherwise resolve to the NEXT one. */
  const drawMarkers = (): void => {
    if (markerPlugin === null) return;
    markerPlugin.setMarkers(
      markersInWindow(
        [
          ...toMarkers(overlayDtos, tradeDtos, palette, markDtos),
          ...setupMarkers(selectedTrade, selectedSeries, palette),
        ],
        candlePoints,
      ).map(toSeriesMarker),
    );
  };

  /** Hatch the loaded bars the holdout start last handed in reaches (`null` clears the band). */
  const drawHoldout = (): void => {
    ensureCandles();
    holdoutBand?.setBars(holdoutBandBars(candlePoints, holdoutStartMs));
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
      drawHoldout();
    },

    setOverlays(next) {
      overlayDtos = [...next];
      ensureCandles();
      for (const stale of layerSeries) {
        chart.removeSeries(stale);
      }
      layerSeries = [];
      // The pinned library (5.2.1) removes a pane with its last series, so the removals above
      // already took WT1's pane away; this takes away any empty pane a library that keeps one
      // (`preserveEmptyPane`) would leave behind.
      while (chart.panes().length > 1) chart.removePane(chart.panes().length - 1);
      for (const layer of toOverlayLayers(overlayDtos, candleDtos, palette)) {
        const line = chart.addSeries(
          LineSeries,
          {
            color: layer.color,
            // Every server layer is dashed: they are the server's, not the trader's
            // (design §0 rule 5). A level holds its value between the bars the engine
            // moved it, an EMA moves every bar — that is the only difference between them.
            lineStyle: LineStyle.Dashed,
            lineType: layer.stepped ? LineType.WithSteps : LineType.Simple,
            lineWidth: layer.width,
            priceLineVisible: false,
            lastValueVisible: false,
            crosshairMarkerVisible: false,
          },
          layer.pane,
        );
        line.setData(toLineData(layer));
        if (layer.zeroGuide) {
          // a guide at 0, not data: the read sends no value for it
          line.createPriceLine({
            price: 0,
            color: palette.oscillatorGuide,
            lineStyle: LineStyle.Dotted,
            lineWidth: 1,
            axisLabelVisible: false,
            title: "",
          });
          // The candles keep three parts of the height to WT1's one.
          chart.panes()[0]?.setStretchFactor(3);
          chart.panes()[1]?.setStretchFactor(1);
        }
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

    setSelectedTrade(trade, series) {
      selectedTrade = trade;
      selectedSeries = series;
      const candles = ensureCandles();
      drawMarkers();
      if (stopPriceLine !== null) {
        candles.removePriceLine(stopPriceLine);
        stopPriceLine = null;
      }
      const stop = stopLine(trade);
      if (stop !== null) {
        stopPriceLine = candles.createPriceLine({
          price: stop.price,
          color: palette.stop,
          lineStyle: LineStyle.Dashed,
          lineWidth: 1,
          axisLabelVisible: true,
          title: stop.title,
        });
      }
    },

    setHoldout(next) {
      holdoutStartMs = next;
      drawHoldout();
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

    zoom(direction) {
      if (candlePoints.length === 0) return;
      const range = chart.timeScale().getVisibleLogicalRange();
      if (range === null) return;
      const centre = (range.from + range.to) / 2;
      const half = (range.to - range.from) / 2;
      const wanted = direction === "in" ? half / ZOOM_FACTOR : half * ZOOM_FACTOR;
      // Never fewer than ten bars, never more than the whole loaded window.
      const most = Math.max(MIN_ZOOM_HALF_BARS, candlePoints.length / 2);
      const next = Math.min(Math.max(wanted, MIN_ZOOM_HALF_BARS), most);
      chart.timeScale().setVisibleLogicalRange({ from: centre - next, to: centre + next });
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
      holdoutBand = null;
      holdoutStartMs = null;
      layerSeries = [];
      stopPriceLine = null;
      selectedTrade = null;
      selectedSeries = "primary";
      candleDtos = [];
      candlePoints = [];
      overlayDtos = [];
      tradeDtos = [];
      markDtos = [];
    },
  };
}
