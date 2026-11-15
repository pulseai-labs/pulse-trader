// The Chart tab's effects (r7.s2.w2, #502 C5), moved unchanged out of `ChartScreen.tsx`.
// The screen calls them in the order the effects were declared, so React runs them in that order.

import { useEffect } from "react";

import { commands } from "../../bindings";
import type { MarkDto } from "../../bindings";
import { createChartAdapter } from "../../chart/adapter";
import type { ChartContext } from "../../chart/context";
import { MAIN_PANE, chartDataRequest } from "../../chart/context";
import { chartDeepLink } from "../../routes";
import type { Leg, MarkForm } from "./model";
import {
  hasTwoLegs,
  loadingText,
  messageOf,
  readKey,
  readRun,
  refusalText,
  requestFor,
  timeframeOf,
} from "./model";
import type { ChartState } from "./state";
import type { ChartView } from "./view";

/** The chart's own life (r5.s2.w2). */
export function useChartCanvas(s: ChartState): void {
  const { containerRef, adapterRef, drawnRef } = s;
  // --- the chart's own life -------------------------------------------------
  useEffect(() => {
    const element = containerRef.current;
    if (element === null) return;
    const adapter = createChartAdapter(element);
    adapterRef.current = adapter;
    return () => {
      adapter.destroy();
      adapterRef.current = null;
      drawnRef.current = false;
    };
  }, []);
}

/** The candle click writes a mark (r5.s3.w3). */
export function useMarkWriter(s: ChartState): void {
  const {
    adapterRef,
    markClickRef,
    markInFlightRef,
    setMarkRefusal,
    setForm,
    setMarksSeq,
    liveHoldoutRef,
  } = s;
  /** Write a mark on the clicked candle, through w1's `write_mark`. The pair, the
   *  timeframe and the bar time are all the ones on screen: the mark is written against
   *  the chart the trader is looking at, never against a remembered one. */
  const writeMarkAt = async (
    openTime: string,
    live: { form: MarkForm; pair: string; timeframe: string | null },
  ): Promise<void> => {
    if (live.timeframe === null) return;
    setMarkRefusal(null);
    setForm((current) => ({ ...current, submitting: true }));
    const note = live.form.note.trim();
    try {
      const answer = await commands.writeMark({
        pair: live.pair,
        timeframe: live.timeframe,
        barTime: openTime,
        side: live.form.side,
        setup: live.form.setup.trim(),
        note: note === "" ? null : note,
      });
      if (answer.status !== "ok") {
        // A refused write writes nothing and says why (G2, G6). The control stays
        // armed, so the trader can click another candle without arming again.
        setMarkRefusal(
          refusalText(
            answer.error.code,
            answer.error.message,
            { pair: live.pair, timeframe: live.timeframe, barTime: openTime },
            liveHoldoutRef.current,
          ),
        );
        setForm((current) => ({ ...current, submitting: false }));
        return;
      }
      // The written mark appears without a reload: the list is re-read, and the setup
      // label is kept for the next mark while the note is cleared.
      setForm((current) => ({ ...current, submitting: false, note: "" }));
      setMarksSeq((seq) => seq + 1);
    } catch (error) {
      setMarkRefusal(messageOf(error));
      setForm((current) => ({ ...current, submitting: false }));
    } finally {
      // The write settled, whatever it answered: the next click may write again.
      markInFlightRef.current = false;
    }
  };

  // --- the candle click writes a mark (r5.s3.w3) ----------------------------
  // Subscribed once, on the chart's own life. `arm` is what makes a click a write:
  // disarmed, a click on the chart is just a click.
  useEffect(() => {
    const adapter = adapterRef.current;
    if (adapter === null) return;
    adapter.onCandleClick((openTime) => {
      const live = markClickRef.current;
      if (!live.form.armed || live.form.submitting || live.timeframe === null) return;
      // `live.form.submitting` is state, and state reaches the ref only after the next
      // render — so two clicks in ONE render pass both read `false` on it. This ref is
      // set SYNCHRONOUSLY, before the write starts, so the second click is refused
      // here. A mark is immutable: a duplicate would stand until the trader withdrew
      // it by hand.
      if (markInFlightRef.current) return;
      const setup = live.form.setup.trim();
      if (setup === "") {
        setMarkRefusal("Type a setup label before marking a candle.");
        return;
      }
      markInFlightRef.current = true;
      void writeMarkAt(openTime, live);
    });
  }, []);
}

