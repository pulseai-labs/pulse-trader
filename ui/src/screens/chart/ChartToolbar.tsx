// The Chart tab's toolbar and mark bar, aligned to canvas v9 by r7.s2.w3 (`chart.jsx` `ChartToolbar`, the A6 mark
// bar). A control a later spine builds shows not built yet in its v9 place (PDR-0060 d3); nothing here is a dead
// button.

import { useState } from "react";

import { NotBuiltYet } from "../../components/NotBuiltYet";
import type { Leg } from "./model";
import { PAIRS, TIMEFRAMES, hasTwoLegs, timeframeOf } from "./model";
import type { ChartState } from "./state";
import type { ChartView } from "./view";

/** Canvas v9's toolbar: the pair chip, the timeframes, Go to date, + Indicator, Layers and Mark example, then the
 *  layout picker, what the chart is bound to and the rail toggle. */
export function ChartToolbar({
  s,
  view,
  onSelectFree,
  onSelectLeg,
  onGoTo,
}: {
  s: ChartState;
  view: ChartView;
  onSelectFree: (pair: string, timeframe: string) => void;
  onSelectLeg: (next: Leg) => void;
  onGoTo: (text: string) => void;
}) {
  const { form, setForm, setMarkRefusal, setRail, railOpen, setRailOpen } = s;
  return (
    <header className="cw-tool">
      <SymbolChip s={s} onSelectFree={onSelectFree} />
      <span className="cwt-div" aria-hidden="true" />
      <TimeframeRow s={s} onSelectFree={onSelectFree} onSelectLeg={onSelectLeg} />
      <span className="cwt-div" aria-hidden="true" />
      <GoToDate s={s} onGoTo={onGoTo} />
      <span className="cwt-later">
        + Indicator
        <NotBuiltYet what="Indicator picker" />
      </span>
      <button
        type="button"
        className="btn-sec-sm cwt-ic"
        onClick={() => {
          setRail("layers");
          setRailOpen(true);
        }}
      >
        <ToolIcon name="layers" />
        Layers
      </button>
      {/* Arms the candle click and opens the mark bar (canvas v9 `markOn`, #439). Never disabled (G6). */}
      <button
        type="button"
        className={`btn-sec-sm ${form.armed ? "is-on" : ""}`}
        aria-pressed={form.armed}
        onClick={() => {
          setMarkRefusal(null);
          setForm((current) => ({ ...current, armed: !current.armed }));
        }}
      >
        Mark example
      </button>
      <span className="cwt-spacer" />
      <span className="cwt-later" title="Chart layout">
        <LayoutIcon />
        <NotBuiltYet what="Chart layouts" />
      </span>
      <ContextChip s={s} view={view} />
      <button
        type="button"
        className={`ic-btn-sm cwt-rail ${railOpen ? "is-on" : ""}`}
        aria-label={railOpen ? "Hide right rail" : "Show right rail"}
        aria-pressed={railOpen}
        title="Toggle right rail"
        onClick={() => setRailOpen(!railOpen)}
      >
        <ToolIcon name="rail" />
      </button>
    </header>
  );
}

/** Canvas v9's symbol chip (`.sym-btn`). On a free chart it opens the pairs the server holds; Escape closes them.
 *  On a run's chart it shows the run's pair and opens nothing: the run sets the pair. */
export function SymbolChip({
  s,
  onSelectFree,
}: {
  s: ChartState;
  onSelectFree: (pair: string, timeframe: string) => void;
}) {
  const { run, free } = s;
  const [open, setOpen] = useState(false);
  if (run !== null) {
    return <span className="sym-btn" title="The run sets the pair"><PairFace pair={run.pair} /></span>;
  }
  return (
    <div
      className="cwt-pop-anchor"
      onKeyDown={(event) => {
        if (event.key === "Escape") setOpen(false);
      }}
    >
      <button
        type="button"
        className="sym-btn"
        aria-label={`Pair: ${free.pair}`}
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <PairFace pair={free.pair} />
        <ChevIcon />
      </button>
      {open && (
        <PairList
          current={free.pair}
          onPick={(pair) => {
            setOpen(false);
            onSelectFree(pair, free.timeframe);
          }}
        />
      )}
    </div>
  );
}

