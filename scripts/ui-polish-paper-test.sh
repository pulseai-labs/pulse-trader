#!/usr/bin/env bash
# The paper/title-strip/Settings ledger line (r4.s5.w4, spine r4.s5) — w4.
#
# One bounded command that proves the walk-filed paper, title-strip and Settings
# defects stay fixed, and exits non-zero the moment any of it stops being true:
#
#   1. **The seven files themselves.** Every case below is named by its issue
#      number in the test title (#338, #337, #333, #334, #336, #335, #386,
#      #389, #371, #382, #304, #319, #310): the session badge's `last seen`
#      read while the server is down and the card/detail agreement, the session
#      id and the version named separately, both badges on a certified fixture,
#      the certifying snapshots, `Promoting…` and the blocked second submit,
#      the strip count refreshed by a promote, the close-side exit fill and the
#      bar/placed times, the Settings buttons disabled after a disconnect, the
#      newest-issued-only status poll, the trailing refresh after a failed
#      summary read, the strip's no-wrap rule and the sidebar version read from
#      the status handshake.
#   2. **Types.** `tsc --noEmit` over the whole `ui/` tree.
#
# Bounded on purpose: seven vitest files and one tsc — not `just check`. This is
# a ledger line re-run at every future spine close, and a line that takes
# minutes is a line people stop running.
#
# `npm` is called from `PATH`: on the Mac and in CI Node 24 is already on it;
# on draco-desk the caller prefixes `~/.local/opt/node24/bin` per command.
#
# Exit 0 iff both stages pass; exit 1 naming EVERY stage that failed (no halt on
# the first — one run should tell you everything that is wrong).

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

failures=()
note_failure() { failures+=("$1"); }

# The seven files, relative to vitest's root (`ui/`). Named explicitly rather
# than globbed: this gate makes a claim about THESE properties, and a glob
# would quietly widen or narrow it as files are added and renamed.
ui_tests=(
  "src/screens/SessionDetail.test.tsx"
  "src/screens/DeploymentDashboard.test.tsx"
  "src/components/PromoteSheet.test.tsx"
  "src/screens/SettingsScreen.test.tsx"
  "src/shell/AppShell.test.tsx"
  "src/hooks/usePaperSession.test.ts"
  "src/hooks/useServerStatus.test.ts"
)

for rel in "${ui_tests[@]}"; do
  if [[ ! -f "ui/$rel" ]]; then
    echo "FAIL: ui/$rel does not exist — this gate names its test files explicitly," >&2
    echo "      so a renamed or deleted file is a failure rather than a silent skip" >&2
    note_failure "ui/$rel is missing"
  fi
done

# --- 1. the seven files -------------------------------------------------------
echo "ui-polish-paper-test: [1/2] npm run test -- --run (the seven files)"
if ! npm run test -- --run "${ui_tests[@]}"; then
  note_failure "npm run test -- --run ${ui_tests[*]}"
fi

# --- 2. types ------------------------------------------------------------------
echo "ui-polish-paper-test: [2/2] npm run typecheck"
if ! npm run typecheck; then
  note_failure "npm run typecheck"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "ui-polish-paper-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "ui-polish-paper-test: OK (the session badge's last-seen read with card and"
echo "  detail agreeing, the session id and version named separately, both badges"
echo "  and the certifying snapshots, Promoting… with the second submit blocked,"
echo "  the strip refreshed by a promote, the close-side fill and bar/placed"
echo "  times, the Settings buttons disabled after a disconnect, the"
echo "  newest-issued-only status poll, the trailing refresh after a failed read,"
echo "  the strip's no-wrap rule and the sidebar version from the handshake,"
echo "  types clean)"
