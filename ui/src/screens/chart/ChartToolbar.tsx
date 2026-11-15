// The Chart tab's toolbar and mark bar (r5.s2.w2, r5.s3.w3, r6.s4.w3), moved unchanged out
// of `ChartScreen.tsx` by r7.s2.w2. r7.s2.w3 restyles both to canvas v9.

import type { Leg } from "./model";
import { PAIRS, TIMEFRAMES, hasTwoLegs, timeframeOf } from "./model";
import type { ChartState } from "./state";
import type { ChartView } from "./view";

/** The pair and timeframe choice, go to a date, zoom, and what the chart is bound to. */
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
  const { run, free, leg, goToText, setGoToText, adapterRef } = s;
  const { drawnTimeframe, timeframe } = view;
  return (
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
                onClick={() => onSelectFree(pair, free.timeframe)}
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
                onClick={() => onSelectFree(free.pair, tf)}
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
                  onClick={() => onSelectLeg(which)}
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
      {/* Go to a date, and zoom (r6.s4.w3). Never disabled (G6): a control that cannot
          be pressed cannot say why it refused. Every time here is UTC. */}
      <form
        className="cw-markfield"
        aria-label="Go to a date"
        onSubmit={(event) => {
          event.preventDefault();
          onGoTo(goToText);
        }}
      >
        Go to (UTC){" "}
        <input
          type="text"
          className="cw-markinput"
          aria-label="Go to (UTC)"
          placeholder="YYYY-MM-DD HH:mm"
          value={goToText}
          onChange={(event) => setGoToText(event.target.value)}
        />{" "}
        <button type="submit" className="seg-btn">
          Go
        </button>
      </form>
      <div className="seg" role="group" aria-label="Zoom">
        <button
          type="button"
          className="seg-btn"
          aria-label="Zoom out"
          onClick={() => adapterRef.current?.zoom("out")}
        >
          −
        </button>
        <button
          type="button"
          className="seg-btn"
          aria-label="Zoom in"
          onClick={() => adapterRef.current?.zoom("in")}
        >
          +
        </button>
      </div>
      {/* What the chart is bound to, named by what is DRAWN: a leg the tab has not
          read yet (it is waiting for a trade) is not what the trader is looking at. */}
      <span className="cw-ctx">
        {run === null
          ? "No run"
          : `Backtest run ${run.runId} · ${drawnTimeframe ?? timeframe}`}
      </span>
    </header>
  );
}

/** The example-trade marks' controls. */
export function MarkBar({ s, view }: { s: ChartState; view: ChartView }) {
  const { form, setForm, setMarkRefusal } = s;
  const { drawnTimeframe, timeframe } = view;
  // The example-trade marks' controls (r5.s3.w3). A mark is a REFERENCE trade,
  // never an order (PDR-0037), and the control says so.
  return (
    <div className="cw-markbar">
      <span className="cw-tool-lab">Mark example</span>
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
      <label className="cw-markfield">
        Setup label
        <input
          type="text"
          className="cw-markinput"
          aria-label="Setup label"
          maxLength={64}
          value={form.setup}
          onChange={(event) =>
            setForm((current) => ({ ...current, setup: event.target.value }))
          }
        />
      </label>
      <label className="cw-markfield">
        Note (optional)
        <input
          type="text"
          className="cw-markinput"
          aria-label="Note (optional)"
          maxLength={280}
          value={form.note}
          onChange={(event) =>
            setForm((current) => ({ ...current, note: event.target.value }))
          }
        />
      </label>
      {/* Never disabled on a blank label: a control that cannot be pressed cannot say
          WHY it refused (G6). The label is checked where the write is decided, and
          that check names the field. */}
      <button
        type="button"
        className={`cw-markarm ${form.armed ? "is-on" : ""}`}
        onClick={() => {
          setMarkRefusal(null);
          setForm((current) => ({ ...current, armed: !current.armed }));
        }}
      >
        {form.armed ? "Mark example — armed" : "Mark example"} · Reference — not an order
      </button>
      {form.armed && !form.submitting && (
        <span className="cw-markhint" role="status">
          {`Click a ${drawnTimeframe ?? timeframe} candle to mark it.`}
        </span>
      )}
      {form.submitting && (
        <span className="cw-markhint" role="status">
          Saving…
        </span>
      )}
    </div>
  );
}
