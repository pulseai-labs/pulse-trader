// The chart read's window arithmetic (r5.s2.w2) — which bars to ask the server for.
//
// The read is bounded: at most 20,000 candles, and a window over the cap is refused
// with the cap and the count named, never silently truncated (grill Q3/G4; ADR-0030).
// So the tab decides BEFORE it asks:
//
//  * a leg whose whole span fits in one read is asked for as the run's own span
//    (`fromMs`/`toMs` null — the read's own default);
//  * a leg that does not fit, with a trade selected, is asked for as a 1,001-bar window
//    around that trade's own bar — the entry fill, which is the bar the trader is
//    looking at — clamped into the run's span;
//  * a leg that does not fit with no trade selected is NOT read at all: the screen says
//    how many bars it would be and asks for a trade, because sending a read the server
//    will refuse only to render the refusal is worse than saying what is wrong.
//
// The span comes from the run itself (`firstOpenTimeMs`/`lastCloseTimeMs` — the primary
// snapshot's own extent, which is exactly what a defaulted window would have covered),
// so this needs no read of its own.
//
// This lives here rather than in the screen for two reasons: it is the chart's own
// geometry, and `ui/src/screens/` is scanned for float conversions of money — an epoch
// bound is a number that must never be read as a figure.

import type { ChartCandleDto } from "../bindings";

/** The bars one read may ask for. The read's own cap, mirrored so the tab can refuse
 *  to send what the server would refuse to answer. */
export const CHART_BAR_CAP = 20_000;

/** How many bars either side of the selected trade's bar the window holds. */
export const WINDOW_HALF_BARS = 500;

/** The intervals this tab can size, in milliseconds. The screen offers exactly these
 *  (the ones the app's server holds today); an interval that is not here cannot be
 *  estimated, and `windowPlan` then asks for the span and lets the read answer. */
const TIMEFRAME_MS: Readonly<Record<string, number>> = {
  "15m": 900_000,
  "1h": 3_600_000,
  "4h": 14_400_000,
  "1d": 86_400_000,
};

/** The run's own extent, as the run DTO spells it. */
export interface RunSpan {
  readonly firstMs: string;
  readonly lastMs: string;
}

/** What the tab will ask the read for. */
export type WindowPlan =
  | { readonly kind: "span" }
  | { readonly kind: "window"; readonly fromMs: string; readonly toMs: string }
  | { readonly kind: "needs-trade"; readonly bars: number };

/** Which bars to ask for: the run's span, a window around `anchorMs`, or a refusal to
 *  read until a trade is picked. */
export function windowPlan(
  span: RunSpan | null,
  timeframe: string,
  anchorMs: string | null,
): WindowPlan {
  const ms = TIMEFRAME_MS[timeframe];
  if (span === null || ms === undefined) return { kind: "span" };

  const first = Number(span.firstMs);
  const last = Number(span.lastMs);
  const bars = Math.floor((last - first) / ms) + 1;
  if (bars <= CHART_BAR_CAP) return { kind: "span" };
  if (anchorMs === null) return { kind: "needs-trade", bars };

  const anchor = Number(anchorMs);
  const half = WINDOW_HALF_BARS * ms;
  return {
    kind: "window",
    fromMs: String(Math.max(anchor - half, first)),
    toMs: String(Math.min(anchor + half, last)),
  };
}

/** Whether the window the chart is drawing holds the bar at `ms`. A trade whose bar is
 *  not here cannot be scrolled to — the screen re-reads around it instead (a trade the
 *  read returned may have entered before the window and exited inside it). */
export function covers(candles: readonly Pick<ChartCandleDto, "t">[], ms: string): boolean {
  if (candles.length === 0) return false;
  const target = Number(ms);
  const first = Number(candles[0].t);
  const last = Number(candles[candles.length - 1].t);
  return target >= first && target <= last;
}
