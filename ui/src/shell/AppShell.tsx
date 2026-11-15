// The ported window chrome and sidebar (r1.s1.w5, ADR-0020's frontend half).
//
// Ported from `docs/design/mock/app-shell.jsx` (`WindowChrome` at :179, the traffic
// lights at :180) into React + TypeScript, per the spec's step 3: `WindowChrome`,
// `Sidebar`, and the small shared primitives the chrome needs (`Dot`, `Pill`,
// `NavIcon`, `ThemeIcon`, `useTheme`). `Kbd` and `Sparkline` are NOT ported — the
// chrome does not use them, and they belong to screens that are not in this item.
//
// Two things the mock did that this file deliberately does NOT do:
//
//   1. `installFit()` / the inline `fit()` script (`app-shell.jsx:213-221`) is not
//      ported (grill G1/A5). The mock scaled its whole canvas with a CSS
//      `transform` because it ran in a browser tab it did not control; this app owns
//      its 1440x900 window (`tauri.conf.json`, `resizable: false`), so there is
//      nothing to fit. `scripts/check-window-config.sh` fails if a scale-transform
//      reappears in any frontend source file, this one included.
//   2. The titlebar's status strip, the sidebar's nav counts and its `.status-card` are
//      ported from canvas v9 by r7.s2.w4 (`ShellStatus.tsx`), each value from an existing
//      read. The canvas's sample values (its cost figure, its agent name and last-seen
//      time, its token label) are not: those have no backend in r7, and each renders
//      `NotBuiltYet` in the canvas's place (PDR-0049 decision 7).
//
// One thing the mock's traffic lights never needed that this file now does: they are
// wired to real Tauri window APIs (`getCurrentWindow().close()` / `.minimize()`),
// because `decorations: false` makes this titlebar the window's only chrome -- inert
// lights would leave the window both unclosable and unmovable (the `.titlebar` carries
// `data-tauri-drag-region="deep"` for the same reason -- see the comment at that
// element for why `deep` and not the plain boolean form). The zoom light is rendered
// `disabled` instead of wired: `tauri.conf.json` sets `resizable: false`, so
// maximizing a fixed 1440x900 canvas is the same contradiction
// `check-window-config.sh` already polices for `fullscreen`/`maximized`.

import { useCallback, useEffect, useState } from "react";
import type { HTMLAttributes, ReactNode } from "react";

import type { ServerStatus } from "../bindings";

import { getCurrentWindow } from "@tauri-apps/api/window";

import { isNavBuilt } from "../routes";
import { EM_DASH } from "../format/numbers";
import { StatusCard, TitleStatus } from "./ShellStatus";

// ---------------------------------------------------------------------------
// Atoms (`app-shell.jsx`'s `Pill`; its `Dot` lives in `ShellStatus.tsx`)
// ---------------------------------------------------------------------------

/** The tone variants `strategy-library.css`/`strategy-designer.css` style `.pill-*` for. */
export type PillTone = "neutral" | "bull" | "bear" | "warn" | "ai";

interface PillProps extends HTMLAttributes<HTMLSpanElement> {
  children: ReactNode;
  tone?: PillTone;
}

/**
 * A small labelled pill. Ported per the spec's step 3, but unused by the chrome
 * itself — its `.pill`/`.pill-*` styling lives in a per-screen stylesheet this item
 * does not port (`strategy-library.css` and friends), so it renders unstyled until
 * `w3`/`w4` bring that CSS. Built-but-unwired on purpose, the same seam shape
 * `#[allow(dead_code)]` marks on the Rust side elsewhere in this codebase.
 */
export function Pill({ children, tone = "neutral", ...rest }: PillProps) {
  return (
    <span className={`pill pill-${tone}`} {...rest}>
      {children}
    </span>
  );
}

// ---------------------------------------------------------------------------
// Nav icons (`app-shell.jsx`'s `NavIcon`)
// ---------------------------------------------------------------------------

