// Canvas v9's right rail (`chart.jsx` `RightRail`, r7.s2.w3): Inspector, Layers and Agent. Inspector shows the picked
// trade and the clicked bar; Layers lists what is drawn; the chart agent is r8's (PDR-0051 d7) and not built yet.

import type { ChartCandleDto, ChartDataDto, MarkDto } from "../../bindings";
import { NotBuiltYet } from "../../components/NotBuiltYet";
import { Figure } from "../../format/Figure";
import { price, qty } from "../../format/numbers";
import { timeUtc } from "../../format/time";
import { LayersPanel } from "./ChartLayers";
import { SelectedTradeTable } from "./ChartTables";
import { RAIL_TABS, atOrAfterMs } from "./model";
import type { ChartState } from "./state";
import type { ChartView } from "./view";

/** The rail beside the canvas, or its closed edge (`.rrail-closed`). It opens on Layers. */
export function RightRail({
  s,
  view,
  onWithdraw,
}: {
  s: ChartState;
  view: ChartView;
  onWithdraw: (mark: MarkDto) => Promise<void>;
}) {
  const { rail, setRail, railOpen } = s;
  if (!railOpen) return <div className="rrail-closed" />;
  const label = RAIL_TABS.find(([id]) => id === rail)?.[1] ?? "";
  return (
    <aside className="rrail">
      <div className="rr-tabs" role="tablist" aria-label="Right rail">
        {RAIL_TABS.map(([id, name]) => (
          <button
            key={id}
            type="button"
            role="tab"
            aria-selected={rail === id}
            className={`rr-tab ${rail === id ? "is-on" : ""} ${id === "agent" ? "is-ai" : ""}`}
            onClick={() => setRail(id)}
          >
            {id === "agent" && (
              <span className="ai-glyph" aria-hidden="true">
                ✦
              </span>
            )}
            {name}
          </button>
        ))}
      </div>
      <div className="rr-body" role="tabpanel" aria-label={label}>
        {rail === "inspector" && <InspectorPanel s={s} view={view} />}
        {rail === "layers" && <LayersPanel s={s} view={view} onWithdraw={onWithdraw} />}
        {rail === "agent" && <AgentPanel />}
      </div>
    </aside>
  );
}

/** Canvas v9's Inspector (`InspectorPanel`): the picked trade's setup and stop, then the clicked bar; with neither,
 *  what a click shows. A drawing's stored fields are r7.s7's. */
export function InspectorPanel({ s, view }: { s: ChartState; view: ChartView }) {
  const { run, selectedKey } = s;
  const { dto, focusCandle } = view;
  const tradePicked = run !== null && selectedKey !== null;
  return (
    <div className="ins-wrap">
      <SelectedTradeTable s={s} view={view} />
      {dto !== null && focusCandle !== null && <BarInspector dto={dto} candle={focusCandle} />}
      {!tradePicked && focusCandle === null && (
        <div className="ins-empty">
          <p>Click a bar to see what the server sees there.</p>
          <p className="dim">
            <NotBuiltYet what="A drawing's stored fields" />
          </p>
        </div>
      )}
    </div>
  );
}

/** One bar's values, as canvas v9's `BarInspector` lists them; Explain is r8's (PDR-0051 d7). A bar inside a free
 *  chart's holdout says so instead, as v9 does. */
export function BarInspector({ dto, candle }: { dto: ChartDataDto; candle: ChartCandleDto }) {
  const inHoldout =
    dto.run === null && dto.holdout != null && atOrAfterMs(candle.t, dto.holdout.holdoutStartMs);
  return (
    <section className="ins" aria-label="Bar">
      <div className="ins-h">
        <b>Bar</b>
        <span className="mono">{timeUtc(candle.t)}</span>
      </div>
      <dl className="ins-dl">
        <dt>open</dt>
        <dd>
          <Figure exact={candle.o}>{price(candle.o)}</Figure>
        </dd>
        <dt>high</dt>
        <dd>
          <Figure exact={candle.h}>{price(candle.h)}</Figure>
        </dd>
        <dt>low</dt>
        <dd>
          <Figure exact={candle.l}>{price(candle.l)}</Figure>
        </dd>
        <dt>close</dt>
        <dd>
          <Figure exact={candle.c}>{price(candle.c)}</Figure>
        </dd>
        <dt>volume</dt>
        <dd>
          <Figure exact={candle.v}>{qty(candle.v)}</Figure>
        </dd>
      </dl>
      {inHoldout ? (
        <p className="ho-note">This bar is inside the frozen holdout; agents can't test here.</p>
      ) : (
        <div className="ins-acts">
          <NotBuiltYet what="Explain this bar" />
        </div>
      )}
    </section>
  );
}

/** Canvas v9's Agent tab. The chart agent is r8's (PDR-0051 d7): no r7 spine builds it. */
export function AgentPanel() {
  return (
    <div className="agent-nb">
      <NotBuiltYet what="Chart agent" />
    </div>
  );
}
