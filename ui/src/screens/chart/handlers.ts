// The Chart tab's handlers (r7.s2.w2, #502 C5): the free choice, the leg switch, the trade
// pick and the go-to, moved unchanged out of `ChartScreen.tsx`. Plain functions over one render's state.

import type { ChartTradeDto } from "../../bindings";
import { covers, parseUtcInstant, windowAround } from "../../chart/window";
import { dateUtc, timeUtc } from "../../format/time";
import type { ChartRequest, Leg } from "./model";
import { atOrAfterMs, beforeMs, readKey, requestFor, timeframeOf, tradeKey } from "./model";
import type { ChartState } from "./state";
import type { ChartView } from "./view";

/** The free choice, the leg switch and the trade pick. */
export function chartHandlers(s: ChartState, view: ChartView) {
  const {
    freeRef,
    setFree,
    select,
    setSelectedKey,
    setRun,
    setAnchorMs,
    setUndrawn,
    setGoToNote,
    putRequest,
    setLeg,
    run,
    free,
    anchorMs,
    anchorWhat,
    setAnchorWhat,
    setRail,
    setRailOpen,
    drawnReadRef,
    adapterRef,
  } = s;
  const { dto, drawnTimeframe, timeframe } = view;
  const selectFree = (pair: string, timeframe: string) => {
    freeRef.current = { pair, timeframe };
    setFree(freeRef.current);
    // An explicit free choice is the tab's selection now, whatever the fragment still
    // names: the in-flight run read must not take it back (and the drawn-answer gate
    // must let the free read through).
    select(null);
    setSelectedKey(null);
    setRun(null);
    setAnchorMs(null);
    setUndrawn(null);
    setGoToNote(null);
    putRequest(requestFor(null, pair, timeframe, null));
  };

  const selectLeg = (next: Leg) => {
    setLeg(next);
    setUndrawn(null);
    putRequest(requestFor(run, run?.pair ?? free.pair, next === "htf" && run !== null
      ? timeframeOf(run, "htf")
      : run === null
        ? free.timeframe
        : run.primaryTimeframe, anchorMs, anchorWhat));
  };

  /** A trade row: centre the chart on its bar, or read a window around it. Two things
   *  send the tab back to the read — the leg on screen is not the leg the trader asked
   *  for, or the bar this trade entered on is not in the window being drawn. A read
   *  returns trades that entered before its window and exited inside it, and scrolling
   *  to one of those bars would scroll to nothing. */
  const selectTrade = (trade: ChartTradeDto) => {
    setSelectedKey(tradeKey(trade));
    // A picked trade's stored fields are the Inspector's (canvas v9, r7.s2.w3).
    setRail("inspector");
    setRailOpen(true);
    setAnchorMs(trade.entryTime);
    setAnchorWhat("entry");
    setUndrawn(null);
    const next = requestFor(run, run?.pair ?? free.pair, timeframe, trade.entryTime);
    const covered = covers(dto?.candles ?? [], trade.entryTime);
    const drawn = drawnReadRef.current;
    // Some bars this leg can never draw, and another read cannot change that: the run's
    // whole span is already on screen (the widest window this leg can ask for, so a
    // narrower one reaches no further), or the very window this click would ask for has
    // been read and does not hold the bar. Name it — a silent reload would promise a bar
    // that never arrives.
    if (
      !covered &&
      dto !== null &&
      drawnTimeframe === timeframe &&
      (drawn?.span === true || drawn?.key === readKey(next))
    ) {
      setUndrawn(trade.entryTime);
      return;
    }
    if (drawnTimeframe !== timeframe || !covered) {
      putRequest(next);
      return;
    }
    adapterRef.current?.centerOn(trade.entryTime);
  };
  return { selectFree, selectLeg, selectTrade };
}

/** Go to a typed UTC date (r6.s4.w3). */
export function goToHandler(s: ChartState, view: ChartView): (text: string) => void {
  const {
    lastAnswerRef,
    run,
    phase,
    setGoToNote,
    setUndrawn,
    setAnchorMs,
    setAnchorWhat,
    adapterRef,
    free,
    drawnReadRef,
    putRequest,
  } = s;
  const { dto, drawnTimeframe, timeframe } = view;
  /** Go to a typed UTC date (r6.s4.w3): centre on its bar when the drawn window holds it,
   *  else read a window around it. Every refusal says why and sends nothing (PDR-0011);
   *  the holdout cut stays the server's guarantee, and the refusal here is only a sentence.
   *  Every date parse is `parseUtcInstant`'s: this screen compares epoch text with `BigInt`. */
  const goTo = (text: string) => {
    // A typed date moves the trader's eye off the chart: take its crosshair off (#505).
    adapterRef.current?.clearCrosshair();
    const instant = parseUtcInstant(text);
    if (instant === null) {
      setGoToNote("Type a UTC date as YYYY-MM-DD or YYYY-MM-DD HH:mm.");
      return;
    }
    const answered = lastAnswerRef.current;
    // The freshest word on the fence: the last chart answer's, else the run read's. After a
    // failed first read neither exists, and the read is sent: the server cuts at the
    // holdout whatever the tab knows.
    const holdoutStart = answered !== null ? answered.holdoutStartMs : run?.holdoutStartMs ?? null;
    if (answered === null && run === null && phase.kind === "loading") {
      setGoToNote("The chart has not loaded yet; go to a date once it has.");
      return;
    }
    // A run's chart is cut at the holdout start, so a date inside it is refused; the free chart
    // draws the holdout's bars (ADR-0030 amendment, 2026-11-14).
    if (run !== null && holdoutStart !== null && atOrAfterMs(instant, holdoutStart)) {
      setGoToNote(
        `That date is inside the holdout from ${dateUtc(holdoutStart)}; the chart withholds it.`,
      );
      return;
    }
    if (
      run !== null &&
      (beforeMs(instant, run.firstOpenTimeMs) || beforeMs(run.lastCloseTimeMs, instant))
    ) {
      setGoToNote(
        `That date is outside this run (${timeUtc(run.firstOpenTimeMs)} to ${timeUtc(run.lastCloseTimeMs)}).`,
      );
      return;
    }
    setGoToNote(null);
    setUndrawn(null);
    // A later leg switch reads around the same date.
    setAnchorMs(instant);
    setAnchorWhat("date");
    if (dto !== null && drawnTimeframe === timeframe && covers(dto.candles, instant)) {
      adapterRef.current?.centerOn(instant);
      return;
    }
    const next: ChartRequest =
      run !== null
        ? requestFor(run, run.pair, timeframe, instant, "date")
        : {
            pair: free.pair,
            timeframe: free.timeframe,
            runId: null,
            plan: windowAround(free.timeframe, instant),
            anchorMs: instant,
            anchorWhat: "date",
          };
    // The whole span of this leg is already drawn and does not hold the bar: no read can.
    if (
      next.plan.kind === "span" &&
      drawnReadRef.current?.span === true &&
      drawnTimeframe === timeframe
    ) {
      setGoToNote(`No ${timeframe} bar at ${timeUtc(instant)} is drawn in this run's window.`);
      return;
    }
    putRequest(next);
  };
  return goTo;
}
