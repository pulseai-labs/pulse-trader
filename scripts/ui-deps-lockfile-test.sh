#!/usr/bin/env bash
# The `ui-deps` lockfile-follow proof (r4.s5.w5, #114).
#
# `just ui-deps` must run `npm ci` when the tree is absent **or** when
# `package-lock.json` no longer matches what was last installed — the #114
# defect was that a lockfile bump left a stale `node_modules` in place and every
# later gate ran against it (the r4.s5 round-2 proof went red on a canonical
# tree holding vitest 3.2.7 against the lockfile's 5.0.3).
#
# The recipe's DECISION is what this proves, on scratch copies inside this
# worktree — never against any real `node_modules`, never with a real install:
#
#   1. **The install decision.** A stub `npm` on `PATH` records every call; the
#      recipe's own side effect (a `node_modules` directory) is mimicked by the
#      stub, so the recipe's stamp write is exercised exactly as it is in the
#      real install path.
#   2. **The cases.** Absent tree installs once; an unchanged tree installs
#      zero times; a changed lockfile installs again exactly once (and the next
#      run is a no-op again); a missing stamp re-installs; a FAILED install
#      writes no stamp and is retried on the next run.
#   3. **The stamp is the lockfile's own hash** (`cksum`, POSIX on Linux and
#      macOS) of `package-lock.json` — asserted after every successful run, so
#      the no-op case cannot pass by a stamp nothing maintains.
#
# Everything happens under a scratch directory in this worktree, removed by the
# trap on every exit path. The canonical checkout's `node_modules` is never
# read, written or referenced — the scratch root is derived from this script's
# own location.
#
# `just` is called from PATH, like the other gates. No node/npm is required by
# this script itself; the stub stands in for npm.
#
# Exit 0 iff every case holds; exit 1 naming EVERY case that failed (it does NOT
# halt on the first one — a single run should tell you everything that is wrong).

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

failures=()
note_failure() { failures+=("$1"); }

if ! command -v just >/dev/null 2>&1; then
  echo "FAIL: 'just' is not on PATH — this proof runs the recipe, not a copy of it" >&2
  note_failure "just not on PATH"
  echo "ui-deps-lockfile-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

scratch="$(mktemp -d "$repo_root/.ui-deps-lockfile-test.XXXXXX")" || {
  echo "FAIL: could not create the scratch directory under $repo_root" >&2
  exit 1
}
cleanup() { rm -rf "$scratch"; }
trap cleanup EXIT INT TERM

# --- the scratch copy: the files the recipe reads, nothing else --------------
cp "$repo_root/package.json" "$repo_root/package-lock.json" "$repo_root/justfile" "$scratch/" || {
  echo "FAIL: could not copy package.json / package-lock.json / justfile into $scratch" >&2
  exit 1
}

stub_bin="$scratch/stub-bin"
mkdir -p "$stub_bin" || exit 1
calls_log="$scratch/npm-calls.log"
: >"$calls_log"
fail_marker="$scratch/npm-must-fail"

cat >"$stub_bin/npm" <<'STUB'
#!/usr/bin/env bash
# Stub npm: record the call and mimic the one side effect the recipe relies on
# (a node_modules directory). When the fail marker exists it exits 1 instead —
# before creating anything — so the failed-install path is exercised too.
printf '%s\n' "$*" >> "$NPM_CALLS_LOG"
if [ -e "$NPM_FAIL_MARKER" ]; then
  echo "stub-npm: failing on purpose (NPM_FAIL_MARKER=$NPM_FAIL_MARKER)" >&2
  exit 1
fi
if [ "${1:-}" = "ci" ]; then
  mkdir -p node_modules
fi
exit 0
STUB
chmod +x "$stub_bin/npm" || exit 1

export NPM_CALLS_LOG="$calls_log"
export NPM_FAIL_MARKER="$fail_marker"

stamp="$scratch/node_modules/.pulse-ui-deps-stamp"

calls() { wc -l <"$calls_log" | tr -d ' '; }
want_stamp() { cksum <"$scratch/package-lock.json"; }

# Run the recipe in the scratch copy with the stub npm first on PATH.
run_recipe() {
  (cd "$scratch" && PATH="$stub_bin:$PATH" just ui-deps)
}

# Assert the stamp equals the lockfile's own hash — the no-op case's foundation.
assert_stamp_current() {
  local label="$1" want
  want="$(want_stamp)"
  if [ ! -f "$stamp" ]; then
    echo "FAIL [$label]: no stamp at node_modules/.pulse-ui-deps-stamp after a successful install" >&2
    note_failure "[$label] stamp missing"
  elif [ "$(cat "$stamp")" != "$want" ]; then
    echo "FAIL [$label]: stamp is '$(cat "$stamp")' but the lockfile hashes to '$want'" >&2
    note_failure "[$label] stamp stale"
  fi
}

# --- case 1: no node_modules -> installs once --------------------------------
label="case 1: absent tree installs"
before="$(calls)"
if run_recipe; then
  after="$(calls)"
  if [ "$((after - before))" -ne 1 ]; then
    echo "FAIL [$label]: expected exactly 1 npm call, saw $((after - before))" >&2
    note_failure "[$label] npm called $((after - before)) time(s)"
  fi
  assert_stamp_current "$label"
