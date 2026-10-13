#!/usr/bin/env bash
# The Library/Backtest Lab/Designer defects ledger line (r4.s5.w3, spine r4.s5) — w3.
#
# One bounded command that proves the walk-filed Library, Backtest Lab and
# Designer defects stay fixed, and exits non-zero the moment any of it stops
# being true:
#
#   1. **The screens themselves.** The four vitest files below carry one or more
#      tests per issue: #331 and #316 (a version's own backtest and a persisted
#      walk-forward open from the Runs list and by id, without starting a run),
#      #330 (the latest walk-forward's folds are labelled and grouped after the
#      version's own runs), #326 (the compare panel names both runs and surfaces
#      the inputs note), #269 (the certification run id is a button that
#      deep-links into the Lab), #322 (a failed composer tool call reads as
#      failed and its retry as a retry), #317 (the final step renders its done
#      mark and result line) and #325 (the hypothesis subline survives a wrapped
#      agent label) — plus the route table's query-stripping and deep-link read.
#   2. **The types.** `npm run typecheck`.
#
# Bounded on purpose: FOUR UI test files and one typecheck — not `just check`.
# This is a ledger line re-run at every future spine close, and a line that
# takes minutes is a line people stop running.
#
# `npm` is called from `PATH`: on the Mac and in CI Node 24 is already on it;
# on draco-desk the caller prefixes `~/.local/opt/node24/bin` per command.
#
# Exit 0 iff both stages pass; exit 1 naming EVERY stage that failed (no halt
# on the first — one run should tell you everything that is wrong).

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

failures=()
note_failure() { failures+=("$1"); }

# The four UI files, relative to vitest's root (`ui/`). Named explicitly rather
# than globbed: this gate makes a claim about THESE properties, and a glob
# would quietly widen or narrow it as files are added and renamed.
ui_tests=(
  "src/screens/BacktestLabScreen.test.tsx"
  "src/screens/LibraryScreen.test.tsx"
  "src/screens/DesignerScreen.test.tsx"
  "src/routes.test.ts"
)

for rel in "${ui_tests[@]}"; do
  if [[ ! -f "ui/$rel" ]]; then
    echo "FAIL: ui/$rel does not exist — this gate names its test files explicitly," >&2
    echo "      so a renamed or deleted file is a failure rather than a silent skip" >&2
    note_failure "ui/$rel is missing"
  fi
done

# --- 1. the screens ----------------------------------------------------------
echo "ui-polish-lab-test: [1/2] npm run test -- --run (Lab + Library + Designer + routes)"
if ! npm run test -- --run "${ui_tests[@]}"; then
  note_failure "npm run test -- --run ${ui_tests[*]}"
fi

# --- 2. the types ------------------------------------------------------------
echo "ui-polish-lab-test: [2/2] npm run typecheck"
if ! npm run typecheck; then
  note_failure "npm run typecheck"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "ui-polish-lab-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "ui-polish-lab-test: OK (a version's own runs and its walk-forward open"
echo "  without starting one, folds are labelled, the compare names both runs,"
echo "  the certification id deep-links, a failed composer call reads failed and"
echo "  the final step reads done)"
