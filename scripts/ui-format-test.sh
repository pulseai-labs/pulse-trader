#!/usr/bin/env bash
# The human-units ledger line (r4.s5.w1, spine r4.s5) — w1.
#
# One bounded command that proves every money, price, quantity, R, percent and
# time surface reads in A2's human units and stays wired to the server's exact
# text, and exits non-zero the moment any of it stops being true:
#
#   1. **The format module and the guard.** `ui/src/format/numbers.test.ts` and
#      `time.test.ts` pin every A2 row over the server's decimal text (the
#      half-away-from-zero rounding, the point-shift percent, the UTC suffix);
#      `ui/src/format/no-float-money.test.ts` reads the screens and components
#      back and fails on any `toFixed(` or unmarked float conversion.
#   2. **The screens themselves.** The five files below render the Backtest
#      Lab, the Library, the session detail, the deployment cards and the
#      promote sheet with their fixtures, asserting the A2 display text AND the
#      server's own string on the element (`title`/`data-exact`) — so "the
#      screen renders server text" stays proven after a display change.
#   3. **Types.** `tsc --noEmit` over the whole `ui/` tree.
#
# Bounded on purpose: three vitest files, five more, and one tsc — not
# `just check`. This is a ledger line re-run at every future spine close, and a
# line that takes minutes is a line people stop running.
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

# The files, relative to vitest's root (`ui/`). Named explicitly rather than
# globbed: this gate makes a claim about THESE properties, and a glob would
# quietly widen or narrow it as files are added and renamed.
format_tests=(
  "src/format/numbers.test.ts"
  "src/format/time.test.ts"
  "src/format/no-float-money.test.ts"
)
ui_tests=(
  "src/screens/BacktestLabScreen.test.tsx"
  "src/screens/LibraryScreen.test.tsx"
  "src/screens/SessionDetail.test.tsx"
  "src/screens/DeploymentDashboard.test.tsx"
  "src/components/PromoteSheet.test.tsx"
)

for rel in "${format_tests[@]}" "${ui_tests[@]}"; do
  if [[ ! -f "ui/$rel" ]]; then
    echo "FAIL: ui/$rel does not exist — this gate names its test files explicitly," >&2
    echo "      so a renamed or deleted file is a failure rather than a silent skip" >&2
    note_failure "ui/$rel is missing"
  fi
done

# --- 1. the format module and the no-float guard ------------------------------
echo "ui-format-test: [1/3] npm run test -- --run (format rules + guard)"
if ! npm run test -- --run "${format_tests[@]}"; then
  note_failure "npm run test -- --run ${format_tests[*]}"
fi

# --- 2. the five surfaces ------------------------------------------------------
echo "ui-format-test: [2/3] npm run test -- --run (the five screens)"
if ! npm run test -- --run "${ui_tests[@]}"; then
  note_failure "npm run test -- --run ${ui_tests[*]}"
fi

# --- 3. types ------------------------------------------------------------------
echo "ui-format-test: [3/3] npm run typecheck"
if ! npm run typecheck; then
  note_failure "npm run typecheck"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "ui-format-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "ui-format-test: OK (every A2 row pinned over exact decimal text, the no-float"
echo "  guard clean over the screens and components, the five surfaces rendering"
echo "  human units with the server's exact text on hover, types clean)"
