// The Chart tab's model (r7.s2.w2, #502 C5): its constants, its state shapes and the pure
// helpers the screen's hooks and components share, moved unchanged out of `ChartScreen.tsx`.

import type { BacktestRunDto, ChartDataDto, ChartTradeDto, MarkDto, TradeRowDto } from "../../bindings";
import type { WindowPlan } from "../../chart/window";
import { windowPlan } from "../../chart/window";
import { dateUtc, timeUtc } from "../../format/time";

/** The pairs the tab opens on (the four coins the server holds). */
export const PAIRS: readonly string[] = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "XRPUSDT"];

/** The timeframes the free chart offers. The server holds these three today; an
 *  interval it does not hold is refused by the read and shown as it came. */
export const TIMEFRAMES: readonly string[] = ["15m", "4h", "1d"];

export const DEFAULT_PAIR = "BTCUSDT";
export const DEFAULT_TIMEFRAME = "4h";

/** Which of the run's two series the tab is drawing. */
export type Leg = "primary" | "htf";

/** What the tab read about the run itself — the fields a chart needs, and no more. */
export interface RunContext {
  readonly runId: string;
  readonly pair: string;
  readonly primaryTimeframe: string;
  readonly htfTimeframe: string | null;
  readonly usesHtf: boolean;
  readonly firstOpenTimeMs: string;
  readonly lastCloseTimeMs: string;
  /** The open freeze's holdout start as the run read saw it, or null when no freeze
   *  was open then. The run read is the tab's FIRST answer about a run, so this is the
   *  only fence the tab can know before its first chart answer — and a trade that
   *  reaches it carries a withheld-period outcome, so it is never listed. */
  readonly holdoutStartMs: string | null;
  /** How many of the run's stored trades the SERVER withheld because they reach
   *  the holdout start (#417). It is the only word that tells a run whose every
   *  trade was withheld from a run that recorded none — the rows themselves
   *  cannot, because they never arrive. */
  readonly withheldTradeCount: number;
  /** The run's stored trades that CLEAR the fence. The run read is the tab's FIRST
   *  answer about the run and it already carries every trade the run recorded, so
   *  a leg over the bar cap has rows to pick from before any chart read happens
   *  (CR1). */
  readonly trades: readonly ChartTradeDto[];
}

/** One thing to ask the read for. */
export interface ChartRequest {
  readonly pair: string;
  readonly timeframe: string;
  readonly runId: string | null;
  readonly plan: WindowPlan;
  /** The window's centre, for the sentence shown while the read is in flight. */
  readonly anchorMs: string | null;
  /** What the centre is: a trade's entry, or a date the trader typed (r6.s4.w3). Only
   *  the sentence shown while the read is in flight differs. */
  readonly anchorWhat: "entry" | "date";
}

export type Phase =
  | { readonly kind: "loading"; readonly what: string }
  | { readonly kind: "failed"; readonly message: string; readonly code: string }
  | { readonly kind: "ready"; readonly dto: ChartDataDto }
  /** Nothing is drawn and nothing was asked for: the leg is over the cap and the tab
   *  waits for a trade to pick. The notice beside it says exactly that. */
  | { readonly kind: "idle" };

/** A leg the trade is too fine to read whole until a trade is picked. */
export interface Notice {
  readonly timeframe: string;
  readonly bars: number;
}

/** The example-trade marks' read for the pair on screen (r5.s3.w3).
 *
 *  ONE read, filtered by pair only, and partitioned here: a timeframe-filtered read
 *  could not also count the marks living on the other timeframes, which Approach 3
 *  requires the tab to say plainly. */
export type MarksPhase =
  | { readonly kind: "loading" }
  | { readonly kind: "failed"; readonly message: string; readonly code: string }
  | {
      readonly kind: "ready";
      /** The pair this read answered for: a re-read of the same pair keeps these marks on screen (#439, r7.s2.w3). */
      readonly pair: string;
      readonly marks: readonly MarkDto[];
      readonly hidden: { readonly holdoutStartMs: string; readonly count: number } | null;
    };

