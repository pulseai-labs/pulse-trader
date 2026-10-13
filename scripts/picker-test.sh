#!/usr/bin/env bash
# The version-picker ledger line (r4.s5.w2, spine r4.s5) — w2.
#
# One bounded command that proves the Backtest Lab's version picker finds a
# version in a 100+ library, groups by strategy, tells same-named strategies
# apart, and keeps the selection (and its shown result) across a catalog
# refetch — and exits non-zero the moment any of it stops being true:
#
#   1. **The picker itself.** `VersionPicker.test.tsx` renders the 120-version
#      fixture: filtering by strategy name, version label and short id; grouping
#      in catalog order; the #318 duplicate-name headers; the option text
#      (label, created date, short id, creator, pinned); the keyboard map with
#      focus held in the input; the empty-filter line; the disabled state.
#   2. **The screen that wears it.** `BacktestLabScreen.test.tsx` selects
#      through the picker and runs the chosen version, keeps the selection and
#      its shown result across a reconnect (#315), and falls back to the default
#      when the selected version is gone.
#   3. **The types.** `npm run typecheck`.
#
# Bounded on purpose: TWO UI test files and one typecheck — not `just check`.
# This is a ledger line re-run at every future spine close, and a line that
# takes minutes is a line people stop running.
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
# than globbed: this gate makes a claim about THESE properties, and a glob would
# quietly widen or narrow it as files are added and renamed.
ui_tests=(
  "src/components/VersionPicker.test.tsx"
  "src/screens/BacktestLabScreen.test.tsx"
)

for rel in "${ui_tests[@]}"; do
  if [[ ! -f "ui/$rel" ]]; then
    echo "FAIL: ui/$rel does not exist — this gate names its test files explicitly," >&2
    echo "      so a renamed or deleted file is a failure rather than a silent skip" >&2
    note_failure "ui/$rel is missing"
  fi
done

# --- 1. the picker and the screen that wears it ------------------------------
echo "picker-test: [1/2] npm run test -- --run (VersionPicker + BacktestLabScreen)"
if ! npm run test -- --run "${ui_tests[@]}"; then
  note_failure "npm run test -- --run ${ui_tests[*]}"
fi

# --- 2. the types ------------------------------------------------------------
echo "picker-test: [2/2] npm run typecheck"
if ! npm run typecheck; then
  note_failure "npm run typecheck"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "picker-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "picker-test: OK (the picker filters, groups, disambiguates and takes the"
echo "  keyboard; the Lab selects through it, keeps the selection and its shown"
echo "  result across a reconnect, and falls back when the version is gone)"
