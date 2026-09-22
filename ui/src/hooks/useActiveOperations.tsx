// The active-operation store (r1.s4.w3, `pulseai-labs/pulse-trader#141`) — the
// frontend half of single-flight and reattachment.
//
// The problem it solves: `App.tsx` resolves the route from `location.hash` and
// RE-MOUNTS the screen on every navigation. A backtest or a coach turn started in
// the Backtest Lab therefore lost its screen the moment the trader looked at the
// Library, and coming back gave them a blank Lab with a live, billable operation
// still running behind it — and an enabled Run button that started a second one.
//
// So the state lives ABOVE the route. `ActiveOperationsProvider` is mounted in
// `App.tsx` above `RouteContent`, where navigation cannot unmount it; it holds, per
// key, the in-flight promise and its settled result. A screen READS that: on
// remount it shows the running state and, when the promise settles, the result —
// without re-invoking anything. The promise is never cancelled on unmount, because
// the operation is not the screen's: a persisted run and a recorded coach turn both
// outlive the pane that asked for them.
//
// **`start` is the refusal point.** A key already in flight is refused HERE, before
// the bus is called. The backend's own latch refuses it again if reached (a second
// window, a double-click that beats a re-render); this one exists so the ordinary
// case never spends a round trip to be told no.
//
// **Nothing here starts anything on mount.** `start` is called from a click handler
// and from nowhere else — no effect in this file invokes a command.

import { createContext, useCallback, useContext, useMemo, useRef, useState } from "react";
import type { ReactNode } from "react";

import type { BusError } from "../bindings";

/** The shape every generated command returns. */
export type BusResult<T> = { status: "ok"; data: T } | { status: "error"; error: BusError };

/** What one key's operation is doing, and what it produced. */
export interface OperationRecord {
  /** Whether the invocation is still in flight. */
  readonly running: boolean;
  /** Its settled result, once it has one. `undefined` while running. */
  readonly outcome: BusResult<unknown> | undefined;
  /**
   * WHAT is running, for a key that several different calls share.
   *
   * The coach rail runs its turn and all three decisions under one key, because
   * the backend latches them under one key. Without a label a remount mid-accept
   * could only say "something is running" — the label is what lets it still say
   * "accepting", which is the difference between a screen that reattached and one
   * that merely noticed.
   */
  readonly label: string | undefined;
}

/** The store the shell provides and the screens read. */
export interface ActiveOperations {
  /** What `key` is doing, or `undefined` if it has never run. */
  lookup: (key: string) => OperationRecord | undefined;
  /** Start `invoke` under `key` — unless that key is already in flight. */
  start: (key: string, invoke: () => Promise<BusResult<unknown>>, label?: string) => void;
  /** Forget a settled record (a fresh selection, a new coaching session). */
  clear: (key: string) => void;
}

/** The key one version's backtest runs under. */
export function backtestKey(versionId: string): string {
  return `backtest:${versionId}`;
}

/**
 * The key one version's walk-forward runs under (r2.s3.w5).
 *
 * A fold row's "open run" does NOT use this key: a fold run is an ordinary
 * persisted run, so opening one starts `get_backtest_run` under the version's
 * own `backtestKey` — the existing result view answers with it, and the
 * store's in-flight refusal on that key is the only locking the open needs.
 */
export function walkForwardKey(versionId: string): string {
  return `walk-forward:${versionId}`;
}

/** The key one coaching session's turn and decisions run under. */
export function coachKey(sessionId: string): string {
  return `coach:${sessionId}`;
}

/** A rejected invocation, rendered as the one error shape the UI already knows. */
function bridgeError(error: unknown): BusResult<never> {
  return {
    status: "error",
    error: {
      code: "internal",
      message: error instanceof Error ? error.message : String(error),
      run_id: null, session_id: null, child_run_id: null,
    } satisfies BusError,
  };
}

/**
 * The store itself.
 *
 * `inFlight` is a ref, not state, and that is load-bearing: a second click in the
 * same tick has to be refused before React has re-rendered, and rendered state is
 * one render behind by definition. The ref decides; the state renders.
 */
