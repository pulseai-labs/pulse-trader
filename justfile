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

# --- deploy / backup / restore (r3.s3.w4, D6/D7/D12, ADR-0026) --------------

# Build a tagged commit and install it as the always-on server: a fresh git
# worktree of `tag` in ~/.cache/pulse-deploy/src (NEVER /tmp), `cargo build
# --release --bin pulse` there, the binary into ~/.local/share/pulse-serve/bin
# (not on PATH — it never shadows a developer's `pulse`), the three units into
# ~/.config/systemd/user/, then reload / enable / restart. THE CUTOVER STEP
# (D7): an operator action at the release walk — never run by an item. Refuses
# without a tag argument and refuses a tag that does not exist.
#
# The server unit gives up after 3 failed starts in 900 s (#342) and stays in the
# systemd `failed` state. To see it:
#   XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user --failed
#   XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user status pulse-serve.service
# To recover: fix the cause, then
#   XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user reset-failed pulse-serve.service
#   XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user start pulse-serve.service
# The start limit counts manual starts too, so the `deploy` and `restore` recipes
# run `reset-failed` before they start or restart the unit.
#
# A change to deploy/pulse-serve.service alone installs on a live host by copying
# the merged unit to ~/.config/systemd/user/ and running
# `systemctl --user daemon-reload`, with no restart. This `deploy` recipe does
# more: it restarts pulse-serve.service.
deploy tag:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{ tag }}" ]; then
        echo "deploy: a tag argument is required (e.g. just deploy r2)" >&2
        exit 1
    fi
    if ! git rev-parse -q --verify "refs/tags/{{ tag }}" >/dev/null; then
        echo "deploy: tag '{{ tag }}' does not exist in this repository" >&2
        exit 1
    fi
    SRC="$HOME/.cache/pulse-deploy/src"
    rm -rf "$SRC"
    git worktree prune
    git worktree add --detach "$SRC" "refs/tags/{{ tag }}"
    # #314 (r4.s2.w1): the server never serves the embedded frontend — its
    # router mounts only /api/v1 routes and /mcp; there is no static handler
    # under src/server/ — so the placeholder dist is the DELIBERATE setting of
    # this release build (and of deploy-mac's): the deploy must not refuse on,
    # or depend on, a frontend bundle the server never reads.
    (cd "$SRC" && PULSE_ALLOW_PLACEHOLDER_DIST=1 cargo build --release --bin pulse)
    mkdir -p "$HOME/.local/share/pulse-serve/bin" "$HOME/.config/systemd/user"
    install -m 0755 "$SRC/target/release/pulse" "$HOME/.local/share/pulse-serve/bin/pulse"
    # The THREE UNITS come from the tag too, like the binary above: installing
    # the caller's working-tree units beside a tagged binary is a mixed release
    # (and a rollback to an old tag would install new units).
    install -m 0644 "$SRC/deploy/pulse-serve.service" "$SRC/deploy/pulse-backup.service" \
        "$SRC/deploy/pulse-backup.timer" "$HOME/.config/systemd/user/"
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user daemon-reload
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user enable --now pulse-backup.timer
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user enable pulse-serve.service
    # The start limit counts manual starts: clear a latched `failed` state first.
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user reset-failed pulse-serve.service || true
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user restart pulse-serve.service
    "$HOME/.local/share/pulse-serve/bin/pulse" --version
    # #346 (r4.s2.w1): `systemctl is-active` reads `active` for the whole
    # 120-second bind retry, so a deploy whose bind never succeeded still
    # reported success. The gate is now a real health probe. It runs the
    # CALLER checkout's copy (justfile and script come from one checkout, so an
    # old tag stays deployable); w4 lands the /healthz route, and until then
    # the script's handshake fallback stands in.
    bash scripts/wait-healthy.sh "http://100.90.203.21:8420" 150

