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
  # The EXACT words the two real clients send for THIS pull (PR-354 fix D3).
  # These are allowlists, not denylists: rsync and openrsync both accept unique
  # abbreviations, so a spelling denylist let `--remove-source`/`--del` through,
  # and a permissive `-*` let `-L` (copy-links) read through a symlink to files
  # outside $ROOT. Anything else is refused by name.
  #
  #   rsync 3.x — draco-desk's client, the deployed one (captured on the host:
  #   `rsync --server --sender -logDtpre.iLsfxCIvu . <root>/` with rsync 3.4.1);
  #
  #   macOS openrsync — the CI runner's client (Apple's openrsync fargs.c: `-a`
  #   sets g/l/o/p/D/r/t, `--ignore-existing` is the pull's own flag, and
  #   `--dirs` is -a's implied dirs — the CI refusal named exactly `--dirs`):
  #   `rsync --server --sender -g -l -o -p -D -r -t --ignore-existing --dirs . <root>/`.
  case "$WORD" in
    --*)
      case "$WORD" in
        --server | --sender | --ignore-existing | --dirs) ;;
        *) refuse "'$WORD' (only the pull's own long options are allowed)" ;;
      esac
      ;;
    -e)
      refuse "'-e' (an rsh command has no place in a forced command)"
      ;;
    -*)
      case "$WORD" in
        -logDtpre.iLsfxCIvu | -g | -l | -o | -p | -D | -r | -t) ;;
        *) refuse "'$WORD' (only the pull's own short options are allowed)" ;;
      esac
      ;;
  esac
  ARGS+=("$WORD")
done

[ "$SERVER" = 1 ] || refuse "an rsync invocation without --server"
[ "$SENDER" = 1 ] || refuse "an rsync invocation without --sender (this key reads; it never writes)"

# Whether the operand passes through a SYMLINK beneath $ROOT (PR-354 fix B2).
# The argument is the operand with `$ROOT/` removed; every component from $ROOT
# down is tested with `[ -L ]` — no `realpath`, no `readlink`, and a dangling
# link counts too. Returns 0 when a symlink is on the way. A FUNCTION at top
# level, so no `case` sits inside a `$( )`/`<( )` (bash 3.2 parses those badly).
symlink_in_path() {
  sym_tail="$1"
  sym_prefix="$ROOT"
  while [ -n "$sym_tail" ]; do
    sym_component="${sym_tail%%/*}"
    sym_prefix="$sym_prefix/$sym_component"
    if [ -L "$sym_prefix" ]; then
      return 0
    fi
    if [ "$sym_component" = "$sym_tail" ]; then
      sym_tail=""
    else
      sym_tail="${sym_tail#*/}"
    fi
  done
  return 1
}

# Every operand must be the transfer root or under it: `.` is the protocol's
# own separator, accepted EXACTLY ONCE, as the first non-option word — rsync
# and openrsync put the transfer root there, before the sources (PR-354 fix P2:
# the loop used to skip EVERY `.`, so `rsync --server --sender <flags> . .
# "$ROOT/"` passed, and that second `.` is ANOTHER transfer root, resolved
# against this script's cwd — the Mini user's home). A second `.`, or one in
# any other position, is refused by name. Everything else has to resolve inside
# $ROOT. The first word is the command itself, never an operand.
#
# Below the root, two shapes are refused by name (PR-354 fix B2): a trailing
# slash on anything but the root itself — rsync follows a command-line symlink
# that ends in `/`, so `$ROOT/link/` would read outside the backup root even
# though the prefix test strips the slash — and any operand that is, or passes
# through, a symlink beneath the root.
FOUND_ROOT=0
SEPARATOR=0
for WORD in "${ARGS[@]:1}"; do
  case "$WORD" in
    -*) continue ;;
    .)
      if [ "$SEPARATOR" = 1 ] || [ "$FOUND_ROOT" = 1 ]; then
        refuse "a second '.' operand (only one transfer root is served)"
      fi
      SEPARATOR=1
      continue
      ;;
  esac
  OPERAND="$WORD"
  case "$OPERAND" in
    */) OPERAND="${OPERAND%/}" ;;
  esac
  case "$OPERAND" in
    "$ROOT") FOUND_ROOT=1 ;;
    "$ROOT"/*)
      if [ "$WORD" != "$OPERAND" ]; then
        refuse "a trailing slash below $ROOT ('$WORD')"
      fi
      if symlink_in_path "${OPERAND#"$ROOT"/}"; then
        refuse "a symlink below $ROOT ('$WORD')"
      fi
      FOUND_ROOT=1
      ;;
    *) refuse "a path outside $ROOT ('$WORD')" ;;
  esac
done
[ "$FOUND_ROOT" = 1 ] || refuse "no path rooted at $ROOT"

# Serve it: the client's own words, no shell in between.
RSYNC="$(command -v rsync)" || refuse "no rsync on this host"
exec "$RSYNC" "${ARGS[@]:1}"
