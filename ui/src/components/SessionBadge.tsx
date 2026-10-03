// The session badge (r3.s4.w5, spec §3) — starting / active / stopped / drift.
//
// The state is DERIVED from the summary, never a separately-tracked flag
// (G8's rule, applied here): a stopped session reads `stopped` whatever else
// its log says, a live epoch whose latest shadow check drifted reads `drift`,
// a session with no consumed bar yet reads `starting`, and everything else is
// `active`.
//
// "Catching up" is deliberately NOT a state: `SessionSummary` exposes no
// catch-up field (w4's summary carries the comparison, the epochs and the
// last bar, nothing about a replay in progress), and a badge that invented one
// would be the mock's fabrication, not the server's fact.

import type { PaperSessionSummary } from "../bindings";

/** The four states the badge renders. */
export type SessionBadgeState = "starting" | "active" | "stopped" | "drift";

const LABELS: Record<SessionBadgeState, string> = {
  starting: "starting",
  active: "active",
  stopped: "stopped",
  drift: "drift",
};

/**
 * Which state a session's summary selects.
 *
 * `stopped` wins first (A4: a stopped session is read-only whatever its
 * history), then a drifted LATEST shadow check (the live epoch's — the checks
 * are in epoch order, so the last one is the live epoch's when it has one),
 * then `starting` while no bar has been consumed, then `active`.
 */
export function sessionBadgeState(summary: PaperSessionSummary): SessionBadgeState {
  if (summary.status.state === "stopped") {
    return "stopped";
  }
  const latest = summary.shadow_checks.at(-1);
  if (latest?.result.verdict === "drift") {
    return "drift";
  }
  if (summary.last_bar_open_time === null) {
    return "starting";
  }
  return "active";
}

export function SessionBadge({ state }: { state: SessionBadgeState }) {
  return (
    <span className={`dep-status st-${state}`}>
      <span className={`dep-status-dot${state === "active" ? " pulse" : ""}`} />
      {LABELS[state]}
    </span>
  );
}
