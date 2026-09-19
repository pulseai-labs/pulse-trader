#!/usr/bin/env bash
# The desktop-parity ledger line (r2.s1.w4) — `d17`.
#
# One bounded command that proves the external-agent parity surfaces hold, and
# exits non-zero the moment any of them stops being true:
#
#   1. **Provenance and the compare on the wire.** `tests/tauri_library.rs`
#      projects `created_by`/`agent_name`/`hypothesis` onto `LibraryVersion`;
#      `tests/tauri_backtest.rs` drives `compare_child_run_core` over real
#      repositories — every typed refusal, and the `inputs_differ` verdict
#      against persisted `BacktestInputs`.
#   2. **The screens themselves.** The three vitest files below cover C1
#      (provenance line + hypothesis in the Library), C2 (refetch on
#      `focus`/`visibilitychange`, selection preserved), and C3 (the
#      `CompareWithParent` section and its `inputs differ` badge) — plus the
#      parked `AcceptedPanel` still rendering the extracted `CompareTable`
#      unchanged.
#   3. **The committed bindings still match.** `check-bindings.sh` re-proves
#      `ui/src/bindings.ts` equals the generated output — the mismatch that is
#      invisible to every suite above until a trader clicks the section.
#   4. **The command is registered in both lists.** A cheap structural
#      backstop: `compare_child_run` must appear in `BUS_COMMANDS` and in
#      `collect_commands!` — a command registered in one list and not the
#      other is a section that invokes into nothing.
#
# Bounded on purpose: TWO Rust test binaries, THREE UI test files, one
# existing gate and a grep — not `just check`. This is a ledger line re-run at
# every future spine close, and a line that takes minutes is a line people
# stop running.
#
# `npm` is called from `PATH`: on the Mac and in CI Node 24 is already on it;
# on draco-desk the caller prefixes `~/.local/opt/node24/bin` per command.
#
# Exit 0 iff all four stages pass; exit 1 naming EVERY stage that failed (no
# halt on the first — one run should tell you everything that is wrong).

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

failures=()
note_failure() { failures+=("$1"); }

# The three UI files, relative to vitest's root (`ui/`). Named explicitly
# rather than globbed: this gate makes a claim about THESE properties, and a
# glob would quietly widen or narrow it as files are added and renamed.
ui_tests=(
  "src/screens/LibraryScreen.test.tsx"
  "src/screens/BacktestLabScreen.test.tsx"
  "src/hooks/useRefetchOnFocus.test.tsx"
)

for rel in "${ui_tests[@]}"; do
  if [[ ! -f "ui/$rel" ]]; then
    echo "FAIL: ui/$rel does not exist — this gate names its test files explicitly," >&2
    echo "      so a renamed or deleted file is a failure rather than a silent skip" >&2
    note_failure "ui/$rel is missing"
  fi
done

# --- 1. provenance + compare over real repositories --------------------------
echo "ui-test: [1/4] cargo nextest run --test tauri_library --test tauri_backtest"
if ! cargo nextest run --test tauri_library --test tauri_backtest; then
  note_failure "cargo nextest run --test tauri_library --test tauri_backtest"
fi

# --- 2. the three screens' worth of UI evidence ------------------------------
echo "ui-test: [2/4] npm run test -- --run (C1 + C2 + C3)"
if ! npm run test -- --run "${ui_tests[@]}"; then
  note_failure "npm run test -- --run ${ui_tests[*]}"
fi

# --- 3. the generated bindings still match the commands ----------------------
echo "ui-test: [3/4] bash scripts/check-bindings.sh"
if ! bash scripts/check-bindings.sh; then
  note_failure "bash scripts/check-bindings.sh"
fi

# --- 4. the command is actually registered ------------------------------------
echo "ui-test: [4/4] compare_child_run is registered"
if ! grep -q '"compare_child_run",' src/tauri/commands.rs; then
  note_failure "BUS_COMMANDS does not list compare_child_run"
fi
if ! grep -q "commands::compare_child_run," src/tauri/mod.rs; then
  note_failure "collect_commands! does not register compare_child_run"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "ui-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "ui-test: OK (provenance + hypothesis on the wire, refetch on focus,"
echo "  compare with parent and the inputs-differ badge, committed bindings"
echo "  matching, compare_child_run registered in both lists)"
