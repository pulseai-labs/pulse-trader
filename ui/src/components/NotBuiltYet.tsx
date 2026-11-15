// The visible "not built yet" state for a designed control whose backend is not
// built (PDR-0049 decision 7, PDR-0011; r7 criterion 2): shown in the design's
// place, never hidden, never placeholder data (r7.s2.w4).

/** The exact words every not-built control shows. */
export const NOT_BUILT_YET = "not built yet";

/** `what` names the control it stands in for: the hover title and, for screen
 *  readers, a visually hidden prefix (review CR3, PR #45). */
export function NotBuiltYet({ what }: { what: string }) {
  return (
    <span className="not-built" title={`${what} — not built yet`}>
      <span className="sr-only">{`${what}: `}</span>
      {NOT_BUILT_YET}
    </span>
  );
}
