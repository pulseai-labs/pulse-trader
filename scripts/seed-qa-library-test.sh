#!/usr/bin/env bash
# The seeded QA library's own test (r4.s5.w2, G1 / AC-2).
#
# The seed is proved against scratch servers THIS script starts from the
# worktree's own `pulse` — never `pulse-qa.service`, never its data dir:
#
#   1. **The QA half.** `--role qa` on a fresh scratch data dir; the seed runs
#      against it and must land 12 strategies / 108 versions, checked through
#      MCP `list_strategies` with this script's OWN short-lived agent token —
#      and the seed's own token must be REVOKED when it exits.
#   2. **The prod refusal (the token-free half).** A second scratch server with
#      `--role prod`; the seed must exit non-zero with a one-line reason naming
#      the marker, BEFORE issuing any token — `pulse token list` on the prod
#      database stays empty and no token file is left behind.
#   3. **The handshake half.** Two staged servers whose handshake cannot pass:
#      a qa-marked database whose server runs WITHOUT `--role` (the handshake
#      then reports no role at all), and a qa-marked database asked to seed a
#      server that does not hold its token. Both must refuse before the first
#      MCP call, and the count on the QA database must not move.
#
# Every scratch path lives under `--scratch` (required, never `/tmp`). On every
# exit path the trap stops the scratch servers, revokes this script's own token,
# removes the scratch DATA DIRS it created — strictly beneath `--scratch`,
# behind a guard that refuses any other path — and KEEPS the logs and the
# counts under `--scratch` as evidence.
#
# `cargo build --bin pulse` runs first: the servers must be the worktree's own
# binary. The build is the only slow step; the rest is bounded I/O.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

usage() {
  echo "usage: seed-qa-library-test.sh --scratch <dir>" >&2
  echo "  --scratch <dir>   scratch root for this run (required; never /tmp)" >&2
}