# The SAFE rehearsal for `deploy` (r3.s3.w4 AC-2): it installs nothing, starts
# nothing and builds nothing. It verifies the three units with
# `systemd-analyze --user verify`, rendered into a scratch dir under
# ~/.cache/pulse-scratch/ with ONLY the ExecStart binary swapped to an
# existing placeholder (the real binary must not exist yet — installing it is
# deploy's job); every other byte of each unit is verbatim. Then it dry-runs
# the deploy steps against a real tag and asserts the live systemd/user paths
# are byte-for-byte untouched.
deploy-check tag="r2":
    #!/usr/bin/env bash
    set -euo pipefail
    if ! git rev-parse -q --verify "refs/tags/{{ tag }}" >/dev/null; then
        echo "deploy-check: tag '{{ tag }}' does not exist in this repository" >&2
        exit 1
    fi
    mkdir -p "$HOME/.cache/pulse-scratch"
    SCRATCH="$(mktemp -d "$HOME/.cache/pulse-scratch/deploy-check.XXXXXX")"
    trap 'rm -rf "$SCRATCH"' EXIT
    mkdir -p "$SCRATCH/units"
    list_live() {
        {
            ls -A "$HOME/.config/systemd/user" 2>/dev/null || true
            echo "--"
            ls -AR "$HOME/.local/share/pulse-serve" 2>/dev/null || true
        } | sort
    }
    BEFORE="$(list_live)"
    for unit in deploy/pulse-serve.service deploy/pulse-backup.service deploy/pulse-backup.timer; do
        sed -E 's|^(ExecStart=).*pulse |ExecStart=/usr/bin/true |' "$unit" \
            > "$SCRATCH/units/$(basename "$unit")"
    done
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemd-analyze --user verify \
        "$SCRATCH/units/pulse-serve.service" \
        "$SCRATCH/units/pulse-backup.service" \
        "$SCRATCH/units/pulse-backup.timer"
    just --dry-run "deploy" "{{ tag }}" >/dev/null
    AFTER="$(list_live)"
    if [ "$BEFORE" != "$AFTER" ]; then
        echo "deploy-check: live systemd/user paths changed during the rehearsal" >&2
        diff <(printf '%s\n' "$BEFORE") <(printf '%s\n' "$AFTER") >&2 || true
        exit 1
    fi
    echo "deploy-check: rehearsal passed — units verified, deploy dry-run only; nothing installed, nothing started"

# Run one backup now, through the timer's service unit (D12).
backup-now:
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user start pulse-backup.service

# Restore a backup (D12): stop the server, run the verified restore, start the
# server again. If the restore FAILS, the server is still restarted — the old
# target is untouched and keeps serving — and the failure is PROPAGATED: the
# recipe exits with restore's exit code and says so.
restore file:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{ file }}" ]; then
        echo "restore: no such backup file: {{ file }}" >&2
        exit 1
    fi
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user stop pulse-serve.service
    set +e
    "$HOME/.local/share/pulse-serve/bin/pulse" restore "{{ file }}" \
        --backup-dir "$HOME/pulse-backups" --replace
    RC=$?
    set -e
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user reset-failed pulse-serve.service || true
    XDG_RUNTIME_DIR="/run/user/$(id -u)" systemctl --user start pulse-serve.service
    if [ "$RC" -ne 0 ]; then
        echo "restore: RESTORE FAILED (rc=$RC) — pulse-serve restarted on the previous database" >&2
        exit "$RC"
    fi
    echo "restore: ok — pulse-serve restarted on the restored database"

# --- the Mac Mini prod service (r4.s2.w1, G5/G6/G7/G9/G10, ADR-0029) ---------