function useOperationStore(): ActiveOperations {
  const [records, setRecords] = useState<Record<string, OperationRecord>>({});
  const inFlight = useRef<Set<string>>(new Set());
  /**
   * Who currently OWNS each key.
   *
   * Freeing the latch is not enough on its own: an abandoned operation's `settle`
   * closure is still live, and a late result from it would overwrite the record of
   * whatever started under the same key afterwards. The generation is minted in
   * `start`, captured by that start's own closure, and checked before the write —
   * so a settle whose generation is no longer the current one drops its result
   * instead of speaking for an operation that is not it.
   */
  const generation = useRef<Map<string, number>>(new Map());
  const nextGeneration = useRef(0);

  const start = useCallback(
    (key: string, invoke: () => Promise<BusResult<unknown>>, label?: string) => {
    if (inFlight.current.has(key)) {
      return;
    }
    inFlight.current.add(key);
    nextGeneration.current += 1;
    const mine = nextGeneration.current;
    generation.current.set(key, mine);
    setRecords((current) => ({
      ...current,
      [key]: { running: true, outcome: undefined, label },
    }));

    const settle = (outcome: BusResult<unknown>) => {
      // Only the CURRENT owner of the key may write to it. A late result from an
      // operation that was cleared, or superseded by a later start, is dropped:
      // it is no longer an answer to the question the key is asking.
      if (generation.current.get(key) !== mine) {
        return;
      }
      inFlight.current.delete(key);
      // No unmount guard: this provider lives above the route and is still
      // mounted, which is the entire point — the result has to land whether or
      // not the screen that asked for it is on screen.
      setRecords((current) => ({ ...current, [key]: { running: false, outcome, label } }));
    };

    // `invoke()` can throw BEFORE it returns a promise — a binding that is not
    // wired, a serialisation failure on the arguments. `.then` never runs on that
    // path, so without the try/catch the key stays in `inFlight` and the record
    // stays `running: true` for the lifetime of the app: the operation can never be
    // started again and never shows a result.
    try {
      invoke().then(settle, (error: unknown) => settle(bridgeError(error)));
    } catch (error: unknown) {
      settle(bridgeError(error));
    }
    },
    [],
  );

  const clear = useCallback((key: string) => {
    // The in-flight ref goes with the record. Dropping only the record leaves the
    // key latched, so the next start for it returns early against an operation
    // nothing is tracking any more — the failure card's own retry is the caller
    // that hits this first.
    inFlight.current.delete(key);
    // Give up ownership too, so the abandoned operation's settle closure finds a
    // generation that is no longer its own and drops its late result rather than
    // resurrecting a record the trader has moved on from.
    generation.current.delete(key);
    setRecords((current) => {
      const { [key]: _removed, ...rest } = current;
      return rest;
    });
  }, []);

  const lookup = useCallback((key: string) => records[key], [records]);

  return useMemo(() => ({ lookup, start, clear }), [lookup, start, clear]);
}

const ActiveOperationsContext = createContext<ActiveOperations | null>(null);

/**
 * Hold every active operation above the route, so navigating away and back
 * reattaches the same one.
 *
 * Mounted in `App.tsx` OUTSIDE `RouteContent`. Mounting it inside a screen would
 * put it back under the remount it exists to survive.
 */
export function ActiveOperationsProvider({ children }: { children: ReactNode }) {
  const store = useOperationStore();
  return (
    <ActiveOperationsContext.Provider value={store}>{children}</ActiveOperationsContext.Provider>
  );
}

/**
 * The shell's store, or a screen-local one when there is no shell.
 *
 * The fallback is deliberate and it is not a silent default: a screen mounted on
 * its own — a focused test, a future preview — keeps working with its own state,
 * exactly the behaviour it had before this item. What the PROVIDER adds is
 * survival across navigation, and that is asserted where it actually happens
 * (`App.test.tsx`'s remount regression, over the real `<App />`). Throwing here
 * instead would make the screen un-renderable outside the shell without making any
 * property truer.
 */
export function useActiveOperations(): ActiveOperations {
  const provided = useContext(ActiveOperationsContext);
  // Called unconditionally: hook order may not depend on whether a provider is
  // above us. An unused local store costs one empty object.
  const local = useOperationStore();
  return provided ?? local;
}
