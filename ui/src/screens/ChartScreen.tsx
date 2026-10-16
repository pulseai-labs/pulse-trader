// The Chart tab (r5.s2.w2) — a backtest drawn on its candles.
//
// The screen reads w1's `chartData` and draws exactly what came back, through the
// adapter that owns the chart library (`ui/src/chart/`; ADR-0030, PDR-0004). It computes
// no indicator, level, pivot, pattern or price of its own (G1): every value on the
// canvas and in the tables below it is the server's, and the only arithmetic here is
// about TIME — which window of bars to ask for, and whether the bar a trade entered on
// is already drawn.
//
// The shape of the screen, in the order it decides things:
//
//  * **No run:** the tab opens on a pair and a timeframe (BTCUSDT 4h) with candles only
//    — the read's run layer is optional, which is what r5.s3 needs too (Q1).
//  * **A run:** `#/chart?run=<id>` names one. The tab reads the RUN first — for its pair,
//    its two timeframes and its span — because a chart read on a timeframe the run did
//    not use is refused, and because the run's own span is what says whether its 15m leg
//    even fits in one read.
//  * **The leg switch** is the run's primary and higher timeframe, nothing else (Q2):
//    each overlay is computed on one of them, server-side, and the tab only draws what
//    the leg it asked for returned.
//  * **A leg over the bar cap** is never sent to be refused: the tab says how many bars
//    it would be and asks for a trade — from the run's OWN stored trades, which the run
//    read already carried, so the ask is one the trader can answer (CR1) — then reads a
//    bounded window around that trade (Q3/G4).
//
// Every state is plain and says what it is (PDR-0011): loading, failed, an empty window,
// no run, a withheld holdout, the operands the rule uses that are not drawn, and a leg
// waiting for a trade to centre on. No zero or placeholder ever stands in for a result.

import { useEffect, useRef, useState } from "react";

import { commands } from "../bindings";
import type {
  BacktestRunDto,
  ChartDataDto,
  ChartTradeDto,
  TradeRowDto,
} from "../bindings";
import type { ChartAdapter } from "../chart/adapter";
import { createChartAdapter } from "../chart/adapter";
import { beforeFence, fenceStartMs } from "../chart/layers";
import type { WindowPlan } from "../chart/window";
import { covers, windowPlan } from "../chart/window";
import { Figure } from "../format/Figure";
import { EM_DASH, price, rMultiple } from "../format/numbers";
import { dateUtc, timeUtc } from "../format/time";
import { chartDeepLink } from "../routes";

/** The pairs the tab opens on (the four coins the server holds). */
const PAIRS: readonly string[] = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "XRPUSDT"];

/** The timeframes the free chart offers. The server holds these three today; an
 *  interval it does not hold is refused by the read and shown as it came. */
const TIMEFRAMES: readonly string[] = ["15m", "4h", "1d"];

const DEFAULT_PAIR = "BTCUSDT";
const DEFAULT_TIMEFRAME = "4h";

/** Which of the run's two series the tab is drawing. */
type Leg = "primary" | "htf";

/** What the tab read about the run itself — the fields a chart needs, and no more. */
interface RunContext {
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
  /** The run's stored trades. The run read is the tab's FIRST answer about the run and
   *  it already carries every trade the run recorded, so a leg over the bar cap has rows
   *  to pick from before any chart read happens (CR1). */
  readonly trades: readonly ChartTradeDto[];
}

/** One thing to ask the read for. */
interface ChartRequest {
  readonly pair: string;
  readonly timeframe: string;
  readonly runId: string | null;
  readonly plan: WindowPlan;
  /** The window's centre, for the sentence shown while the read is in flight. */
  readonly anchorMs: string | null;
}

type Phase =
  | { readonly kind: "loading"; readonly what: string }
  | { readonly kind: "failed"; readonly message: string; readonly code: string }
  | { readonly kind: "ready"; readonly dto: ChartDataDto };

/** A leg the trade is too fine to read whole until a trade is picked. */
interface Notice {
  readonly timeframe: string;
  readonly bars: number;
}

/** The timeframe a leg names, or the primary's when the run recorded only one. */
function timeframeOf(run: RunContext, leg: Leg): string {
  if (leg === "htf" && run.htfTimeframe !== null) return run.htfTimeframe;
  return run.primaryTimeframe;
}

/** Whether the run offers a second leg at all: a run that recorded one timeframe twice
 *  (w1's D6) has one, and the tab shows it as a chip rather than as a switch. */
