// Canvas v9's Layers tab (`chart.jsx` `LayersPanel`, r7.s2.w3): what the read drew, the run's trades, the example
// marks and the operands the rule reads but the chart does not draw (#507), then every drawn value as a table
// (PDR-0011). Showing or hiding a layer is not built: each section shows not built yet in its eye's place
// (PDR-0060 d3).

import type { ReactNode } from "react";

import type { MarkDto } from "../../bindings";
import { NotBuiltYet } from "../../components/NotBuiltYet";
import { EM_DASH } from "../../format/numbers";
import { timeUtc } from "../../format/time";
import { CandleTable, ChartDataTables, ChartNotes } from "./ChartTables";
import { otherTimeframeNote, writerText } from "./model";
import type { ChartState } from "./state";
import type { ChartView, LayerRow } from "./view";
import { layerGroups } from "./view";

/** The Layers tab, in canvas v9's order: the notes, Indicators, Structure, Candle patterns, Trades, My drawings,
 *  Example marks, Not drawn, the key, then the table copies. */
export function LayersPanel({
  s,
  view,
  onWithdraw,
}: {
  s: ChartState;
  view: ChartView;
  onWithdraw: (mark: MarkDto) => Promise<void>;
}) {
  const groups = layerGroups(view.dto);
  // A run's chart reads its rule's structure and patterns, so an empty list there is a built read that found none;
  // a free chart's are r7.s4's and r7.s5's (CR3). Before the chart read answers, nothing is known either way, so
  // no empty-state line shows (C-1).
  const built = s.run !== null && view.dto !== null;
  const loading = view.dto === null;
  return (
    <div className="lyr">
      <ChartNotes view={view} />
      <LayerSection toggle="Show or hide indicators" name="Indicators" rows={groups.indicators} empty={null} />
      <LayerSection
        toggle="Show or hide structure"
        name="Structure"
        rows={groups.structure}
        empty="Market structure"
        built={built}
        loading={loading}
      >
        {groups.hasPivots && (
          <p className="lg-note">
            Pivots appear on the bar that confirmed them. The faint marker is the peak; the solid one is when the
            strategy could know it.
          </p>
        )}
      </LayerSection>
      <LayerSection
        toggle="Show or hide candle patterns"
        name="Candle patterns"
        rows={groups.patterns}
        empty="Candle patterns"
        built={built}
        loading={loading}
      >
        <div className="pl">
          <span>
            <i className="pd pd-bull" />
            bullish · below bar
          </span>
          <span>
            <i className="pd pd-bear" />
            bearish · above
          </span>
          <span>
            <i className="pd pd-open" />
            neutral
          </span>
        </div>
      </LayerSection>
      <TradesSection s={s} view={view} />
      <section className="lg">
        <LayerHead toggle={null} name="My drawings" count={null} />
        <p className="lg-note">
          <NotBuiltYet what="Drawings" />
        </p>
      </section>
      <MarksTable s={s} view={view} onWithdraw={onWithdraw} />
      <NotDrawnSection view={view} />
      <LayersKey />
      <div className="lyr-data">
        <ChartDataTables s={s} view={view} />
        <CandleTable dto={view.dto} />
      </div>
    </div>
  );
}

/** A Layers section's header (`.lg-h`): the show-or-hide toggle, not built yet in the eye's place; the name; the
 *  count. */
export function LayerHead({ toggle, name, count }: { toggle: string | null; name: string; count: number | null }) {
  return (
    <header className="lg-h">
      {toggle !== null && <NotBuiltYet what={toggle} />}
      <span className="lg-n">{name}</span>
      {count !== null && <span className="lg-c mono">{count}</span>}
    </header>
  );
}

/** A Layers section of drawn layers: one row per layer, or, with none, what stands in their place. On a `built`
 *  read (a run's chart) an empty list is a plain note; otherwise `empty` names the not-built layer (CR3). While the
 *  chart read has not answered (`loading`), an empty list says nothing at all (C-1). */
