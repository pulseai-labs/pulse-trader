// The session badge (r3.s4.w5, spec §3; the not-live read r4.s5.w4) —
// active / stopped / drift, plus the `last seen <state> · <time> UTC` form the
// badge takes while the server is not answering.
//
// The state is DERIVED from the summary, never a separately-tracked flag
// (G8's rule, applied here): a stopped session reads `stopped` whatever else
// its log says, a live epoch whose latest shadow check drifted reads `drift`,
// and everything else is `active` FROM PROMOTION (r4.s5.w4, #337) — the server
// reports a session `running` from the moment it is promoted, so the old
// "no consumed bar yet reads starting" rule said `starting` about sessions the
// server had already started. No `starting` state exists any more: the wire's
// `PaperStatus` carries only `running` and `stopped`, and no field says "not
// yet started". If the server ever grows one, this function switches on it.
//
// While the server is unreachable (the app's one status poll reports anything
// but `up`), the badge never claims a live state (r4.s5.w4, #338): it reads
// `last seen <state> · <time> UTC`, the time being the last poll that answered
// `up`.

import type { PaperSessionSummary } from "../bindings";
import { timeUtc } from "../format/time";

/** The three states the badge renders. */
export type SessionBadgeState = "active" | "stopped" | "drift";

const LABELS: Record<SessionBadgeState, string> = {
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
 * then `active` — a session the server reports `running` is active, whether or
 * not it has consumed its first bar.
 */
export function sessionBadgeState(summary: PaperSessionSummary): SessionBadgeState {
  if (summary.status.state === "stopped") {
    return "stopped";
  }
  const latest = summary.shadow_checks.at(-1);
  if (latest?.result.verdict === "drift") {
    return "drift";
  }
  return "active";
}

/** How much of an id the paper surfaces show (the Library's own 8-character
 * convention). */
const SHORT_ID_LEN = 8;

/**
 * The short form of an id (r4.s5.w4, #333): one copy for the session card and
 * the detail header so two surfaces naming the same session cannot disagree.
 */
export function shortId(id: string): string {
  return id.slice(0, SHORT_ID_LEN);
}

export function SessionBadge({
  state,
  live = true,
  lastSeenAt = null,
}: {
  state: SessionBadgeState;
  /** Whether the server's last status read answered `up`. Absent means the
   * caller states nothing about liveness (the pre-r4.s5.w4 rendering). */
  live?: boolean;
  /** Epoch-ms of the last status poll that answered `up`, when one has. */
  lastSeenAt?: number | null;
}) {
  if (!live) {
    // The server is not answering: state exactly what was last seen — with the
    // time when there is one — and paint nothing live (no state class, no
    // pulse).
    const when = lastSeenAt === null ? "" : ` · ${timeUtc(String(lastSeenAt))}`;
    return (
      <span className="dep-status is-stale">
        <span className="dep-status-dot" />
        last seen {LABELS[state]}
        {when}
      </span>
    );
  }
  return (
    <span className={`dep-status st-${state}`}>
      <span className={`dep-status-dot${state === "active" ? " pulse" : ""}`} />
      {LABELS[state]}
    </span>
  );
}