/** The letter canvas v9's coin glyph shows: the pair's first letter, past a `1000` prefix. */
function glyphOf(pair: string): string {
  return pair.replace(/^1000/, "").charAt(0);
}

/** The chip's face: the coin glyph, the pair, PERP and Binance. Every pair the app charts is a Binance USDT-M
 *  perpetual. */
function PairFace({ pair }: { pair: string }) {
  return (
    <>
      <span className="sym-glyph mono" aria-hidden="true">
        {glyphOf(pair)}
      </span>
      <b className="mono">{pair}</b>
      <span className="sym-mk mono">PERP</span>
      <span className="sym-ex">Binance</span>
    </>
  );
}

/** The pairs the server holds, as canvas v9's symbol search lists them under "Bars on server"
 *  (`chart.jsx:287-288`). The search over every Binance coin is r7.s3's (PDR-0060 d3). */
function PairList({ current, onPick }: { current: string; onPick: (pair: string) => void }) {
  return (
    <div className="sym-pop" role="dialog" aria-label="Pairs">
      <div className="sym-sec">
        <span>Bars on server</span>
        <i className="mono">{PAIRS.length}</i>
        <em>ready to chart</em>
      </div>
      {PAIRS.map((pair) => (
        <button key={pair} type="button" className="sym-pick" onClick={() => onPick(pair)}>
          <span className="sym-glyph mono" aria-hidden="true">
            {glyphOf(pair)}
          </span>
          <span className="sym-t mono">
            {pair}
            {pair === current && <span className="sym-cur">on chart</span>}
          </span>
          <span className="sym-exch">Binance</span>
        </button>
      ))}
      <div className="sym-later">
        <NotBuiltYet what="Symbol search" />
      </div>
    </div>
  );
}

/** Canvas v9's timeframe row (`.tf-row`): the free chart's timeframes, or the run's legs (Q2). The menu of every
 *  interval is r7.s3's (PDR-0060 d3). */
export function TimeframeRow({
  s,
  onSelectFree,
  onSelectLeg,
}: {
  s: ChartState;
  onSelectFree: (pair: string, timeframe: string) => void;
  onSelectLeg: (next: Leg) => void;
}) {
  const { run, free, leg } = s;
  return (
    <div className="tf-row">
      {run === null &&
        TIMEFRAMES.map((tf) => (
          <button
            key={tf}
            type="button"
            className={`tf-btn mono ${free.timeframe === tf ? "is-active" : ""}`}
            onClick={() => onSelectFree(free.pair, tf)}
          >
            {tf}
          </button>
        ))}
      {run !== null &&
        hasTwoLegs(run) &&
        (["primary", "htf"] as const).map((which) => (
          <button
            key={which}
            type="button"
            className={`tf-btn mono ${leg === which ? "is-active" : ""}`}
            title={which === "primary" ? "Primary timeframe" : "Higher timeframe"}
            onClick={() => onSelectLeg(which)}
          >
            {timeframeOf(run, which)}
          </button>
        ))}
      {run !== null && !hasTwoLegs(run) && (
        <span className="tf-btn mono is-active" title="Primary timeframe">
          {run.primaryTimeframe}
        </span>
      )}
      <NotBuiltYet what="All timeframes" />
    </div>
  );
}

/** Canvas v9's Go to date (r6.s4.w3's field, in v8's popover): the button opens the field; Go and Escape close it.
 *  The typed text stays for the next open. Never disabled (G6). Every time here is UTC. */
export function GoToDate({ s, onGoTo }: { s: ChartState; onGoTo: (text: string) => void }) {
  const { goToText, setGoToText } = s;
  const [open, setOpen] = useState(false);
  return (
    <div
      className="cwt-pop-anchor"
      onKeyDown={(event) => {
        if (event.key === "Escape") setOpen(false);
      }}
    >
      <button
        type="button"
        className={`btn-sec-sm cwt-ic ${open ? "is-on" : ""}`}
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <ToolIcon name="cal" />
        Go to date
      </button>
      {open && (
        <form
          className="date-pop"
          aria-label="Go to a date"
          onSubmit={(event) => {
            event.preventDefault();
            setOpen(false);
            onGoTo(goToText);
          }}
        >
          <label>
            <span className="if-k">Go to (UTC)</span>
            <input
              type="text"
              className="date-in mono"
              placeholder="YYYY-MM-DD HH:mm"
              value={goToText}
              onChange={(event) => setGoToText(event.target.value)}
            />
          </label>
          <button type="submit" className="btn-sec-sm">
            Go
          </button>
        </form>
      )}
    </div>
  );
}

