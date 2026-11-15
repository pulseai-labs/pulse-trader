// What sits below the Chart tab's canvas (r7.s2.w2): the notes, the selected trade, the run's
// trades, the example marks, the structure and bar tables, and the candle table (#420). Every value drawn on the
// canvas is here as text (PDR-0011). r7.s2.w3 turns the trades into canvas v9's trade strip.

import { useState } from "react";

import type { ChartDataDto, ChartTradeDto, MarkDto } from "../../bindings";
import { Figure } from "../../format/Figure";
import { EM_DASH, price, qty, rMultiple } from "../../format/numbers";
import { dateUtc, timeUtc } from "../../format/time";
import { hasTwoLegs, otherTimeframeNote, writerText } from "./model";
import type { ChartState } from "./state";
import type { ChartView } from "./view";

/** The notes under the canvas: the holdout, what is not drawn, an undrawn trade, the other timeframes' marks. */
export function ChartNotes({ s, view }: { s: ChartState; view: ChartView }) {
  const { undrawn, marksPhase } = s;
  const { dto, timeframe, drawnTimeframe, holdoutTag } = view;
  return (
    <div className="cw-notes">
      {dto?.holdout != null && holdoutTag === null && (
        <p className="cw-note" role="note">
          {dto.run === null
            ? `Holdout — from ${dateUtc(dto.holdout.holdoutStartMs)} · agents can't test here`
            : `Holdout from ${dateUtc(dto.holdout.holdoutStartMs)} — ${dto.holdout.barsWithheld} bars withheld`}
        </p>
      )}
      {dto?.run != null && dto.run.notDrawn.length > 0 && (
        <p className="cw-note" role="note">
          {`Not drawn on the chart: ${dto.run.notDrawn.join(", ")}`}
        </p>
      )}
      {dto?.run != null && dto.run.overlays.length === 0 && dto.run.notDrawn.length === 0 && (
        <p className="cw-note" role="note">
          {`This rule uses no S/R, pivot, candle-pattern, EMA, SMA or WaveTrend operand on ${dto.timeframe}.`}
        </p>
      )}
      {undrawn !== null && (
        <p className="cw-note" role="note">
          {`The trade's entry is not drawn in this ${timeframe} window.`}
        </p>
      )}
      {/* The marks on the OTHER timeframes are named here: they are stored and
          readable, but this chart's leg cannot draw them (PDR-0011). */}
      {marksPhase.kind === "ready" && drawnTimeframe !== null && (
        <OtherTimeframeNote marks={marksPhase.marks} drawn={drawnTimeframe} />
      )}
      {marksPhase.kind === "ready" && marksPhase.hidden !== null && (
        <p className="cw-note" role="note">
          {`${marksPhase.hidden.count} ${marksPhase.hidden.count === 1 ? "mark" : "marks"} hidden: inside the holdout`}
        </p>
      )}
    </div>
  );
}

/** Says plainly how many marks live on a timeframe this chart is not drawing (r5.s3.w3).
 *  Nothing when there are none: a note that says "0 marks elsewhere" is a line the
 *  trader learns to skip past, and PDR-0011 asks for what IS there, not for a
 *  placeholder. */
function OtherTimeframeNote({
  marks,
  drawn,
}: {
  readonly marks: readonly MarkDto[];
  readonly drawn: string;
}) {
  const note = otherTimeframeNote(marks, drawn);
  if (note === "") return null;
  return (
    <p className="cw-note" role="note">
      {note}
    </p>
  );
}

/** The selected trade's setup and stop, as a table. */
export function SelectedTradeTable({ s, view }: { s: ChartState; view: ChartView }) {
  const { run, selectedKey } = s;
  const { selectedTrade, drawnTimeframe, timeframe } = view;
  // The selected trade's setup and stop, as a table: the arm, dip, trigger and stop are
  // drawn on the canvas, and none is reachable only through it (PDR-0011).
  if (run === null || selectedKey === null) return null;
  return (
    <section className="cw-strip" aria-label="Selected trade">
      {selectedTrade === null ? (
        <p className="cw-note" role="note">
          {`The selected trade is not in the drawn ${drawnTimeframe ?? timeframe} window.`}
        </p>
      ) : (
        <>
          <div className="cw-scroll">
            <table className="cw-tbl cw-setup">
              <caption>Selected trade — setup and stop</caption>
              <thead>
                <tr>
                  <th scope="col">what</th>
                  <th scope="col">bar (open) / price</th>
                </tr>
              </thead>
              <tbody>
                <tr>
                  <th scope="row">{`arm (${run.primaryTimeframe})`}</th>
                  <td className="mono">{timeUtc(selectedTrade.armBarTime)}</td>
                </tr>
                {hasTwoLegs(run) && (
                  <tr>
                    <th scope="row">{`arm (${run.htfTimeframe})`}</th>
                    <td className="mono">{timeUtc(selectedTrade.htfArmBarTime)}</td>
                  </tr>
                )}
                <tr>
                  <th scope="row">dip</th>
                  <td className="mono">{timeUtc(selectedTrade.dipBarTime)}</td>
                </tr>
                <tr>
                  <th scope="row">trigger</th>
                  <td className="mono">{timeUtc(selectedTrade.triggerBarTime)}</td>
                </tr>
                <tr>
                  <th scope="row">stop</th>
                  <td className="mono">
                    <Figure exact={selectedTrade.stopPrice}>{price(selectedTrade.stopPrice)}</Figure>
                  </td>
                </tr>
              </tbody>
            </table>
          </div>
          {selectedTrade.armBarTime === null &&
            selectedTrade.htfArmBarTime === null &&
            selectedTrade.dipBarTime === null &&
            selectedTrade.triggerBarTime === null && (
              <p className="cw-note" role="note">
                This trade records no setup.
              </p>
            )}
        </>
      )}
    </section>
  );
}

