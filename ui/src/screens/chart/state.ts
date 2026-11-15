// The Chart tab's state (r7.s2.w2, #502 C5): every useState and useRef the screen held,
// in three hooks the screen spreads into one `ChartState`. Each declaration keeps the doc comment it carried.

import { useRef, useState } from "react";

import type { ChartAdapter } from "../../chart/adapter";
import type { PaneReadGate } from "../../chart/context";
import { createPaneReadGate } from "../../chart/context";
import { chartDeepLink } from "../../routes";
import type {
  ChartRequest,
  FocusBar,
  Leg,
  MarkForm,
  MarksPhase,
  Notice,
  Phase,
  RailTab,
  RunContext,
} from "./model";
import { DEFAULT_PAIR, DEFAULT_TIMEFRAME, DISARMED, loadingText } from "./model";

/** The read's state: the chart's handles, the phase, the request, and what the drawn answer answered. */
export function useReadState() {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const adapterRef = useRef<ChartAdapter | null>(null);
  /** The pane's read ids (ADR-0030 amendment, 2026-11-14). */
  const readGateRef = useRef<PaneReadGate>(createPaneReadGate());
  /** Whether anything is currently drawn, so a new read clears it first: the chart
   *  never shows one window's candles under the next window's label. */
  const drawnRef = useRef(false);

  const [phase, setPhase] = useState<Phase>({ kind: "loading", what: loadingText({
    pair: DEFAULT_PAIR,
    timeframe: DEFAULT_TIMEFRAME,
    runId: null,
    plan: { kind: "span" },
    anchorMs: null,
    anchorWhat: "entry",
  }) });
  const [notice, setNotice] = useState<Notice | null>(null);

  const [request, setRequest] = useState<ChartRequest | null>(null);

  /** The request the tab last asked for, kept in step with the state SYNCHRONOUSLY. An
   *  answer that arrives after its own request was replaced must write nothing, and a
   *  render that has not happened yet cannot be relied on to say so — so every path that
   *  sets the request goes through here, and the read effect's answers compare
   *  themselves with this ref (CR5). */
  const requestRef = useRef<ChartRequest | null>(null);
  const putRequest = (next: ChartRequest | null): void => {
    requestRef.current = next;
    setRequest(next);
  };

  /** The anchor the READY answer was read for (`null` for a span read). The draw
   *  effect centres on it instead of fitting the whole window, so a bounded read the
   *  plan clamped into the run's span still lands on the trader's trade. */
  const drawnAnchorRef = useRef<string | null>(null);

  /** The read the drawn answer answers: the question it asked, and whether it asked for
   *  the run's own span with no bounds. A span read is the widest window this leg can
   *  make, so a bar it does not hold cannot be reached by any narrower one — the read's
   *  own draw edge trims the same trailing higher-timeframe candle however the window is
   *  asked; and a question already answered cannot answer differently. Either makes a
   *  click's read futile, and the tab names the bar instead of reloading for it. */
  const drawnReadRef = useRef<{ key: string; span: boolean } | null>(null);

  /** The holdout start the last successful chart answer carried (`null`: no freeze was
   *  open at that read). Set on every successful answer and never reset: a free choice,
   *  a leg switch or a run switch leaves it, because it is the freshest word the tab
   *  has on the fence. `null` as the ref's value means no answer has landed yet. */
  const lastAnswerRef = useRef<{ holdoutStartMs: string | null } | null>(null);

  /** Take whatever is drawn off the canvas, and remember that nothing is. Nothing is
   *  put back until the next read's own layers arrive: an empty chart is not a stale one
   *  (PDR-0011). Called by any read that replaces the window, and by a run switch (CR3). */
  const clearDrawing = (): void => {
    const adapter = adapterRef.current;
    if (adapter !== null && drawnRef.current) {
      adapter.setCandles([]);
      adapter.setOverlays([]);
      adapter.setTrades([]);
      adapter.setMarks([]);
      adapter.setSelectedTrade(null, "primary");
      adapter.setHoldout(null);
    }
    drawnRef.current = false;
    drawnReadRef.current = null;
  };
  return {
    containerRef,
    adapterRef,
    readGateRef,
    drawnRef,
    phase,
    setPhase,
    notice,
    setNotice,
    request,
    requestRef,
    putRequest,
    drawnAnchorRef,
    drawnReadRef,
    lastAnswerRef,
    clearDrawing,
  };
}

/** What the tab is drawing and what the trader chose: the run, the leg, the free pair and timeframe, the
 *  fragment, the selection, and the go-to field. */
