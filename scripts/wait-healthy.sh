#!/usr/bin/env bash
# r4.s2.w1 (#346) — the deploy health gate: prove the server ANSWERS, not only
# that a process exists.
#
# `just deploy` (draco-desk, systemd) and `just deploy-mac` (the Mini, launchd)
# both run this AFTER starting the service; before this, `systemctl is-active`
# read `active` through the whole 120-second bind retry, so a deploy whose bind
# never succeeded still reported success.
#
# Usage: wait-healthy.sh <url> [timeout-seconds]     (default timeout: 150)
#
# It polls `<url>/healthz` every 2 seconds until a 200 whose body carries
# `"status":"ok"` (Q1's shape). `degraded` keeps waiting: the paper runtime not
# running is not healthy. While `/healthz` does not exist yet — w4 lands the
# route (a 404 here) — it falls back to a 401 from `/api/v1/handshake` that
# carries `X-Pulse-Api-Version`: a refusal that proves the listener and the auth
# stack are answering. It says so when the fallback is what passed.
#
# On timeout it exits non-zero and prints the last response it saw.
#
# It sends no token, reads no credential and writes no file anywhere (no
# scratch, no /tmp). bash 3.2-compatible: this also runs on macOS's /bin/bash.
set -euo pipefail

URL="${1:?usage: wait-healthy.sh <url> [timeout-seconds]}"
TIMEOUT="${2:-150}"

case "$TIMEOUT" in
  '' | *[!0-9]*)
    echo "wait-healthy: timeout-seconds must be a whole number, got '$TIMEOUT'" >&2
    exit 2
    ;;
esac

case "$URL" in
  http://* | https://*) ;;
  *)
    echo "wait-healthy: <url> must start with http:// or https://, got '$URL'" >&2
    exit 2
    ;;
esac

BASE="${URL%/}"
DEADLINE=$(( $(date +%s) + TIMEOUT ))
LAST_HEALTHZ="(no response)"
LAST_HANDSHAKE=""
FALLBACK_SAID=0

while :; do
  resp="$(curl -sS --max-time 10 -w $'\n%{http_code}' "$BASE/healthz" 2>/dev/null || true)"
  code="${resp##*$'\n'}"
  body="${resp%$'\n'*}"

  if [ "$code" = "200" ]; then
    case "$body" in
      *'"status":"ok"'*)
        echo "wait-healthy: ${BASE}/healthz answered 200 with status=ok"
        exit 0
        ;;
    esac
  fi

  if [ "$resp" = "" ]; then
    LAST_HEALTHZ="(no response from curl)"
  else
    LAST_HEALTHZ="$code $body"
  fi

  # The pre-/healthz fallback (w4 lands the route): the handshake answers 401
  # to a tokenless request, and EVERY response carries the API-version header.
  if [ "$code" = "404" ]; then
    hresp="$(curl -sS --max-time 10 -o /dev/null -D - -w $'\n%{http_code}' "$BASE/api/v1/handshake" 2>/dev/null || true)"
    hcode="${hresp##*$'\n'}"
    hheaders="${hresp%$'\n'*}"
    LAST_HANDSHAKE="$hcode"
    case "$hheaders" in
      *[Xx]-[Pp]ulse-[Aa]pi-[Vv]ersion:*)
        if [ "$hcode" = "401" ]; then
          if [ "$FALLBACK_SAID" = "0" ]; then
            echo "wait-healthy: ${BASE}/healthz is not there yet; ${BASE}/api/v1/handshake answers 401 with X-Pulse-Api-Version - the listener and the auth stack are answering (pre-/healthz fallback)"
            FALLBACK_SAID=1
          fi
          exit 0
        fi
        ;;
    esac
  fi

  now="$(date +%s)"
  if [ "$now" -ge "$DEADLINE" ]; then
    break
  fi
  sleep 2
done

echo "wait-healthy: timed out after ${TIMEOUT}s waiting for ${BASE}/healthz" >&2
echo "last /healthz response: ${LAST_HEALTHZ}" >&2
if [ -n "$LAST_HANDSHAKE" ]; then
  echo "last /api/v1/handshake response: ${LAST_HANDSHAKE}" >&2
fi
exit 1
