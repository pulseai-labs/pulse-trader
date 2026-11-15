// The chart context's own tests (r7.s2.w1, ADR-0030 amendment 2026-11-14).
//
// What a pane reads (its pair and timeframe, and nothing else) and which answer a pane
// still wants (only its newest request's, never another pane's).

import { describe, expect, it } from "vitest";

import {
  MAIN_PANE,
  chartDataRequest,
  createPaneReadGate,
  type ChartContext,
} from "./context";

describe("the chart context (r7.s2.w1)", () => {
  it("builds the bus request from the pane's pair and timeframe, and nothing else", () => {
    const context: ChartContext = {
      paneId: "left",
      pair: "ETHUSDT",
      timeframe: "1h",
      indicators: [{ token: "ema:50" }],
    };
    const request = chartDataRequest(context, "1", "2", "run-9");
    expect(request).toEqual({
      pair: "ETHUSDT",
      timeframe: "1h",
      fromMs: "1",
      toMs: "2",
      runId: "run-9",
    });
    expect(Object.keys(request).sort()).toEqual(["fromMs", "pair", "runId", "timeframe", "toMs"]);
  });

  it("accepts only a pane's newest read", () => {
    const gate = createPaneReadGate();
    const a = gate.begin("main");
    const b = gate.begin("main");
    expect(b).toBeGreaterThan(a);
    expect(gate.accepts("main", a)).toBe(false);
    expect(gate.accepts("main", b)).toBe(true);
  });

  it("keeps each pane's newest read apart", () => {
    const gate = createPaneReadGate();
    const l = gate.begin("left");
    const r = gate.begin("right");
    expect(gate.accepts("left", l)).toBe(true);
    expect(gate.accepts("right", r)).toBe(true);
    expect(gate.accepts("right", l)).toBe(false);
    expect(gate.accepts("left", r)).toBe(false);
  });

  it("accepts nothing for a pane that never read", () => {
    expect(createPaneReadGate().accepts(MAIN_PANE, 1)).toBe(false);
    expect(MAIN_PANE).toBe("main");
  });
});
