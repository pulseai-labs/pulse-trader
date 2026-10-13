// The Backtest Lab's remembered version selection (r4.s5.w2, #315).
//
// The Lab RE-MOUNTS on every navigation (`App.tsx` resolves the route from
// `location.hash`) and, when the gate renders Connect in the app's place for a
// server restart, the screen unmounts with it. Screen state cannot outlive
// that, so the trader's choice lives here — the `paper/handoff.ts` pattern,
// applied to a remembered selection rather than a pending request.
//
// What this store does NOT do is decide whether the selection is still real.
// The CATALOG owns that: `loadCatalog` keeps the remembered id only while the
// fresh catalog carries it and falls back to the first option otherwise, so a
// version that vanished on the server can never be shown as selected. The run
// and walk-forward RESULTS are not here either — they live in
// `ActiveOperationsProvider`, keyed by version id, and are read back for
// whichever version the restored selection names.
//
// Session memory only, deliberately: this is a reconnect's lifetime, not a
// relaunch's (a persisted id would outlive the catalog that justified it, for
// nothing the walk asked for).

let lastSelectedVersionId: string | null = null;

/** The version the trader last chose in this session, or `null` before any. */
export function readBacktestSelection(): string | null {
  return lastSelectedVersionId;
}

/** Remember the trader's choice — every path that SELECTS a version calls this,
 * so a remount seeds from the same place the original choice came from. */
export function rememberBacktestSelection(versionId: string): void {
  lastSelectedVersionId = versionId;
}

/** Forget the remembered selection. No production caller: a version id from a
 * replaced server is dropped by the catalog's own reconcile. The test suite
 * calls it between cases so the singleton cannot leak a selection across them. */
export function clearBacktestSelection(): void {
  lastSelectedVersionId = null;
}
