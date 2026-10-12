#!/usr/bin/env bash
# The Connect/onboarding ledger line (r4.s3.w3, spine r4.s3) — w3.
#
# One bounded command that proves the Connect and Settings screens render from
# server outcomes and stays wired to them, and exits non-zero the moment any of
# it stops being true:
#
#   1. **The screens themselves.** `ConnectScreen.test.tsx` renders the whole
#      `App` with mocked bindings and asserts every state the mock names (idle,
#      connecting with its three pending steps, connected with the handshake's
#      own facts, refused, agent token, unreachable, skew), the step marking
#      that is derived from the one `server_connect` outcome and nothing else,
#      the reveal toggle, the copyable recovery command and the one storage
#      sentence. `SettingsScreen.test.tsx` asserts the "Server connection"
#      section: the state from the polled status, the address from
#      `server_address`, "Change server" opening Connect, and Disconnect's
#      confirm step and its result sentence. `App.test.tsx` pins the #247 gate:
#      an explicit refusal takes the app down on the FIRST poll that reports it,
#      while a dropped poll keeps today's tolerance.
#   2. **The committed bindings still match.** `check-bindings.sh` re-proves
#      `ui/src/bindings.ts` equals a fresh generation — the mismatch that is
#      invisible to every suite above until a trader opens the screen.
#
# Bounded on purpose: THREE UI test files and one existing gate — not
# `just check`. This is a ledger line re-run at every future spine close, and a
# line that takes minutes is a line people stop running.
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

# The three UI files, relative to vitest's root (`ui/`). Named explicitly rather
# than globbed: this gate makes a claim about THESE properties, and a glob would
# quietly widen or narrow it as files are added and renamed.
ui_tests=(
  "src/screens/ConnectScreen.test.tsx"
  "src/screens/SettingsScreen.test.tsx"
  "src/App.test.tsx"
)

for rel in "${ui_tests[@]}"; do
  if [[ ! -f "ui/$rel" ]]; then
    echo "FAIL: ui/$rel does not exist — this gate names its test files explicitly," >&2
    echo "      so a renamed or deleted file is a failure rather than a silent skip" >&2
    note_failure "ui/$rel is missing"
  fi
done

# --- 1. the Connect, Settings and gate files ---------------------------------
echo "connect-view-test: [1/2] npm run test -- --run (Connect + Settings + the gate)"
if ! npm run test -- --run "${ui_tests[@]}"; then
  note_failure "npm run test -- --run ${ui_tests[*]}"
fi

# --- 2. the generated bindings still match the commands ----------------------
echo "connect-view-test: [2/2] bash scripts/check-bindings.sh"
if ! bash scripts/check-bindings.sh; then
  note_failure "bash scripts/check-bindings.sh"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "connect-view-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "connect-view-test: OK (every Connect state and the outcome-only step marking,"
echo "  the reveal toggle and the storage sentence, the Settings server section"
echo "  with Disconnect's confirm and its result, the #247 first-poll gate,"
echo "  committed bindings matching)"
