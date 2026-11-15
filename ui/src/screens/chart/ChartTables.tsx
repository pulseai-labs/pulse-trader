// The Chart tab's notes, the selected trade's table, canvas v9's trade strip (r7.s2.w3) and the table copies
// (r7.s2.w2): every value drawn on the canvas is here as text (PDR-0011). The example marks moved to
// `ChartLayers.tsx`.

import { useState } from "react";

import type { ChartDataDto, ChartTradeDto } from "../../bindings";
import { tradeOutcome } from "../../chart/layers";
import { barsHeld } from "../../chart/window";
import { NotBuiltYet } from "../../components/NotBuiltYet";
import { Figure } from "../../format/Figure";
import { EM_DASH, price, qty, rMultiple } from "../../format/numbers";
import { dateUtc, timeUtc } from "../../format/time";
import { hasTwoLegs, tradeKey } from "./model";
import type { ChartState } from "./state";
import type { ChartView } from "./view";
import { setupText } from "./view";

/** The chart's own notes, at the top of the Layers rail (r7.s2.w3): the holdout when no tag stands on the canvas,
 *  and a rule that draws nothing on this leg. */
export function ChartNotes({ view }: { view: ChartView }) {
  const { dto, holdoutTag } = view;
  return (
    <div className="cw-notes">
      {dto?.holdout != null && holdoutTag === null && (
        <p className="cw-note" role="note">
          {dto.run === null
            ? `Holdout — from ${dateUtc(dto.holdout.holdoutStartMs)} · agents can't test here`
            : `Holdout from ${dateUtc(dto.holdout.holdoutStartMs)} — ${dto.holdout.barsWithheld} bars withheld`}
        </p>
      )}
      {dto?.run != null && dto.run.overlays.length === 0 && dto.run.notDrawn.length === 0 && (
        <p className="cw-note" role="note">
          {`This rule uses no S/R, pivot, candle-pattern, EMA, SMA or WaveTrend operand on ${dto.timeframe}.`}
        </p>
      )}
    </div>
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
    <section className="ins" aria-label="Selected trade">
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

/** Canvas v9's trade strip (`chart.jsx` `TradeStrip`, r7.s2.w3): the run's own stored trades, one row each, with the
 *  stop (#507), the bars held and, on a setup strategy's run, v9's setup column (A4). A click anywhere on a row
 *  picks its trade (#422, #506). Every row is rendered; none is virtualised (#507). A free chart has no strip, as
 *  in v9. */
export function TradeStrip({
  s,
  view,
  onSelectTrade,
}: {
  s: ChartState;
  view: ChartView;
  onSelectTrade: (trade: ChartTradeDto) => void;
}) {
  const { run, selectedKey, undrawn } = s;
  const { trades, fencedOut, windowTrades, timeframe } = view;
  if (run === null) return null;
  // A setup strategy's run, told by the drawn window's own rows: the same server bar times the setup markers use.
  const drawn = windowTrades ?? [];
  const showSetup = drawn.some((row) => setupText(row) !== null);
  const drawnByKey = new Map(drawn.map((row) => [tradeKey(row), row]));
  const count =
    windowTrades === null
      ? `${trades.length} in the run`
      : `${windowTrades.length} of ${trades.length} in the drawn window`;
  return (
    <section className="tstrip" aria-label="Trades">
      <div className="ts-h">
        <b>Trades</b>
        <span className="mono dim">{`${run.runId} · ${count}`}</span>
        {undrawn !== null && (
          <span className="ts-note" role="note">
            {`The trade's entry is not drawn in this ${timeframe} window.`}
          </span>
        )}
        <span className="cwt-spacer" />
        <span className="mono dim">click a row to center it</span>
      </div>
      <div className="ts-body">
        <table className="ts-t">
          {/* Named by what the rows ARE: the run's own stored trades, always — a bounded window narrows what is
              DRAWN, never which trades the trader may pick next (the close review's C). */}
          <caption className="sr-only">Trades in the run</caption>
          <thead>
            <tr>
              <th scope="col">#</th>
              <th scope="col">side</th>
              <th scope="col">entry</th>
              <th scope="col" className="num">price</th>
              <th scope="col">exit</th>
              <th scope="col" className="num">price</th>
              <th scope="col" className="num">stop</th>
              <th scope="col" className="num">bars</th>
              <th scope="col">reason</th>
              {showSetup && <th scope="col">setup · arm → dip → trigger</th>}
              <th scope="col" className="num">R</th>
            </tr>
          </thead>
          <tbody>
            {trades.map((trade, index) => (
              <TradeRow
                key={`${trade.entryTime}-${trade.side}-${index}`}
                trade={trade}
                index={index}
                on={tradeKey(trade) === selectedKey}
                timeframe={run.primaryTimeframe}
                showSetup={showSetup}
                setup={setupText(drawnByKey.get(tradeKey(trade)) ?? null)}
                onSelect={onSelectTrade}
              />
            ))}
            {trades.length === 0 && (
              <tr>
                <td colSpan={showSetup ? 11 : 10}>
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
    </section>
  );
}

/** One trade in the strip. The "#" button is the keyboard's way in: it has no handler of its own, and its click,
 *  from a pointer or a key, reaches the row's. The setup cell, on a setup run, is the drawn window's own row's;
 *  a trade the drawn window does not carry with its bars shows not built yet. The R is coloured by the trade's
 *  outcome, as its markers are. */
function TradeRow({
  trade,
  index,
  on,
  timeframe,
  showSetup,
  setup,
  onSelect,
}: {
  trade: ChartTradeDto;
  index: number;
  on: boolean;
  timeframe: string;
  showSetup: boolean;
  setup: string | null;
  onSelect: (trade: ChartTradeDto) => void;
}) {
  const bars = barsHeld(trade.entryTime, trade.exitTime, timeframe);
  const outcome = tradeOutcome(trade);
  return (
    <tr className={on ? "is-on" : ""} onClick={() => onSelect(trade)}>
      <th scope="row">
        <button
          type="button"
          className="cw-row-btn mono"
          aria-label={`Trade ${index + 1}, ${trade.side}, entry ${timeUtc(trade.entryTime)}`}
        >
          {index + 1}
        </button>
      </th>
      <td>
        <span className={`dir-tag dir-${trade.side}`}>{trade.side}</span>
      </td>
      <td className="mono">{timeUtc(trade.entryTime)}</td>
      <td className="num mono">
        <Figure exact={trade.entryPrice}>{price(trade.entryPrice)}</Figure>
      </td>
      <td className="mono">{timeUtc(trade.exitTime)}</td>
      <td className="num mono">
        <Figure exact={trade.exitPrice}>{price(trade.exitPrice)}</Figure>
      </td>
      <td className="num mono">
        {trade.stopPrice === null ? EM_DASH : <Figure exact={trade.stopPrice}>{price(trade.stopPrice)}</Figure>}
      </td>
      <td className="num mono">{bars === null ? EM_DASH : bars}</td>
      <td>
        <span className="exit-tag">{trade.exitReason ?? EM_DASH}</span>
      </td>
      {showSetup && (
        <td className="mono">{setup === null ? <NotBuiltYet what="Setup outside the drawn window" /> : setup}</td>
      )}
      <td className={`num mono ${outcome === "open" ? "warn" : outcome}`}>
        <Figure exact={trade.realizedR}>{rMultiple(trade.realizedR)}</Figure>
      </td>
    </tr>
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