/** What the chart is bound to (canvas v9 `.ctx-chip`), named by what is DRAWN: a leg the tab has not read yet (it
 *  is waiting for a trade) is not what the trader is looking at. */
export function ContextChip({ s, view }: { s: ChartState; view: ChartView }) {
  const { run } = s;
  const { drawnTimeframe, timeframe } = view;
  return (
    <span className={`ctx-chip ${run === null ? "ctx-free" : "ctx-bt"}`}>
      <span className="ctx-dot" aria-hidden="true" />
      {run === null ? "Free chart" : `Backtest run ${run.runId} · ${drawnTimeframe ?? timeframe}`}
    </span>
  );
}

/** Canvas v9's toolbar icons (`chart.jsx` `TIcon`: cal, layers, rail), in the text colour, hidden from screen
 *  readers. */
function ToolIcon({ name }: { name: "cal" | "layers" | "rail" }) {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.4"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      {name === "cal" && (
        <>
          <rect x="2.5" y="3.5" width="11" height="10" rx="1" />
          <path d="M2.5 6.5 H13.5 M5.5 2 V4.5 M10.5 2 V4.5" />
        </>
      )}
      {name === "layers" && (
        <>
          <path d="M8 2 L14 5.2 L8 8.4 L2 5.2 Z" />
          <path d="M2 8 L8 11.2 L14 8" />
          <path d="M2 10.8 L8 14 L14 10.8" />
        </>
      )}
      {name === "rail" && (
        <>
          <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
          <path d="M10 2.5 V13.5" />
        </>
      )}
    </svg>
  );
}

/** Canvas v9's chevron (`.sym-chev`). */
function ChevIcon() {
  return (
    <svg className="sym-chev" viewBox="0 0 10 10" aria-hidden="true">
      <path d="M2.5 4 L5 6.5 L7.5 4" fill="none" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
    </svg>
  );
}

/** Canvas v9's one-chart layout icon (`LayoutIcon` for layout `1` at 18 × 14). */
function LayoutIcon() {
  return (
    <svg width="18" height="14" viewBox="0 0 18 14" aria-hidden="true">
      <rect x="1.75" y="1.75" width="14.5" height="10.5" rx="1.5" fill="none" stroke="currentColor" strokeWidth="1.2" />
    </svg>
  );
}

/** Canvas v9's mark bar (`.markbar`, A6), shown while Mark example is armed. A mark is a REFERENCE trade, never an
 *  order (PDR-0037), and the bar says so. Never disabled on a blank label (G6): the click handler names the field.
 *  Done disarms it (#439). */
export function MarkBar({ s }: { s: ChartState }) {
  const { form, setForm, setMarkRefusal } = s;
  if (!form.armed) return null;
  return (
    <div className="markbar">
      <b>Mark example</b>
      <div className="seg">
        {(["long", "short"] as const).map((side) => (
          <button
            key={side}
            type="button"
            className={`seg-btn ${form.side === side ? "is-active" : ""}`}
            onClick={() => setForm((current) => ({ ...current, side }))}
          >
            {side === "long" ? "Long" : "Short"}
          </button>
        ))}
      </div>
      <label className="mb-f">
        <span>Setup</span>
        <input
          type="text"
          aria-label="Setup label"
          maxLength={64}
          value={form.setup}
          onChange={(event) => setForm((current) => ({ ...current, setup: event.target.value }))}
        />
      </label>
      <label className="mb-f mb-note">
        <span>Note</span>
        <input
          type="text"
          aria-label="Note (optional)"
          maxLength={280}
          placeholder="what made this a setup"
          value={form.note}
          onChange={(event) => setForm((current) => ({ ...current, note: event.target.value }))}
        />
      </label>
      <span className="mb-ref">Reference — not an order</span>
      <span className="cwt-spacer" />
      <span className="mono dim" role="status">
        {form.submitting ? "Saving…" : "click a candle to place it"}
      </span>
      <button
        type="button"
        className="btn-sec-sm"
        onClick={() => {
          setMarkRefusal(null);
          setForm((current) => ({ ...current, armed: false }));
        }}
      >
        Done
      </button>
    </div>
  );
}
