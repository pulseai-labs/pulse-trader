#!/usr/bin/env bash
# The paper money-view ledger line (r4.s4.w2, spine r4.s4) — w2.
#
# One bounded command that proves the session detail shows the server's money
# view and stays wired to it, and exits non-zero the moment any of it stops
# being true:
#
#   1. **The exact mirror over the wire.** `tests/tauri_paper.rs` decodes the
#      paper routes' JSON into the app DTOs and asserts the DTO's own
#      serialization equals the route's raw JSON field for field — over a
#      seeded session that holds an open position and a closed trade, so the
#      marked position and each trade's money fields are inside the comparison
#      (ADR-0020, ADR-0026; ruling 2, 2026-10-11 restored the exact mirror).
#   2. **The screens themselves.** `SessionDetail.test.tsx` renders a seeded
#      summary and trades — the running total, the marked open position with
#      its stops, the orders-and-fills rows (the entry fill included), each
#      closed trade's R and USDT and the last processed bar — asserting every
#      value is the fixture's own exact string, and that the absent ones render
#      as the dash (or "flat", or the stops note). `usePaperSession.test.ts`
#      drives the hook so a processed bar refetches the summary.
#   3. **The committed bindings still match.** `check-bindings.sh` re-proves
#      `ui/src/bindings.ts` equals a fresh generation — the mismatch that is
#      invisible to every suite above until a trader opens the screen.
#
# Bounded on purpose: ONE Rust test binary, TWO UI test files and one existing
# gate — not `just check`. This is a ledger line re-run at every future spine
# close, and a line that takes minutes is a line people stop running.
#
# `npm` is called from `PATH`: on the Mac and in CI Node 24 is already on it;
# on draco-desk the caller prefixes `~/.local/opt/node24/bin` per command.
#
# Exit 0 iff all three stages pass; exit 1 naming EVERY stage that failed (no
# halt on the first — one run should tell you everything that is wrong).

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

failures=()
note_failure() { failures+=("$1"); }

# The two UI files, relative to vitest's root (`ui/`). Named explicitly rather
# than globbed: this gate makes a claim about THESE properties, and a glob
# would quietly widen or narrow it as files are added and renamed.
ui_tests=(
  "src/screens/SessionDetail.test.tsx"
  "src/hooks/usePaperSession.test.ts"
)

for rel in "${ui_tests[@]}"; do
  if [[ ! -f "ui/$rel" ]]; then
    echo "FAIL: ui/$rel does not exist — this gate names its test files explicitly," >&2
    echo "      so a renamed or deleted file is a failure rather than a silent skip" >&2
    note_failure "ui/$rel is missing"
  fi
done

# --- 1. the DTO mirrors the routes exactly -----------------------------------
echo "paper-view-test: [1/3] cargo nextest run --test tauri_paper"
if ! cargo nextest run --test tauri_paper; then
  note_failure "cargo nextest run --test tauri_paper"
fi

# --- 2. the screen and the hook ----------------------------------------------
echo "paper-view-test: [2/3] npm run test -- --run (SessionDetail + usePaperSession)"
if ! npm run test -- --run "${ui_tests[@]}"; then
  note_failure "npm run test -- --run ${ui_tests[*]}"
fi

# --- 3. the generated bindings still match the DTOs --------------------------
echo "paper-view-test: [3/3] bash scripts/check-bindings.sh"
if ! bash scripts/check-bindings.sh; then
  note_failure "bash scripts/check-bindings.sh"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "paper-view-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "paper-view-test: OK (the DTO mirrors the paper routes exactly over a seeded"
echo "  session, the session detail renders the money view from server text and"
echo "  refetches per processed bar, committed bindings matching)"
