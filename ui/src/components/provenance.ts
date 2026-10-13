// The version provenance label, shared by the Library and the Backtest Lab
// (r4.s5.w2).
//
// r2.s1.w4 C1's rule, in ONE place: an `external_agent` version reads
// `external_agent · <agent name>`, every other provenance reads its bare
// `created_by` label — and an agent version whose submission row is absent
// still reads `external_agent` (the label, never a guessed name). The Library
// rendered this inline from r2.s1.w4 until the picker needed the same line; a
// second copy is how two screens start disagreeing about who wrote a version.

import type { LibraryVersion } from "../bindings";

/** A version's provenance label, exactly as the Library shows it. */
export function provenanceLine(version: LibraryVersion): string {
  return version.agentName !== null
    ? `${version.createdBy} · ${version.agentName}`
    : version.createdBy;
}