/** The run's trades, one row each. */
export function TradesTable({
  s,
  view,
  onSelectTrade,
}: {
  s: ChartState;
  view: ChartView;
  onSelectTrade: (trade: ChartTradeDto) => void;
}) {
  const { run } = s;
  const { trades, fencedOut, windowTrades } = view;
  return (
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
                      onClick={() => onSelectTrade(trade)}
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
  );
}

/** The example marks' accessible copy, and a withdraw per row. */
export function MarksTable({
  s,
  view,
  onWithdraw,
}: {
  s: ChartState;
  view: ChartView;
  onWithdraw: (mark: MarkDto) => Promise<void>;
}) {
  const { marksPhase, adapterRef } = s;
  const { markPair, drawnTimeframe, marksOnLeg } = view;
  // The example-trade marks' accessible copy (PDR-0011): every stored field, and a
  // withdraw per row. No mark is reachable only through the canvas.
  return (
    <section className="cw-strip" aria-label="Example marks">
      {marksPhase.kind === "loading" && (
        <p className="cw-msg" role="status">
          Loading example marks…
        </p>
      )}
      {marksPhase.kind === "failed" && (
        <p className="cw-msg" role="alert">
          {marksPhase.message} ({marksPhase.code})
        </p>
      )}
      {marksPhase.kind === "ready" && (
        <>
          <div className="cw-scroll">
            <table className="cw-tbl cw-marks">
              <caption>Example marks — Reference — not an order</caption>
              <thead>
                <tr>
                  <th scope="col">#</th>
                  <th scope="col">time</th>
                  <th scope="col">side</th>
                  <th scope="col">setup</th>
                  <th scope="col">note</th>
                  <th scope="col">writer</th>
                  <th scope="col">withdraw</th>
                </tr>
              </thead>
              <tbody>
                {marksPhase.marks.map((mark, index) => (
                  <tr
                    key={mark.markId}
                    data-side={mark.side}
                    className={mark.writer.kind === "agent" ? "is-agent" : ""}
                  >
                    <th scope="row">
                      <button
                        type="button"
                        className="cw-row-btn mono"
                        aria-label={`Mark ${index + 1}, ${mark.side}, ${timeUtc(mark.barTime)}`}
                        onClick={() => adapterRef.current?.centerOn(mark.barTime)}
                      >
                        {index + 1}
                      </button>
                    </th>
                    <td className="mono">{timeUtc(mark.barTime)}</td>
                    <td>{mark.side}</td>
                    <td>{mark.setup}</td>
                    <td>{mark.note ?? EM_DASH}</td>
                    <td>{writerText(mark.writer)}</td>
                    <td>
                      <button
                        type="button"
                        className="cw-markwithdraw"
                        aria-label={`Withdraw mark ${index + 1}`}
                        onClick={() => void onWithdraw(mark)}
                      >
                        Withdraw
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
          {marksPhase.marks.length === 0 && (
            <p className="cw-note" role="status">
              No example marks yet — arm "Mark example", then click a candle.
            </p>
          )}
          <p className="cw-note" role="note">
            {`${marksPhase.marks.length} ${marksPhase.marks.length === 1 ? "mark" : "marks"} on ${markPair}${drawnTimeframe === null ? "" : `, ${marksOnLeg.length} on ${drawnTimeframe}`}.`}
          </p>
        </>
      )}
    </section>
  );
}

/** The structure events and the bars table of a run's layers. */
export function ChartDataTables({ s, view }: { s: ChartState; view: ChartView }) {
  const { barsOpen, setBarsOpen } = s;
  const { dto, events, series } = view;
  // The table copy: no value may be reachable only through the canvas (the
  // `ChartTwins` precedent).
  if (dto === null || dto.run === null) return null;
  return (
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
                            ) : overlay.kind === "oscillator" ? (
                              // A WT1 value is not a price: the read's own text.
                              <Figure exact={value}>{value}</Figure>
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
  );
}

/** Every drawn candle's exact open, high, low, close and volume (#420): no candle value is reachable only through
 *  the canvas, with a run or without one (PDR-0011). Rendered on demand, as the bars table is. */
export function CandleTable({ dto }: { dto: ChartDataDto | null }) {
  const [open, setOpen] = useState(false);
  if (dto === null || dto.candles.length === 0) return null;
  return (
    <section className="cw-data" aria-label="Candles">
      <details className="cw-bars" open={open}>
        <summary
          onClick={(event) => {
            event.preventDefault();
            setOpen((now) => !now);
          }}
        >
          {`Candles — ${dto.candles.length} bars (open, high, low, close, volume)`}
        </summary>
        {open && (
          <div className="cw-scroll">
            <table className="cw-tbl">
              <caption>Candles — open, high, low, close, volume</caption>
              <thead>
                <tr>
                  <th scope="col">bar</th>
                  <th scope="col">open</th>
                  <th scope="col">high</th>
                  <th scope="col">low</th>
                  <th scope="col">close</th>
                  <th scope="col">volume</th>
                </tr>
              </thead>
              <tbody>
                {dto.candles.map((candle) => (
                  <tr key={candle.t}>
                    <td className="mono">{timeUtc(candle.t)}</td>
                    <td className="mono"><Figure exact={candle.o}>{price(candle.o)}</Figure></td>
                    <td className="mono"><Figure exact={candle.h}>{price(candle.h)}</Figure></td>
                    <td className="mono"><Figure exact={candle.l}>{price(candle.l)}</Figure></td>
                    <td className="mono"><Figure exact={candle.c}>{price(candle.c)}</Figure></td>
                    <td className="mono"><Figure exact={candle.v}>{qty(candle.v)}</Figure></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </details>
    </section>
  );
}
