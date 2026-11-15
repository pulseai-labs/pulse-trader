// What one render of the Chart tab derives from its state (r7.s2.w2, #502 C5): the drawn
// answer, the selected trade, the timeframes, the marks on the leg, the trade rows, the overlay rows, the holdout
// tag and the legend's bar. Pure: no hook, no handle.

import type { ChartCandleDto, ChartDataDto, ChartOverlayDto, ChartTradeDto } from "../../bindings";
import { beforeFence, fenceStartMs } from "../../chart/layers";
import { EM_DASH } from "../../format/numbers";
import { dateUtc, timeUtc } from "../../format/time";
import type { RunContext } from "./model";
import { atOrAfterMs, timeframeOf, tradeKey } from "./model";
import type { ChartState } from "./state";

/** The rows the trades table lists, fenced at the freshest holdout the tab knows. */
export function tradeRows(run: RunContext | null, dto: ChartDataDto | null) {
  // --- what is drawn right now ---------------------------------------------
  // The rows the trader picks a window from. The run's own stored trades are ALWAYS
  // here: a bounded window's list must never erase the distant trades the trader can
  // navigate to next — a window response holding one trade is not the run (the close
  // review's C) — and a drawn window's rows are added to them, in their own order, so
  // the accessible copy stays the drawn window's twin (F2). Both lists are the server's
  // own records, and both are fenced at the freeze the tab knows (B): a trade that
  // reaches the holdout start carries a withheld-period outcome and is never listed.
  // The freshest word wins: a chart answer's own holdout — a `null` there means no
  // freeze was open at that read, which is newer than the run read — else the run
  // read's boundary, the only one available before the first chart answer.
  const fence = fenceStartMs(
    dto === null ? run?.holdoutStartMs ?? null : dto.holdout?.holdoutStartMs ?? null,
  );

  const runTrades = (run?.trades ?? []).filter((trade) => beforeFence(trade, fence));
  const windowTrades =
    dto?.run == null ? null : dto.run.trades.filter((trade) => beforeFence(trade, fence));
  const trades = (() => {
    if (windowTrades === null || windowTrades.length === 0) return runTrades;
    // The run's rows are already listed; a window row is added only when the run read
    // did not carry it (the two reads are the same server rows, so this is a guard).
    const listed = new Set(runTrades.map(tradeKey));
    return [...runTrades, ...windowTrades.filter((trade) => !listed.has(tradeKey(trade)))];
  })();
  /** Whether the FENCE emptied the list rather than the run having none: the empty
   *  state says which of the two it is (PDR-0011 — no state names a trade that is not
   *  there, and no fence reads as "this run traded nothing").
   *
   *  The run read's own count is the authority (#417): the server now withholds a
   *  trade that reaches the holdout, so a run whose EVERY trade is withheld comes
   *  back with `trades` empty and `withheldTradeCount` naming how many it left out.
   *  The local list alone cannot tell that from a run that traded nothing — the
   *  client-side filter sees only what it was sent. */
  const fencedOut =
    trades.length === 0 &&
    ((run?.withheldTradeCount ?? 0) > 0 || (run?.trades.length ?? 0) > 0);
  return { trades, windowTrades, fencedOut };
}

/** The structure events and the bar-table layers the run's overlays carry. */
export function overlayRows(dto: ChartDataDto | null) {
  const overlays = dto?.run?.overlays ?? [];
  const events = overlays.flatMap((overlay) => {
    if (overlay.kind === "pivot") {
      // A pivot's value is a PRICE, and it appears twice: the bar the engine confirmed
      // it on, and the peak bar `right` bars earlier that the chart marks with a ghost.
      return overlay.events.flatMap((event) => [
        { layer: overlay.label, event: "confirmed", bar: event.confirmTime, value: event.price, asPrice: true },
        { layer: overlay.label, event: "peak (ghost)", bar: event.peakTime, value: event.price, asPrice: true },
      ]);
    }
    if (overlay.kind === "pattern") {
      // A pattern's value is a SCORE (`100`/`-100`/`80`/`-80`), not a price: it is shown
      // exactly as the read sent it, because "100.0000" would read as a price of 100.
      return overlay.events.map((event) => ({
        layer: overlay.label,
        event: "pattern",
        bar: event.time,
        value: event.value,
        asPrice: false,
      }));
    }
    return [];
  });
  // The layers the bars table carries a column for: every drawn series, WT1 and the band
  // edges included. A pivot or pattern is a marker, and has its own table.
  const series = overlays.filter(
    (overlay) =>
      overlay.kind === "level" ||
      overlay.kind === "line" ||
      overlay.kind === "bandEdge" ||
      overlay.kind === "oscillator",
  );
  return { events, series };
}

/** The holdout band's tag on the canvas (r7.s2.w2): a read that names no run, whose loaded bars reach the
 *  holdout start, so the band is drawn. `null` when no band is drawn: the notes then carry the sentence. */
export function holdoutTag(dto: ChartDataDto | null): string | null {
  if (dto === null || dto.run !== null || dto.holdout == null) return null;
  const start = dto.holdout.holdoutStartMs;
  if (!dto.candles.some((candle) => atOrAfterMs(candle.t, start))) return null;
  return `Holdout — from ${dateUtc(start)} · agents can't test here`;
}

/** The bar the OHLC legend shows (r7.s2.w2, canvas v9 `Readout`): the loaded bar under the crosshair, else the
 *  last loaded bar. `null` when nothing is loaded. */
export function legendBar(
  candles: readonly ChartCandleDto[],
  hoverMs: string | null,
): ChartCandleDto | null {
  if (candles.length === 0) return null;
  return candles.find((candle) => candle.t === hoverMs) ?? candles[candles.length - 1];
}

