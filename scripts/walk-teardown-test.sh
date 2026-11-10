#!/usr/bin/env bash
# The walk teardown's own test (r7.s1.w3, demo line d134).
#
# `scripts/walk-teardown.sh` is proved against ONE scratch server this script
# starts from the worktree's own `pulse` (--role prod, a scratch data dir) —
# never a real server. It takes no arguments and runs from any directory.
#
# Setup: build `pulse`, start the server, issue an app token and an agent token
# into 0600 files (a third file is a 0644 copy of the app token), log the agent
# token into a scratch MCP config, seed four strategies (K1 and K2 are the
# keep-list; OLD and X stand for what prod holds today) and two paper sessions
# (SX on X, SOLD on OLD). No token is ever held in a variable or an argument.
#
# The 22 checks, in order (each prints `PASS <name>` or `FAIL <name>: <why>`;
# all 22 are always attempted once setup succeeds):
#
#    1 usage                               help, no mode, and cleanup without a mode flag
#    2 tmp-path-refused                    a /tmp manifest is refused and nothing is created
#    3 start-writes-manifest               format, mode 0600, the before snapshot, a second start refused
#    4 interrupted-walk-manifest-survives  a walk killed mid-way leaves every recorded row
#    5 record-is-idempotent                a second record of the same id writes nothing
#    6 keep-id-in-manifest-refused         a keep id that is a manifest strategy refuses the teardown
#    7 agent-token-refused                 an agent token is refused (403 scope_refused)
#    8 loose-token-file-refused            a token file with group or other bits is refused
#    9 other-server-refused                a manifest started against another server is refused
#   10 manifest-ids-stopped-and-archived   the walk's own ids stop, then archive, sessions first
#   11 ids-outside-the-manifest-untouched  an id outside the manifest is never touched
#   12 pre-walk-manifest-ids-refused       a manifest id from the before snapshot is refused
#   13 keep-list-assertion-fails-on-extra  the keep-list check names the extras and exits 5
#   14 teardown-rerun-is-idempotent        the same teardown again changes nothing
#   15 cleanup-unknown-keep-refused        a keep id the server does not hold is refused
#   16 cleanup-dry-run-writes-nothing      the lists, the digest, and no write
#   17 cleanup-wrong-digest-refused        an apply with a wrong digest writes nothing
#   18 cleanup-stale-digest-refused        an apply after the live list changed writes nothing
#   19 cleanup-apply-leaves-the-keep-list  the apply leaves exactly the keep-list; nothing is deleted
#   20 walk-teardown-passes-on-a-clean-walk  a recorded walk tears down to exit 0
#   21 refused-rows-exit-4                 a pre-walk id in the manifest exits 4
#   22 token-never-in-output               neither token is in any log
#
# Every scratch path lives under ~/.cache/pulse-scratch/. On exit 0 the trap
# removes the whole scratch directory; on any other status it keeps only logs/.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
wt_script="$repo_root/scripts/walk-teardown.sh"