/** The icon names the sidebar's nav rows use. */
export type NavIconName =
  | "tree"
  | "chat"
  | "chart"
  | "candle"
  | "play"
  | "book"
  | "graph"
  | "gear"
  | "help";

const ICON_SVG_PROPS = {
  width: 14,
  height: 14,
  stroke: "currentColor",
  fill: "none",
  strokeWidth: 1.5,
  strokeLinecap: "round" as const,
  strokeLinejoin: "round" as const,
};

/** One of the sidebar's nav-row icons, ported verbatim from `app-shell.jsx`. */
export function NavIcon({ name }: { name: NavIconName }) {
  switch (name) {
    case "tree":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <circle cx="8" cy="3" r="1.5" />
          <circle cx="3" cy="12" r="1.5" />
          <circle cx="13" cy="12" r="1.5" />
          <path d="M8 4.5 L8 8 M8 8 L3 10.5 M8 8 L13 10.5" />
        </svg>
      );
    case "chat":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <path d="M2 4 a1.5 1.5 0 0 1 1.5 -1.5 H12.5 A1.5 1.5 0 0 1 14 4 V10 A1.5 1.5 0 0 1 12.5 11.5 H7 L4 14 V11.5 H3.5 A1.5 1.5 0 0 1 2 10 Z" />
        </svg>
      );
    case "chart":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <path d="M2 13 L2 3 M2 13 L14 13" />
          <path d="M4 11 L7 7 L9 9 L13 4" />
        </svg>
      );
    case "candle":
      // r5.s2.w2: the Chart tab's row, ported from the mock's own `candle` case
      // (`docs/design/mock/app-shell.jsx`). The Backtest Lab keeps `chart`: the two
      // are different destinations and one icon for both would read as one screen.
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <path d="M4.5 2 V14 M11.5 3 V13" />
          <rect x="3" y="5" width="3" height="5" rx="0.5" />
          <rect x="10" y="6" width="3" height="4" rx="0.5" fill="currentColor" />
        </svg>
      );
    case "play":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <circle cx="8" cy="8" r="6" />
          <path d="M6.5 5.5 L11 8 L6.5 10.5 Z" fill="currentColor" />
        </svg>
      );
    case "book":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <path d="M3 3 H12 A1 1 0 0 1 13 4 V13 H4 A1 1 0 0 1 3 12 Z" />
          <path d="M3 12 A1 1 0 0 1 4 11 H13" />
        </svg>
      );
    case "graph":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <rect x="2" y="9" width="2.5" height="5" />
          <rect x="6.5" y="5" width="2.5" height="9" />
          <rect x="11" y="2" width="2.5" height="12" />
        </svg>
      );
    case "gear":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <circle cx="8" cy="8" r="2" />
          <path d="M8 1.5 V3 M8 13 V14.5 M1.5 8 H3 M13 8 H14.5 M3.3 3.3 L4.4 4.4 M11.6 11.6 L12.7 12.7 M3.3 12.7 L4.4 11.6 M11.6 4.4 L12.7 3.3" />
        </svg>
      );
    case "help":
      return (
        <svg {...ICON_SVG_PROPS} viewBox="0 0 16 16">
          <circle cx="8" cy="8" r="6" />
          <path d="M6 6.5 a2 2 0 0 1 4 0 c0 1 -1 1.3 -2 2 V10" />
          <circle cx="8" cy="12" r="0.4" fill="currentColor" />
        </svg>
      );
  }
}

// ---------------------------------------------------------------------------
// Sidebar (`app-shell.jsx`'s `Sidebar`)
// ---------------------------------------------------------------------------

export interface NavEntry {
  readonly id: string;
  readonly label: string;
  readonly icon: NavIconName;
  /** A roadmap-tier badge (truthful, e.g. "v3") — never a fabricated count. */
  readonly tier?: string;
}

