#!/usr/bin/env bash
# r4.s2.w1 (G9) — the daily rotation of the Mac Mini's prod server logs.
#
# Run by the launchd calendar job deploy/com.pulsetrader.logrotate.plist, as
# draco. It copy-truncates `serve.log` and `serve.err` into dated files and
# deletes dated files older than 7 days.
#
# COPY-then-truncate, never move: launchd holds the live files open and keeps
# appending to the inodes — a `mv` would leave the server writing to the moved
# file and the new one permanently empty. Truncating in place (`: > file`)
# resets the size while launchd's descriptor keeps working.
#
# It deletes ONLY under ~/Library/Logs/PulseTrader/, and only files matching
# the two dated patterns; `-maxdepth 1` plus the fixed LOG_DIR keep it there.
#
# bash 3.2-compatible: this also runs on macOS's /bin/bash.
set -euo pipefail

LOG_DIR="${HOME:?HOME is not set}/Library/Logs/PulseTrader"
KEEP_DAYS=7

# Nothing to rotate yet: the server has never written a log on this host.
[ -d "$LOG_DIR" ] || exit 0

STAMP="$(date +%Y-%m-%d)"

for name in serve.log serve.err; do
  file="$LOG_DIR/$name"
  [ -f "$file" ] || continue
  # An empty live file rotates to nothing.
  [ -s "$file" ] || continue
  if [ -f "$file.$STAMP" ]; then
    # A second run the same day appends to the day's file instead of
    # overwriting bytes already rotated out of the live file.
    cat "$file" >> "$file.$STAMP"
  else
    cp "$file" "$file.$STAMP"
  fi
  : > "$file"
done

# The dated copies older than KEEP_DAYS days.
find "$LOG_DIR" -maxdepth 1 -type f \( -name 'serve.log.*' -o -name 'serve.err.*' \) \
  -mtime "+$KEEP_DAYS" -delete
