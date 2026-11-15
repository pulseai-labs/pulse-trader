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
//  * **The strategy layers and the selected trade's setup (r6.s4.w4)** are drawn from the
//    read: the ribbon, its band edges, SMA and WT1, and the selected trade's arm, dip,
//    trigger and stop. The screen picks the trade and the leg, nothing else.
//
// Every state is plain and says what it is (PDR-0011): loading, failed, an empty window,
// no run, a withheld holdout, the operands the rule uses that are not drawn, and a leg
// waiting for a trade to centre on. No zero or placeholder ever stands in for a result.
// r7.s2.w2 (#502 C5, canvas v9): the screen is split under the 80-logic-line cap. Its state, effects,
// handlers and derived values live in `./chart/`, moved unchanged; this file only calls them in the order the
// effects were declared and lays the screen out as canvas v9 does: the toolbar, the mark bar, then the draw
// rail beside the canvas pane, with the notes and tables in the strip region below it.

import {
  CandleTable,
  ChartDataTables,
  ChartNotes,
  MarksTable,
  SelectedTradeTable,
  TradesTable,
} from "./chart/ChartTables";
import { ChartPane, DrawRail } from "./chart/ChartPane";
import { ChartToolbar, MarkBar } from "./chart/ChartToolbar";
import {
  useChartCanvas,
  useChartDraw,
  useChartRead,
  useMarkWriter,
  useMarksRead,
  useRunFragment,
  useZoomKeys,
} from "./chart/effects";
import { chartHandlers, goToHandler } from "./chart/handlers";
import type { ChartState } from "./chart/state";
import { useMarkState, useReadState, useSelectionState } from "./chart/state";
import { chartView } from "./chart/view";

export default function ChartScreen() {
  const s: ChartState = { ...useReadState(), ...useSelectionState(), ...useMarkState() };
  useChartCanvas(s);
  useMarkWriter(s);
  useZoomKeys(s);
  useRunFragment(s);
  const view = chartView(s);
  const withdraw = useMarksRead(s, view.markPair);
  useChartRead(s);
  useChartDraw(s, view);
  const { selectFree, selectLeg, selectTrade } = chartHandlers(s, view);
  const goTo = goToHandler(s, view);
  // The once-subscribed click handler and a write's continuation read these refs, so every render writes
  // them, as the screen did before the split (lines 786–792, 937–940).
  s.markClickRef.current = {
    form: s.form,
    pair: view.markPair,
    timeframe: view.drawnTimeframe ?? view.timeframe,
  };
  s.liveHoldoutRef.current = view.liveHoldout;
  return (
    <section className="cw" aria-label="Chart">
      <ChartToolbar s={s} view={view} onSelectFree={selectFree} onSelectLeg={selectLeg} onGoTo={goTo} />
      <MarkBar s={s} view={view} />
      <div className="cw-body">
        <DrawRail />
        <div className="cw-main">
          <ChartPane s={s} view={view} />
          <div className="cw-below">
            <ChartNotes s={s} view={view} />
            <SelectedTradeTable s={s} view={view} />
            <TradesTable s={s} view={view} onSelectTrade={selectTrade} />
            <MarksTable s={s} view={view} onWithdraw={withdraw} />
            <ChartDataTables s={s} view={view} />
            <CandleTable dto={view.dto} />
          </div>
        </div>
      </div>
    </section>
  );
}