# Build a TAGGED commit ON the Mac Mini and install it as prod's always-on
# service: a launchd LaunchAgent in draco's gui/ domain, restarts bounded at 3
# in 15 minutes by the server's own start counter (launchd has no start limit).
#
# Runs HERE, on draco-desk, and drives the Mini over ssh (`Host macmini` in
# ~/.ssh/config). The cutover step of ADR-0029 — an operator action, never run
# by an item against the real service. The Mini gets SOURCE, never a GitHub
# credential (G6): `git archive <tag>` over ssh. It has no `just` and no Node,
# so the build happens there with cargo from rustup, and the placeholder dist
# is deliberate (#314, see `deploy` above: the server never serves the
# frontend).
#
# Before it loads anything it checks the host's permissions (G10): the data
# directory is mode 0700, and `<data dir>/.env`, if present, is 0600 and owned
# by draco — refused by name otherwise. The .env's CONTENTS are never read,
# printed or copied; only its mode and owner are.
#
# It clears the start limit (the marker and the start log, as `prod-reset`
# does), (re)loads the two agents, kickstarts the server, and only then reports
# success — through the SAME health gate as `deploy` (#346): the tailnet
# address has to answer, not just exist. If that gate fails, the agent is left
# loaded for inspection and the recipe exits non-zero.
deploy-mac tag:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ -z "{{ tag }}" ]; then
        echo "deploy-mac: a tag argument is required (e.g. just deploy-mac r4)" >&2
        exit 1
    fi
    if ! git rev-parse -q --verify "refs/tags/{{ tag }}" >/dev/null; then
        echo "deploy-mac: tag '{{ tag }}' does not exist in this repository" >&2
        exit 1
    fi
    # 1. The tag's source to the Mini (G6). The remote dir is cleared first.
    git archive "{{ tag }}" | ssh macmini 'rm -rf "$HOME/.cache/pulse-deploy/src" && mkdir -p "$HOME/.cache/pulse-deploy/src" && tar -x -C "$HOME/.cache/pulse-deploy/src"'
    # 2. Build there (`$HOME/.cargo/bin` because a non-interactive ssh gets no
    #    login PATH). #314: no Node on the Mini, and the server never reads the
    #    bundle — the placeholder dist is the deliberate setting.
    ssh macmini 'export PATH="$HOME/.cargo/bin:$PATH"; cd "$HOME/.cache/pulse-deploy/src" && PULSE_ALLOW_PLACEHOLDER_DIST=1 cargo build --release --bin pulse'
    # 3. Install the binary, the config dir, the log-rotation script and the
    #    two plists. A rollback to an old tag installs that tag's artifacts,
    #    so everything comes from $SRC on the Mini.
    ssh macmini 'set -e
        mkdir -p "$HOME/.local/share/pulse-serve/bin" "$HOME/.local/share/pulse-serve/deploy" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/PulseTrader"
        install -m 0755 "$HOME/.cache/pulse-deploy/src/target/release/pulse" "$HOME/.local/share/pulse-serve/bin/pulse"
        rm -rf "$HOME/.local/share/pulse-serve/config"
        mkdir -p "$HOME/.local/share/pulse-serve/config"
        cp -R "$HOME/.cache/pulse-deploy/src/config/." "$HOME/.local/share/pulse-serve/config/"
        install -m 0755 "$HOME/.cache/pulse-deploy/src/deploy/pulse-logrotate.sh" "$HOME/.local/share/pulse-serve/deploy/pulse-logrotate.sh"
        install -m 0644 "$HOME/.cache/pulse-deploy/src/deploy/com.pulsetrader.serve.plist" "$HOME/.cache/pulse-deploy/src/deploy/com.pulsetrader.logrotate.plist" "$HOME/Library/LaunchAgents/"'
    # 4. Permissions (G10). Names and modes only — the file is never printed.
    ssh macmini 'set -e
        DATA="$HOME/Library/Application Support/PulseTrader"
        if [ -d "$DATA" ]; then
            mode="$(stat -f %Lp "$DATA")"
            if [ "$mode" != "700" ]; then
                echo "deploy-mac: $DATA is mode $mode, want 0700 (G10) - refusing" >&2
                exit 1
            fi
        else
            echo "deploy-mac: $DATA does not exist yet; nothing to check (the server creates it on first start)" >&2
        fi
        if [ -f "$DATA/.env" ]; then
            mode="$(stat -f %Lp "$DATA/.env")"
            owner="$(stat -f %Su "$DATA/.env")"
            if [ "$mode" != "600" ]; then
                echo "deploy-mac: $DATA/.env is mode $mode, want 0600 (G10) - refusing (contents never printed)" >&2
                exit 1
            fi
            if [ "$owner" != "draco" ]; then
                echo "deploy-mac: $DATA/.env is owned by $owner, want draco (G10) - refusing (contents never printed)" >&2
                exit 1
            fi
        fi'
    # 5. Clear the start limit, (re)load both agents, kickstart the server.
    ssh macmini 'set -e
        DATA="$HOME/Library/Application Support/PulseTrader"
        rm -f "$DATA/serve-start-limit" "$DATA/serve-starts"
        launchctl bootout "gui/$(id -u)/com.pulsetrader.serve" 2>/dev/null || true
        launchctl bootout "gui/$(id -u)/com.pulsetrader.logrotate" 2>/dev/null || true
        launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.pulsetrader.serve.plist"
        launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/com.pulsetrader.logrotate.plist"
        launchctl kickstart -k "gui/$(id -u)/com.pulsetrader.serve"'
    # 6. The real health gate (#346), from here: the Mini's tailnet address has
    #    to answer, not just exist. 150 s covers the 120-second bind retry.
    bash scripts/wait-healthy.sh "http://100.103.30.74:8420" 150
    ssh macmini '"$HOME/.local/share/pulse-serve/bin/pulse" --version'

# Clear the Mini's start limit and restart its agent: the recovery for a tripped
# `--start-limit 3/900` (G5, #342's parity — systemd's equivalent here is
# `systemctl --user reset-failed pulse-serve.service`). It removes ONLY the
# marker and the start log in the data dir, then kickstarts.
prod-reset:
    #!/usr/bin/env bash
    set -euo pipefail
    ssh macmini 'rm -f "$HOME/Library/Application Support/PulseTrader/serve-start-limit" "$HOME/Library/Application Support/PulseTrader/serve-starts"'
    ssh macmini 'launchctl kickstart -k "gui/$(id -u)/com.pulsetrader.serve"'

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
