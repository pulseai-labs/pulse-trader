// The chart window's own tests (r5.s2.w2).
//
// The read is bounded: 20,000 bars, and a window over the cap is a refusal that names
// the cap, never a silent truncation (grill Q3/G4). So the tab has to decide, BEFORE it
// asks, whether a leg fits in one read — and when it does not, which 1,001-bar window
// around the selected trade to ask for instead (grill Q3).
//
// This is arithmetic on two server times and a timeframe, not on any market value, and
// it lives in `ui/src/chart/` with the rest of the chart's own geometry: the screens
// directory is scanned for float conversions of money, and a window bound is exactly
// the kind of number that must not be read as a figure.

import { describe, expect, it } from "vitest";

import { CHART_BAR_CAP, WINDOW_HALF_BARS, covers, windowPlan } from "./window";

const DAY = 86_400_000;
/** 2025-01-01T00:00Z. */
const FIRST = 1_735_689_600_000;
/** 400 days later. */
const LAST = FIRST + 400 * DAY;
/** 2025-04-11T00:00Z — inside the span, far from either edge. */
const ENTRY = FIRST + 100 * DAY;
const HALF_MS = WINDOW_HALF_BARS * 900_000;

const SPAN = { firstMs: String(FIRST), lastMs: String(LAST) };

describe("windowPlan", () => {
  it("asks for the run's whole span when the leg fits in one read", () => {
    // 400 days of 4h bars is 2,401 — comfortably inside the cap.
    expect(windowPlan(SPAN, "4h", null)).toEqual({ kind: "span" });
  });

  it("names the bar count when the leg is over the cap and no trade is selected", () => {
    // The same 400 days of 15m bars is 38,401 — the count the tab shows the trader.
    expect(windowPlan(SPAN, "15m", null)).toEqual({ kind: "needs-trade", bars: 38_401 });
  });

  it("asks for a window around the selected trade's own bar", () => {
    expect(windowPlan(SPAN, "15m", String(ENTRY))).toEqual({
      kind: "window",
      fromMs: String(ENTRY - HALF_MS),
      toMs: String(ENTRY + HALF_MS),
    });
  });

  it("clamps the window into the run's own span", () => {
    // A trade one day in: the window would start before the run does.
    const near = FIRST + DAY;
    expect(windowPlan(SPAN, "15m", String(near))).toEqual({
      kind: "window",
      fromMs: String(FIRST),
      toMs: String(near + HALF_MS),
    });
    const late = LAST - DAY;
    expect(windowPlan(SPAN, "15m", String(late))).toEqual({
      kind: "window",
      fromMs: String(late - HALF_MS),
      toMs: String(LAST),
    });
  });

  it("asks for the span when the timeframe's own length is unknown", () => {
    // An interval this tab has no length for cannot be estimated. The read is sent as
    // the run's span and the server's refusal, if it refuses, is shown as it came.
    expect(windowPlan(SPAN, "3d", null)).toEqual({ kind: "span" });
  });

  it("asks for the span when no run is open at all", () => {
    expect(windowPlan(null, "15m", null)).toEqual({ kind: "span" });
  });

  it("counts the cap the read itself enforces", () => {
    expect(CHART_BAR_CAP).toBe(20_000);
  });
});

describe("covers", () => {
  // Three 15m bars one interval apart. The window's right edge is the END of the last
  // loaded bar — its open plus the loaded bars' own spacing — so the fixture has to
  // carry that spacing for either edge to mean anything.
  const tf = 900_000;
  const candles = [
    { t: String(ENTRY) },
    { t: String(ENTRY + tf) },
    { t: String(ENTRY + 2 * tf) },
  ];

  it("is true for a bar the window holds", () => {
    expect(covers(candles, String(ENTRY))).toBe(true);
    expect(covers(candles, String(ENTRY + tf))).toBe(true);
  });

  it("covers a fill inside the final loaded bar's own interval", () => {
    // A fill inside a bar's period — the last one included — is drawn ON that bar (the
    // close review's CR4), so the window covers it: the same interval the marker clip
    // reads off the loaded candles' smallest positive gap.
    expect(covers(candles, String(ENTRY + 2 * tf + tf / 2))).toBe(true);
    expect(covers(candles, String(ENTRY + tf + tf / 2))).toBe(true);
  });

  it("is false for a bar outside the window, on either side", () => {
    expect(covers(candles, String(ENTRY - 1))).toBe(false);
    // At or past the last bar's END belongs to a bar the window does not hold.
    expect(covers(candles, String(ENTRY + 3 * tf))).toBe(false);
  });

  it("is false when nothing is drawn, whatever the time", () => {
    // Nothing loaded means nothing is on screen, so selecting a trade has to read.
    expect(covers([], String(ENTRY))).toBe(false);
  });

  it("covers only the open of a single loaded bar, which has no spacing to read", () => {
    expect(covers([{ t: String(ENTRY) }], String(ENTRY))).toBe(true);
    expect(covers([{ t: String(ENTRY) }], String(ENTRY + 1))).toBe(false);
  });
});