/** What this render draws and lists, derived from the state. */
export function chartView(s: ChartState) {
  const { phase, selected, selectedKey, run, free, leg, marksPhase, focusBar } = s;
  // Only what the tab has SELECTED is this tab's to draw: an answer read for another
  // run — or a run's answer that outlived a free choice — is not drawn, not listed and
  // not centred on (CR3). The gate compares the DTO against the selection, never
  // against the raw fragment: a free read's answer carries no run layer, and the
  // fragment may still name the run the trader deep-linked away from.
  const dto =
    phase.kind === "ready" && (phase.dto.run?.runId ?? null) === selected ? phase.dto : null;
  // The bar the trader clicked, from the drawn window's own candles, only while the drawn read is the pair and
  // timeframe it was clicked on: another chart, or a bar the window does not hold, leaves the Inspector empty
  // (r7.s2.w3, CR1).
  const focusCandle =
    focusBar === null || dto === null || dto.pair !== focusBar.pair || dto.timeframe !== focusBar.timeframe
      ? null
      : (dto.candles.find((candle) => candle.t === focusBar.t) ?? null);
  // The selected trade, from the drawn window's own rows: the run read's rows carry no setup
  // bars (r6.s4.w4). After the draw effect, so the markers land on the candles just set.
  const selectedTrade =
    selectedKey === null
      ? null
      : (dto?.run?.trades.find((candidate) => tradeKey(candidate) === selectedKey) ?? null);
  const drawnTimeframe = dto?.timeframe ?? null;
  const timeframe = run === null ? free.timeframe : timeframeOf(run, leg);
  const markPair = run === null ? free.pair : run.pair;
  // --- draw the marks (r5.s3.w3) --------------------------------------------
  // The marks that belong on THIS series: the on-screen timeframe's own. A mark on
  // another timeframe sits on one of that timeframe's candles, so drawing it here would
  // place it on a bar it does not belong to.
  const drawnMarks = marksPhase.kind === "ready" ? marksPhase.marks : [];
  const marksOnLeg = drawnMarks.filter((mark) => mark.timeframe === drawnTimeframe);
  // The holdout start a refused write's sentence names, read from the freshest word the
  // tab holds: the marks read's own fence when a freeze was open at it, else the drawn
  // chart's, else the run read's. `null` means no freeze was open — a real answer, and
  // the refusal then shows the server's own message rather than an invented date.
  const marksHidden = marksPhase.kind === "ready" ? marksPhase.hidden : null;
  const liveHoldout =
    marksHidden?.holdoutStartMs ??
    (dto === null ? run?.holdoutStartMs ?? null : dto.holdout?.holdoutStartMs ?? null);
  return {
    dto,
    selectedTrade,
    drawnTimeframe,
    timeframe,
    markPair,
    marksOnLeg,
    liveHoldout,
    focusCandle,
    holdoutTag: holdoutTag(dto),
    ...tradeRows(run, dto),
    ...overlayRows(dto),
  };
}

/** One row of the Layers rail (r7.s2.w3): a drawn layer's label, its event count for a pivot or a pattern, and
 *  whether its swatch is dashed (a server layer) or solid (the volume). */
export interface LayerRow {
  readonly label: string;
  readonly count: number | null;
  readonly dashed: boolean;
}

/** The Layers rail's rows, grouped as canvas v9 groups them (`chart.jsx` `LayersPanel`): the volume and the run's
 *  lines, oscillator and band edges under Indicators; its S/R levels and pivots under Structure; its patterns under
 *  Candle patterns. Every row is a layer the read drew. */
export function layerGroups(dto: ChartDataDto | null) {
  const overlays = dto?.run?.overlays ?? [];
  const row = (overlay: ChartOverlayDto): LayerRow => ({
    label: overlay.label,
    count: overlay.kind === "pivot" || overlay.kind === "pattern" ? overlay.events.length : null,
    dashed: true,
  });
  const volume: LayerRow[] =
    dto !== null && dto.candles.length > 0 ? [{ label: "Volume", count: null, dashed: false }] : [];
  return {
    indicators: [
      ...volume,
      ...overlays
        .filter((each) => each.kind === "line" || each.kind === "oscillator" || each.kind === "bandEdge")
        .map(row),
    ],
    structure: overlays.filter((each) => each.kind === "level" || each.kind === "pivot").map(row),
    patterns: overlays.filter((each) => each.kind === "pattern").map(row),
    hasPivots: overlays.some((each) => each.kind === "pivot"),
  };
}

/** The strip's setup cell for one drawn-window row (canvas v9's `setup · arm → dip → trigger`, A4; r7.s2.w3), from
 *  the read's own bar times through `timeUtc`: the arm as `MM-DD HH:mm`, the dip and the trigger as `HH:mm`, a bar
 *  the trade did not record as an em dash. `null` when the row carries no setup bar. The read sends no setup
 *  label (`ChartTradeDto` has none), so none is shown. */
export function setupText(row: ChartTradeDto | null): string | null {
  if (row === null || (row.armBarTime === null && row.dipBarTime === null && row.triggerBarTime === null)) {
    return null;
  }
  const part = (ms: string | null, from: number): string =>
    ms === null ? EM_DASH : timeUtc(ms).slice(from, 16);
  return `${part(row.armBarTime, 5)} → ${part(row.dipBarTime, 11)} → ${part(row.triggerBarTime, 11)}`;
}

/** One render's derived values. */
export type ChartView = ReturnType<typeof chartView>;