/** The mark controls' state. Disarmed, armed, submitting, refused (G6). */
export interface MarkForm {
  readonly side: "long" | "short";
  readonly setup: string;
  readonly note: string;
  readonly armed: boolean;
  readonly submitting: boolean;
}

export const DISARMED: MarkForm = {
  side: "long",
  setup: "",
  note: "",
  armed: false,
  submitting: false,
};

/** Whether the instant `raw` is at or after `boundary`, both epoch-ms integer text.
 *
 *  `BigInt`, never `Number`: the screens are scanned by the no-float guard
 *  (`format/no-float-money.test.ts`), and an epoch bound is not chart geometry, so it
 *  does not get to hide behind that marker. The comparison is integer arithmetic on the
 *  exact strings, the same way `format/time.ts` reads them. Unreadable text answers
 *  `false`: the tab then says what the server said rather than claiming a fence it cannot
 *  place. */
export function atOrAfterMs(raw: string, boundary: string): boolean {
  try {
    return BigInt(raw.trim()) >= BigInt(boundary.trim());
  } catch {
    return false;
  }
}

/** Whether the instant `raw` is before `boundary`, both epoch-ms integer text. The
 *  same exact `BigInt` compare as `atOrAfterMs`, and the same answer for unreadable
 *  text: `false`. */
export function beforeMs(raw: string, boundary: string): boolean {
  try {
    return BigInt(raw.trim()) < BigInt(boundary.trim());
  } catch {
    return false;
  }
}

/** What a refused write says, in plain words (G6, PDR-0011).
 *
 *  The server's message is ALWAYS what is shown when nothing better can be said: it is
 *  prose, safe to show a trader, and it names the offending member.
 *
 *  Two refusals get the spec's own sentence instead, and both are decided from the
 *  refusal's code rather than from the message's wording:
 *
 *  * **The holdout fence** crosses the bus as its own code, `holdout` (#427), so a field
 *    refusal at a bar inside the holdout keeps its own message. With the holdout start known
 *    the tab names its date; when the freeze opened after the tab's last read the start is
 *    unknown, and the server's message, which names the date itself, is shown.
 *  * **A candle that does not exist.** `candle_not_found` survives the fold as
 *    `Validation` too, and the tab cannot tell it from another validation, so it names the
 *    time, the pair and the timeframe ONLY when the message's own code came through
 *    unchanged. */
export function refusalText(
  code: string,
  message: string,
  context: { readonly pair: string; readonly timeframe: string; readonly barTime: string },
  holdoutStartMs: string | null,
): string {
  if (code === "holdout" && holdoutStartMs !== null) {
    return `This candle is inside the holdout from ${dateUtc(holdoutStartMs)}; marks there are refused for every writer`;
  }
  if (code === "candle_not_found") {
    return `No candle opens at ${timeUtc(context.barTime)} on ${context.pair} ${context.timeframe} in the data the server holds.`;
  }
  return message;
}

/** A mark's writer, as the table shows it: the trader by its token's label, an agent by
 *  its own name and marked as self-reported (G5, PDR-0015). */
export function writerText(writer: MarkDto["writer"]): string {
  return `${writer.kind} · ${writer.name}${writer.selfReported ? " (self-reported)" : ""}`;
}

/** `1 mark on 15m` / `2 marks on 15m`, several timeframes joined. */
export function otherTimeframeNote(marks: readonly MarkDto[], drawn: string): string {
  const counts = new Map<string, number>();
  for (const mark of marks) {
    if (mark.timeframe === drawn) continue;
    counts.set(mark.timeframe, (counts.get(mark.timeframe) ?? 0) + 1);
  }
  return [...counts.entries()]
    .sort((a, b) => a[0].localeCompare(b[0]))
    .map(([timeframe, count]) => `${count} ${count === 1 ? "mark" : "marks"} on ${timeframe}`)
    .join(" · ");
}

/** The timeframe a leg names, or the primary's when the run recorded only one. */
export function timeframeOf(run: RunContext, leg: Leg): string {
  if (leg === "htf" && run.htfTimeframe !== null) return run.htfTimeframe;
  return run.primaryTimeframe;
}

/** Whether the run offers a second leg at all: a run that recorded one timeframe twice
 *  (w1's D6) has one, and the tab shows it as a chip rather than as a switch. */
