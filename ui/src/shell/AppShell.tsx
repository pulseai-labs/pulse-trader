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
//   2. The titlebar's `.title-status` pills (Claude Code turn count, Binance
//      connection, paper/live counts, LLM cost MTD) and the sidebar's per-item nav
//      counts and `.status-card` are NOT ported. Every one of those numbers in the
//      mock is invented sample data sized for layout (`r1.s1` SPINE.md, "Mock
//      numbers are fiction") — none of it is backed by a real read in this item's
//      scope, and rendering it would present a fabrication as real. This item ships
//      the frame, not fake state; a later item wires each of those to a real read
//      when the feature behind it actually exists.
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

import { useEffect, useState } from "react";
import type { HTMLAttributes, ReactNode } from "react";

import type { ServerStatus } from "../bindings";

import { getCurrentWindow } from "@tauri-apps/api/window";

import { isNavBuilt } from "../routes";

// ---------------------------------------------------------------------------
// Atoms (`app-shell.jsx`'s `Dot` / `Pill`)
// ---------------------------------------------------------------------------

interface DotProps {
  /** A CSS colour value — usually one of `tokens.css`'s semantic custom properties. */
  color: string;
  /** Whether the dot pulses (`shared.css`'s `@keyframes pulse`). */
  pulse?: boolean;
}

/**
 * A small status dot. Ported per the spec's step 3, but unused by the chrome
 * itself for the same reason `Pill` is below — the mock's only uses of it are the
 * fabricated `.title-status`/`.status-card` widgets this item does not render.
 * Built-but-unwired, ready for a later item with a real status to show.
 */
export function Dot({ color, pulse = false }: DotProps) {
  return (
    <span
      className="dot"
      style={{
        width: 6,
        height: 6,
        borderRadius: 99,
        background: color,
        flexShrink: 0,
        display: "inline-block",
        boxShadow: pulse ? `0 0 0 0 ${color}` : "none",
        animation: pulse ? "pulse 1.6s var(--ease-out) infinite" : "none",
      }}
    />
  );
}

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
 * Deliberately NOT ported: the per-item nav counts, the `filter-pills` block (a
 * per-screen concern `Strategy Library.html` alone used — no consumer needs it
 * yet, and an unused prop is exactly the speculative abstraction this codebase's
 * "Simplicity First" discipline rules out), and the `.status-card` panel. See this
 * file's header comment.
 */
export function Sidebar({ active }: SidebarProps) {
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
          <div className="brand-ver">v0.1.0 · local</div>
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

/**
 * The strip's exact words for a `ServerStatus` — the state word plus the
 * server's version when a handshake answered (`up`), or the recorded reason
 * when the last exchange was refused. No invented numbers.
 */
export function serverStatusText(status: ServerStatus): string {
  switch (status.state) {
    case "up":
      return `server up · ${status.binary_version ?? "?"}`;
    case "down":
      return "server down";
    case "not_connected":
      return "not connected";
    case "refused":
      return status.reason ?? "connection refused";
  }
}

interface WindowChromeProps {
  /** The open document's title, rendered after an em-dash. Omitted when nothing is open. */
  docTitle?: string;
  /**
   * The live server connection status (r3.s3.w5). The `.title-status` strip
   * is ported ONLY now that a real read exists — r1.s1 deliberately shipped
   * the chrome without it because the mock's pills were fabricated sample
   * data; this one renders exactly what `server_status` answered and nothing
   * else. Absent (undefined) renders no pill at all.
   */
  serverStatus?: ServerStatus;
  children?: ReactNode;
}

/**
 * The app-drawn titlebar + traffic lights (`tauri.conf.json` sets
 * `decorations: false` for exactly this reason) plus whatever `children` mounts
 * below it. `installFit()` is NOT ported — see this file's header comment.
 */
export function WindowChrome({ docTitle, serverStatus, children }: WindowChromeProps) {
  const { mode, theme, cycle } = useTheme();
  return (
    <div className="window" data-theme={theme}>
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
        <div className="title-block">
          <span className="title-app">PulseTrader</span>
          {docTitle !== undefined && (
            <>
              <span className="title-sep">—</span>
              <span className="title-doc">{docTitle}</span>
            </>
          )}
          {serverStatus !== undefined && (
            <span className="title-status">
              <span className="dot" data-state={serverStatus.state} />
              {serverStatusText(serverStatus)}
            </span>
          )}
        </div>

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
