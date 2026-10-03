// The certification badge (r3.s4.w5, spec §3) — certified / uncertified /
// fixture / stale, shared by the Library, the promote sheet and the session
// screens.
//
// `stale` is E3's addition: a certification from another engine build is
// SHOWN, never enforced — on a running session it is a note, not a blocker.
// `uncertified` carries the override reason as a tooltip, because the reason
// is part of the record (it is what the session was promoted on).

/** Which certification state the badge states. */
export type CertBadgeKind = "certified" | "uncertified" | "fixture" | "stale";

const LABELS: Record<CertBadgeKind, string> = {
  certified: "certified",
  uncertified: "uncertified",
  fixture: "fixture",
  stale: "stale",
};

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
