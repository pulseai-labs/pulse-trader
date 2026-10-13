// The walk-forward fold labels (r4.s5.w3, #330) — one owner for "which of a
// version's recent runs are walk-forward folds, and what are they called" —
// plus the one read-per-id those labels come from.
//
// The version's catalogue row names ONE walk-forward run
// (`LibraryVersion.latestWalkForwardRunId`), and reading that run is the only
// way to learn which ordinary runs are its folds: `dto.folds[].backtestRunId`
// plus `fold.index`. `LibraryRunSummary` itself carries no fold membership, so
// only the walk-forward the screen has READ can be marked — a fold of an older
// walk-forward stays an unmarked ordinary run (recorded as a known limit in
// r4.s5.w3's report §7; it needs a `walk_forward_run_id` on the summary).
//
// **One read per walk-forward id, for the whole session.** The Library's
// details pane and the Lab's Runs list each mount the hook for the version on
// screen, and the Lab's reopened pane reads the same run through the same
// cache — so a version the trader looks at from two places costs one
// `getWalkForwardRun`, and coming back to a version never re-reads it. A
// settled success stays cached (a persisted run is immutable — the rule the
// Lab's reopened-read hook states); a FAILED read is dropped, so the next
// consumer retries instead of inheriting the failure.

import { useEffect, useState } from "react";

import { commands } from "../bindings";
import type { LibraryRunSummary, WalkForwardRunDto } from "../bindings";
import type { BusResult } from "./useActiveOperations";

/** How much of an id a label shows: the picker's and the Library rows' own
 * short-id convention (`8`). */
const SHORT_ID_LEN = 8;

/** One run's membership in a walk-forward: its parent id and 0-based fold
 * position, exactly as `folds[]` carries them. */
export interface FoldRef {
  readonly walkForwardRunId: string;
  readonly index: number;
}

/** `backtestRunId` → its fold membership, for one walk-forward's DTO. */
export type FoldMap = ReadonlyMap<string, FoldRef>;

/** The fold membership of every one of a walk-forward's fold runs. */
export function foldMapOf(dto: WalkForwardRunDto): FoldMap {
  const folds = new Map<string, FoldRef>();
  for (const fold of dto.folds) {
    folds.set(fold.backtestRunId, {
      walkForwardRunId: dto.walkForwardRunId,
      index: fold.index,
    });
  }
  return folds;
}

/** The one label a fold run wears (#330's own example: `fold 2 of WF 076449c0`).
 * `index` is the wire's 0-based fold position; the label counts folds from 1. */
export function foldLabel(fold: FoldRef): string {
  return `fold ${fold.index + 1} of WF ${fold.walkForwardRunId.slice(0, SHORT_ID_LEN)}`;
}

/** One recent-run row: the run, and its fold membership when it has one. */
export interface FoldRunRow {
  readonly run: LibraryRunSummary;
  readonly fold: FoldRef | null;
}

/**
 * The list order #330 asks for: the version's OWN runs first, the walk-forward's
 * folds grouped after them — each group in the payload's own order (the list is
 * "Recent backtests", newest first, and this screen invents no second sort). A
 * run the fold map does not name — including an older walk-forward's fold — is
 * an own run here, which is the known limit, rendered rather than guessed at.
 */
export function orderRunsWithFolds(
  runs: readonly LibraryRunSummary[],
  folds: FoldMap,
): FoldRunRow[] {
  const own: FoldRunRow[] = [];
  const grouped: FoldRunRow[] = [];
  for (const run of runs) {
    const fold = folds.get(run.id);
    if (fold === undefined) {
      own.push({ run, fold: null });
    } else {
      grouped.push({ run, fold });
    }
  }
  return [...own, ...grouped];
}

/** What the fold read has: no id → idle; a read in flight → loading; a read
 * that failed → failed (no labels, never an invented one); a read that landed
 * → ready with its fold map. */
export type FoldReadState =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "failed" }
  | { kind: "ready"; folds: FoldMap };

/** The reads, by walk-forward id. A settled success stays (one read per id, ever);
 * a failure is dropped so the next consumer retries instead of inheriting it. */
const reads = new Map<string, Promise<BusResult<WalkForwardRunDto>>>();

/**
 * Read one walk-forward run, at most once per id per session.
 *
 * Exported because the Lab's reopened pane and its deep-link handler are two
 * more consumers of the same read: without one shared cache a link would spend
 * a second `getWalkForwardRun` naming the version the pane is about to read for
 * its folds anyway.
 */
export function readWalkForwardRunOnce(
  walkForwardRunId: string,
): Promise<BusResult<WalkForwardRunDto>> {
  const cached = reads.get(walkForwardRunId);
  if (cached !== undefined) {
    return cached;
  }
  const read = commands.getWalkForwardRun({ walkForwardRunId }).then(
    (result) => {
      if (result.status !== "ok") {
        reads.delete(walkForwardRunId);
      }
      return result;
    },
    (error: unknown) => {
      reads.delete(walkForwardRunId);
      throw error;
    },
  );
  reads.set(walkForwardRunId, read);
  return read;
}

/** Forget every cached read. No production caller: the test suite calls it
 * between cases so a fixture's read cannot leak into the next case's
 * call-count assertions (the `clearBacktestSelection` precedent). */
export function clearWalkForwardFoldReads(): void {
  reads.clear();
}

/**
 * The fold map for a version's walk-forward, read once per id.
 *
 * `null` (a version with no pointer) reads nothing and stays idle — a version
 * without a walk-forward has no folds to mark, so no request is spent asking.
 * An id change re-arms the read (the previous answer's `alive` flag is cleared,
 * so a superseded answer never lands on the new version's list).
 */
export function useWalkForwardFolds(walkForwardRunId: string | null): FoldReadState {
  const [state, setState] = useState<FoldReadState>({ kind: "idle" });

  useEffect(() => {
    if (walkForwardRunId === null) {
      setState({ kind: "idle" });
      return;
    }
    let alive = true;
    setState({ kind: "loading" });
    readWalkForwardRunOnce(walkForwardRunId)
      .then((result) => {
        if (!alive) return;
        setState(
          result.status === "ok"
            ? { kind: "ready", folds: foldMapOf(result.data) }
            : { kind: "failed" },
        );
      })
      .catch(() => {
        // The IPC call itself failing — no labels, never a fabricated one.
        if (alive) setState({ kind: "failed" });
      });
    return () => {
      alive = false;
    };
  }, [walkForwardRunId]);

  return state;
}