export function hasTwoLegs(run: RunContext): boolean {
  return run.htfTimeframe !== null && run.htfTimeframe !== run.primaryTimeframe;
}

export function requestFor(
  run: RunContext | null,
  pair: string,
  timeframe: string,
  anchorMs: string | null,
  anchorWhat: "entry" | "date" = "entry",
): ChartRequest {
  const span = run === null ? null : { firstMs: run.firstOpenTimeMs, lastMs: run.lastCloseTimeMs };
  return {
    pair,
    timeframe,
    runId: run?.runId ?? null,
    plan: windowPlan(span, timeframe, anchorMs),
    anchorMs,
    anchorWhat,
  };
}

/** What the tab says it is reading, in one sentence. */
export function loadingText(request: ChartRequest): string {
  if (request.plan.kind === "window" && request.anchorMs !== null) {
    if (request.anchorWhat === "date") {
      return `Loading ${request.timeframe} around ${timeUtc(request.anchorMs)}…`;
    }
    return `Loading ${request.timeframe} around the entry at ${timeUtc(request.anchorMs)}…`;
  }
  return `Loading ${request.pair} ${request.timeframe}…`;
}

/** The identity of a read: the four fields the server answers by. Two requests with the
 *  same identity are the same question, and its answer can only repeat the first. */
export function readKey(request: ChartRequest): string {
  const from = request.plan.kind === "window" ? request.plan.fromMs : "";
  const to = request.plan.kind === "window" ? request.plan.toMs : "";
  return [request.pair, request.timeframe, request.runId ?? "", from, to].join("|");
}

/** The run's stored trades (`TradeRowDto`) in the chart's own trade shape. Both describe
 *  the same recorded fills; the run row spells them `entryFillTime`/`exitFillTime`
 *  because it also records the SIGNAL bar and the size, which this chart never draws. */
function toChartTrades(rows: readonly TradeRowDto[]): ChartTradeDto[] {
  return rows.map((row) => ({
    side: row.direction,
    entryTime: row.entryFillTime,
    entryPrice: row.entryPrice,
    exitTime: row.exitFillTime,
    exitPrice: row.exitPrice,
    exitReason: row.exitReason,
    realizedR: row.realizedR,
    stopPrice: row.stopPrice,
    // the run read carries no setup trace; the drawn window's rows do (w4 draws from those)
    armBarTime: null,
    htfArmBarTime: null,
    dipBarTime: null,
    triggerBarTime: null,
  }));
}

/** One trade's identity across the two reads that can carry it — the run's stored rows
 *  and the drawn window's rows — so the union of the two lists never doubles a trade. */
export function tradeKey(trade: ChartTradeDto): string {
  return `${trade.entryTime}-${trade.side}-${trade.exitTime ?? ""}`;
}

/** The run's own facts a chart needs, read off the run DTO. */
export function readRun(dto: BacktestRunDto): RunContext {
  return {
    runId: dto.runId,
    pair: dto.pair,
    primaryTimeframe: dto.primaryTimeframe,
    htfTimeframe: dto.htfTimeframe,
    usesHtf: dto.usesHtf,
    firstOpenTimeMs: dto.firstOpenTimeMs,
    lastCloseTimeMs: dto.lastCloseTimeMs,
    holdoutStartMs: dto.holdoutStartMs,
    withheldTradeCount: dto.withheldTradeCount,
    trades: toChartTrades(dto.trades),
  };
}

export function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** The right rail's tabs, in canvas v9's order (`chart.jsx` `RightRail`, r7.s2.w3). */
export type RailTab = "inspector" | "layers" | "agent";

/** Each rail tab and the label it shows. */
export const RAIL_TABS: readonly (readonly [RailTab, string])[] = [
  ["inspector", "Inspector"],
  ["layers", "Layers"],
  ["agent", "Agent"],
];

/** The bar the trader clicked with the mark control disarmed, tied to the chart it was clicked on: the drawn
 *  read's pair and timeframe and the bar's open (CR1, r7.s2.w3). The Inspector shows it only on that chart. */
export interface FocusBar {
  readonly pair: string;
  readonly timeframe: string;
  readonly t: string;
}
