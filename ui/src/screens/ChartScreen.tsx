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
  const [barsOpen, setBarsOpen] = useState(false);

  // The free selection, readable from the fragment effect without re-running it.
  const freeRef = useRef(free);

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
    if (fragmentRun === null) {
      setRun(null);
      setLeg("primary");
      setAnchorMs(null);
      setRequest(requestFor(null, freeRef.current.pair, freeRef.current.timeframe, null));
      return;
    }
    let cancelled = false;
    void (async () => {
      try {
        const answer = await commands.getBacktestRun({ runId: fragmentRun });
        if (cancelled) return;
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
        setRequest(requestFor(context, context.pair, timeframeOf(context, startLeg), null));
      } catch (error) {
        if (cancelled) return;
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
    const adapter = adapterRef.current;
    // A leg over the cap with nothing selected is not read at all: the previous window
    // stays drawn and the notice says what to do about it. Sending a read the server
    // will refuse, only to render the refusal, would be worse than saying so.
    if (request.plan.kind === "needs-trade") {
      setNotice({ timeframe: request.timeframe, bars: request.plan.bars });
      return;
    }
    setNotice(null);
    if (drawnRef.current && adapter !== null) {
      adapter.setCandles([]);
      adapter.setOverlays([]);
      adapter.setTrades([]);
      drawnRef.current = false;
    }
    setPhase({ kind: "loading", what: loadingText(request) });
    let cancelled = false;
    void (async () => {
      try {
        const answer = await commands.chartData({
          pair: request.pair,
          timeframe: request.timeframe,
          fromMs: request.plan.kind === "window" ? request.plan.fromMs : null,
          toMs: request.plan.kind === "window" ? request.plan.toMs : null,
          runId: request.runId,
        });
        if (cancelled) return;
        if (answer.status !== "ok") {
          setPhase({ kind: "failed", message: answer.error.message, code: answer.error.code });
          return;
        }
        setPhase({ kind: "ready", dto: answer.data });
      } catch (error) {
        if (cancelled) return;
        setPhase({ kind: "failed", message: messageOf(error), code: "transport" });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [request]);

  // --- draw what came back --------------------------------------------------
  const dto = phase.kind === "ready" ? phase.dto : null;
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
    adapter.fitAll();
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
    setRun(null);
    setAnchorMs(null);
    setRequest(requestFor(null, pair, timeframe, null));
  };

  const selectLeg = (next: Leg) => {
    setLeg(next);
    setRequest(requestFor(run, run?.pair ?? free.pair, next === "htf" && run !== null
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
      setRequest(requestFor(
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
  // The rows the trader picks a window from: the DRAWN window's trades when one is
  // drawn, and the run's own stored trades otherwise — a leg waiting for a trade, a
  // read in flight, a failed read (CR1). Both are the server's own records; nothing is
  // derived here. The caption says which of the two the table is showing.
  const windowTrades = dto?.run != null ? dto.run.trades : null;
  const trades = windowTrades ?? run?.trades ?? [];
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
              {/* Named by what the rows ARE: a window's trades once one is drawn, the
                  run's own stored trades while the tab has no window to show (CR1). */}
              <caption>
                {windowTrades === null ? "Trades in the run" : "Trades in the window"}
              </caption>
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
                      {windowTrades === null
                        ? "The run recorded no trades."
                        : "No trades fall in this window."}
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
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
    trades: toChartTrades(dto.trades),
  };
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
