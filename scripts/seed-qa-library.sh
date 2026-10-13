#!/usr/bin/env bash
# The seeded QA library (r4.s5.w2, grill G1) — ~12 strategies × 9 versions
# (108 in total) written through the real MCP entry point
# (`submit_strategy_version`) on the QA server, so the version picker and the
# criterion-8 walk have a real 100+ library to search.
#
# ## RESET (read before the first run)
#
#   On QA, BEFORE the first seed:   just backup-now
#   To reset the library:           just restore <that backup>
#
#   `just restore` requires the server stopped — it is the deliberate,
#   operator-run reset; this script never deletes anything on the server.
#
# ## The guards, in the order they run (see the report's §7 for the deviation)
#
#   1. the arguments are validated; the scratch dir is created 0700;
#   2. the revoke `trap` is installed — BEFORE the first command that can fail;
#   3. the TOKEN-FREE refusal: `--db`'s directory must carry a `server-role`
#      marker reading exactly `qa`. No token exists yet at this point, so a run
#      aimed at a prod database refuses having issued nothing;
#   4. the run-scoped agent token is issued (label `seed-r4s5-<UTC stamp>`) and
#      held only in a 0600 file under `--scratch`;
#   5. the HANDSHAKE refusal: `/api/v1/handshake` must answer with a token this
#      database issued (`role` present and exactly `qa`). The handshake route
#      requires a valid token, so this half cannot run before step 4; it runs
#      before the FIRST MCP call, and the trap revokes on its way out;
#   6. only then does the first `submit_strategy_version` leave the machine.
#
# ## Hygiene (binding)
#
#   The token never enters argv (it reaches `pulse mcp login` on stdin; the
#   handshake probe reads it from its 0600 file), no log line or report carries
#   it, and this script never enables `xtrace`. The MCP connection file lives
#   under `--scratch` via `PULSE_CONFIG_DIR`, so the operator's own
#   `mcp-connection.toml` is neither read nor written. `trap on_exit EXIT INT
#   TERM` revokes the token by label and deletes the file on EVERY exit path.
#
# ## Idempotency
#
#   A strategy whose name already exists with >= 9 versions is skipped; one that
#   exists with fewer STOPS the run, naming `just restore` as the reset. The
#   run ends by printing the library's strategy and version counts read back
#   through `list_strategies`.
#
# `pulse` must be on PATH (the CLI that talks to the same server); `python3` is
# used for the JSON work and the handshake probe.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# draco-desk's QA server and data dir (ADR-0029) — overridable, and the test
# always overrides both.
QA_SERVER="http://100.90.203.21:8421"
QA_DB="${HOME}/.local/share/pulse-qa/pulse.db"

usage() {
  echo "usage: seed-qa-library.sh --scratch <dir> [--server <url>] [--db <path>]" >&2
  echo "  --scratch <dir>   run scratch (required; never /tmp)" >&2
  echo "  --server <url>    the server to seed (default: $QA_SERVER)" >&2
  echo "  --db <path>       that server's database (default: $QA_DB)" >&2
}

fail() {
  echo "seed-qa-library: refusing: $1" >&2
  exit 1
}

# --- arguments ---------------------------------------------------------------

