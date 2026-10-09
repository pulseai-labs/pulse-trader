#!/usr/bin/env bash
# r4.s2.w5 (C3, spec §Approach 4) — the off-box backup pull: draco-desk fetches
# the Mini's nightly backups into its own directory, additively.
#
# The Mini never pushes (draco-desk:22 is closed from the Mini; a pull keeps
# the Mini free of an outbound credential). This script runs from
# deploy/pulse-backup-pull.timer (daily 04:30, Persistent=true) through
# deploy/pulse-backup-pull.service, with the dedicated read-only key
# ~/.ssh/pulse_backup_ed25519. On the Mini that key is forced to
# deploy/pulse-backup-serve.sh, which can only READ ~/pulse-backups.
#
# Usage: pulse-backup-pull.sh <source> [dest]
#   <source>  the ssh target in the unit (`macmini:/Users/draco/pulse-backups`
#             — an ABSOLUTE remote path: a forced command expands no `~`), or
#             a LOCAL directory (the tests; they never contact the Mini).
#   [dest]    where the off-box copy lives; default
#             `$HOME/pulse-backups-offbox/mini` (env PULSE_OFFBOX_DIR).
#
# Environment:
#   PULSE_SSH_KEY       the pull key (default `$HOME/.ssh/pulse_backup_ed25519`)
#   PULSE_OFFBOX_KEEP   how many `pulse-*.db` to keep locally (default 30)
#   PULSE_BIN           the pulse binary `backup-verify` runs
#                       (default `$HOME/.local/share/pulse-qa/bin/pulse` —
#                       draco-desk's live deployment after the cutover)
#
# The contract, in order:
#
#   1. pull ADDITIVELY: `--ignore-existing`, and never `--delete`. The backup
#      artifacts are immutable, so a file already off-box is never
#      overwritten — a backup deleted or corrupted on the Mini cannot erase
#      the off-box copy, and a tampered off-box file stays visible to the
#      verify below instead of being papered over by the next pull.
#   2. verify the newest pulled backup against its OWN `.heads.json`
#      (`pulse backup-verify`, read-only). A mismatch exits non-zero HERE,
#      before any pruning — nothing is thrown away on the strength of a bad
#      newest artifact.
#   3. retain the newest PULSE_OFFBOX_KEEP databases (each with its manifest);
#      `candles/` is one shared, additive store and is NEVER pruned.
#
# bash 3.2-compatible: same discipline as deploy/pulse-logrotate.sh.
set -euo pipefail

SRC="${1:?usage: pulse-backup-pull.sh <source> [dest]}"
DEST="${2:-${PULSE_OFFBOX_DIR:-$HOME/pulse-backups-offbox/mini}}"
KEY="${PULSE_SSH_KEY:-$HOME/.ssh/pulse_backup_ed25519}"
KEEP="${PULSE_OFFBOX_KEEP:-30}"
BIN="${PULSE_BIN:-$HOME/.local/share/pulse-qa/bin/pulse}"

case "$KEEP" in
  '' | *[!0-9]*)
    echo "pulse-backup-pull: PULSE_OFFBOX_KEEP must be a whole number, got '$KEEP'" >&2
    exit 2
    ;;
esac
if [ "$KEEP" -lt 1 ]; then
  echo "pulse-backup-pull: PULSE_OFFBOX_KEEP must be at least 1, got '$KEEP'" >&2
  exit 2
fi

mkdir -p "$DEST"

# Remote iff the source has a colon before any slash — rsync's own reading of
# `[user@]host:path`. A local path (the tests) never takes an ssh hop, and the
# remote branch always uses the dedicated key in BatchMode (no password, no
# agent prompt: this runs from a timer).
TRANSPORT=()
case "$SRC" in
  *:*)
    case "${SRC%%:*}" in
      */*) : ;;
      *) TRANSPORT=(-e "ssh -i $KEY -o BatchMode=yes -o IdentitiesOnly=yes") ;;
    esac
    ;;
esac

echo "pulse-backup-pull: pulling $SRC into $DEST"
# The bash-3.2-safe spelling (PR-354 fix C1): `"${TRANSPORT[@]}"` on an EMPTY
# array aborts under `set -u` on bash < 4.4 — macOS's /bin/bash is 3.2 — so the
# local-source path (the tests) must not expand it bare.
rsync -a --ignore-existing ${TRANSPORT[@]+"${TRANSPORT[@]}"} "$SRC/" "$DEST/"

# The pulled databases, oldest first by name (the `pulse-<stamp>` names sort
# chronologically).
BACKUPS=()
while IFS= read -r backup; do
  BACKUPS+=("$backup")
done < <(
  for file in "$DEST"/pulse-*.db; do
    [ -f "$file" ] || continue
    printf '%s\n' "$file"
  done | LC_ALL=C sort
)
COUNT=${#BACKUPS[@]}

if [ "$COUNT" -eq 0 ]; then
  echo "pulse-backup-pull: FAILED — no pulse-*.db in $DEST after the pull; the Mini's backup job may never have run" >&2
  exit 1
fi

# Verify the NEWEST before pruning: a bad newest must not cost the older
# copies, and the operator needs it named.
NEWEST="${BACKUPS[$((COUNT - 1))]}"
if ! "$BIN" backup-verify "$NEWEST"; then
  echo "pulse-backup-pull: FAILED — the newest pulled backup failed verification: $NEWEST" >&2
  exit 1
fi

# Retention: the newest KEEP stay, each with its own manifest; candles/ is
# never touched here.
KEPT="$COUNT"
if [ "$COUNT" -gt "$KEEP" ]; then
  PRUNE=$((COUNT - KEEP))
  index=0
  while [ "$index" -lt "$PRUNE" ]; do
    victim="${BACKUPS[$index]}"
    rm -f "$victim" "${victim}.heads.json"
    index=$((index + 1))
  done
  KEPT="$KEEP"
fi

echo "pulse-backup-pull: ok — $COUNT backup(s) pulled, newest verified, $KEPT kept in $DEST"
