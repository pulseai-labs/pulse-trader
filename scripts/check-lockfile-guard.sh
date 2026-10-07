#!/usr/bin/env bash
# r4.s1.w1 — the OQ-C lockfile guard (the comment-only rule, made executable).
#
# The dependency-determinism pins that must NOT move without a visible edit:
#   polars + every polars-* crate, rust_decimal, ta, zip, and sqlx + every
#   sqlx-* crate.
#
# `build_support/lockfile-guard.txt` holds one `<crate> <version>` line per
# guarded crate. This script fails — naming the crate — when a listed crate's
# version in `Cargo.lock` differs from the file, when it appears more than once
# in the lock (a partial bump that leaves two versions resolved), or when it is
# missing from the lock entirely. It does NOT fail on an unlisted new
# `polars-*`/`sqlx-*` member: a family bump moves `polars`/`sqlx` themselves,
# which this guard does catch, and the guarded set is the listed crates by
# design.
#
# CI runs it in the `fmt + clippy + nextest` job, before the tests. From then on
# a bump of a guarded crate needs an edit of the guard file — the visible act the
# 2026-09-25 lockfile-guard decision asks for.
#
# Exit 0 iff every assertion holds; exit 1 listing every failure (it does NOT
# halt on the first one — a single run should tell you everything that is wrong).

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
lock="$repo_root/Cargo.lock"
guard="$repo_root/build_support/lockfile-guard.txt"

if [ ! -f "$lock" ]; then
  echo "check-lockfile-guard: Cargo.lock not found at $lock" >&2
  exit 1
fi
if [ ! -f "$guard" ]; then
  echo "check-lockfile-guard: guard file not found at $guard" >&2
  exit 1
fi

failures=()
fail() { failures+=("$1"); }

# Every `[[package]]` in the lock as one `name version` line. Cargo.lock blocks
# are blank-line separated and the last one ends at EOF.
packages="$(awk '
  /^\[\[package\]\]$/ { name = ""; version = "" }
  /^name = "/    { if (name == "")    { name = $3;    gsub(/"/, "", name) } }
  /^version = "/ { if (version == "") { version = $3; gsub(/"/, "", version) } }
  /^$/           { if (name != "" && version != "") print name, version; name = ""; version = "" }
  END            { if (name != "" && version != "") print name, version }
' "$lock")"

guarded=0
while IFS= read -r line || [ -n "$line" ]; do
  case "$line" in
    "" | "#"* | " "* | $'\t'*) continue ;;
  esac
  case "$line" in
    *" "*)
      crate="${line%% *}"
      expected="${line#* }"
      case "$expected" in
        *" "*)
          fail "guard line '$line' is not '<crate> <version>'"
          continue
          ;;
      esac
      ;;
    *)
      fail "guard line '$line' is not '<crate> <version>'"
      continue
      ;;
  esac

  guarded=$((guarded + 1))
  matches="$(printf '%s\n' "$packages" | awk -v c="$crate" '$1 == c { print $2 }')"
  count=0
  if [ -n "$matches" ]; then
    count="$(printf '%s\n' "$matches" | grep -c . || true)"
  fi
  if [ "$count" -eq 0 ]; then
    fail "$crate is missing from Cargo.lock (the guard requires exactly one entry)"
  elif [ "$count" -gt 1 ]; then
    versions="$(printf '%s\n' "$matches" | tr '\n' ',')"
    fail "$crate appears $count times in Cargo.lock (versions: ${versions%,} — the guard requires exactly one)"
  elif [ "$matches" != "$expected" ]; then
    fail "$crate is $matches in Cargo.lock but the guard pins $expected"
  fi
done < "$guard"

if ((${#failures[@]} > 0)); then
  echo "check-lockfile-guard: FAILED (${#failures[@]} problem(s))"
  for f in "${failures[@]}"; do
    echo "  - $f"
  done
  exit 1
fi

echo "check-lockfile-guard: OK ($guarded guarded crates match Cargo.lock)"