server="$QA_SERVER"
db="$QA_DB"
scratch=""
while (($# > 0)); do
  case "$1" in
    --server)
      server="${2:?--server needs a URL}"
      shift 2
      ;;
    --db)
      db="${2:?--db needs a path}"
      shift 2
      ;;
    --scratch)
      scratch="${2:?--scratch needs a directory}"
      shift 2
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      echo "seed-qa-library: unknown argument: $1" >&2
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
  /tmp | /tmp/*) fail "--scratch must not be under /tmp" ;;
esac
[[ "$scratch" == /* ]] || fail "--scratch must be an absolute path"
case "$server" in
  http://* | https://*) ;;
  *) fail "--server must be an http(s) URL, got '$server'" ;;
esac
[[ "$db" == /* ]] || fail "--db must be an absolute path, got '$db'"
mkdir -p "$scratch" || fail "cannot create --scratch $scratch"
chmod 700 "$scratch"
server="${server%/}"
db_dir="$(dirname "$db")"

# --- the trap, BEFORE the first command that can fail -------------------------

label=""
token_file=""
mcp_config="$scratch/mcp-config"

on_exit() {
  local status=$?
  trap - EXIT INT TERM
  # Revoke by label, best effort: a run that never issued a token has nothing
  # to revoke, and the refusal is silent because the run is already failing.
  if [[ -n "$label" ]]; then
    pulse token revoke --label "$label" --db "$db" >/dev/null 2>&1 || true
  fi
  [[ -n "$token_file" ]] && rm -f "$token_file"
  rm -f "$mcp_config/mcp-connection.toml"
  exit "$status"
}
trap on_exit EXIT INT TERM

# --- 3. the token-free refusal: the data dir's own marker --------------------

if [[ ! -f "$db_dir/server-role" ]]; then
  fail "$db_dir has no server-role marker; seed only a QA data dir (a QA server writes it with --role qa)"
fi
marker_role="$(tr -d '[:space:]' <"$db_dir/server-role")"
if [[ "$marker_role" != "qa" ]]; then
  fail "$db_dir is marked '${marker_role:-<unreadable>}', not 'qa'"
fi

# --- 4. the run-scoped token, on stdout once, into a 0600 file ----------------

label="seed-r4s5-$(date -u +%Y%m%dT%H%M%SZ)"
token_file="$scratch/seed-token"
if ! token="$(pulse token issue --scope agent --label "$label" --db "$db")"; then
  fail "token issue failed (label '$label', db $db)"
fi
[[ -n "$token" ]] || fail "token issue printed nothing"
(umask 077; printf '%s\n' "$token" >"$token_file") || fail "cannot write $token_file"
file_mode="$(stat -c %a "$token_file" 2>/dev/null || echo "")"
if [[ -n "$file_mode" && "$file_mode" != "600" ]]; then
  fail "$token_file is mode $file_mode, not 0600"
fi

# --- 5. the handshake refusal: this token, THIS server, role `qa` -------------
#
# The probe reads the token from its file (never argv) and reports the HTTP
# outcome separately from the role, so the refusal names which half failed.

handshake="$(python3 - "$server" "$token_file" <<'PY'
import json, sys, urllib.error, urllib.request

server, token_file = sys.argv[1:3]
with open(token_file, encoding="utf-8") as handle:
    token = handle.read().strip()
request = urllib.request.Request(
    f"{server}/api/v1/handshake",
    headers={"authorization": f"Bearer {token}"},
)
try:
    with urllib.request.urlopen(request, timeout=10) as response:
        body = json.load(response)
except urllib.error.HTTPError as error:
    print(f"HTTP {error.code}")
    sys.exit(3)
except Exception as error:  # noqa: BLE001 — one refusal for any transport failure
    print(type(error).__name__)
    sys.exit(4)
print(body.get("role") or "none")
PY
)"
handshake_status=$?
if ((handshake_status == 3 || handshake_status == 4)); then
  fail "$server did not accept this database's token (is --server that database's server?)"
elif ((handshake_status != 0)); then
  fail "the handshake probe failed"
fi
if [[ "$handshake" != "qa" ]]; then
  fail "$server reports role '$handshake', not 'qa'"
fi

# --- the MCP connection, isolated into the scratch dir -----------------------

mkdir -p "$mcp_config" && chmod 700 "$mcp_config"
if ! printf '%s\n' "$token" | PULSE_CONFIG_DIR="$mcp_config" pulse mcp login --server "$server" >/dev/null; then
  fail "pulse mcp login failed for $server"
fi

# --- the MCP plumbing --------------------------------------------------------

# One tools/call through the relay: initialize + initialized + the call, all in
# one process (the relay's session is per-process), answered by the frame whose
# id is 2. Prints that frame.
mcp_call() {
  local params="$1"
  {
    printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"seed-r4s5","version":"0.1.0"}}}'
    printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
    printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":%s}\n' "$params"
  } | PULSE_CONFIG_DIR="$mcp_config" pulse mcp | python3 -c '
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

# The tool result's `structuredContent`, or a refusal naming what the server
# said. `submit_strategy_version` answers `{"version_id": ..., ...}`.
tool_result() {
  local frame="$1"
  printf '%s' "$frame" | python3 -c '
import json, sys
frame = json.load(sys.stdin)
result = frame.get("result")
if frame.get("error") is not None or result is None:
    sys.exit("the MCP layer refused the call: " + json.dumps(frame))
if result.get("isError") is True:
    sys.exit("the server refused it: " + json.dumps(result.get("structuredContent", result)))
print(json.dumps(result["structuredContent"]))
'
}

# `<name>\t<version count>` per strategy, read through `list_strategies`.
library_state() {
  local frame
  frame="$(mcp_call '{"name":"list_strategies","arguments":{}}')" || return 1
  tool_result "$frame" | python3 -c '
import json, sys
library = json.load(sys.stdin)
for strategy in library["strategies"]:
    print(strategy["name"] + "\t" + str(len(strategy["versions"])))
'
}

# The tools/call params for ONE submission, built from the committed known-valid
# DSL. `variant` 0 is the strategy's root; 1..8 are its children, each varying
# exactly one parameter value from the version it clones (v7 on v1, v8 on v4 —
# "some chained", grill G1). Per-strategy bases differ, so all 108 documents are
# distinct. Prints the params JSON.
build_submission() {
  local family="$1" name="$2" ordinal="$3" variant="$4" parent="$5"
  python3 - "$repo_root" "$family" "$name" "$ordinal" "$variant" "$parent" <<'PY'
import json
import sys
from decimal import Decimal

repo_root, family, name, ordinal, variant, parent = sys.argv[1:7]
ordinal = int(ordinal)
variant = int(variant)

if family == "rsi":
    template = f"{repo_root}/tests/fixtures/strategies/rsi-oversold-long.json"
else:
    template = f"{repo_root}/examples/strategies/sr-breakout-4h.json"
with open(template, encoding="utf-8") as handle:
    dsl = json.load(handle)
dsl["name"] = name

# (parameter values, one-line description) for this variant, relative to the
# variant it clones: the root is the base, each child moves ONE value.
if family == "rsi":
    period = 8 + ordinal % 12
    threshold = 25 + ordinal % 8
    stop = Decimal("0.020") + Decimal("0.005") * (ordinal % 6)
    target = Decimal("1.0") + Decimal("0.5") * (ordinal % 4)
    risk = Decimal("0.005") + Decimal("0.0025") * (ordinal % 4)
    leverage = 3
    if variant == 0:
        note = f"root: RSI({period}) < {threshold}, stop {stop}, {target}R"
    elif variant == 1:
        period += 2
        note = f"RSI period {period} (was {period - 2})"
    elif variant == 2:
        threshold += 3
        note = f"RSI threshold {threshold} (was {threshold - 3})"
    elif variant == 3:
        stop += Decimal("0.005")
        note = f"stop {stop} (was {stop - Decimal('0.005')})"
    elif variant == 4:
        target += Decimal("0.5")
        note = f"take-profit {target}R (was {target - Decimal('0.5')})"
    elif variant == 5:
        risk += Decimal("0.0025")
        note = f"risk {risk} per trade (was {risk - Decimal('0.0025')})"
    elif variant == 6:
        leverage += 1
        note = f"max leverage {leverage} (was {leverage - 1})"
    elif variant == 7:
        period += 4
        note = f"RSI period {period} — child of v1, two more than its parent"
    else:
        stop += Decimal("0.010")
        note = f"stop {stop} — child of v4, one step wider than its parent"
    dsl["entry"]["lhs"]["spec"]["period"] = period
    dsl["entry"]["rhs"]["value"] = str(threshold)
    dsl["exits"][0]["distance_pct"] = str(stop)
    dsl["exits"][1]["target_r"] = str(target)
    dsl["risk"]["risk_per_trade_pct"] = str(risk)
    dsl["risk"]["max_leverage"] = leverage
else:
    lookback = 100 + 10 * ordinal % 110
    tolerance = Decimal("0.005") * (2 ** (ordinal % 4))
    touches = 2 + ordinal % 2
    stop = Decimal("0.015") + Decimal("0.005") * (ordinal % 4)
    target = Decimal("1.0") + Decimal("0.5") * (ordinal % 3)
    risk = Decimal("0.010")
    if variant == 0:
        note = f"root: resistance lookback {lookback}, tolerance {tolerance}, {touches} touches"
    elif variant == 1:
        lookback += 50
        note = f"resistance lookback {lookback} (was {lookback - 50})"
    elif variant == 2:
        tolerance *= 2
        note = f"resistance tolerance {tolerance} (was {tolerance / 2})"
    elif variant == 3:
        touches += 1
        note = f"resistance min_touches {touches} (was {touches - 1})"
    elif variant == 4:
        stop += Decimal("0.005")
        note = f"stop {stop} (was {stop - Decimal('0.005')})"
    elif variant == 5:
        target += Decimal("0.5")
        note = f"take-profit {target}R (was {target - Decimal('0.5')})"
    elif variant == 6:
        risk += Decimal("0.0025")
        note = f"risk {risk} per trade (was {risk - Decimal('0.0025')})"
    elif variant == 7:
        lookback += 100
        note = f"resistance lookback {lookback} — child of v1, two steps on"
    else:
        stop += Decimal("0.010")
        note = f"stop {stop} — child of v4, one step wider than its parent"
    resistance = dsl["entry"]["rhs"]["value"]["spec"]
    resistance["lookback"] = lookback
    resistance["tolerance_pct"] = str(tolerance)
    resistance["min_touches"] = touches
    dsl["exits"][0]["distance_pct"] = str(stop)
    dsl["exits"][1]["target_r"] = str(target)
    dsl["risk"]["risk_per_trade_pct"] = str(risk)

target_field = {"strategy_name": name} if parent == "" else {"parent_version_id": parent}
params = {
    "name": "submit_strategy_version",
    "arguments": {
        "dsl": dsl,
        "hypothesis": f"r4s5 seed: {name} — {note}",
        **target_field,
    },
}
print(json.dumps(params))
PY
}

# --- the 12 strategies -------------------------------------------------------

# `family|name` — 12 DISTINCT names, three per pair in the descriptor, both DSL
# families (the submission carries no pair field: the descriptor and the
# hypothesis are where the pair lives, grill G1).
strategies=(
  "rsi|r4s5 BTC RSI pullback long"
  "rsi|r4s5 BTC RSI oversold fade"
  "rsi|r4s5 BTC RSI trend rejoin"
  "rsi|r4s5 ETH RSI pullback long"
  "rsi|r4s5 ETH RSI oversold fade"
  "rsi|r4s5 ETH RSI trend rejoin"
  "breakout|r4s5 SOL S/R breakout 4h"
  "breakout|r4s5 SOL S/R reclaim 4h"
  "breakout|r4s5 SOL range expansion 4h"
  "breakout|r4s5 XRP S/R breakout 4h"
  "breakout|r4s5 XRP S/R reclaim 4h"
  "breakout|r4s5 XRP range expansion 4h"
)

echo "seed-qa-library: reading the library at $server"
state="$(library_state)" || fail "list_strategies failed against $server"
declare -A existing_counts=()
while IFS=$'\t' read -r existing_name existing_count; do
  if [[ -n "$existing_name" ]]; then
    existing_counts["$existing_name"]="$existing_count"
  fi
done <<<"$state"

seeded=0
skipped=0
ordinal=0
for entry in "${strategies[@]}"; do
  family="${entry%%|*}"
  name="${entry#*|}"
  existing="${existing_counts[$name]:-}"
  if [[ -n "$existing" ]]; then
    if ((existing >= 9)); then
      echo "seed-qa-library: skipping '$name' — $existing versions already there"
      skipped=$((skipped + 1))
      continue
    fi
    fail "'$name' already exists with $existing versions (fewer than 9); reset the library first: just backup-now, then just restore <backup>"
  fi

  # The root, then its eight children in order. `variant_ids[0]` is the root;
  # v7 clones v1 and v8 clones v4, so their parent ids come from the ids just
  # recorded.
  declare -A variant_ids=()
  for variant in 0 1 2 3 4 5 6 7 8; do
    if ((variant == 0)); then
      parent=""
    elif ((variant == 7)); then
      parent="${variant_ids[1]:-}"
    elif ((variant == 8)); then
      parent="${variant_ids[4]:-}"
    else
      parent="${variant_ids[0]:-}"
    fi
    if [[ -z "$parent" && "$variant" != "0" ]]; then
      fail "internal: no parent id recorded for '$name' v$((variant + 1))"
    fi
    params="$(build_submission "$family" "$name" "$ordinal" "$variant" "$parent")" ||
      fail "cannot build the submission for '$name' v$((variant + 1))"
    frame="$(mcp_call "$params")" || fail "the relay failed for '$name' v$((variant + 1))"
    result="$(tool_result "$frame")" || fail "submitting '$name' v$((variant + 1)) failed"
    version_id="$(printf '%s' "$result" | python3 -c '
import json, sys
print(json.load(sys.stdin)["version_id"])
')" || fail "the answer for '$name' v$((variant + 1)) carries no version id"
    variant_ids["$variant"]="$version_id"
    echo "seed-qa-library: $name v$((variant + 1)) -> $version_id"
  done
  ordinal=$((ordinal + 1))
  seeded=$((seeded + 1))
done

# --- the counts, read back through the same read the Library uses -------------

state="$(library_state)" || fail "the closing list_strategies failed"
strategy_total=0
version_total=0
while IFS=$'\t' read -r count_name count_versions; do
  if [[ -n "$count_name" ]]; then
    strategy_total=$((strategy_total + 1))
    version_total=$((version_total + count_versions))
  fi
done <<<"$state"

echo "seed-qa-library: $strategy_total strategies, $version_total versions (seeded $seeded, skipped $skipped)"
if ((strategy_total < 12 || version_total < 100)); then
  fail "the library reads $strategy_total strategies / $version_total versions; a 100+ library was the point"
fi
