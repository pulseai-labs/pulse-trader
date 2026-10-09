#!/usr/bin/env bash
# r4.s2.w5 (C3, spec §Approach 3) — the Mac Mini's forced command for the
# off-box backup key: the only thing that key can run.
#
# The Mini's rsync is macOS's own (openrsync, "rsync version 2.6.9
# compatible") and carries NO `rrsync` — checked read-only at planning
# (`ssh macmini 'rsync --version; command -v rrsync'`) — so the operator's
# authorized_keys line forces THIS script instead:
#
#   from="100.90.203.21",restrict,command="/Users/draco/.local/share/pulse-serve/deploy/pulse-backup-serve.sh" ssh-ed25519 <the pull key> draco-desk off-box backup pull
#
# `just deploy-mac` installs it to ~/.local/share/pulse-serve/deploy/ beside
# the rotation script. It reads the command sshd was asked to run from
# $SSH_ORIGINAL_COMMAND and serves exactly ONE shape — an rsync SENDER
# invocation rooted at `~/pulse-backups` (`PULSE_BACKUP_ROOT` overrides the
# root for the local tests) — and refuses everything else BY NAME, without
# executing it: a write, a delete, a `..` path, a shell, a path outside the
# root, any shell metacharacter. The key can therefore never write, delete or
# read outside the backup directory.
#
# It never prints to stdout before the exec: stdout IS the rsync protocol.
#
# bash 3.2-compatible: this runs on macOS's /bin/bash.
set -euo pipefail

ROOT="${PULSE_BACKUP_ROOT:-$HOME/pulse-backups}"
CMD="${SSH_ORIGINAL_COMMAND:-}"

refuse() {
  printf 'pulse-backup-serve: refused: %s\n' "$1" >&2
  exit 1
}

[ -n "$CMD" ] || refuse "no command; this key only serves $ROOT over rsync"

# The allowed character set is the whole injection defense: no quote, no
# backslash, no `$`, no backtick, no `;`/`|`/`&`/`(`/`)`/`{`/`}`/`<`/`>`, no
# glob, no newline, no tab — only what an rsync invocation is made of, spaces
# between its words included.
case "$CMD" in
  *[!A-Za-z0-9._/:+=~\ -]*)
    refuse "a character this key does not allow"
    ;;
esac
case "$CMD" in
  *..*)
    refuse "a '..' path"
    ;;
esac

# Split into words — no eval, no globbing (the charset check above rules both
# out; `read -a` splits on the default IFS only).
read -r -a WORDS <<< "$CMD"
[ "${#WORDS[@]}" -ge 5 ] || refuse "not an rsync sender invocation"
[ "${WORDS[0]}" = "rsync" ] || refuse "only rsync is allowed, got '${WORDS[0]}'"

SERVER=0
SENDER=0
ARGS=()
for WORD in "${WORDS[@]}"; do
  case "$WORD" in
    --server) SERVER=1 ;;
    --sender) SENDER=1 ;;
    # A shell would expand `~`; a forced command has none, so translate the
    # form a human would type.
    "~/"*) WORD="$HOME/${WORD#\~/}" ;;
  esac
  case "$WORD" in
    --*)
      # An EXACT allowlist of the long options the real clients send for this
      # pull (PR-354 fix C4). rsync 3.x sends `--server --sender` and carries
      # everything else in the short cluster; macOS's openrsync — the CI
      # runner's `rsync`, and the shape the Mini serves — also sends the pull's
      # own `--ignore-existing` as a long option. A DENYLIST of spellings was a
      # forced-command bypass: rsync/openrsync accept unique abbreviations, so
      # `--remove-source`, `--del` and `--delete-after` (deletes under $ROOT)
      # were never named. Anything outside the allowlist is refused by name.
      case "$WORD" in
        --server | --sender | --ignore-existing) ;;
        *) refuse "'$WORD' (only the pull's own long options are allowed)" ;;
      esac
      ;;
    -e)
      refuse "'-e' (an rsh command has no place in a forced command)"
      ;;
  esac
  ARGS+=("$WORD")
done

[ "$SERVER" = 1 ] || refuse "an rsync invocation without --server"
[ "$SENDER" = 1 ] || refuse "an rsync invocation without --sender (this key reads; it never writes)"

# Every operand must be the transfer root or under it: `.` is the protocol's
# own separator, and everything else has to resolve inside $ROOT. The first
# word is the command itself, never an operand.
FOUND_ROOT=0
for WORD in "${ARGS[@]:1}"; do
  case "$WORD" in
    -*) continue ;;
    .) continue ;;
  esac
  OPERAND="$WORD"
  case "$OPERAND" in
    */) OPERAND="${OPERAND%/}" ;;
  esac
  case "$OPERAND" in
    "$ROOT") FOUND_ROOT=1 ;;
    "$ROOT"/*) FOUND_ROOT=1 ;;
    *) refuse "a path outside $ROOT ('$WORD')" ;;
  esac
done
[ "$FOUND_ROOT" = 1 ] || refuse "no path rooted at $ROOT"

# Serve it: the client's own words, no shell in between.
RSYNC="$(command -v rsync)" || refuse "no rsync on this host"
exec "$RSYNC" "${ARGS[@]:1}"
