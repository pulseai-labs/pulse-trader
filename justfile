# PulseTrader command runner. `just check` is the aggregate local gate
# mirrored by CI (.github/workflows/ci.yml).

# Aggregate gate: frontend, then Rust, then the desktop shell's check scripts.
#
# r1.s1.w1 (grill A4) grew this from `fmt clippy test` so that ONE command still
# gates EVERYTHING now that the repo has a second language and eight shell gates.
# The alternative -- a Rust gate plus a separate frontend gate someone has to
# remember -- is how a TypeScript regression reaches `main` while `just check` is
# green.
#
# Order is deliberate: `ui` runs FIRST because `cargo` needs `ui/dist` to exist
# (`generate_context!` embeds it at compile time), so building the frontend before
# the Rust targets means clippy and the tests compile against the REAL bundle
# rather than build.rs's placeholder.

# The aggregate gate: frontend + Rust + the ten content check scripts.
check: ui fmt clippy test gates

# --- frontend ---------------------------------------------------------------

# Typecheck, test, and build the frontend bundle Tauri embeds.
#
# `npm run test` (r1.s1.w6, G9) runs `vitest run` -- the non-interactive form.
# It sits between typecheck and build so a regression fails fast, before the
# slower production build runs.
ui: ui-deps
    npm run typecheck
    npm run test
    npm run build

# Install node modules only when they are missing. `npm ci` (not `npm install`)
# so the committed package-lock.json is authoritative and a gate run can never
# silently change the dependency tree.

# Install node modules when missing (npm ci, lockfile-authoritative).
ui-deps:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -d node_modules ]; then
        npm ci
    fi

# --- rust -------------------------------------------------------------------

# Verify formatting without rewriting.
fmt:
    cargo fmt --check

# Lint all targets, warnings as errors.
clippy:
    cargo clippy --all-targets -- -D warnings

# Run the test suite via nextest.
test:
    cargo nextest run

# --- content gates (r1.s1.w1, r1.s1.w5, r1.s1.w6, r1.s5.w1, r1.s5.w2, r1.s2.w1, r1.s4.w1) --

# The ten content gates. Each asserts a property that review
# cannot be trusted to hold: ADR-0020's decision is recorded and the ADR-0001 /
# ADR-0019 class sweep landed; no fs/shell/http capability is reachable from the
# frontend; the window stays 1440x900 non-resizable with no scale-transform; the
# committed bindings match a fresh generation; the ported design system
# (tokens.css/shared.css, no per-screen sheet, no macos-window.jsx) landed for
# real (r1.s1.w5, AC-2); every nav row navigates with exactly one active at
# a time (r1.s1.w6, AC-2 -- a backstop over the rendered vitest tests, per
# audit finding C6); ADR-0022's Status/Decision/Consequences/Alternatives
# content is intact and the ADR-0019 class sweep it requires has landed
# (r1.s5.w1, AC-2); and the specta/tauri-specta generator workaround
# (`post_process_bindings`, a post-write transform of `bindings.ts`, or a
# re-pin to the pre-bump rc.21/rc.22 versions) has not come back (r1.s5, d5);
# and the coach boundary holds (r1.s4.w1, AC-2): no public `Coach::new`/`run_turn`,
# no production `save_session` caller, and `record_inapplicable` advertised only
# alongside `propose_mutation` -- the #132 seal and the #131 honesty protocol, both
# of which are source properties that decay silently unless something asserts them;
# and ADR-0021 records the coach decisions r1.s2's work items implement against,
# now `Accepted` (authored `Proposed` at r1.s2.w1 per AC-1, flipped at r1.s2's
# close on 2026-08-29 with check-adr-0021.sh's Status assertion updated in the SAME
# act -- the check-adr-0020.sh precedent). Only that close may flip it.

# Run the ten content gates (ADR-0020, capabilities, window, bindings, design system, shell navigation, ADR-0022, no-specta-workaround, ADR-0021, coach boundary).
gates:
    bash scripts/check-adr-0020.sh
    bash scripts/check-capabilities.sh
    bash scripts/check-window-config.sh
    bash scripts/check-bindings.sh
    bash scripts/check-design-system.sh
    bash scripts/check-shell-navigation.sh
    bash scripts/check-adr-0022.sh
    bash scripts/check-adr-0021.sh
    bash scripts/check-no-specta-workaround.sh
    bash scripts/check-coach-boundary.sh
    bash scripts/check-mcp-boundary.sh

# VS-1.1.4 work-1.01 — regenerate the committed .sqlx offline query cache
# (NFR-12). Needs sqlx-cli (a developer-local tool, NOT installed in this slice's
# pre-flight). Creates a temp sqlite file, runs the migrations against it, runs
# `cargo sqlx prepare`, then removes the temp file. Its real payoff is 1.03 onward,
# once `query!` macros exist; it does NOT run in CI's build.
prepare:
    rm -f pulse-prepare.db pulse-prepare.db-wal pulse-prepare.db-shm
    DATABASE_URL=sqlite://pulse-prepare.db sqlx database create
    DATABASE_URL=sqlite://pulse-prepare.db sqlx migrate run
    DATABASE_URL=sqlite://pulse-prepare.db cargo sqlx prepare
    rm -f pulse-prepare.db pulse-prepare.db-wal pulse-prepare.db-shm

# r3.s3.w1 (d30) — the LIVE bind-policy check on draco-desk: builds the debug
# binary, then proves the server binds its tailnet address only, refuses the
# LAN address, answers the authenticated handshake over the tailnet and exits
# 0 on SIGTERM. Needs `tailscale` + one UP non-loopback interface. NEVER runs
# under `set -x` and never prints the issued token.
check-serve-bind:
    cargo build
    bash scripts/check-serve-bind.sh


# --- desktop bundle (r1.s1.w1) ----------------------------------------------

# Build PulseTrader.app for a LOCAL dev run — this is what r1.s1.w5's AC-11 manual
# walk needs.
#
# Deliberately NOT part of `just check`: it is a release build of the whole Tauri
# graph (minutes, not seconds), and gating every local check run on it would make the
# gate something people skip. Code signing, notarization and auto-update are out of
# scope for r1.s1.w1 (ADR-0020) — this produces an unsigned local artifact at
# `target/release/bundle/macos/PulseTrader.app`.

# Build an unsigned local PulseTrader.app (what AC-11's manual walk needs).
bundle:
    npm run bundle

# Run the desktop shell against the Vite dev server (hot reload).
desktop:
    npm run desktop