export function LayerSection({
  toggle,
  name,
  rows,
  empty,
  built = false,
  loading = false,
  children,
}: {
  toggle: string | null;
  name: string;
  rows: readonly LayerRow[];
  empty: string | null;
  built?: boolean;
  loading?: boolean;
  children?: ReactNode;
}) {
  return (
    <section className="lg">
      <LayerHead toggle={toggle} name={name} count={rows.length} />
      {rows.map((row, index) => (
        <div key={`${row.label}-${index}`} className="lg-sub">
          <span className={row.dashed ? "ro-sw dash" : "ro-sw"} />
          <span className="mono">{row.label}</span>
          {row.count !== null && <span className="lg-c mono">{row.count}</span>}
        </div>
      ))}
      {rows.length === 0 && empty !== null && !loading && (
        <p className="lg-note">{built ? "None in the drawn window" : <NotBuiltYet what={empty} />}</p>
      )}
      {children}
    </section>
  );
}

/** The Trades section: a run's count and its winner, loser and open key. A free chart has none, and binding a paper
 *  session is not built (RELEASE scope note s2). */
export function TradesSection({ s, view }: { s: ChartState; view: ChartView }) {
  const { run } = s;
  const { trades } = view;
  return (
    <section className={run === null ? "lg is-dis" : "lg"}>
      <LayerHead toggle="Show or hide trades" name="Trades" count={run === null ? 0 : trades.length} />
      {run === null ? (
        <>
          <p className="lg-note">No run is open, so there are no trades to draw.</p>
          <p className="lg-note">
            <NotBuiltYet what="Binding a paper session" />
          </p>
        </>
      ) : (
        <div className="pl">
          <span>
            <i className="pd pd-bull" />
            winner
          </span>
          <span>
            <i className="pd pd-bear" />
            loser
          </span>
          <span>
            <i className="pd pd-open" />
            open
          </span>
        </div>
      )}
    </section>
  );
}

/** Canvas v9's Example marks section (A6, r7.s2.w3): every stored field of each mark (PDR-0011), a withdraw per row,
 *  and how many marks live elsewhere. The table stays while a write or a withdraw re-reads it (#439). */
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
    <section className="lg">
      <LayerHead
        toggle="Show or hide example marks"
        name="Example marks"
        count={marksPhase.kind === "ready" ? marksPhase.marks.length : null}
      />
      <p className="lg-note">
        Reference — not an order. Marks show where a setup should have fired; no rule or session trades them.
      </p>
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
                    <td>
                      <span className={`dir-tag dir-${mark.side}`}>{mark.side}</span>
                    </td>
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
    </section>
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

/** The operands the rule reads on this leg that the chart does not draw (#507; canvas v9's "Not drawn" section, A7).
 *  The read names each by its label only. */
export function NotDrawnSection({ view }: { view: ChartView }) {
  const { dto } = view;
  if (dto?.run == null || dto.run.notDrawn.length === 0) return null;
  const rows = dto.run.notDrawn.map((label) => ({ label, count: null, dashed: true }));
  return (
    <LayerSection toggle={null} name={`Not drawn on the ${dto.timeframe} leg`} rows={rows} empty={null}>
      <p className="lg-note">
        The rule reads these on this leg. The chart draws only S/R levels, pivots, candle patterns, EMA, SMA,
        WaveTrend and band edges, so it lists the rest here.
      </p>
    </LayerSection>
  );
}

/** Canvas v9's key: a server layer is dashed, the trader's solid, an agent's purple. */
export function LayersKey() {
  return (
    <div className="lyr-key">
      <span>
        <i className="k-dash" />
        server
      </span>
      <span>
        <i className="k-solid" />
        you
      </span>
      <span>
        <i className="k-ai" />
        agent
      </span>
    </div>
  );
}
