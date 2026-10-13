// The certification badge (r3.s4.w5, spec §3) — certified / uncertified /
// fixture / stale, shared by the Library, the promote sheet and the session
// screens.
//
// `stale` is E3's addition: a certification from another engine build is
// SHOWN, never enforced — on a running session it is a note, not a blocker.
// `uncertified` carries the override reason as a tooltip, because the reason
// is part of the record (it is what the session was promoted on).
//
// `certKinds` (r4.s5.w4, #334) is the one place a session's badges are decided:
// a certified fixture session carries BOTH `fixture` and `certified` — the old
// single-kind pick showed the fixture badge and hid the certification — and the
// two session surfaces derive from it, never from a second copy of the rule.

import type { PaperSessionSummary } from "../bindings";

/** Which certification state the badge states. */
export type CertBadgeKind = "certified" | "uncertified" | "fixture" | "stale";

const LABELS: Record<CertBadgeKind, string> = {
  certified: "certified",
  uncertified: "uncertified",
  fixture: "fixture",
  stale: "stale",
};

/**
 * Every badge a session's summary carries, in render order: `fixture` when the
 * session's certified data are fixture snapshots, then the certification half
 * — `stale` when the certifying run's fingerprint is another build's,
 * `uncertified` for an override, `certified` otherwise.
 */
export function certKinds(summary: PaperSessionSummary): CertBadgeKind[] {
  const kinds: CertBadgeKind[] = [];
  if (summary.fixture) {
    kinds.push("fixture");
  }
  if (summary.certification_stale) {
    kinds.push("stale");
  } else if (summary.graduation.graduation === "override") {
    kinds.push("uncertified");
  } else {
    kinds.push("certified");
  }
  return kinds;
}

export function CertBadge({ kind, reason }: { kind: CertBadgeKind; reason?: string | null }) {
  return (
    <span
      className={`cbadge cb-${kind}`}
      tabIndex={kind === "uncertified" ? 0 : undefined}
      title={kind === "uncertified" && reason ? `Override reason: ${reason}` : undefined}
    >
      {/* The label is its own node so a screen reader (and a test) can read
          the state without the tooltip's text. */}
      <span className="cb-label">{LABELS[kind]}</span>
      {kind === "uncertified" && reason ? (
        <span className="cb-tip">
          <span className="cb-tip-lab">Override reason</span>“{reason}”
        </span>
      ) : null}
    </span>
  );
}
