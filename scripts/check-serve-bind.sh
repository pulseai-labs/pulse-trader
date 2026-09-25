#!/usr/bin/env bash
# r3.s3.w1 (d30) — the LIVE bind-policy check on draco-desk.
#
# Proves, on the real machine with the real binary:
#   1. the server binds its TAILNET address (100.64.0.0/10) and NOT the LAN one;
#   2. an authenticated handshake answers over the tailnet;
#   3. SIGTERM shuts the server down cleanly (exit 0).
#
# NEVER prints the issued token: it lives in a shell variable only, and the
# script must never run under `set -x`. Everything it writes goes into one
# mktemp directory, removed on exit. Needs `tailscale` and at least one UP
# non-loopback non-tailscale0 interface (to prove the LAN refusal is possible).
set -euo pipefail

cd "$(dirname "$0")/.."
BIN="${PULSE_BIN:-target/debug/pulse}"

# --- Step 1: discover the addresses -----------------------------------------
TAILNET_IP="$(tailscale ip -4 | head -n1 | tr -d '[:space:]')"
if [[ -z "$TAILNET_IP" || "$TAILNET_IP" != 100.* ]]; then
  echo "check-serve-bind: no Tailscale IPv4 found ('${TAILNET_IP:-none}') — run on draco-desk with tailscaled up" >&2
  exit 2
fi
LAN_IP="$(ip -4 -brief addr show up | awk '$1 != "lo" && $1 != "tailscale0" {print $3}' | head -n1 | cut -d/ -f1 | tr -d '[:space:]')"
if [[ -z "$LAN_IP" ]]; then
  echo "check-serve-bind: no UP non-loopback non-tailscale0 IPv4 found — cannot prove the LAN refusal" >&2
  exit 2
fi

# --- Step 2: a throwaway token in a throwaway DB ------------------------------
TMP="$(mktemp -d)"
SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill -TERM "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT
DB="$TMP/pulse.db"
LOG="$TMP/serve.log"
LABEL="check-serve-bind-$$"
# Captured, never echoed. A refused issue dies here under set -e.
TOKEN="$("$BIN" token issue --scope app --label "$LABEL" --db "$DB")"

# --- Step 3: start the server on an ephemeral tailnet port --------------------
"$BIN" serve --bind "$TAILNET_IP:0" --db "$DB" 2>"$LOG" &
SERVER_PID=$!
BOUND=""
for _ in $(seq 1 50); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "check-serve-bind: server exited during startup:" >&2
    cat "$LOG" >&2
    exit 1
  fi
  BOUND="$(sed -n 's/^pulse serve: listening on //p' "$LOG" | head -n1 | tr -d '[:space:]')"
  if [[ -n "$BOUND" ]]; then
    break
  fi
  sleep 0.2
done
if [[ -z "$BOUND" ]]; then
  echo "check-serve-bind: no 'listening on' line within 10s:" >&2
  cat "$LOG" >&2
  exit 1
fi
PORT="${BOUND##*:}"

# --- Step 4: the policy, live --------------------------------------------------
# The LAN address must refuse the connection (no listener there).
if timeout 2 bash -c "exec 3<>/dev/tcp/$LAN_IP/$PORT" 2>/dev/null; then
  echo "check-serve-bind: FAIL — $LAN_IP:$PORT accepted a connection; the bind leaked off the tailnet" >&2
  exit 1
fi
# The tailnet address must answer the authenticated handshake.
BODY="$(curl -fsS -m 5 -H "Authorization: Bearer $TOKEN" "http://$TAILNET_IP:$PORT/api/v1/handshake")"
if ! printf '%s' "$BODY" | grep -q '"api_version":1'; then
  echo "check-serve-bind: FAIL — handshake over the tailnet did not report api_version 1" >&2
  exit 1
fi

# --- Step 5: SIGTERM is a clean shutdown ---------------------------------------
kill -TERM "$SERVER_PID"
RC=0
wait "$SERVER_PID" || RC=$?
SERVER_PID=""
if [[ "$RC" -ne 0 ]]; then
  echo "check-serve-bind: FAIL — the server exited $RC on SIGTERM, expected 0" >&2
  exit 1
fi

echo "check-serve-bind: OK — tailnet $BOUND answered, $LAN_IP refused, SIGTERM clean"
