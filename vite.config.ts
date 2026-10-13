// Vite config for the PulseTrader desktop frontend (ADR-0020).
//
// `root` is `ui/`, so the HTML entry point and the TypeScript sources live beside each
// other and away from the Rust tree. `outDir` resolves to `ui/dist`, which is what
// `tauri.conf.json`'s `frontendDist` points at and what `build.rs` writes a placeholder
// into when it is absent (so a Rust-only `cargo test` still compiles).
//
// `emptyOutDir` is on: a stale asset from a previous build must not survive into the
// bundle, because `generate_context!` embeds whatever is in that directory.
//
// `@vitejs/plugin-react` (r1.s1.w5, ADR-0020 step 1) is the JSX/Fast-Refresh transform
// for the design system. r4.s5.w5 moved the toolchain to Vite 8 (rolldown +
// lightningcss, #121/#122) and the plugin to its Vite 8 major (`^6.x`, whose peer
// range is `vite ^8.0.0`; the `^5.x` line was the pre-Vite-8 major the old note
// pinned while this repo was still on Vite 6).
//
// `build.target: "safari15"` is honoured by Vite 8's rolldown/oxc transform, which
// matters because the app's renderer is WKWebView (ADR-0020's recorded risk):
// verified for r4.s5.w5 by lowering a class static block (Safari 16.4+) out of a
// probe bundle, and an unknown target is a hard build error, not a silent default.
//
// `defineConfig` comes from `vitest/config` rather than `vite` (r1.s1.w6, G9): it
// re-exports Vite's own `defineConfig` with the `test` key merged into the type, so
// this stays ONE config file/format for both `vite build` and `vitest run` rather than
// a second config format bolted on beside it (vitest `^5.0.4` still ships that export,
// re-checked at r4.s5.w5). `test.environment` is `jsdom` (not the
// default `node`) because the rendered layer's assertions (exactly one nav row is
// `.is-active`, no row is a dead link) need a DOM to query.
import react from "@vitejs/plugin-react";
import { defineConfig } from "vitest/config";

export default defineConfig({
  root: "ui",
  plugins: [react()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "safari15",
    // The window is a fixed 1440x900 desktop shell, not a page served over a network.
    // Sourcemaps cost nothing at that size and make a WKWebView-only bug debuggable --
    // which matters given ADR-0020's recorded "WKWebView is not Chromium" risk.
    sourcemap: true,
  },
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  test: {
    environment: "jsdom",
    include: ["src/**/*.test.{ts,tsx}"],
    // No `globals: true` -- `describe`/`it`/`expect` are imported explicitly per file,
    // matching this codebase's no-implicit-globals style everywhere else.
    // No `restoreMocks` -- it resets every `vi.fn()` (including ones a
    // `vi.mock()` factory or a setup file establishes once) before EACH
    // test, which silently empties a `mockResolvedValue`/`mockImplementation`
    // set up outside a `beforeEach`. Tests that need per-test isolation
    // reset their own mocks explicitly.
    setupFiles: ["src/test/setup.ts"],
  },
});
