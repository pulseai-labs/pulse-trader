// Pure tests over the route table (r1.s1.w6, spec step 7's "pure" layer).
//
// These exercise `routes.ts` in isolation -- no DOM, no React. They assert
// fragment normalization, route resolution including the no-`element` path,
// the derived unbuilt set, and the `path === "/" + nav` convention over the
// REAL `ROUTES` table (not a fixture), so a round-4 append that breaks the
// convention fails here rather than only at runtime.

import { describe, expect, it } from "vitest";

import {
  DEFAULT_NAV_ID,
  ROUTES,
  backtestDeepLink,
  chartDeepLink,
  isNavBuilt,
  resolveNavId,
  resolveRoute,
} from "./routes";

const KNOWN_NAV_IDS = ["library", "designer", "backtest", "chart", "deploy", "journal", "analytics", "settings", "help"];

describe("resolveNavId (fragment normalization)", () => {
  it("normalizes an empty fragment to the default nav", () => {
    expect(resolveNavId("", KNOWN_NAV_IDS)).toBe(DEFAULT_NAV_ID);
  });

  it("normalizes a bare '#/' to the default nav", () => {
    expect(resolveNavId("#/", KNOWN_NAV_IDS)).toBe(DEFAULT_NAV_ID);
  });

  it("normalizes an unrecognized fragment to the default nav", () => {
    expect(resolveNavId("#/not-a-real-destination", KNOWN_NAV_IDS)).toBe(DEFAULT_NAV_ID);
  });

  it("keeps a recognized, non-default nav id as-is", () => {
    expect(resolveNavId("#/settings", KNOWN_NAV_IDS)).toBe("settings");
  });

  it("is insensitive to a leading '#' being absent", () => {
    expect(resolveNavId("/settings", KNOWN_NAV_IDS)).toBe("settings");
  });
});

describe("resolveRoute (including the no-element path)", () => {
  it("resolves the '/' entry, which has no element", () => {
    const route = resolveRoute("/");
    expect(route).toBeDefined();
    expect(route?.element).toBeUndefined();
  });

  it("returns undefined for a path with no matching entry at all", () => {
    expect(resolveRoute("/not-a-real-destination")).toBeUndefined();
  });
});

describe("isNavBuilt (the derived unbuilt set)", () => {
  // r1.s1.w3: the library row went live, so the flat "everything is unbuilt"
  // assertion w6 authored no longer holds. The derived form below is the
  // invariant that survives every future append (w4's included): a nav id is
  // built exactly when its ROUTES entry carries an element.
  it("is true exactly for nav ids whose ROUTES entry carries an element", () => {
    expect(isNavBuilt("library")).toBe(true);
    for (const route of ROUTES) {
      if (route.nav !== undefined) {
        expect(isNavBuilt(route.nav)).toBe(route.element !== undefined);
      }
    }
  });
});

describe("the details track stays table-driven (r1.s1.w3)", () => {
  it("a route declaring the details track also declares an element", () => {
    for (const route of ROUTES) {
      if (route.details === true) {
        expect(route.element).toBeDefined();
      }
    }
  });
});

describe("the path === '/' + nav convention, asserted over the real ROUTES", () => {
  it("holds for every entry that declares a nav", () => {
    for (const route of ROUTES) {
      if (route.nav !== undefined) {
        expect(route.path).toBe("/" + route.nav);
      }
    }
  });
});

// ---------------------------------------------------------------------------
// #269 — the Backtest Lab's deep link (`#/backtest?wf=<id>`)
// ---------------------------------------------------------------------------

describe("resolveNavId (a query is not part of the nav id — #269)", () => {
  it("strips a query before resolving the nav id", () => {
    // The Library's certification line lands here: the fragment names a run,
    // and the nav id is still the Backtest Lab.
    expect(resolveNavId("#/backtest?wf=wf-6c1d9f38", KNOWN_NAV_IDS)).toBe("backtest");
    expect(resolveNavId("/backtest?run=run-1", KNOWN_NAV_IDS)).toBe("backtest");
  });

  it("leaves every query-less reading exactly as it was", () => {
    expect(resolveNavId("#/backtest", KNOWN_NAV_IDS)).toBe("backtest");
    expect(resolveNavId("#/settings", KNOWN_NAV_IDS)).toBe("settings");
    expect(resolveNavId("#/", KNOWN_NAV_IDS)).toBe(DEFAULT_NAV_ID);
    expect(resolveNavId("", KNOWN_NAV_IDS)).toBe(DEFAULT_NAV_ID);
    // An unknown id is still unknown, query or not.
    expect(resolveNavId("#/not-a-real-destination?wf=1", KNOWN_NAV_IDS)).toBe(DEFAULT_NAV_ID);
  });
});

describe("backtestDeepLink (#269)", () => {
  it("reads the walk-forward run the Backtest Lab's fragment names", () => {
    expect(backtestDeepLink("#/backtest?wf=wf-6c1d9f38")).toEqual({ wf: "wf-6c1d9f38" });
  });

  it("is null for every fragment that does not ask for one", () => {
    expect(backtestDeepLink("#/backtest")).toBeNull();
    expect(backtestDeepLink("#/backtest?wf=")).toBeNull();
    expect(backtestDeepLink("#/library?wf=wf-1")).toBeNull();
    expect(backtestDeepLink("")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// r5.s2.w2 — the Chart tab's deep link (`#/chart?run=<id>`)
// ---------------------------------------------------------------------------

describe("chartDeepLink (r5.s2.w2)", () => {
  it("reads the backtest run the Chart tab's fragment names", () => {
    expect(chartDeepLink("#/chart?run=39486572")).toEqual({ run: "39486572" });
  });

  it("is null for every fragment that does not ask for one", () => {
    expect(chartDeepLink("#/chart")).toBeNull();
    expect(chartDeepLink("#/chart?run=")).toBeNull();
    expect(chartDeepLink("#/backtest?run=run-1")).toBeNull();
    expect(chartDeepLink("")).toBeNull();
  });

  it("resolves to the chart nav row with its query still attached", () => {
    // The tab reads its own fragment; the shell reads only the nav id, and the
    // query is not part of it (#269's rule, applied to the second deep link).
    expect(resolveNavId("#/chart?run=39486572", KNOWN_NAV_IDS)).toBe("chart");
  });
});