export function useSelectionState() {
  const [run, setRun] = useState<RunContext | null>(null);
  const [leg, setLeg] = useState<Leg>("primary");
  const [free, setFree] = useState({ pair: DEFAULT_PAIR, timeframe: DEFAULT_TIMEFRAME });
  const [anchorMs, setAnchorMs] = useState<string | null>(null);
  /** What `anchorMs` is: a trade's entry or a date the trader typed, so a later leg switch's loading sentence names
   *  it rightly (#502 C9, r7.s2.w3). */
  const [anchorWhat, setAnchorWhat] = useState<"entry" | "date">("entry");
  /** The bar the trader clicked with the mark control disarmed, with the pair and timeframe it was clicked on:
   *  the Inspector shows it while that chart is drawn (canvas v9 `focusBar`, r7.s2.w3; CR1). */
  const [focusBar, setFocusBar] = useState<FocusBar | null>(null);

  const [fragmentRun, setFragmentRun] = useState<string | null>(
    () => chartDeepLink(window.location.hash)?.run ?? null,
  );

  /** The identity this tab is DRAWING: the fragment's run, or null once a free
   *  pair/timeframe has been chosen. The fragment says where the tab ENTERED; an
   *  explicit choice made in the session wins over it, and both the run read's
   *  continuation and the drawn-answer gate are compared against this. */
  const [selected, setSelected] = useState<string | null>(fragmentRun);
  const [barsOpen, setBarsOpen] = useState(false);
  /** The trade the trader picked, by `tradeKey`. The trade itself is found in the drawn
   *  window's own rows (`dto.run.trades`), which carry the setup bars; the run read's rows
   *  do not (r6.s4.w4). */
  const [selectedKey, setSelectedKey] = useState<string | null>(null);

  // The free selection, readable from the fragment effect without re-running it.
  const freeRef = useRef(free);

  /** The selected identity as an async continuation reads it: `selected` is state, and
   *  a continuation that resolves before the next render must still see the choice the
   *  trader made AFTER it started. Every path that selects writes both. */
  const selectedRef = useRef<string | null>(fragmentRun);
  const select = (next: string | null): void => {
    selectedRef.current = next;
    setSelected(next);
  };

  /** The entry time of a trade the trader clicked whose bar no read of this leg can
   *  draw. Named where the notes are: a click that cannot draw a bar must not read for
   *  it in silence. */
  const [undrawn, setUndrawn] = useState<string | null>(null);

  // --- go to a date (r6.s4.w3) ----------------------------------------------
  /** What the Go to (UTC) field holds, and the sentence a refused date leaves. */
  const [goToText, setGoToText] = useState("");
  const [goToNote, setGoToNote] = useState<string | null>(null);
  return {
    run,
    setRun,
    leg,
    setLeg,
    free,
    setFree,
    freeRef,
    anchorMs,
    setAnchorMs,
    fragmentRun,
    setFragmentRun,
    selected,
    selectedRef,
    select,
    barsOpen,
    setBarsOpen,
    selectedKey,
    setSelectedKey,
    undrawn,
    setUndrawn,
    goToText,
    setGoToText,
    goToNote,
    setGoToNote,
    anchorWhat,
    setAnchorWhat,
    focusBar,
    setFocusBar,
  };
}

/** The example marks' state: the read, the form, the refusal, and the refs the once-subscribed click
 *  handler reads. */
export function useMarkState() {
  const [marksPhase, setMarksPhase] = useState<MarksPhase>({ kind: "loading" });
  const [form, setForm] = useState<MarkForm>(DISARMED);
  /** The mark controls' refusal, and its own dismissing: a write's refusal and a
   *  withdraw's share this one slot, because both are answers to the marks the tab is
   *  showing and both are read in the same place. */
  const [markRefusal, setMarkRefusal] = useState<string | null>(null);

  /** The mark controls' live state, as the click handler reads it. The handler is
   *  subscribed ONCE and must see the state the trader is in NOW — a click landing
   *  after an arm must write, and one landing after a disarm must not — so it reads
   *  this ref rather than the closure that registered it. */
  const markClickRef = useRef<{ form: MarkForm; pair: string; timeframe: string | null }>({
    form: DISARMED,
    pair: DEFAULT_PAIR,
    timeframe: null,
  });

  /** The open freeze's holdout start as the drawn read saw it, for the holdout refusal's
   *  sentence. Read through a ref because a write's continuation reads it after the render
   *  that started the write, and `null` is a real answer (no freeze was open), never a
   *  reason to invent a date. */
  const liveHoldoutRef = useRef<string | null>(null);

  /** Whether a mark write is in flight, set SYNCHRONOUSLY in the click handler and
   *  cleared when the write settles. `form.submitting` cannot serve this: it is state,
   *  so two clicks in one render pass both observe the old `false` and both write. */
  const markInFlightRef = useRef<boolean>(false);

  /** The marks whose withdraw is in flight (#439, r7.s2.w3): a second press before the first settles sends
   *  nothing. */
  const withdrawInFlightRef = useRef<Set<string>>(new Set());

  const [marksSeq, setMarksSeq] = useState(0);
  return {
    marksPhase,
    setMarksPhase,
    marksSeq,
    setMarksSeq,
    form,
    setForm,
    markRefusal,
    setMarkRefusal,
    markClickRef,
    liveHoldoutRef,
    markInFlightRef,
    withdrawInFlightRef,
  };
}

/** The right rail's state (r7.s2.w3, canvas v9 `rail` and `railOpen`): the tab shown and whether the rail is open.
 *  It opens on Layers, as v9 does. */
export function useRailState() {
  const [rail, setRail] = useState<RailTab>("layers");
  const [railOpen, setRailOpen] = useState(true);
  return { rail, setRail, railOpen, setRailOpen };
}

/** Everything the screen's hooks, handlers and components read. */
export type ChartState = ReturnType<typeof useReadState> &
  ReturnType<typeof useSelectionState> &
  ReturnType<typeof useMarkState> &
  ReturnType<typeof useRailState>;