/** Zoom keys (r6.s4.w3). */
export function useZoomKeys(s: ChartState): void {
  const { adapterRef } = s;
  // --- zoom keys (r6.s4.w3) --------------------------------------------------
  // `+` and `=` zoom in, `-` and `_` zoom out. A chord with the platform key is the
  // browser's own zoom, and a key typed into a field is text: neither reaches the chart.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.metaKey || event.ctrlKey || event.altKey) return;
      const target = event.target;
      if (
        target instanceof Element &&
        (["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName) ||
          target.closest('[contenteditable=""], [contenteditable="true"]') !== null)
      ) {
        return;
      }
      if (event.key === "+" || event.key === "=") {
        event.preventDefault();
        adapterRef.current?.zoom("in");
      } else if (event.key === "-" || event.key === "_") {
        event.preventDefault();
        adapterRef.current?.zoom("out");
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);
}

/** What the fragment names (Q1): the run deep link. */
export function useRunFragment(s: ChartState): void {
  const {
    setFragmentRun,
    fragmentRun,
    putRequest,
    clearDrawing,
    select,
    setSelectedKey,
    setRun,
    setLeg,
    setAnchorMs,
    setNotice,
    setUndrawn,
    setGoToNote,
    freeRef,
    setPhase,
    selectedRef,
  } = s;
  // --- what the fragment names (Q1: a run deep link opens the tab with that run) ---
  // Read on mount and on every later hash change. Deliberately NOT consumed the way the
  // Lab consumes `#/backtest?wf=`: there the fragment is a one-shot selection that a
  // remount must not replay over a remembered one, while here the run IS the tab's
  // state, and a reload or a reconnect has to come back to the same chart.
  useEffect(() => {
    const take = () => setFragmentRun(chartDeepLink(window.location.hash)?.run ?? null);
    window.addEventListener("hashchange", take);
    return () => window.removeEventListener("hashchange", take);
  }, []);

  useEffect(() => {
    // A new run is a new chart. Everything the previous run left here — its context, its
    // phase, its notice and its drawing — belongs to THAT run, and none of it is this
    // one's (CR3): a trader who deep-links from one run to another must never see the
    // previous run's candles or trades under the new run's name. This effect runs only
    // when the FRAGMENT changes, so a leg switch inside one run clears nothing and w2's
    // D4 stands: the window already drawn for that run stays drawn.
    //
    // The previous run's chart request is invalidated before anything else (CR5). That
    // clears `request`, which cancels the read effect's in-flight read; and because the
    // new request is a different object, any answer that still slips through — the old
    // read's success OR its failure — writes nothing for this run (see `stillWanted`).
    putRequest(null);
    clearDrawing();
    select(fragmentRun);
    setSelectedKey(null);
    setRun(null);
    setLeg("primary");
    setAnchorMs(null);
    setNotice(null);
    setUndrawn(null);
    setGoToNote(null);
    if (fragmentRun === null) {
      const freeRequest = requestFor(null, freeRef.current.pair, freeRef.current.timeframe, null);
      setPhase({ kind: "loading", what: loadingText(freeRequest) });
      putRequest(freeRequest);
      return;
    }
    setPhase({ kind: "loading", what: `Loading backtest run ${fragmentRun}…` });
    let cancelled = false;
    /** Whether this read's answer is still the one to write. `cancelled` covers the
     *  fragment moving on; the selected identity covers the trader choosing a free
     *  pair/timeframe while the read was in flight — that choice wins, so neither this
     *  read's success nor its refusal may write over it. */
    const stale = (): boolean => cancelled || selectedRef.current !== fragmentRun;
    void (async () => {
      try {
        const answer = await commands.getBacktestRun({ runId: fragmentRun });
        if (stale()) return;
        if (answer.status !== "ok") {
          setPhase({ kind: "failed", message: answer.error.message, code: answer.error.code });
          return;
        }
        const context = readRun(answer.data);
        // Open on the leg the rule's own values live on when the run read its higher
        // timeframe, and on the primary otherwise: the overlays exist on one of them.
        const startLeg: Leg = context.usesHtf && hasTwoLegs(context) ? "htf" : "primary";
        setRun(context);
        setLeg(startLeg);
        setAnchorMs(null);
        putRequest(requestFor(context, context.pair, timeframeOf(context, startLeg), null));
      } catch (error) {
        if (stale()) return;
        setPhase({ kind: "failed", message: messageOf(error), code: "transport" });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [fragmentRun]);
}

/** The marks read and the withdraw (r5.s3.w3). */
export function useMarksRead(s: ChartState, markPair: string): (mark: MarkDto) => Promise<void> {
  const { setMarksPhase, marksSeq, setMarkRefusal, setMarksSeq } = s;
  // --- the marks read (r5.s3.w3) --------------------------------------------
  // ONE read for the pair on screen, partitioned below. Re-run whenever the pair
  // changes, and after each write or withdraw (`marksSeq`).

  useEffect(() => {
    let cancelled = false;
    setMarksPhase({ kind: "loading" });
    void (async () => {
      try {
        const answer = await commands.listMarks({ pair: markPair });
        if (cancelled) return;
        if (answer.status !== "ok") {
          setMarksPhase({
            kind: "failed",
            message: answer.error.message,
            code: answer.error.code,
          });
          return;
        }
        setMarksPhase({
          kind: "ready",
          marks: answer.data.marks,
          hidden: answer.data.hiddenByHoldout,
        });
      } catch (error) {
        if (cancelled) return;
        setMarksPhase({ kind: "failed", message: messageOf(error), code: "transport" });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [markPair, marksSeq]);

  /** Withdraw one mark, once. The row leaves the table and the layer together. */
  const withdraw = async (mark: MarkDto): Promise<void> => {
    setMarkRefusal(null);
    try {
      const answer = await commands.withdrawMark({ markId: mark.markId });
      if (answer.status !== "ok") {
        setMarkRefusal(answer.error.message);
        return;
      }
      // The row leaves the table and the layer together, before the re-read answers:
      // a withdrawn mark is not drawn for a moment either (PDR-0011).
      setMarksPhase((current) =>
        current.kind === "ready"
          ? { ...current, marks: current.marks.filter((each) => each.markId !== mark.markId) }
          : current,
      );
      setMarksSeq((seq) => seq + 1);
    } catch (error) {
      setMarkRefusal(messageOf(error));
    }
  };
  return withdraw;
}

/** The read itself. */
export function useChartRead(s: ChartState): void {
  const {
    request,
    setNotice,
    setPhase,
    clearDrawing,
    readGateRef,
    requestRef,
    lastAnswerRef,
    drawnAnchorRef,
    drawnReadRef,
  } = s;
  // --- the read itself ------------------------------------------------------
  useEffect(() => {
    if (request === null) return;
    // A leg over the cap with nothing selected is not read at all: the previous window
    // stays drawn and the notice says what to do about it. Sending a read the server
    // will refuse, only to render the refusal, would be worse than saying so.
    if (request.plan.kind === "needs-trade") {
      setNotice({ timeframe: request.timeframe, bars: request.plan.bars });
      // This leg asks for nothing but a trade, so only that waiting is true of it. A
      // READY drawing of the previous leg stays drawn (D4), and the loading sentence of
      // the read that opened the run stays too (CR6) — but a FAILED read belongs to the
      // leg that asked for it. Left standing, the last leg's error would sit under this
      // leg's notice and read as this leg's failure, though no read was ever sent.
      setPhase((current) => (current.kind === "failed" ? { kind: "idle" } : current));
      return;
    }
    setNotice(null);
    clearDrawing();
    setPhase({ kind: "loading", what: loadingText(request) });
    let cancelled = false;
    // This pane's request id: a later read of the pane makes this answer late, and a late
    // answer is dropped.
    const requestId = readGateRef.current.begin(MAIN_PANE);
    /** Whether an answer that just arrived is still the answer to the request the tab
     *  holds now. It is not, when this request was replaced while the read was in flight
     *  — a leg switch, another trade, a free selection — or when the fragment named
     *  another run, which invalidates the request outright (CR5): an answer asked for
     *  under an identity the tab has left writes nothing, neither a refusal nor a window. */
    const stillWanted = (): boolean =>
      !cancelled &&
      requestRef.current === request &&
      readGateRef.current.accepts(MAIN_PANE, requestId);
    const paneContext: ChartContext = {
      paneId: MAIN_PANE,
      pair: request.pair,
      timeframe: request.timeframe,
      indicators: [],
    };
    void (async () => {
      try {
        const answer = await commands.chartData(
          chartDataRequest(
            paneContext,
            request.plan.kind === "window" ? request.plan.fromMs : null,
            request.plan.kind === "window" ? request.plan.toMs : null,
            request.runId,
          ),
        );
        if (!stillWanted()) return;
        if (answer.status !== "ok") {
          setPhase({ kind: "failed", message: answer.error.message, code: answer.error.code });
          return;
        }
        // The anchor this answer was read FOR, for the draw effect: a bounded read
        // clamped to the run's span edge must still land on the trade the trader
        // picked, and the effect that draws has no other way to know which it was.
        lastAnswerRef.current = { holdoutStartMs: answer.data.holdout?.holdoutStartMs ?? null };
        drawnAnchorRef.current = request.anchorMs;
        drawnReadRef.current = {
          key: readKey(request),
          span: request.plan.kind === "span",
        };
        setPhase({ kind: "ready", dto: answer.data });
      } catch (error) {
        if (!stillWanted()) return;
        setPhase({ kind: "failed", message: messageOf(error), code: "transport" });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [request]);
}

/** Draw what came back: the candles and layers, the selected trade, the marks on the leg. */
export function useChartDraw(s: ChartState, view: ChartView): void {
  const { adapterRef, drawnAnchorRef, drawnRef, selectedKey, marksPhase } = s;
  const { dto, selectedTrade, marksOnLeg, drawnTimeframe } = view;
  // --- draw what came back --------------------------------------------------
  useEffect(() => {
    const adapter = adapterRef.current;
    // An empty window draws nothing: the read that emptied the chart already cleared
    // the previous window, and the state below says the window is empty. There is no
    // layer to draw and none is fabricated (PDR-0011).
    if (adapter === null || dto === null || dto.candles.length === 0) return;
    const overlays = dto.run?.overlays ?? [];
    adapter.setCandles(dto.candles);
    adapter.setOverlays(overlays);
    adapter.setTrades(dto.run?.trades ?? []);
    // A run-less read carries the holdout's bars; a run's read was cut before them (ADR-0030
    // amendment, 2026-11-14).
    adapter.setHoldout(dto.run === null ? (dto.holdout?.holdoutStartMs ?? null) : null);
    // Where the view lands: on the anchor the read was asked around (a bounded read, clamped or not), else
    // the latest bars, canvas v9's default view (#456).
    const anchor = drawnAnchorRef.current;
    if (anchor === null) {
      adapter.showLatest();
    } else {
      adapter.centerOn(anchor);
    }
    drawnRef.current = true;
  }, [dto]);

  useEffect(() => {
    adapterRef.current?.setSelectedTrade(
      selectedTrade,
      dto?.run?.series === "htf" ? "htf" : "primary",
    );
  }, [dto, selectedKey]);

  useEffect(() => {
    adapterRef.current?.setMarks(marksOnLeg);
  }, [marksPhase, drawnTimeframe]);
}
