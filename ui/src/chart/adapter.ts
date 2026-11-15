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
//    library draws it in the container's bottom-left corner. The screen's legend, holdout
//    tag, state cards and refusals sit over the canvas (canvas v9), each clear of that
//    corner (r7.s2.w2).
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
//    holdout start onward; a series primitive hatches them over the candles, as canvas v9
//    does, and on through the time axis (r7.s2.w2), from halfway between the last bar before
//    the start and the first bar inside it to the pane's right edge, with a dashed edge line.
//  * **The volume (r7.s2.w2).** A histogram on its own price scale fills the candle pane's bottom 12%, as
//    canvas v9 draws it; it never moves the candles' scale.
//  * **The default view (r7.s2.w2, #456).** A read with no anchor shows its latest 164 bars, canvas v9's
//    view, where every 4h bar is wide enough to click; a shorter window is shown whole.
//  * **Labels never overlap (r7.s2.w2, #504).** On every draw, scroll, zoom and resize the marker labels are
//    thinned by their on-screen gap (`declutterTexts`); the markers stay.

import {
  CandlestickSeries,
  ColorType,
  HistogramSeries,
  LineSeries,
  LineStyle,
  LineType,
  createChart,
  createSeriesMarkers,
} from "lightweight-charts";
import type {
  CandlestickData,
  HistogramData,
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
  declutterTexts,
  holdoutBandBars,
  markersInWindow,
  setupMarkers,
  stopLine,
  toCandles,
  toMarkers,
  toOverlayLayers,
  toVolumeBars,
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
  /** Call `handler` with the open of the loaded candle under the crosshair, or `null` when the crosshair is off
   *  the loaded candles (r7.s2.w2, the OHLC legend). One handler: a second call replaces the first. */
  onCrosshairMove(handler: (openTime: string | null) => void): void;
  /** Take the crosshair off the chart (#505). */
  clearCrosshair(): void;
  /** Show the latest `DEFAULT_VISIBLE_BARS` loaded bars, canvas v9's default view (#456), or every loaded bar
   *  when fewer are loaded. */
  showLatest(): void;
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

/** How many bars the default view shows: canvas v9's chart (164 4h bars, about four weeks), where a bar is
 *  several pixels wide and every one takes a click (#456). */
const DEFAULT_VISIBLE_BARS = 164;

/** The volume's own price scale, and the share of the candle pane above it: canvas v9's volume fills the
 *  bottom 12% (`VOLH = PH * 0.12`). */
const VOLUME_SCALE = "volume";
const VOLUME_TOP_MARGIN = 0.88;

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

/** Hatch `target` from `edge` (CSS pixels) to its right edge, with the dashed edge line: canvas v9's hatch. */
function drawHatch(
  target: Parameters<IPrimitivePaneRenderer["draw"]>[0],
  edge: number,
  palette: ChartPalette,
): void {
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
}

/** The holdout band primitive. Its left edge is halfway between the last bar before the start and
 *  the first bar inside it (the pane's left edge when the first loaded bar is inside), so the hatch
 *  covers whole bars; it is drawn over the candles, as canvas v9 does (PDR-0060 d1: the candles lie under the
 *  band), and again across the time axis (r7.s2.w2). */
function createHoldoutBand(palette: ChartPalette): HoldoutBand {
  let bars: HoldoutBandBars | null = null;
  let chart: SeriesAttachedParameter<Time>["chart"] | null = null;
  let requestUpdate: (() => void) | null = null;
  let left: number | null = null;
  const renderer: IPrimitivePaneRenderer = {
    draw: (target) => {
      if (left !== null) drawHatch(target, left, palette);
    },
  };
  const view: IPrimitivePaneView = { renderer: () => renderer, zOrder: () => "top" };
  const axisView: IPrimitivePaneView = { renderer: () => renderer, zOrder: () => "top" };
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
    timeAxisPaneViews() {
      return [axisView];
    },
    setBars(next) {
      bars = next;
      requestUpdate?.();
    },
  };
}

/** The volume histogram (r7.s2.w2): canvas v9's bars in the candle pane's bottom 12%, on a price scale of their
 *  own so they never move the candles' scale. */
