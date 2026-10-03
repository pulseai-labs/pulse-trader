// The route table (ADR-0020, bus contract clause 4) — the frontend half of the
// append-only registration contract.
//
// ONE table, ONE line per screen. Adding a screen means appending one entry here and
// one entry to `BUS_COMMANDS` in `src/tauri/commands.rs`. Nothing else.
//
// Why this shape is load-bearing rather than tidy: `r1.s1.w3` and `r1.s1.w4` run in
// PARALLEL in round 3, and each adds one screen. With one append-only table they
// conflict TEXTUALLY — two adjacent added entries, resolved by keeping both — and never
// SEMANTICALLY. A nested router, a per-screen registration call, or two tables would
// re-create the dependency edge the DAG was authored without. The round-3 plan dropped
// that edge on this property; weakening it is not a refactor, it is a replan.
//
// `tests/tauri_bus_contract.rs::the_route_table_is_one_append_only_entry_per_screen`
// asserts the shape: exactly one `ROUTES` table, one `path:` per line, no duplicates.
//
// r1.s1.w5 replaces the placeholder screen's rendering wholesale. It must keep the
// TABLE, not just the file.
//
// r1.s1.w5 retitles the one entry below (the placeholder page it replaces is gone,
// so "Shell placeholder" no longer describes anything) without adding a row -- w5
// mounts no product screen, so there is still exactly one entry. w3/w4 append
// theirs in round 3.

// r1.s1.w6 grows this table with two OPTIONAL fields (G7, G8) so the shell can
// navigate before any screen exists: `nav` names the sidebar row a route is
// reached from, and `element` is the screen to mount. Absent `element` means
// "not built yet" -- unbuilt-ness is DERIVED from that absence, never a
// separately-tracked flag (G8; see `isNavBuilt` below).
import type { ReactNode } from "react";
// `createElement`, not JSX: this file is the route TABLE (a `.ts` file by
// contract -- `tests/tauri_bus_contract.rs` reads this exact path), and esbuild
// parses no JSX in `.ts`. The entry below mounts the screen exactly as an
// `element: () => <DesignerScreen />` literal would.
import { createElement } from "react";

import BacktestLabScreen from "./screens/BacktestLabScreen";

import DeploymentDashboard from "./screens/DeploymentDashboard";

import DesignerScreen from "./screens/DesignerScreen";

import LibraryScreen from "./screens/LibraryScreen";

/** One screen in the shell. */
export interface Route {
  /** URL fragment that selects this screen. Unique across the table. */
  readonly path: string;
  /** Human-readable title, rendered in the app's own titlebar (w5). */
  readonly title: string;
  /**
   * The `Sidebar` nav-row id this route is reached from. When present, the
   * convention (asserted by `routes.test.ts` and `check-shell-navigation.sh`)
   * is `path === "/" + nav`.
   */
  readonly nav?: string;
  /** The screen to mount. Absent means the destination is not built yet. */
  readonly element?: () => ReactNode;
  /**
   * Whether this screen's layout uses the `.layout` grid's third (360px)
   * details track (r1.s1.w3). Table-driven on purpose: the shell opens the
   * track purely from this flag — never from a screen-name list in `App.tsx`,
   * which would be the second table G7 exists to prevent. A route declaring
   * it must also declare an `element` (asserted by `routes.test.ts`).
   */
  readonly details?: boolean;
}

export const ROUTES: readonly Route[] = [
  {
    path: "/",
    title: "PulseTrader",
  },
  // r1.s1.w3: the app's first real screen. Appending this entry upgrades the
  // default landing by itself (`DEFAULT_NAV_ID` is "library") — `isNavBuilt`
  // derives true and the nav row's "Soon" badge disappears with no nav-side
  // edit (G8). `element: LibraryScreen` — the zero-prop screen convention
  // `UnbuiltScreen.tsx` demonstrates; `details: true` opts the route back into
  // the third track `w6` collapsed.
  {
    path: "/library",
    title: "Strategy Library",
    nav: "library",
    element: LibraryScreen,
    details: true,
  },
  {
    path: "/designer",
    title: "Strategy Designer",
    nav: "designer",
    element: () => createElement(DesignerScreen),
  },
  // r1.s3.w4: the Backtest Lab goes live the same way — one appended entry
  // and the existing `backtest` nav row's derived "Soon" badge disappears on
  // its own (`isNavBuilt`, G8), with zero nav-side edits. `details` stays
  // unset: the screen is single-track (provenance band → result), not a
  // two-pane reader like the Library.
  {
    path: "/backtest",
    title: "Backtest Lab",
    nav: "backtest",
    element: BacktestLabScreen,
  },
  // r3.s4.w5: the Deployment Dashboard goes live the same way — one appended
  // entry, and the existing `deploy` nav row's derived "Soon" badge disappears
  // on its own (`isNavBuilt`, G8), with zero nav-side edits. `details: true`
  // opts into the third track, where the selected session's detail renders
  // (the Library's two-pane shape).
  {
    path: "/deploy",
    title: "Deployment Dashboard",
    nav: "deploy",
    element: DeploymentDashboard,
    details: true,
  },
];

/** Resolve a fragment to a route, or `undefined` when nothing matches. */
export function resolveRoute(fragment: string): Route | undefined {
  return ROUTES.find((route) => route.path === fragment);
}

/**
 * The default landing nav (G8). Nothing in `ROUTES` is its destination --
 * `/` is the table's non-empty floor, never a normalization target (see
 * `resolveNavId`'s header comment).
 */
export const DEFAULT_NAV_ID = "library";

/**
 * Normalize a `location.hash` value (with or without its leading `#`) to a
 * nav id, given the sidebar's known nav ids (r1.s1.w6, G5/G6/G8).
 *
 * There is exactly ONE normalization target: an empty fragment, `#/`, and any
 * fragment whose id is not one of `knownNavIds` all become `DEFAULT_NAV_ID`.
 * A recognized nav id is returned as-is, whether or not it is built yet --
 * "unrecognized" means "not a real nav row", not "not built".
 */
export function resolveNavId(hash: string, knownNavIds: readonly string[]): string {
  const fragment = hash.startsWith("#") ? hash.slice(1) : hash;
  if (fragment === "" || fragment === "/") {
    return DEFAULT_NAV_ID;
  }
  const candidate = fragment.startsWith("/") ? fragment.slice(1) : fragment;
  return knownNavIds.includes(candidate) ? candidate : DEFAULT_NAV_ID;
}

/**
 * Whether a nav id's destination is built (G8: derived, never pre-seeded). A
 * nav id is built iff a `ROUTES` entry declares that `nav` AND carries an
 * `element`.
 */
export function isNavBuilt(navId: string): boolean {
  return resolveRoute("/" + navId)?.element !== undefined;
}
