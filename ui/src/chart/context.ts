// The chart context (r7.s2.w1, ADR-0030 amendment 2026-11-14) — what one chart pane shows
// and how its reads are told apart. Nothing here crosses the wire beyond the read's own
// request: the pane id and the request id decide only which answer a pane still wants.

import type { ChartDataRequest } from "../bindings";

/** One chart pane's identity: stable for the pane's life, unique among the panes on screen. */
export type PaneId = string;

/** The pane the single-chart Chart tab draws into, until the layouts land (r7.s8). */
export const MAIN_PANE: PaneId = "main";

/** One indicator a trader added to a pane. r7.s4 fills this slot under its own ADR-0030
 *  amendment; until then every pane's list is empty. `token` is the engine's own label
 *  grammar, the one MCP `export_indicators` accepts (`ema:50`). */
export interface IndicatorSetting {
  readonly token: string;
}

/** What one pane shows: its identity, its coin, its timeframe and its indicators. No
 *  drawing is attached: drawings are server state (r7.s7 design). */
export interface ChartContext {
  readonly paneId: PaneId;
  readonly pair: string;
  readonly timeframe: string;
  readonly indicators: readonly IndicatorSetting[];
}

/** The bus request for one read of `context`: its pair and timeframe, the window bounds
 *  and the optional run. The pane id and the indicators do not cross the wire in r7.s2. */
export function chartDataRequest(
  context: ChartContext,
  fromMs: string | null,
  toMs: string | null,
  runId: string | null,
): ChartDataRequest {
  return { pair: context.pair, timeframe: context.timeframe, fromMs, toMs, runId };
}

/** Per-pane request ids: a pane's newest read wins, and an answer to an older read of
 *  that pane is dropped. */
export interface PaneReadGate {
  /** Start a read of `paneId`. The id returned supersedes every earlier id of that pane. */
  begin(paneId: PaneId): number;
  /** Whether `requestId` is still the newest read of `paneId`. */
  accepts(paneId: PaneId, requestId: number): boolean;
}

/** A gate whose ids increase across every pane, so no two reads share one. */
export function createPaneReadGate(): PaneReadGate {
  let last = 0;
  const newest = new Map<PaneId, number>();
  return {
    begin(paneId) {
      last += 1;
      newest.set(paneId, last);
      return last;
    },
    accepts(paneId, requestId) {
      return newest.get(paneId) === requestId;
    },
  };
}