function createVolumeSeries(chart: IChartApi): ISeriesApi<"Histogram"> {
  const volume = chart.addSeries(
    HistogramSeries,
    { priceScaleId: VOLUME_SCALE, priceFormat: { type: "volume" }, lastValueVisible: false, priceLineVisible: false },
    0,
  );
  volume.priceScale().applyOptions({ scaleMargins: { top: VOLUME_TOP_MARGIN, bottom: 0 } });
  return volume;
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
  let volumeSeries: ISeriesApi<"Histogram"> | null = null;
  let layerSeries: ISeriesApi<"Line">[] = [];
  /** The selected trade's stop line on the candle series, kept so it can be taken off. */
  let stopPriceLine: IPriceLine | null = null;
  /** The one click handler the screen subscribed, kept so `destroy` unsubscribes the
   *  EXACT function it subscribed rather than every handler on the chart. */
  let clickHandler: ((param: MouseEventParams<Time>) => void) | null = null;
  let crosshairHandler: ((param: MouseEventParams<Time>) => void) | null = null;

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
    volumeSeries = createVolumeSeries(chart);
    return candleSeries;
  };

  /** Rebuild the merged marker list from the overlays, trades, marks and the selected
   *  trade's setup last handed in, and
   *  hand the library ONLY the markers the loaded bars can hold, each on its containing
   *  bar (CR2, CR4): a marker time the series cannot place is snapped onto the first or
   *  last loaded candle, and a time inside a bar would otherwise resolve to the NEXT one. */
  const drawMarkers = (): void => {
    if (markerPlugin === null) return;
    // Labels that would overlap at this zoom are dropped (#504).
    markerPlugin.setMarkers(
      declutterTexts(
        markersInWindow(
          [
            ...toMarkers(overlayDtos, tradeDtos, palette, markDtos),
            ...setupMarkers(selectedTrade, selectedSeries, palette),
          ],
          candlePoints,
        ),
        (time) => chart.timeScale().timeToCoordinate(time as UTCTimestamp),
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

  /** Re-thin the labels whenever the bars move under them (#504). */
  const redrawMarkers = (): void => drawMarkers();
  chart.timeScale().subscribeVisibleLogicalRangeChange(redrawMarkers);
  chart.timeScale().subscribeSizeChange(redrawMarkers);
  /** The pointer left the chart: no crosshair stays pinned behind it (#505). */
  const onPointerLeave = (): void => chart.clearCrosshairPosition();
  container.addEventListener("mouseleave", onPointerLeave);

  return {
    setCandles(next) {
      candleDtos = [...next];
      candlePoints = toCandles(candleDtos);
      ensureCandles().setData(candlePoints as CandlestickData<Time>[]);
      volumeSeries?.setData(toVolumeBars(candleDtos, palette) as HistogramData<Time>[]);
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

    onCrosshairMove(handler) {
      if (crosshairHandler !== null) chart.unsubscribeCrosshairMove(crosshairHandler);
      crosshairHandler = (param: MouseEventParams<Time>): void => {
        // The same reading as a click's: the raw position decides, and off the loaded bars is no bar.
        if (!loadedLogical(param.logical as number | undefined)) {
          handler(null);
          return;
        }
        handler(String(candlePoints[Math.round(param.logical as number)].time * 1000));
      };
      chart.subscribeCrosshairMove(crosshairHandler);
    },

    clearCrosshair() {
      chart.clearCrosshairPosition();
    },

    showLatest() {
      if (candlePoints.length <= DEFAULT_VISIBLE_BARS) {
        chart.timeScale().fitContent();
        return;
      }
      chart.timeScale().setVisibleLogicalRange({
        from: candlePoints.length - DEFAULT_VISIBLE_BARS,
        to: candlePoints.length - 1,
      });
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
      // Never fewer than ten bars, never more than the whole loaded window, unless the view already is.
      const most = Math.max(MIN_ZOOM_HALF_BARS, candlePoints.length / 2);
      const clamped = Math.min(Math.max(wanted, MIN_ZOOM_HALF_BARS), most);
      // A step never moves against its direction (#501): zooming in never widens the view, and zooming out never
      // narrows it, even from a view outside the clamp.
      const next = direction === "in" ? Math.min(clamped, half) : Math.max(clamped, half);
      chart.timeScale().setVisibleLogicalRange({ from: centre - next, to: centre + next });
    },

    destroy() {
      // Unsubscribe the exact handler this adapter put on, before the chart it belongs to
      // is taken down. A handler left subscribed would fire against a destroyed chart.
      if (clickHandler !== null) {
        chart.unsubscribeClick(clickHandler);
        clickHandler = null;
      }
      if (crosshairHandler !== null) {
        chart.unsubscribeCrosshairMove(crosshairHandler);
        crosshairHandler = null;
      }
      chart.timeScale().unsubscribeVisibleLogicalRangeChange(redrawMarkers);
      chart.timeScale().unsubscribeSizeChange(redrawMarkers);
      container.removeEventListener("mouseleave", onPointerLeave);
      chart.remove();
      candleSeries = null;
      markerPlugin = null;
      holdoutBand = null;
      volumeSeries = null;
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