else
  echo "FAIL [$label]: the recipe exited non-zero on an absent node_modules" >&2
  note_failure "[$label] recipe failed"
fi

# --- case 2: nothing changed -> no install -----------------------------------
label="case 2: unchanged tree is a no-op"
before="$(calls)"
if run_recipe; then
  after="$(calls)"
  if [ "$((after - before))" -ne 0 ]; then
    echo "FAIL [$label]: expected no npm call, saw $((after - before)) — a no-op reinstalled" >&2
    note_failure "[$label] npm called $((after - before)) time(s)"
  fi
else
  echo "FAIL [$label]: the recipe exited non-zero on an unchanged tree" >&2
  note_failure "[$label] recipe failed"
fi

# --- case 3: the lockfile changed -> installs again, then no-op --------------
label="case 3: changed lockfile reinstalls"
# A portable, version-independent mutation: one appended newline rewrites the
# scratch lockfile's bytes — and with them its `cksum` stamp — with no GNU-only
# `sed -i` and no assumption that any particular version text is in the file.
# The stamp is compared before and after, so a mutation that failed to move it
# fails here instead of letting the case pass by proving nothing.
stamp_before="$(want_stamp)"
if printf '\n' >>"$scratch/package-lock.json" && [ "$(want_stamp)" != "$stamp_before" ]; then
  before="$(calls)"
  if run_recipe; then
    after="$(calls)"
    if [ "$((after - before))" -ne 1 ]; then
      echo "FAIL [$label]: expected exactly 1 npm call after a lockfile change, saw $((after - before))" >&2
      note_failure "[$label] npm called $((after - before)) time(s)"
    fi
    assert_stamp_current "$label"
    # and the run right after it is a no-op again (the stamp was refreshed)
    before="$(calls)"
    if run_recipe; then
      after="$(calls)"
      if [ "$((after - before))" -ne 0 ]; then
        echo "FAIL [$label]: the run after the reinstall still called npm ($((after - before)) time(s))" >&2
        note_failure "[$label] post-reinstall run not a no-op"
      fi
    else
      echo "FAIL [$label]: the recipe exited non-zero after refreshing the stamp" >&2
      note_failure "[$label] post-reinstall run failed"
    fi
  else
    echo "FAIL [$label]: the recipe exited non-zero on a changed lockfile" >&2
    note_failure "[$label] recipe failed"
  fi
else
  echo "FAIL [$label]: could not change the scratch lockfile" >&2
  note_failure "[$label] could not mutate the lockfile"
fi

# --- case 4: stamp missing -> installs again ---------------------------------
label="case 4: missing stamp reinstalls"
rm -f "$stamp"
before="$(calls)"
if run_recipe; then
  after="$(calls)"
  if [ "$((after - before))" -ne 1 ]; then
    echo "FAIL [$label]: expected exactly 1 npm call with no stamp, saw $((after - before))" >&2
    note_failure "[$label] npm called $((after - before)) time(s)"
  fi
  assert_stamp_current "$label"
else
  echo "FAIL [$label]: the recipe exited non-zero with node_modules present and no stamp" >&2
  note_failure "[$label] recipe failed"
fi

# --- case 5: a failed install writes no stamp and is retried -----------------
label="case 5: failed install is retried"
rm -rf "$scratch/node_modules"
touch "$fail_marker"
before="$(calls)"
if run_recipe; then
  echo "FAIL [$label]: the recipe reported success while npm ci failed" >&2
  note_failure "[$label] recipe swallowed the failure"
else
  after="$(calls)"
  if [ "$((after - before))" -ne 1 ]; then
    echo "FAIL [$label]: expected exactly 1 npm call on the failing run, saw $((after - before))" >&2
    note_failure "[$label] npm called $((after - before)) time(s)"
  fi
  if [ -f "$stamp" ]; then
    echo "FAIL [$label]: a stamp was written by a FAILED install" >&2
    note_failure "[$label] stamp written on failure"
  fi
fi
rm -f "$fail_marker"
before="$(calls)"
if run_recipe; then
  after="$(calls)"
  if [ "$((after - before))" -ne 1 ]; then
    echo "FAIL [$label]: the retry after the failure did not install (saw $((after - before)) call(s))" >&2
    note_failure "[$label] retry did not install"
  fi
  assert_stamp_current "$label"
else
  echo "FAIL [$label]: the retry after the failure exited non-zero" >&2
  note_failure "[$label] retry failed"
fi

# --- report -------------------------------------------------------------------
if ((${#failures[@]} > 0)); then
  for failure in "${failures[@]}"; do
    echo "FAIL: $failure" >&2
  done
  echo "ui-deps-lockfile-test: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "ui-deps-lockfile-test: OK (absent tree installs once; unchanged tree is a"
echo "  no-op; a changed lockfile reinstalls once and is a no-op again; a missing"
echo "  stamp reinstalls; a failed install writes no stamp and is retried)"
