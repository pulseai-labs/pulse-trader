#!/usr/bin/env bash
# AC-2 — the mcp-boundary content gate (r2.s1.w2).
#
# rmcp is permitted under tests/ via the dev-dependency client; src/ outside
# src/mcp/ may not import it.
#
# What it asserts (each a source property that decays silently without a gate):
#
#   1. STDOUT PURITY in the serving path. No `println!`/`print!` in
#      src/mcp/**, src/cli/mcp.rs, src/application/mcp_read.rs, or
#      src/application/mcp_write.rs — one stray print corrupts the JSON-RPC
#      stream on stdout and every client sees garbage. `eprintln!`/`eprint!`
#      stay legal: stderr is the diagnostics channel, stdout is the wire.
#
#   2. LEAST PRIVILEGE in the serving path. The same files may not name
#      `adapters::secrets`, `adapters::llm`, `agent::`, or
#      `resolve_llm_api_key` — the read ring has no business holding a
#      credential or a provider client, and the gate makes "it compiled once"
#      insufficient for a future edit that reaches for one.
#
#   3. THE rmcp SEAM. No src/**.rs file outside src/mcp/ may name `rmcp` in
#      code — the SDK is the delivery adapter, and letting it leak into the
#      CLI or the application ring couples the use cases to a transport.
#      Cargo.toml is out of scope here (it must declare the dep); tests/ is
#      out of scope entirely (the dev-dependency client drives the AC tests).
#
#   4. Positive controls, so a renamed or emptied tree cannot pass vacuously:
#      src/mcp/mod.rs exists and exposes `serve`, the tools router exists, and
#      `McpState` carries the four fields the spec declares.
#
# Exit 0 iff every assertion holds; exit 1 listing every failure.

set -uo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root" || exit 1

failures=()
fail() { failures+=("$1"); }

# A file's code with line comments blanked, so prose naming a symbol never
# trips a code assertion (the `blank_comments` discipline the other
# source-scan gates use).
code_of() {
  sed -E 's://.*$::' "$1"
}

# --- 1/2. stdout purity + least privilege in the serving path ----------------

serving_files() {
  find src/mcp -name '*.rs' -type f 2>/dev/null
  for f in src/cli/mcp.rs src/application/mcp_read.rs src/application/mcp_write.rs; do
    [[ -f "$f" ]] && printf '%s\n' "$f"
  done
}

while IFS= read -r file; do
  [[ -z "$file" ]] && continue
  code="$(code_of "$file")"
  if printf '%s\n' "$code" | grep -qE '\bprintln!|\bprint!'; then
    fail "$file writes to stdout (\`println!\`/\`print!\`) — stdout is the MCP wire; one stray print corrupts every JSON-RPC frame (spec: diagnostics are eprintln!)"
  fi
  for needle in 'adapters::secrets' 'adapters::llm' 'resolve_llm_api_key'; do
    if printf '%s\n' "$code" | grep -qF "$needle"; then
      fail "$file names \`$needle\` — the read ring holds no credential and no provider client by construction (least privilege)"
    fi
  done
  if printf '%s\n' "$code" | grep -qE '\bagent::'; then
    fail "$file names \`agent::\` — the coach/agent ring is out of the MCP boundary"
  fi
done < <(serving_files)

# --- 3. the rmcp seam ----------------------------------------------------------

while IFS= read -r file; do
  case "$file" in
    src/mcp/*) continue ;;
  esac
  if code_of "$file" | grep -qE '\brmcp\b'; then
    fail "$file names \`rmcp\` — the SDK is confined to src/mcp/ (tests/ uses the dev-dependency client; Cargo.toml must declare the dep — both out of this scan's scope)"
  fi
done < <(find src -name '*.rs' -type f | sort)

# --- 4. positive controls ------------------------------------------------------

if [[ ! -f src/mcp/mod.rs ]]; then
  fail "src/mcp/mod.rs is missing — the delivery ring this gate guards does not exist"
else
  if ! code_of src/mcp/mod.rs | grep -qE '\bfn[[:space:]]+serve\b'; then
    fail "src/mcp/mod.rs exposes no \`serve\` entry point — the composition root has nothing to call"
  fi
  for field in 'db' 'candles' 'exports' 'identity'; do
    if ! code_of src/mcp/mod.rs | grep -qE "\b$field:"; then
      fail "src/mcp/mod.rs McpState is missing field \`$field\` — the spec declares db, candles, exports_dir/exports and identity"
    fi
  done
fi

if [[ ! -f src/mcp/tools.rs ]]; then
  fail "src/mcp/tools.rs is missing — the seven read tools have no home"
fi

if [[ ! -f src/mcp/identity.rs ]]; then
  fail "src/mcp/identity.rs is missing — agent-name validation has no home until w1's domain type lands"
fi

# --- report --------------------------------------------------------------------

if ((${#failures[@]} > 0)); then
  printf 'FAIL: %s\n' "${failures[@]}" >&2
  echo "check-mcp-boundary: ${#failures[@]} failure(s)" >&2
  exit 1
fi

echo "check-mcp-boundary: OK (no stdout writes or credential/llm/agent references in the serving path, rmcp confined to src/mcp/, McpState intact)"