// r1.s1.w6 wires every row to `"#/" + item.id` (step 5) and derives, per row,
// whether its destination is built (`isNavBuilt`, G8) -- it does not add a
// pre-seeded "is this built" flag here. `w3`/`w4` make a row real by
// appending a `ROUTES` entry with a matching `nav` and an `element`; nothing
// in this table changes when they do.
export const NAV_MAIN: readonly NavEntry[] = [
  { id: "library", label: "Strategy Library", icon: "tree" },
  { id: "designer", label: "Strategy Designer", icon: "chat" },
  { id: "backtest", label: "Backtest Lab", icon: "chart" },
  // r5.s2.w2: the Chart tab, right after the Lab that opens it (the mock's own order).
  // The row's "Soon" badge disappears by derivation once its ROUTES entry lands (G8).
  { id: "chart", label: "Chart", icon: "candle" },
  { id: "deploy", label: "Deployment Dashboard", icon: "play" },
  { id: "journal", label: "Trade Journal", icon: "book" },
  { id: "analytics", label: "Analytics", icon: "graph", tier: "v3" },
];

export const NAV_BOTTOM: readonly NavEntry[] = [
  { id: "settings", label: "Settings", icon: "gear" },
  { id: "help", label: "Help", icon: "help" },
];

/** Every nav row, main + bottom, in display order. */
export const NAV_ALL: readonly NavEntry[] = [...NAV_MAIN, ...NAV_BOTTOM];

interface SidebarProps {
  /** The nav row id to mark `.is-active`, if any is mounted yet. */
  active?: string;
  /**
   * The live server connection status (r4.s5.w4, #310). The version label is
   * read from the handshake's `binary_version`, not a literal: a version bump
   * that misses this file used to ship a sidebar showing a stale number while
   * the Connect screen showed the real one.
   */
  serverStatus?: ServerStatus;
  /** The stored server URL (`server_address`), for the status card's server name (r7.s2.w4). */
  serverAddress?: string | null;
  /** The Library row's count: `shell_info.strategyCount`, only while the server is up (r7.s2.w4). */
  strategyCount?: number;
  /** The Deployment row's count: running paper sessions, only while the server is up (r7.s2.w4). */
  paperRunning?: number;
}

/** Navigate to the Strategy Designer -- shared by the "New strategy" button's
 * click and its `⌘N` shortcut so the two can never drift apart. Module-level
 * (not a closure) since it depends on nothing from `Sidebar`'s props or state. */
function goToDesigner() {
  window.location.hash = "#/designer";
}

/**
 * The ported sidebar: brand mark, the "New strategy" affordance, and the nav list.
 *
 * The nav counts and the `.status-card` are ported from canvas v9 (r7.s2.w4). The
 * `filter-pills` block stays unported: it is the Library screen's own (r8).
 */