if (($# > 0)); then
  echo "usage: walk-teardown-test.sh" >&2
  exit 2
fi

scratch_parent="$HOME/.cache/pulse-scratch"
scratch="$scratch_parent/walk-teardown-test.$(date -u +%Y%m%dT%H%M%SZ).$$"
mkdir -p "$scratch_parent" || exit 1
mkdir -m 700 "$scratch" || exit 1
for sub in server run mcp walk1 walk2 logs; do
  mkdir -m 700 "$scratch/$sub" || exit 1
done

server_pid=""
finished=0
failed_checks=0

remove_under_scratch() {
  local path="$1"
  case "$path" in
    "$scratch"/*) rm -rf -- "$path" ;;
    *)
      echo "walk-teardown-test: refusing to remove $path: not under $scratch/" >&2
      return 1
      ;;
  esac
}

# Stop the scratch server: TERM, a bounded wait, then KILL.
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

on_exit() {
  local status=$?
  trap - EXIT INT TERM
  # A signal can reach here with status 0; only a finished, passing run is a pass.
  if ((status == 0 && finished == 0)); then
    status=1
  fi
  stop_one "$server_pid"
  rm -f "$scratch/run/app-token" "$scratch/run/agent-token" "$scratch/run/loose-token"
  local sub
  for sub in server run mcp walk1 walk2; do
    remove_under_scratch "$scratch/$sub" || true
  done
  if ((status == 0)); then
    case "$scratch" in
      "$scratch_parent"/walk-teardown-test.*) rm -rf -- "$scratch" ;;
    esac
  else
    echo "walk-teardown-test: logs kept at $scratch/logs"
  fi
  exit "$status"
}
trap on_exit EXIT INT TERM

setup_fail() {
  echo "walk-teardown-test: setup failed: $1"
  exit 1
}

# --- python helpers (each reads a token from its file; none takes it in argv) --

# py_api <server> <app token file> <command> [args...]
#   promote <version id>   POST a promote with a typed override; prints the session id
#   state                  sorted `strategy <id> archived=<bool>` and `session <id> <state>` lines
#   default-ids            sorted ids of the Library's default list
py_api() {
  python3 -I - "$@" <<'PY'
import json
import sys
import urllib.error
import urllib.request

server, token_file, command = sys.argv[1:4]
with open(token_file, encoding="utf-8") as handle:
    token = handle.read().strip()
opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def call(method, path, payload=None):
    data = None if payload is None else json.dumps(payload).encode("utf-8")
    request = urllib.request.Request(server + path, data=data, method=method)
    request.add_header("Authorization", "Bearer " + token)
    request.add_header("Accept", "application/json")
    if data is not None:
        request.add_header("Content-Type", "application/json")
    try:
        with opener.open(request, timeout=120) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as error:
        return error.code, None


if command == "promote":
    body = {
        "version_id": sys.argv[4],
        "override": {
            "reason": "walk-teardown-test",
            "pair": "BTCUSDT",
            "primary_timeframe": "15m",
            "htf_timeframe": "4h",
            "uses_d1": False,
        },
    }
    status, answer = call("POST", "/api/v1/paper/promote", body)
    if status != 201 or not isinstance(answer, dict):
        sys.exit("promote answered HTTP %s" % status)
    print(answer["id"])
elif command == "state":
    status, library = call("GET", "/api/v1/library-overview/with-archived")
    if status != 200:
        sys.exit("with-archived answered HTTP %s" % status)
    status, sessions = call("GET", "/api/v1/paper/sessions")
    if status != 200:
        sys.exit("sessions answered HTTP %s" % status)
    lines = [
        "strategy %s archived=%s" % (item["id"], "true" if item["archived"] else "false")
        for item in library["strategies"]
    ]
    lines += ["session %s %s" % (item["id"], item["status"]["state"]) for item in sessions]
    print("\n".join(sorted(lines)))
elif command == "default-ids":
    status, library = call("GET", "/api/v1/library-overview")
    if status != 200:
        sys.exit("library-overview answered HTTP %s" % status)
    print("\n".join(sorted(item["id"] for item in library["strategies"])))
else:
    sys.exit("unknown command")
PY
}

# digest_of <server> <keep csv> <stop csv> <archive csv>: the cleanup digest, from the byte layout.
digest_of() {
  python3 -I - "$@" <<'PY'
import hashlib
import sys

server, keep, stop, archive = sys.argv[1:5]


def ids(csv):
    return sorted(item for item in csv.split(",") if item)


parts = ["server\t%s\n" % server]
parts += ["keep\t%s\n" % item for item in ids(keep)]
parts += ["stop\t%s\n" % item for item in ids(stop)]
parts += ["archive\t%s\n" % item for item in ids(archive)]
print(hashlib.sha256("".join(parts).encode("utf-8")).hexdigest())
PY
}

sha256_of() {
  python3 -I -c '
import hashlib
import sys

with open(sys.argv[1], "rb") as handle:
    print(hashlib.sha256(handle.read()).hexdigest())
' "$1"
}

# One MCP tools/call through the relay, answered by the frame with `id: 2`. The
# connection comes from the scratch PULSE_CONFIG_DIR, never the operator's own.
mcp_call() {
  local params="$1"
  {
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"walk-teardown-test","version":"0.1.0"}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":%s}\n' "$params"
  } | PULSE_CONFIG_DIR="$scratch/mcp" "$pulse_bin" mcp | python3 -I -c '
import json
import sys

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

# submit <name>: one submit_strategy_version; prints "<strategy id> <version id>".
submit() {
  local name="$1" params frame
  params="$(python3 -I - "$repo_root/tests/fixtures/strategies/rsi-oversold-long.json" "$name" <<'PY'
import json
import sys

path, name = sys.argv[1:3]
with open(path, encoding="utf-8") as handle:
    dsl = json.load(handle)
dsl["name"] = name
print(json.dumps({
    "name": "submit_strategy_version",
    "arguments": {
        "dsl": dsl,
        "hypothesis": "walk-teardown-test: " + name,
        "strategy_name": name,
    },
}))
PY
)" || return 1
  frame="$(mcp_call "$params")" || return 1
  printf '%s' "$frame" | python3 -I -c '
import json
import sys

frame = json.load(sys.stdin)
result = frame.get("result")
if frame.get("error") is not None or result is None or result.get("isError") is True:
    sys.exit("submit_strategy_version refused: " + json.dumps(frame)[:300])
content = result["structuredContent"]
print(content["strategy_id"], content["version_id"])
'
}

promote() { py_api "$S" "$scratch/run/app-token" promote "$1"; }
state() { py_api "$S" "$scratch/run/app-token" state; }
default_ids() { py_api "$S" "$scratch/run/app-token" default-ids; }

# wt <log name> <args...>: run the script; its output goes to logs/<log name>.out; sets rc.
rc=0
last_log=""
wt() {
  last_log="$scratch/logs/$1.out"
  bash "$wt_script" "${@:2}" >"$last_log" 2>&1
  rc=$?
}

# new_strategy <id var> <version var> <name>: a setup step.
new_strategy() {
  local result
  result="$(submit "$3")" || setup_fail "submit_strategy_version for '$3'"
  printf -v "$1" '%s' "${result%% *}"
  printf -v "$2" '%s' "${result#* }"
}

new_session() { # <session var> <version id>
  local result
  result="$(promote "$2")" || setup_fail "promote of version $2"
  printf -v "$1" '%s' "$result"
}

st_before=""
st_after=""
snap_before() { st_before="$(state)" || setup_fail "state read"; }
snap_after() { st_after="$(state)" || setup_fail "state read"; }

# --- the check plumbing ----------------------------------------------------------

cur=""
problems=()
begin() {
  cur="$1"
  problems=()
}
need() { problems+=("$1"); }
rc_is() { [[ "$rc" -eq "$1" ]] || need "exit $rc, want $1"; }
has() { grep -qF -- "$1" "$last_log" || need "output lacks '$1'"; }
lacks() { grep -qF -- "$1" "$last_log" && need "output has '$1'"; }
unchanged() { [[ "$st_before" == "$st_after" ]] || need "the server state changed"; }
state_has() { grep -qxF -- "$1" <<<"$st_after" || need "state lacks '$1'"; }
end_check() {
  if ((${#problems[@]} == 0)); then
    echo "PASS $cur"
  else
    local message="" item
    for item in "${problems[@]}"; do
      message+="$item; "
    done
    echo "FAIL $cur: ${message%; }"
    failed_checks=$((failed_checks + 1))
  fi
}

# --- setup -------------------------------------------------------------------------

cd "$repo_root" || setup_fail "cannot enter $repo_root"

echo "walk-teardown-test: building the scratch server's binary (cargo build --bin pulse)"
cargo build --bin pulse >"$scratch/logs/build.log" 2>&1 || setup_fail "cargo build --bin pulse (see $scratch/logs/build.log)"
target_dir="$(cargo metadata --format-version 1 --no-deps | python3 -I -c '
import json
import sys

print(json.load(sys.stdin)["target_directory"])
')" || setup_fail "cargo metadata"
pulse_bin="$target_dir/debug/pulse"
[[ -x "$pulse_bin" ]] || setup_fail "$pulse_bin is not executable"

"$pulse_bin" serve --dev-loopback --bind 127.0.0.1:0 --role prod \
  --db "$scratch/server/pulse.db" --data-dir "$scratch/server" >"$scratch/logs/serve.log" 2>&1 &
server_pid=$!
port=""
deadline=$((SECONDS + 60))
while ((SECONDS < deadline)); do
  if grep -q "listening on " "$scratch/logs/serve.log"; then
    port="$(sed -n 's/.*listening on 127\.0\.0\.1:\([0-9][0-9]*\).*/\1/p' "$scratch/logs/serve.log" | tail -1)"
    break
  fi
  kill -0 "$server_pid" 2>/dev/null || setup_fail "the server exited before listening (see $scratch/logs/serve.log)"
  sleep 0.2
done
[[ -n "$port" ]] || setup_fail "no listening line within 60 s (see $scratch/logs/serve.log)"
S="http://127.0.0.1:$port"

stamp="$(date -u +%Y%m%dt%H%M%Sz)"
(
  umask 077
  "$pulse_bin" token issue --scope app --label "wtt-app-$stamp" --db "$scratch/server/pulse.db" >"$scratch/run/app-token"
) || setup_fail "token issue (app)"
(
  umask 077
  "$pulse_bin" token issue --scope agent --label "wtt-agent-$stamp" --db "$scratch/server/pulse.db" >"$scratch/run/agent-token"
) || setup_fail "token issue (agent)"
[[ -s "$scratch/run/app-token" && -s "$scratch/run/agent-token" ]] || setup_fail "a token file is empty"
cp "$scratch/run/app-token" "$scratch/run/loose-token" || setup_fail "loose token copy"
chmod 644 "$scratch/run/loose-token" || setup_fail "loose token chmod"

PULSE_CONFIG_DIR="$scratch/mcp" "$pulse_bin" mcp login --server "$S" <"$scratch/run/agent-token" >/dev/null \
  || setup_fail "pulse mcp login"

new_strategy K1 K1V "wtt keep one"
new_strategy K2 K2V "wtt keep two"
new_strategy OLD OLDV "wtt old"
new_strategy X XV "wtt outsider"
new_session SX "$XV"
new_session SOLD "$OLDV"
M1="$scratch/walk1/walk-manifest.tsv"
M2="$scratch/walk2/walk-manifest.tsv"
app_token="$scratch/run/app-token"
keep_both="$K1,$K2"

# --- 1 usage -----------------------------------------------------------------------

begin usage
help_out="$(bash "$wt_script" --help 2>/dev/null)"
help_rc=$?
[[ "$help_rc" -eq 0 ]] || need "--help exited $help_rc, want 0"
[[ "$help_out" == *"walk-teardown.sh cleanup  --keep"* ]] || need "--help stdout lacks the cleanup usage line"
wt usage-none
rc_is 2
wt usage-no-mode-flag cleanup --keep "$K1" --server "$S" --token-file "$app_token"
rc_is 2
end_check

# --- 2 tmp-path-refused ------------------------------------------------------------

begin tmp-path-refused
wt tmp-start start --manifest "/tmp/walk-teardown-test-$$/m.tsv" --server "$S" --token-file "$app_token"
rc_is 3
has "must not be under /tmp"
[[ ! -e "/tmp/walk-teardown-test-$$" ]] || need "/tmp/walk-teardown-test-$$ exists"
end_check

# --- 3 start-writes-manifest ---------------------------------------------------------

begin start-writes-manifest
wt start-1 start --manifest "$M1" --server "$S" --token-file "$app_token"
rc_is 0
if [[ -f "$M1" ]]; then
  IFS=$'\t' read -r m_kind m_instant m_server <"$M1"
  [[ "$m_kind" == "start" ]] || need "line 1 is not a start line"
  [[ "$m_instant" =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\.[0-9]{3}Z$ ]] || need "the start instant is malformed"
  [[ "$m_server" == "$S" ]] || need "the start line names '$m_server', want $S"
  got="$(awk -F'\t' '$1 == "before-strategy" { print $2 }' "$M1" | LC_ALL=C sort)"
  want="$(printf '%s\n' "$K1" "$K2" "$OLD" "$X" | LC_ALL=C sort)"
  [[ "$got" == "$want" ]] || need "the before-strategy rows are not {K1, K2, OLD, X}"
  got="$(awk -F'\t' '$1 == "before-session" { print $2 }' "$M1" | LC_ALL=C sort)"
  want="$(printf '%s\n' "$SX" "$SOLD" | LC_ALL=C sort)"
  [[ "$got" == "$want" ]] || need "the before-session rows are not {SX, SOLD}"
  mode="$(stat -c %a "$M1" 2>/dev/null || stat -f %Lp "$M1")"
  [[ "$mode" == "600" ]] || need "the manifest is mode $mode, want 600"
  if compgen -G "$M1.partial.*" >/dev/null; then need "a partial file is left behind"; fi
  sum_before="$(sha256_of "$M1")"
  wt start-2 start --manifest "$M1" --server "$S" --token-file "$app_token"
  rc_is 3
  has "already exists"
  [[ "$(sha256_of "$M1")" == "$sum_before" ]] || need "a second start changed the manifest"
else
  need "no manifest at $M1"
fi
end_check

# --- 4 interrupted-walk-manifest-survives --------------------------------------------

begin interrupted-walk-manifest-survives
(
  result="$(submit "wtt walk a")" || exit 1
  walk_a="${result%% *}"
  walk_a_version="${result#* }"
  wt walk-record-a record --manifest "$M1" --strategy "$walk_a"
  [[ "$rc" -eq 0 ]] || exit 1
  walk_sa="$(promote "$walk_a_version")" || exit 1
  wt walk-record-sa record --manifest "$M1" --session "$walk_sa"
  [[ "$rc" -eq 0 ]] || exit 1
  walk_sk="$(promote "$K1V")" || exit 1
  wt walk-record-sk record --manifest "$M1" --session "$walk_sk"
  [[ "$rc" -eq 0 ]] || exit 1
  result="$(submit "wtt walk b")" || exit 1
  walk_b="${result%% *}"
  wt walk-record-b record --manifest "$M1" --strategy "$walk_b"
  [[ "$rc" -eq 0 ]] || exit 1
  kill -KILL "$BASHPID"
)
sub_rc=$?
[[ "$sub_rc" -eq 137 ]] || need "the walk subshell exited $sub_rc, want 137"
mapfile -t last4 < <(tail -n 4 "$M1")
A="" SA="" SK="" B=""
if ((${#last4[@]} == 4)); then
  [[ "${last4[0]}" == strategy$'\t'* ]] || need "the fourth-from-last row is not a strategy row"
  [[ "${last4[1]}" == session$'\t'* ]] || need "the third-from-last row is not a session row"
  [[ "${last4[2]}" == session$'\t'* ]] || need "the second-from-last row is not a session row"
  [[ "${last4[3]}" == strategy$'\t'* ]] || need "the last row is not a strategy row"
  A="${last4[0]#*$'\t'}"
  SA="${last4[1]#*$'\t'}"
  SK="${last4[2]#*$'\t'}"
  B="${last4[3]#*$'\t'}"
else
  need "the manifest has fewer than four rows"
fi
snap_after
state_has "strategy $A archived=false"
state_has "strategy $B archived=false"
state_has "session $SA running"
state_has "session $SK running"
end_check

# --- 5 record-is-idempotent ----------------------------------------------------------

begin record-is-idempotent
lines_before="$(wc -l <"$M1")"
wt record-again record --manifest "$M1" --strategy "$A"
rc_is 0
[[ "$(cat "$last_log")" == "walk-teardown: already recorded strategy $A" ]] || need "the output is not the already-recorded line"
[[ "$(wc -l <"$M1")" == "$lines_before" ]] || need "the manifest's line count changed"
end_check

# --- 6 keep-id-in-manifest-refused ---------------------------------------------------

begin keep-id-in-manifest-refused
snap_before
wt keep-in-manifest teardown --manifest "$M1" --keep "$K1,$B" --server "$S" --token-file "$app_token"
snap_after
rc_is 3
has "keep-list id $B is in the manifest"
unchanged
end_check

# --- 7 agent-token-refused -----------------------------------------------------------

begin agent-token-refused
snap_before
wt agent-token teardown --manifest "$M1" --keep "$keep_both" --server "$S" --token-file "$scratch/run/agent-token"
snap_after
rc_is 3
has "403 scope_refused"
unchanged
end_check

# --- 8 loose-token-file-refused ------------------------------------------------------

begin loose-token-file-refused
snap_before
wt loose-token teardown --manifest "$M1" --keep "$keep_both" --server "$S" --token-file "$scratch/run/loose-token"
snap_after
rc_is 3
has "mode 644"
unchanged
end_check

# --- 9 other-server-refused ----------------------------------------------------------

begin other-server-refused
wt other-server teardown --manifest "$M1" --keep "$keep_both" --server "http://127.0.0.1:1" --token-file "$app_token"
rc_is 3
has "the manifest was started against"
end_check

# Three rows the walk did not create, as a bad manifest would carry them.
printf 'strategy\t%s\nsession\t%s\nsession\tno-such-session\n' "$OLD" "$SOLD" >>"$M1"

# --- 10 to 13: one teardown ----------------------------------------------------------

snap_before
wt teardown-1 teardown --manifest "$M1" --keep "$keep_both" --server "$S" --token-file "$app_token"
td1_rc=$rc
td1_log="$last_log"
snap_after

begin manifest-ids-stopped-and-archived
last_log="$td1_log"
has "stopped session $SA"
has "stopped session $SK"
has "archived strategy $A wtt walk a"
has "archived strategy $B wtt walk b"
has "unknown session no-such-session: the server does not know it; skipped"
awk '
  /^walk-teardown: (already )?stopped session / { last_session = NR }
  /^walk-teardown: (already archived|archived|refused) strategy / { if (!first_strategy) first_strategy = NR }
  END { exit !(last_session > 0 && first_strategy > 0 && last_session < first_strategy) }
' "$td1_log" || need "a session line does not come before every strategy line"
state_has "strategy $A archived=true"
state_has "strategy $B archived=true"
state_has "session $SA stopped"
state_has "session $SK stopped"
end_check

begin ids-outside-the-manifest-untouched
state_has "strategy $X archived=false"
state_has "session $SX running"
end_check

begin pre-walk-manifest-ids-refused
last_log="$td1_log"
has "refused strategy $OLD: predates the walk"
has "refused session $SOLD: predates the walk"
state_has "strategy $OLD archived=false"
state_has "session $SOLD running"
end_check

begin keep-list-assertion-fails-on-extra
last_log="$td1_log"
rc="$td1_rc"
rc_is 5
has "FAIL keep-list: listed but not kept: $X wtt outsider"
has "FAIL keep-list: listed but not kept: $OLD wtt old"
has "FAIL keep-list: running session $SX of strategy $X wtt outsider is off the keep-list"
has "keep-list check failed (exit 5)"
end_check

# --- 14 teardown-rerun-is-idempotent ---------------------------------------------------

begin teardown-rerun-is-idempotent
snap_before
wt teardown-2 teardown --manifest "$M1" --keep "$keep_both" --server "$S" --token-file "$app_token"
snap_after
rc_is 5
has "already stopped session $SA"
has "already archived strategy $A wtt walk a"
unchanged
end_check

# A keep-list session no manifest records: it must survive the cleanup.
new_session SK2 "$K2V"

# --- 15 cleanup-unknown-keep-refused ---------------------------------------------------

begin cleanup-unknown-keep-refused
wt cleanup-unknown cleanup --keep "$K1,no-such-strategy" --server "$S" --token-file "$app_token" --dry-run
rc_is 3
has "keep-list id no-such-strategy is not on the server"
end_check

# --- 16 cleanup-dry-run-writes-nothing --------------------------------------------------

begin cleanup-dry-run-writes-nothing
snap_before
wt cleanup-dry-1 cleanup --keep "$keep_both" --server "$S" --token-file "$app_token" --dry-run
snap_after
rc_is 0
has "walk-teardown: stop $SOLD wtt old"
has "walk-teardown: stop $SX wtt outsider"
has "walk-teardown: archive $OLD wtt old"
has "walk-teardown: archive $X wtt outsider"
has "walk-teardown: left running $SK2 wtt keep two"
has "walk-teardown: 2 to stop, 2 to archive"
has "walk-teardown: dry run; nothing written"
lacks "archive $A"
lacks "archive $B"
lacks "stop $SA"
lacks "stop $SK"
lacks "stop $SK2"
D1="$(sed -n 's/^walk-teardown: confirm: //p' "$last_log")"
want_digest="$(digest_of "$S" "$keep_both" "$SOLD,$SX" "$OLD,$X")"
[[ -n "$D1" && "$D1" == "$want_digest" ]] || need "the digest '$D1' is not the one computed from the byte layout"
unchanged
end_check

# --- 17 cleanup-wrong-digest-refused ----------------------------------------------------

begin cleanup-wrong-digest-refused
zeros="$(printf '0%.0s' {1..64})"
snap_before
wt cleanup-zeros cleanup --keep "$keep_both" --server "$S" --token-file "$app_token" --apply --confirm "$zeros"
snap_after
rc_is 3
has "does not match"
unchanged
end_check

# --- 18 cleanup-stale-digest-refused ------------------------------------------------------

begin cleanup-stale-digest-refused
new_strategy Y YV "wtt late"
snap_before
wt cleanup-stale cleanup --keep "$keep_both" --server "$S" --token-file "$app_token" --apply --confirm "$D1"
snap_after
rc_is 3
has "does not match"
unchanged
end_check

# --- 19 cleanup-apply-leaves-the-keep-list ------------------------------------------------

begin cleanup-apply-leaves-the-keep-list
wt cleanup-dry-2 cleanup --keep "$keep_both" --server "$S" --token-file "$app_token" --dry-run
D2="$(sed -n 's/^walk-teardown: confirm: //p' "$last_log")"
[[ -n "$D2" ]] || need "the second dry run printed no digest"
wt cleanup-apply cleanup --keep "$keep_both" --server "$S" --token-file "$app_token" --apply --confirm "$D2"
rc_is 0
has "keep-list holds: 2 strategies listed"
has "walk-teardown: ok"
snap_after
want="$(printf '%s\n' "$K1" "$K2" | LC_ALL=C sort)"
[[ "$(default_ids)" == "$want" ]] || need "the default list is not exactly {K1, K2}"
[[ "$(grep -c ' running$' <<<"$st_after")" == "1" ]] || need "the number of running sessions is not 1"
state_has "session $SK2 running"
[[ "$(grep -c '^strategy ' <<<"$st_after")" == "7" ]] || need "with-archived does not list 7 strategies"
[[ "$(grep -c '^session ' <<<"$st_after")" == "5" ]] || need "sessions does not list 5"
end_check

# --- 20 walk-teardown-passes-on-a-clean-walk ------------------------------------------------

begin walk-teardown-passes-on-a-clean-walk
walk_ok=1
wt start-walk2 start --manifest "$M2" --server "$S" --token-file "$app_token"
rc_is 0
result="$(submit "wtt walk c")" || { need "submit of walk c failed"; walk_ok=0; }
if ((walk_ok)); then
  C="${result%% *}"
  CV="${result#* }"
  wt walk2-record-c record --manifest "$M2" --strategy "$C"
  rc_is 0
  SC="$(promote "$CV")" || { need "promote of walk c failed"; walk_ok=0; }
fi
if ((walk_ok)); then
  wt walk2-record-sc record --manifest "$M2" --session "$SC"
  rc_is 0
  wt teardown-walk2 teardown --manifest "$M2" --keep "$keep_both" --server "$S" --token-file "$app_token"
  rc_is 0
  has "stopped session $SC"
  has "archived strategy $C wtt walk c"
  has "keep-list holds: 2 strategies listed"
  has "walk-teardown: ok"
  snap_after
  want="$(printf '%s\n' "$K1" "$K2" | LC_ALL=C sort)"
  [[ "$(default_ids)" == "$want" ]] || need "the default list is not exactly {K1, K2}"
  state_has "session $SK2 running"
  [[ "$(grep -c '^strategy ' <<<"$st_after")" == "8" ]] || need "with-archived does not list 8 strategies"
  [[ "$(grep -c '^session ' <<<"$st_after")" == "6" ]] || need "sessions does not list 6"
fi
end_check

# --- 21 refused-rows-exit-4 -----------------------------------------------------------------

begin refused-rows-exit-4
printf 'strategy\t%s\n' "$OLD" >>"$M2"
snap_before
wt teardown-walk2-again teardown --manifest "$M2" --keep "$keep_both" --server "$S" --token-file "$app_token"
snap_after
rc_is 4
has "refused strategy $OLD: predates the walk"
has "walk-teardown: done; refused rows above (exit 4)"
unchanged
end_check

# --- 22 token-never-in-output ---------------------------------------------------------------

begin token-never-in-output
grep -rqF -f "$scratch/run/app-token" "$scratch/logs"
[[ $? -eq 1 ]] || need "the app token (or a grep error) shows in the logs"
grep -rqF -f "$scratch/run/agent-token" "$scratch/logs"
[[ $? -eq 1 ]] || need "the agent token (or a grep error) shows in the logs"
end_check

if ((failed_checks == 0)); then
  echo "walk-teardown-test: OK (22 checks)"
  finished=1
else
  echo "walk-teardown-test: $failed_checks of 22 checks failed"
fi
# The last command sets the script's own status: 0 only when every check passed.
((failed_checks == 0))