function hasTwoLegs(run: RunContext): boolean {
  return run.htfTimeframe !== null && run.htfTimeframe !== run.primaryTimeframe;
}

function requestFor(
  run: RunContext | null,
  pair: string,
  timeframe: string,
  anchorMs: string | null,
): ChartRequest {
  const span = run === null ? null : { firstMs: run.firstOpenTimeMs, lastMs: run.lastCloseTimeMs };
  return {
    pair,
    timeframe,
    runId: run?.runId ?? null,
    plan: windowPlan(span, timeframe, anchorMs),
    anchorMs,
  };
}

/** What the tab says it is reading, in one sentence. */
function loadingText(request: ChartRequest): string {
  if (request.plan.kind === "window" && request.anchorMs !== null) {
    return `Loading ${request.timeframe} around the entry at ${timeUtc(request.anchorMs)}…`;
  }
  return `Loading ${request.pair} ${request.timeframe}…`;
}

export default function ChartScreen() {
  const containerRef = useRef<HTMLDivElement | null>(null);
  const adapterRef = useRef<ChartAdapter | null>(null);
  /** Whether anything is currently drawn, so a new read clears it first: the chart
   *  never shows one window's candles under the next window's label. */
  const drawnRef = useRef(false);

  const [phase, setPhase] = useState<Phase>({ kind: "loading", what: loadingText({
    pair: DEFAULT_PAIR,
    timeframe: DEFAULT_TIMEFRAME,
    runId: null,
    plan: { kind: "span" },
    anchorMs: null,
  }) });
  const [notice, setNotice] = useState<Notice | null>(null);
  const [run, setRun] = useState<RunContext | null>(null);
  const [leg, setLeg] = useState<Leg>("primary");
  const [free, setFree] = useState({ pair: DEFAULT_PAIR, timeframe: DEFAULT_TIMEFRAME });
  const [anchorMs, setAnchorMs] = useState<string | null>(null);
  const [request, setRequest] = useState<ChartRequest | null>(null);
  const [fragmentRun, setFragmentRun] = useState<string | null>(
    () => chartDeepLink(window.location.hash)?.run ?? null,
  );
  /** The identity this tab is DRAWING: the fragment's run, or null once a free
   *  pair/timeframe has been chosen. The fragment says where the tab ENTERED; an
   *  explicit choice made in the session wins over it, and both the run read's
   *  continuation and the drawn-answer gate are compared against this. */
  const [selected, setSelected] = useState<string | null>(fragmentRun);
  const [barsOpen, setBarsOpen] = useState(false);

  // The free selection, readable from the fragment effect without re-running it.
  const freeRef = useRef(free);

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

  /** The selected identity as an async continuation reads it: `selected` is state, and
   *  a continuation that resolves before the next render must still see the choice the
   *  trader made AFTER it started. Every path that selects writes both. */
  const selectedRef = useRef<string | null>(fragmentRun);
  const select = (next: string | null): void => {
    selectedRef.current = next;
    setSelected(next);
  };

  /** The anchor the READY answer was read for (`null` for a span read). The draw
   *  effect centres on it instead of fitting the whole window, so a bounded read the
   *  plan clamped into the run's span still lands on the trader's trade. */
  const drawnAnchorRef = useRef<string | null>(null);

  /** Take whatever is drawn off the canvas, and remember that nothing is. Nothing is
   *  put back until the next read's own layers arrive: an empty chart is not a stale one
   *  (PDR-0011). Called by any read that replaces the window, and by a run switch (CR3). */
  const clearDrawing = (): void => {
    const adapter = adapterRef.current;
    if (adapter !== null && drawnRef.current) {
      adapter.setCandles([]);
      adapter.setOverlays([]);
      adapter.setTrades([]);
    }
    drawnRef.current = false;
  };

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
    setRun(null);
    setLeg("primary");
    setAnchorMs(null);
    setNotice(null);
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

  // --- the read itself ------------------------------------------------------
  useEffect(() => {
    if (request === null) return;
    // A leg over the cap with nothing selected is not read at all: the previous window
    // stays drawn and the notice says what to do about it. Sending a read the server
    // will refuse, only to render the refusal, would be worse than saying so.
    if (request.plan.kind === "needs-trade") {
      setNotice({ timeframe: request.timeframe, bars: request.plan.bars });
      return;
    }
    setNotice(null);
    clearDrawing();
    setPhase({ kind: "loading", what: loadingText(request) });
    let cancelled = false;
    /** Whether an answer that just arrived is still the answer to the request the tab
     *  holds now. It is not, when this request was replaced while the read was in flight
     *  — a leg switch, another trade, a free selection — or when the fragment named
     *  another run, which invalidates the request outright (CR5): an answer asked for
     *  under an identity the tab has left writes nothing, neither a refusal nor a window. */
    const stillWanted = (): boolean => !cancelled && requestRef.current === request;
    void (async () => {
      try {
        const answer = await commands.chartData({
          pair: request.pair,
          timeframe: request.timeframe,
          fromMs: request.plan.kind === "window" ? request.plan.fromMs : null,
          toMs: request.plan.kind === "window" ? request.plan.toMs : null,
          runId: request.runId,
        });
        if (!stillWanted()) return;
        if (answer.status !== "ok") {
          setPhase({ kind: "failed", message: answer.error.message, code: answer.error.code });
          return;
        }
        // The anchor this answer was read FOR, for the draw effect: a bounded read
        // clamped to the run's span edge must still land on the trade the trader
        // picked, and the effect that draws has no other way to know which it was.
        drawnAnchorRef.current = request.anchorMs;
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

  // --- draw what came back --------------------------------------------------
  // Only what the tab has SELECTED is this tab's to draw: an answer read for another
  // run — or a run's answer that outlived a free choice — is not drawn, not listed and
  // not centred on (CR3). The gate compares the DTO against the selection, never
  // against the raw fragment: a free read's answer carries no run layer, and the
  // fragment may still name the run the trader deep-linked away from.
  const dto =
    phase.kind === "ready" && (phase.dto.run?.runId ?? null) === selected ? phase.dto : null;
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
    // Where the view lands: on the anchor the read was asked around (a bounded read,
    // clamped or not), else the whole window.
    const anchor = drawnAnchorRef.current;
    if (anchor === null) {
      adapter.fitAll();
    } else {
      adapter.centerOn(anchor);
    }
    drawnRef.current = true;
  }, [dto]);

  // --- handlers -------------------------------------------------------------
  // What is drawn, and what the trader has asked for; the handlers below are the only
  // readers of both.
  const drawnTimeframe = dto?.timeframe ?? null;
  const timeframe = run === null ? free.timeframe : timeframeOf(run, leg);
  const selectFree = (pair: string, timeframe: string) => {
    freeRef.current = { pair, timeframe };
    setFree(freeRef.current);
    // An explicit free choice is the tab's selection now, whatever the fragment still
    // names: the in-flight run read must not take it back (and the drawn-answer gate
    // must let the free read through).
    select(null);
    setRun(null);
    setAnchorMs(null);
    putRequest(requestFor(null, pair, timeframe, null));
  };

  const selectLeg = (next: Leg) => {
    setLeg(next);
    putRequest(requestFor(run, run?.pair ?? free.pair, next === "htf" && run !== null
      ? timeframeOf(run, "htf")
      : run === null
        ? free.timeframe
        : run.primaryTimeframe, anchorMs));
  };

  /** A trade row: centre the chart on its bar, or read a window around it. Two things
   *  send the tab back to the read — the leg on screen is not the leg the trader asked
   *  for, or the bar this trade entered on is not in the window being drawn. A read
   *  returns trades that entered before its window and exited inside it, and scrolling
   *  to one of those bars would scroll to nothing. */
  const selectTrade = (trade: ChartTradeDto) => {
    setAnchorMs(trade.entryTime);
    if (drawnTimeframe !== timeframe || !covers(dto?.candles ?? [], trade.entryTime)) {
      putRequest(requestFor(
        run,
        run?.pair ?? free.pair,
        timeframe,
        trade.entryTime,
      ));
      return;
    }
    adapterRef.current?.centerOn(trade.entryTime);
  };

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
   *  there, and no fence reads as "this run traded nothing"). */
  const fencedOut = trades.length === 0 && (run?.trades.length ?? 0) > 0;
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
  const series = overlays.filter((overlay) => overlay.kind === "level" || overlay.kind === "line");

  return (
    <section className="cw" aria-label="Chart">
      <header className="cw-tool">
        {run === null ? (
          <>
            <span className="cw-tool-lab">Pair</span>
            <div className="seg">
              {PAIRS.map((pair) => (
                <button
                  key={pair}
                  type="button"
                  className={`seg-btn mono ${free.pair === pair ? "is-active" : ""}`}
                  onClick={() => selectFree(pair, free.timeframe)}
                >
                  {pair}
                </button>
              ))}
            </div>
            <span className="cw-tool-lab">Timeframe</span>
            <div className="seg">
              {TIMEFRAMES.map((tf) => (
                <button
                  key={tf}
                  type="button"
                  className={`seg-btn mono ${free.timeframe === tf ? "is-active" : ""}`}
                  onClick={() => selectFree(free.pair, tf)}
                >
                  {tf}
                </button>
              ))}
            </div>
          </>
        ) : (
          <>
            <span className="cw-tool-lab">Pair</span>
            <span className="mono">{run.pair}</span>
            <span className="cw-tool-lab">
              {hasTwoLegs(run) ? "Timeframe" : "Primary timeframe"}
            </span>
            {hasTwoLegs(run) ? (
              <div className="seg">
                {(["primary", "htf"] as const).map((which) => (
                  <button
                    key={which}
                    type="button"
                    className={`seg-btn mono ${leg === which ? "is-active" : ""}`}
                    title={which === "primary" ? "Primary timeframe" : "Higher timeframe"}
                    onClick={() => selectLeg(which)}
                  >
                    {timeframeOf(run, which)}
                  </button>
                ))}
              </div>
            ) : (
              <span className="mono">{run.primaryTimeframe}</span>
            )}
          </>
        )}
        {/* What the chart is bound to, named by what is DRAWN: a leg the tab has not
            read yet (it is waiting for a trade) is not what the trader is looking at. */}
        <span className="cw-ctx">
          {run === null
            ? "No run"
            : `Backtest run ${run.runId} · ${drawnTimeframe ?? timeframe}`}
        </span>
      </header>

      <div className="cw-canvas" ref={containerRef} />

      {phase.kind === "loading" && (
        <p className="cw-msg" role="status">
          {phase.what}
        </p>
      )}
      {phase.kind === "failed" && (
        <p className="cw-msg" role="alert">
          {phase.message} ({phase.code})
        </p>
      )}
      {phase.kind === "ready" && dto !== null && dto.candles.length === 0 && (
        <p className="cw-msg" role="status">
          {`No ${dto.timeframe} bars for ${dto.pair} in this window.`}
        </p>
      )}

      {notice !== null && (
        <p className="cw-msg" role="status">
          {`The whole run at ${notice.timeframe} is ${notice.bars.toLocaleString("en-US")} bars — more than one read (the cap is 20,000). Pick a trade to load ${notice.timeframe} around it.`}
        </p>
      )}

      <div className="cw-notes">
        {dto?.holdout != null && (
          <p className="cw-note" role="note">
            {`Holdout from ${dateUtc(dto.holdout.holdoutStartMs)} — ${dto.holdout.barsWithheld} bars withheld`}
          </p>
        )}
        {dto?.run != null && dto.run.notDrawn.length > 0 && (
          <p className="cw-note" role="note">
            {`Not drawn on the chart: ${dto.run.notDrawn.join(", ")}`}
          </p>
        )}
        {dto?.run != null && dto.run.overlays.length === 0 && dto.run.notDrawn.length === 0 && (
          <p className="cw-note" role="note">
            {`This rule uses no S/R, pivot, candle-pattern or EMA operand on ${dto.timeframe}.`}
          </p>
        )}
      </div>

      <section className="cw-strip" aria-label="Trades">
        {run === null ? (
          <p className="cw-note">No run is open, so there are no trades to draw.</p>
        ) : (
          <div className="cw-scroll">
            <table className="cw-tbl cw-trades">
              {/* Named by what the rows ARE: the run's own stored trades, always — a
                  bounded window narrows what is DRAWN, never which trades the trader
                  may pick next (the close review's C). */}
              <caption>Trades in the run</caption>
              <thead>
                <tr>
                  <th scope="col">#</th>
                  <th scope="col">side</th>
                  <th scope="col">entry</th>
                  <th scope="col">price</th>
                  <th scope="col">exit</th>
                  <th scope="col">price</th>
                  <th scope="col">reason</th>
                  <th scope="col">R</th>
                </tr>
              </thead>
              <tbody>
                {trades.map((trade, index) => (
                  <tr key={`${trade.entryTime}-${trade.side}-${index}`} data-side={trade.side}>
                    <th scope="row">
                      <button
                        type="button"
                        className="cw-row-btn mono"
                        aria-label={`Trade ${index + 1}, ${trade.side}, entry ${timeUtc(trade.entryTime)}`}
                        onClick={() => selectTrade(trade)}
                      >
                        {index + 1}
                      </button>
                    </th>
                    <td>{trade.side}</td>
                    <td className="mono">{timeUtc(trade.entryTime)}</td>
                    <td className="mono">
                      <Figure exact={trade.entryPrice}>{price(trade.entryPrice)}</Figure>
                    </td>
                    <td className="mono">{timeUtc(trade.exitTime)}</td>
                    <td className="mono">
                      <Figure exact={trade.exitPrice}>{price(trade.exitPrice)}</Figure>
                    </td>
                    <td>{trade.exitReason ?? EM_DASH}</td>
                    <td className="mono">
                      <Figure exact={trade.realizedR}>{rMultiple(trade.realizedR)}</Figure>
                    </td>
                  </tr>
                ))}
                {trades.length === 0 && (
                  <tr>
                    <td colSpan={8}>
                      {fencedOut
                        ? "This run's stored trades reach the holdout and are withheld."
                        : windowTrades === null
                          ? "The run recorded no trades."
                          : "No trades fall in this window."}
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
        )}
        {/* What the rows are, and how the drawn window narrows them: the table lists the
            run's trades, and this says how many of them the chart is drawing. */}
        {windowTrades !== null && (
          <p className="cw-note" role="note">
            {`The drawn window holds ${windowTrades.length} of these trades.`}
          </p>
        )}
      </section>

      {/* The table copy: no value may be reachable only through the canvas (the
          `ChartTwins` precedent). */}
      {dto?.run != null && (
        <section className="cw-data" aria-label="Chart data">
          {events.length > 0 && (
            <div className="cw-scroll">
              <table className="cw-tbl">
                <caption>Structure and patterns</caption>
                <thead>
                  <tr>
                    <th scope="col">layer</th>
                    <th scope="col">event</th>
                    <th scope="col">bar</th>
                    <th scope="col">value</th>
                  </tr>
                </thead>
                <tbody>
                  {events.map((row, index) => (
                    <tr key={`${row.layer}-${row.event}-${row.bar}-${index}`}>
                      <td className="mono">{row.layer}</td>
                      <td>{row.event}</td>
                      <td className="mono">{timeUtc(row.bar)}</td>
                      <td className="mono">
                        <Figure exact={row.value}>
                          {row.asPrice ? price(row.value) : row.value}
                        </Figure>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}

          {series.length > 0 && (
            <details className="cw-bars" open={barsOpen}>
              <summary
                onClick={(event) => {
                  event.preventDefault();
                  setBarsOpen((open) => !open);
                }}
              >
                {`Bars, levels and lines — ${dto.candles.length} bars × ${series.length} layers`}
              </summary>
              {barsOpen && (
                <div className="cw-scroll">
                  <table className="cw-tbl">
                    <caption>Bars, levels and lines</caption>
                    <thead>
                      <tr>
                        <th scope="col">bar</th>
                        {series.map((overlay) => (
                          <th key={overlay.label} scope="col">
                            {overlay.label}
                          </th>
                        ))}
                      </tr>
                    </thead>
                    <tbody>
                      {dto.candles.map((candle, index) => (
                        <tr key={candle.t}>
                          <td className="mono">{timeUtc(candle.t)}</td>
                          {series.map((overlay) => {
                            const value = overlay.values[index];
                            return (
                              <td key={overlay.label} className="mono">
                                {value == null ? (
                                  EM_DASH
                                ) : (
                                  <Figure exact={value}>{price(value)}</Figure>
                                )}
                              </td>
                            );
                          })}
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
            </details>
          )}
        </section>
      )}
    </section>
  );
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
  }));
}

/** One trade's identity across the two reads that can carry it — the run's stored rows
 *  and the drawn window's rows — so the union of the two lists never doubles a trade. */
function tradeKey(trade: ChartTradeDto): string {
  return `${trade.entryTime}-${trade.side}-${trade.exitTime ?? ""}`;
}

/** The run's own facts a chart needs, read off the run DTO. */
function readRun(dto: BacktestRunDto): RunContext {
  return {
    runId: dto.runId,
    pair: dto.pair,
    primaryTimeframe: dto.primaryTimeframe,
    htfTimeframe: dto.htfTimeframe,
    usesHtf: dto.usesHtf,
    firstOpenTimeMs: dto.firstOpenTimeMs,
    lastCloseTimeMs: dto.lastCloseTimeMs,
    holdoutStartMs: dto.holdoutStartMs,
    trades: toChartTrades(dto.trades),
  };
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