scratch=""
while (($# > 0)); do
  case "$1" in
    --scratch)
      scratch="${2:?--scratch needs a directory}"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "seed-qa-library-test: unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [[ -z "$scratch" ]]; then
  usage
  exit 2
fi
case "$scratch" in
  /tmp | /tmp/*)
    echo "seed-qa-library-test: refusing: --scratch must not be under /tmp" >&2
    exit 2
    ;;
esac
if [[ "$scratch" != /* ]]; then
  echo "seed-qa-library-test: refusing: --scratch must be an absolute path" >&2
  exit 2
fi

mkdir -p "$scratch" || exit 1
chmod 700 "$scratch"

qa_dir="$scratch/qa"
prod_dir="$scratch/prod"
unmarked_dir="$scratch/unmarked"
logs_dir="$scratch/logs"
test_run_dir="$scratch/test-run"
test_mcp_dir="$scratch/test-mcp"
mkdir -p "$logs_dir" "$test_mcp_dir"
chmod 700 "$test_mcp_dir"

# --- the trap: everything this run created goes with it, except the evidence --
server_pids=()
last_server_pid=""
test_label=""
test_token_file="$test_run_dir/test-token"
failures=()

remove_under_scratch() {
  local path="$1"
  case "$path" in
    "$scratch"/*) rm -rf -- "$path" ;;
    *)
      echo "seed-qa-library-test: refusing to remove $path: not under $scratch" >&2
      return 1
      ;;
  esac
}

# Stop ONE scratch server: TERM, a bounded wait, then KILL.
stop_one() {
  local pid="$1"
  [[ -n "$pid" ]] || return 0
  kill -TERM "$pid" 2>/dev/null || true
  local deadline=$((SECONDS + 15))
  while kill -0 "$pid" 2>/dev/null && ((SECONDS < deadline)); do
    sleep 0.1
  done
  kill -KILL "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
}

stop_servers() {
  local pid
  for pid in "${server_pids[@]}"; do
    stop_one "$pid"
  done
  server_pids=()
}

on_exit() {
  local status=$?
  trap - EXIT INT TERM
  stop_servers
  if [[ -n "${pulse_bin:-}" && -n "$test_label" && -n "${qa_db:-}" ]]; then
    "$pulse_bin" token revoke --label "$test_label" --db "$qa_db" >/dev/null 2>&1 || true
  fi
  rm -f "$test_token_file" "$test_mcp_dir/mcp-connection.toml"
  remove_under_scratch "$qa_dir" || true
  remove_under_scratch "$prod_dir" || true
  remove_under_scratch "$unmarked_dir" || true
  exit "$status"
}
trap on_exit EXIT INT TERM

note_failure() { failures+=("$1"); }

report_failures() {
  if ((${#failures[@]} > 0)); then
    local failure
    for failure in "${failures[@]}"; do
      echo "FAIL: $failure" >&2
    done
    echo "seed-qa-library-test: ${#failures[@]} failure(s)" >&2
    exit 1
  fi
}

echo "seed-qa-library-test: building the scratch servers (cargo build --bin pulse)"
cargo build --bin pulse || {
  echo "FAIL: cargo build --bin pulse" >&2
  exit 1
}
target_dir="$(cargo metadata --format-version 1 --no-deps | python3 -c '
import json, sys
print(json.load(sys.stdin)["target_directory"])
')" || {
  echo "FAIL: cargo metadata" >&2
  exit 1
}
pulse_bin="$target_dir/debug/pulse"
pulse_dir="$(dirname "$pulse_bin")"
if [[ ! -x "$pulse_bin" ]]; then
  echo "FAIL: $pulse_bin is not executable" >&2
  exit 1
fi

# --- helpers -----------------------------------------------------------------

# Start one scratch server; on success sets `last_server_port` (and
# `last_server_pid`, already in `server_pids`). Deliberately NO command
# substitution at the call sites: a `$(start_server …)` runs in a subshell, so
# the pid would never reach the parent and the trap would leave the server — and
# its database lock — behind. The `listening on` line is the readiness signal
# and the log is the evidence; the wait is bounded.
start_server() {
  local role_flag="$1" # "--role qa" | "--role prod" | "" (unmarked)
  local dir="$2"
  local log="$3"
  mkdir -p "$dir"
  # shellcheck disable=SC2086 # $role_flag is deliberately word-split
  "$pulse_bin" serve --dev-loopback --bind 127.0.0.1:0 $role_flag \
    --db "$dir/pulse.db" --data-dir "$dir" >"$log" 2>&1 &
  local pid=$!
  server_pids+=("$pid")
  last_server_pid="$pid"
  last_server_port=""
  local deadline=$((SECONDS + 60))
  while ((SECONDS < deadline)); do
    if grep -q "listening on " "$log"; then
      last_server_port="$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "$log" | tail -1)"
      if [[ -z "$last_server_port" ]]; then
        echo "seed-qa-library-test: the listening line in $log names no loopback port" >&2
        return 1
      fi
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "seed-qa-library-test: the server exited before listening; see $log" >&2
      return 1
    fi
    sleep 0.2
  done
  echo "seed-qa-library-test: no listening line within 60 s; see $log" >&2
  return 1
}

# One MCP tools/call through the relay, answered by the response frame with
# `id: 2`. The connection comes from the scratch `PULSE_CONFIG_DIR`, never the
# operator's real connection file.
mcp_call() {
  local params="$1"
  {
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"seed-qa-library-test","version":"0.1.0"}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":%s}\n' "$params"
  } | PULSE_CONFIG_DIR="$test_mcp_dir" "$pulse_bin" mcp | python3 -c '
import json, sys
found = None
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        frame = json.loads(line)
    except ValueError:
        continue
    if frame.get("id") == 2:
        found = frame
if found is None:
    sys.exit("no tools/call answer on the relay stream")
print(json.dumps(found))
'
}

# `list_strategies` through the relay; echoes "<strategies> <versions>".
count_strategies() {
  local frame
  frame="$(mcp_call '{"name":"list_strategies","arguments":{}}')" || return 1
  printf '%s' "$frame" | python3 -c '
import json, sys
frame = json.load(sys.stdin)
result = frame.get("result", {})
if frame.get("error") is not None or result.get("isError") is True:
    sys.exit("list_strategies refused: " + json.dumps(frame))
content = result["structuredContent"]
strategies = content["strategies"]
print(len(strategies), sum(len(strategy["versions"]) for strategy in strategies))
'
}

# --- 1. the QA half ----------------------------------------------------------

if ! start_server "--role qa" "$qa_dir" "$logs_dir/qa-serve.log"; then
  note_failure "the QA scratch server did not start"
  report_failures
fi
qa_port="$last_server_port"
qa_server="http://127.0.0.1:$qa_port"
qa_db="$qa_dir/pulse.db"
echo "seed-qa-library-test: QA scratch server on $qa_server"

echo "seed-qa-library-test: [1/3] the seed against the QA scratch server"
if PATH="$pulse_dir:$PATH" bash "$repo_root/scripts/seed-qa-library.sh" \
  --server "$qa_server" --db "$qa_db" --scratch "$scratch/qa-run" \
  >"$logs_dir/seed-qa-run.out" 2>"$logs_dir/seed-qa-run.err"; then
  echo "seed-qa-library-test: the seed exited 0; its counts:"
  sed -n 's/^/  /p' "$logs_dir/seed-qa-run.out"
else
  note_failure "the seed against the QA scratch server exited non-zero; stderr at $logs_dir/seed-qa-run.err"
  sed -n 's/^/  /p' "$logs_dir/seed-qa-run.err" >&2
fi

# The seed's own token is revoked, and nothing about it is left live.
qa_tokens="$(PATH="$pulse_dir:$PATH" "$pulse_bin" token list --db "$qa_db" 2>&1)"
echo "seed-qa-library-test: token rows in the QA database (label scope created state):"
sed -n 's/^/  /p' <<<"$qa_tokens"
if ! grep -Eq '^seed-r4s5-[^ ]+ agent [^ ]+ revoked:' <<<"$qa_tokens"; then
  note_failure "the seed's token is not revoked in the QA database; token list says: $qa_tokens"
fi
if grep -Eq '^seed-r4s5-[^ ]+ agent [^ ]+ active' <<<"$qa_tokens"; then
  note_failure "the seed left an ACTIVE token in the QA database"
fi

# The library itself, counted independently of the seed's own report: this
# script issues its own short-lived agent token for the read.
test_label="seed-test-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$test_run_dir"
if ! test_token="$(PATH="$pulse_dir:$PATH" "$pulse_bin" token issue \
  --scope agent --label "$test_label" --db "$qa_db")"; then
  note_failure "could not issue this test's own agent token"
elif [[ -z "$test_token" ]]; then
  note_failure "this test's own agent token came back empty"
else
  (umask 077; printf '%s\n' "$test_token" >"$test_token_file")
  if ! printf '%s\n' "$test_token" | PULSE_CONFIG_DIR="$test_mcp_dir" \
    "$pulse_bin" mcp login --server "$qa_server" >/dev/null 2>&1; then
    note_failure "this test's MCP login failed"
  else
    counts="$(count_strategies)" || note_failure "list_strategies through the relay failed"
    if [[ -n "${counts:-}" ]]; then
      read -r strategy_count version_count <<<"$counts"
      echo "seed-qa-library-test: list_strategies reports $strategy_count strategies, $version_count versions"
      if [[ "$strategy_count" != "12" ]]; then
        note_failure "the library holds $strategy_count strategies, expected 12"
      fi
      if [[ "$version_count" -lt 100 ]]; then
        note_failure "the library holds $version_count versions, expected >= 100"
      fi
      printf '%s strategies, %s versions\n' "$strategy_count" "$version_count" \
        >"$logs_dir/library-counts.txt"
    fi
  fi
  unset test_token
fi

# --- 2. the prod refusal (the token-free half) -------------------------------

echo "seed-qa-library-test: [2/3] the seed must refuse a prod scratch server"
if ! start_server "--role prod" "$prod_dir" "$logs_dir/prod-serve.log"; then
  note_failure "the prod scratch server did not start"
  report_failures
fi
prod_port="$last_server_port"
prod_db="$prod_dir/pulse.db"
if PATH="$pulse_dir:$PATH" bash "$repo_root/scripts/seed-qa-library.sh" \
  --server "http://127.0.0.1:$prod_port" --db "$prod_db" --scratch "$scratch/prod-run" \
  >"$logs_dir/seed-prod-run.out" 2>"$logs_dir/seed-prod-run.err"; then
  note_failure "the seed exited 0 against a prod scratch server"
else
  if ! grep -q "marked 'prod'" "$logs_dir/seed-prod-run.err"; then
    note_failure "the prod refusal does not name the marker; stderr says: $(cat "$logs_dir/seed-prod-run.err")"
  fi
  if ! grep -q "not 'qa'" "$logs_dir/seed-prod-run.err"; then
    note_failure "the prod refusal does not name qa as the requirement"
  fi
fi
prod_tokens="$(PATH="$pulse_dir:$PATH" "$pulse_bin" token list --db "$prod_db" 2>&1)"
if [[ -n "$prod_tokens" ]]; then
  note_failure "the seed issued a token on the prod scratch database: $prod_tokens"
fi
if [[ -e "$scratch/prod-run/seed-token" ]]; then
  note_failure "the prod refusal left a token file behind"
fi

# --- 3. the handshake half ---------------------------------------------------

echo "seed-qa-library-test: [3/3] the seed must refuse when the handshake is not qa"
# (a) A qa-MARKED data dir whose server runs WITHOUT `--role`: the marker passes
#     the local half, and the handshake then reports no role at all.
if ! start_server "--role qa" "$unmarked_dir" "$logs_dir/unmarked-init.log"; then
  note_failure "the qa-marked scratch server did not start"
  unmarked_port=""
else
  # Stop THAT server and bring the same data dir back up unmarked: the marker
  # persists, the server stops reporting a role.
  stop_one "$last_server_pid"
  if ! start_server "" "$unmarked_dir" "$logs_dir/unmarked-serve.log"; then
    note_failure "the unmarked scratch server did not start"
    unmarked_port=""
  else
    unmarked_port="$last_server_port"
  fi
fi
if [[ -n "$unmarked_port" ]]; then
  unmarked_db="$unmarked_dir/pulse.db"
  if PATH="$pulse_dir:$PATH" bash "$repo_root/scripts/seed-qa-library.sh" \
    --server "http://127.0.0.1:$unmarked_port" --db "$unmarked_db" \
    --scratch "$scratch/unmarked-run" \
    >"$logs_dir/seed-unmarked-run.out" 2>"$logs_dir/seed-unmarked-run.err"; then
    note_failure "the seed exited 0 against a server that reports no role"
  else
    if ! grep -q "reports role .* not 'qa'" "$logs_dir/seed-unmarked-run.err"; then
      note_failure "the role refusal does not name the reported role; stderr says: $(cat "$logs_dir/seed-unmarked-run.err")"
    fi
  fi
  unmarked_tokens="$(PATH="$pulse_dir:$PATH" "$pulse_bin" token list --db "$unmarked_db" 2>&1)"
  if grep -Eq '^seed-r4s5-[^ ]+ agent [^ ]+ active' <<<"$unmarked_tokens"; then
    note_failure "the role refusal left an ACTIVE token: $unmarked_tokens"
  fi
  if grep -Eq '^seed-r4s5-' <<<"$unmarked_tokens" && ! grep -Eq '^seed-r4s5-[^ ]+ agent [^ ]+ revoked:' <<<"$unmarked_tokens"; then
    note_failure "the role refusal issued a token that was not revoked: $unmarked_tokens"
  fi
fi

# (b) A qa-marked database asked to seed a server that does not hold its token:
#     the handshake is refused (HTTP 401), so the seed must refuse too — and the
#     QA library must not have grown.
if [[ -n "${version_count:-}" ]]; then
  if PATH="$pulse_dir:$PATH" bash "$repo_root/scripts/seed-qa-library.sh" \
    --server "http://127.0.0.1:$prod_port" --db "$qa_db" --scratch "$scratch/mismatch-run" \
    >"$logs_dir/seed-mismatch-run.out" 2>"$logs_dir/seed-mismatch-run.err"; then
    note_failure "the seed exited 0 when --server held a different database's token"
  else
    if ! grep -q "did not accept this database's token" "$logs_dir/seed-mismatch-run.err"; then
      note_failure "the mismatch refusal does not say the server refused the token; stderr says: $(cat "$logs_dir/seed-mismatch-run.err")"
    fi
  fi
  after_counts="$(count_strategies)" || note_failure "the post-refusal list_strategies failed"
  if [[ -n "${after_counts:-}" && "$after_counts" != "$counts" ]]; then
    note_failure "a refused run wrote to the QA library: $counts became $after_counts"
  fi
fi

report_failures

echo "seed-qa-library-test: OK (12 strategies / ${version_count:-0} versions through MCP"
echo "  list_strategies on a scratch QA server, the seed token revoked, the prod"
echo "  marker and both handshake refusals refused without a token or a write,"
echo "  scratch data dirs removed and the logs kept under $scratch)"