export function Sidebar({
  active,
  serverStatus,
  serverAddress,
  strategyCount,
  paperRunning,
}: SidebarProps) {
  // The version label (#310): the handshake's own `binary_version`, and the
  // mode of the app's actual connection — a client of a server, never the
  // mock's hard-coded `local`. Until an `up` handshake answers, the em dash
  // claims nothing (a stale literal is the bug this replaced).
  const version =
    serverStatus?.state === "up" && serverStatus.binary_version !== null
      ? `v${serverStatus.binary_version} · server`
      : EM_DASH;
  // r7.s2.w4: the canvas counts two rows, each from an existing read.
  const counts: Readonly<Record<string, number | undefined>> = {
    library: strategyCount,
    deploy: paperRunning,
  };
  // The `⌘N` hint next to "New strategy" below only means something if the
  // shortcut is actually bound -- this is that binding (`/designer` is a real
  // route as of `r1.s1.w4`).
  useEffect(() => {
    function onKeyDown(event: KeyboardEvent) {
      if (event.metaKey && event.key === "n") {
        event.preventDefault();
        goToDesigner();
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  return (
    <aside className="sidebar">
      <div className="brand">
        <div className="brand-mark">
          <svg width="22" height="22" viewBox="0 0 32 32" aria-hidden="true">
            <rect
              x="0.5"
              y="0.5"
              width="31"
              height="31"
              rx="7"
              fill="var(--ai-bg)"
              stroke="var(--ai-line)"
            />
            <path
              d="M5 17 L11 17 L13 11 L17 23 L19 16 L23 19 L27 19"
              stroke="var(--ai-1)"
              strokeWidth="1.6"
              fill="none"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
        </div>
        <div className="brand-text">
          <div className="brand-name">PulseTrader</div>
          <div className="brand-ver">{version}</div>
        </div>
      </div>

      <button type="button" className="new-btn" onClick={goToDesigner}>
        <span>＋</span> New strategy
        <span className="kbd-mini">⌘N</span>
      </button>

      <nav className="nav">
        {NAV_MAIN.map((item) => (
          <a
            key={item.id}
            href={`#/${item.id}`}
            className={`nav-row ${active === item.id ? "is-active" : ""}`}
          >
            <NavIcon name={item.icon} />
            <span className="nav-label">{item.label}</span>
            {counts[item.id] !== undefined && <span className="nav-count">{counts[item.id]}</span>}
            {/* The tier badge and the derived "not built yet" badge share one
                slot and one vehicle -- `nav-soon` (r1.s1.w6, step 5) -- rather
                than stacking two badges on a row. A tier is itself a
                roadmap-truthful "not yet" signal, so it takes priority. */}
            {item.tier !== undefined ? (
              <span className="nav-soon">{item.tier}</span>
            ) : (
              !isNavBuilt(item.id) && <span className="nav-soon">Soon</span>
            )}
          </a>
        ))}
      </nav>

      <div className="spacer" />

      <nav className="nav">
        {NAV_BOTTOM.map((item) => (
          <a
            key={item.id}
            href={`#/${item.id}`}
            className={`nav-row ${active === item.id ? "is-active" : ""}`}
          >
            <NavIcon name={item.icon} />
            <span className="nav-label">{item.label}</span>
            {!isNavBuilt(item.id) && <span className="nav-soon">Soon</span>}
          </a>
        ))}
      </nav>

      <StatusCard status={serverStatus} serverAddress={serverAddress} />
    </aside>
  );
}

// ---------------------------------------------------------------------------
// Theme (`app-shell.jsx`'s `useTheme` / `ThemeIcon`)
// ---------------------------------------------------------------------------

type ThemeMode = "dark" | "light" | "system";
type Theme = "dark" | "light";

const THEME_STORAGE_KEY = "pt-theme";

function readStoredMode(): ThemeMode {
  try {
    const stored = localStorage.getItem(THEME_STORAGE_KEY);
    return stored === "light" || stored === "system" ? stored : "dark";
  } catch {
    // Storage can be unavailable (private mode, quota) -- default to dark rather
    // than fail the paint.
    return "dark";
  }
}

/** Cycle dark -> light -> system -> dark, persisted to `localStorage`. */
function useTheme(): { mode: ThemeMode; theme: Theme; cycle: () => void } {
  const [mode, setMode] = useState<ThemeMode>(readStoredMode);
  const [systemPrefersLight, setSystemPrefersLight] = useState<boolean>(
    () => window.matchMedia("(prefers-color-scheme: light)").matches,
  );

  useEffect(() => {
    const query = window.matchMedia("(prefers-color-scheme: light)");
    const onChange = (event: MediaQueryListEvent) => setSystemPrefersLight(event.matches);
    query.addEventListener("change", onChange);
    return () => query.removeEventListener("change", onChange);
  }, []);

  const theme: Theme = mode === "system" ? (systemPrefersLight ? "light" : "dark") : mode;

  useEffect(() => {
    try {
      localStorage.setItem(THEME_STORAGE_KEY, mode);
    } catch {
      // Not persisting is not fatal -- the theme still applies for this session.
    }
    document.documentElement.setAttribute("data-theme", theme);
  }, [mode, theme]);

  const cycle = () =>
    setMode((current) =>
      current === "dark" ? "light" : current === "light" ? "system" : "dark",
    );

  return { mode, theme, cycle };
}

const SIDEBAR_STORAGE_KEY = "pt-sidebar";

function readStoredSidebar(): boolean {
  try {
    return localStorage.getItem(SIDEBAR_STORAGE_KEY) !== "off";
  } catch {
    return true;
  }
}

/** The canvas's sidebar show/hide (`app-shell.jsx` `useSidebar`): persisted as "on"/"off",
 * toggled by the titlebar button and ⌘\ (Ctrl+\ too, as the canvas binds it). */
function useSidebar(enabled: boolean): { open: boolean; toggle: () => void } {
  const [open, setOpen] = useState<boolean>(readStoredSidebar);
  const toggle = useCallback(() => setOpen((current) => !current), []);
  useEffect(() => {
    try {
      localStorage.setItem(SIDEBAR_STORAGE_KEY, open ? "on" : "off");
    } catch {
      // Not persisting is not fatal.
    }
  }, [open]);
  useEffect(() => {
    if (!enabled) {
      return;
    }
    function onKeyDown(event: KeyboardEvent) {
      if ((event.metaKey || event.ctrlKey) && event.key === "\\") {
        event.preventDefault();
        toggle();
      }
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [enabled, toggle]);
  return { open, toggle };
}

/** The titlebar's theme-toggle glyph, one per mode. */
function ThemeIcon({ mode }: { mode: ThemeMode }) {
  const shared = {
    width: 14,
    height: 14,
    viewBox: "0 0 16 16",
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.5,
  };
  if (mode === "light") {
    return (
      <svg {...shared}>
        <circle cx="8" cy="8" r="3.2" />
        <path
          d="M8 1.5 V3 M8 13 V14.5 M1.5 8 H3 M13 8 H14.5 M3.4 3.4 L4.5 4.5 M11.5 11.5 L12.6 12.6 M3.4 12.6 L4.5 11.5 M11.5 4.5 L12.6 3.4"
          strokeLinecap="round"
        />
      </svg>
    );
  }
  if (mode === "system") {
    return (
      <svg {...shared}>
        <circle cx="8" cy="8" r="6" />
        <path d="M8 2 A6 6 0 0 1 8 14 Z" fill="currentColor" stroke="none" />
      </svg>
    );
  }
  return (
    <svg {...shared}>
      <path d="M8 2 A6 6 0 1 0 14 8 A4.5 4.5 0 0 1 8 2 Z" />
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Window chrome (`app-shell.jsx`'s `WindowChrome`)
// ---------------------------------------------------------------------------

interface WindowChromeProps {
  /** The open document's title, rendered after an em-dash. Omitted when nothing is open. */
  docTitle?: string;
  /**
   * The live server connection status. The strip renders only from it and the
   * props below, never sample data. Absent renders no strip.
   */
  serverStatus?: ServerStatus;
  /** The stored server URL, for the strip's server name and popover (r7.s2.w4). */
  serverAddress?: string | null;
  /**
   * How many paper sessions are running, when the count is known (r3.s4.w5).
   * The strip renders "N paper · 0 live" — the live half is the recorded truth
   * that no live-execution path exists in this release, never a count read
   * from anywhere. Absent renders the pill with an em dash.
   */
  paperRunning?: number;
  /** Whether a sidebar is mounted below: renders the canvas's show/hide toggle and binds ⌘\ (r7.s2.w4). */
  hasSidebar?: boolean;
  children?: ReactNode;
}

/**
 * The app-drawn titlebar + traffic lights (`tauri.conf.json` sets
 * `decorations: false` for exactly this reason) plus whatever `children` mounts
 * below it. `installFit()` is NOT ported — see this file's header comment.
 */
export function WindowChrome({
  docTitle,
  serverStatus,
  serverAddress = null,
  paperRunning,
  hasSidebar = false,
  children,
}: WindowChromeProps) {
  const { mode, theme, cycle } = useTheme();
  const { open: sidebarOpen, toggle: toggleSidebar } = useSidebar(hasSidebar);
  return (
    <div className={`window${hasSidebar && !sidebarOpen ? " sb-off" : ""}`} data-theme={theme}>
      {/* `deep` (Tauri 2.11+, resolved here at 2.11.5) makes every non-interactive
          descendant a drag target too, not just the elements literally carrying the
          attribute -- Tauri's drag handler tests the EVENT TARGET, not its ancestors,
          so the plain boolean form only dragged from the bare padding of `.titlebar`
          and `.title-block` themselves, missing the `.title-app`/`.title-doc` spans a
          user actually clicks on. Interactive descendants (the traffic-light buttons,
          the theme `icon-btn`) are excluded automatically and stay clickable, so
          `.title-block` no longer needs its own `data-tauri-drag-region` -- `deep` on
          this parent already covers it, and a second overlapping declaration would
          just be one more thing to keep in sync. */}
      <div className="titlebar" data-tauri-drag-region="deep">
        <div className="traffic">
          <button
            type="button"
            style={{ background: "#ff5f57" }}
            aria-label="Close window"
            onClick={() => {
              getCurrentWindow()
                .close()
                .catch(() => {
                  // A rejection here means the window API itself is unreachable
                  // (e.g. no Tauri runtime in a non-Tauri preview) -- nothing else
                  // to do about it from a click handler.
                });
            }}
          />
          <button
            type="button"
            style={{ background: "#febc2e" }}
            aria-label="Minimize window"
            onClick={() => {
              getCurrentWindow()
                .minimize()
                .catch(() => {
                  // Same reasoning as the close button above.
                });
            }}
          />
          <button
            type="button"
            style={{ background: "#28c840" }}
            aria-label="Zoom (this window is a fixed size)"
            title="This window is a fixed 1440×900 size — zoom is disabled"
            disabled
          />
        </div>
        {hasSidebar && (
          <button
            type="button"
            className={`icon-btn sb-toggle${sidebarOpen ? "" : " is-off"}`}
            onClick={toggleSidebar}
            aria-pressed={!sidebarOpen}
            aria-label={sidebarOpen ? "Hide sidebar" : "Show sidebar"}
            title={`${sidebarOpen ? "Hide" : "Show"} sidebar  ⌘\\`}
          >
            <svg
              width="15"
              height="15"
              viewBox="0 0 16 16"
              fill="none"
              stroke="currentColor"
              strokeWidth="1.4"
              aria-hidden="true"
            >
              <rect x="2" y="2.5" width="12" height="11" rx="1.5" />
              <line x1="6" y1="2.5" x2="6" y2="13.5" />
              {sidebarOpen && (
                <path d="M3.4 5.5 H4.6 M3.4 7.5 H4.6 M3.4 9.5 H4.6" strokeLinecap="round" />
              )}
            </svg>
          </button>
        )}
        <div className="title-block">
          <span className="title-app">PulseTrader</span>
          {docTitle !== undefined && (
            <>
              <span className="title-sep">—</span>
              <span className="title-doc">{docTitle}</span>
            </>
          )}
        </div>
        {serverStatus !== undefined && (
          <TitleStatus
            status={serverStatus}
            serverAddress={serverAddress}
            paperRunning={paperRunning}
          />
        )}

        <div className="title-right">
          <button
            type="button"
            className="icon-btn"
            title={`Theme: ${mode} — click to switch`}
            onClick={cycle}
          >
            <ThemeIcon mode={mode} />
          </button>
        </div>
      </div>
      {children}
    </div>
  );
}
