// The chart's palette (r5.s2.w2) — the ONLY place the chart's colours are named.
//
// The chart library paints on a canvas, so it needs colour VALUES, not custom
// properties. This module reads them off the element the chart is mounted in, where
// they are inherited from the screen's own sheet (`ui/src/styles/chart-tab.css`).
// Two consequences that are the whole point:
//
//  * the colours live in CSS, so a theme flip changes them without touching code, and
//    `tokens.css` stays the one owner of every hue;
//  * nothing here is a literal. `chart-tab.css` declares each alias as `var(--token)`
//    of an existing hue, and `--focus` is never one of them (PDR-0031: focus blue is
//    not an indicator colour).
//
// An unresolvable alias (no sheet loaded) falls back to `CanvasText` — the platform's
// OWN text colour, which follows the system theme. It is deliberately not a hex or an
// hsl literal: this codebase does not invent colours, even in its fallbacks.

/** The CSS custom property every chart colour is read from. */
export const PALETTE_TOKENS = {
  /** The chart's own background. */
  background: "--chart-bg",
  /** Axis, crosshair label and watermark text. */
  text: "--chart-text",
  /** Grid lines. */
  grid: "--chart-grid",
  /** A candle that closed up, and a winner's marker. */
  bull: "--chart-bull",
  /** A candle that closed down, and a loser's marker. */
  bear: "--chart-bear",
  /** An open trade's marker (amber). */
  open: "--chart-open",
  /** A server-computed indicator line (an EMA): dashed, neutral. */
  serverLine: "--chart-server-line",
  /** A server-computed S/R level: dashed, neutral. */
  serverLevel: "--chart-server-level",
  /** A confirmed pivot's marker. */
  pivot: "--chart-pivot",
  /** The faint ghost on the bar the pivot peaked, `right` bars earlier (G3). */
  pivotGhost: "--chart-pivot-ghost",
} as const;

/** Every colour the chart draws with, resolved to a value the canvas accepts. */
export type ChartPalette = { readonly [K in keyof typeof PALETTE_TOKENS]: string };

/** What an alias that resolves to nothing becomes (see this file's header). */
const FALLBACK = "CanvasText";

/** Resolve the chart's palette from the element the chart is mounted in. */
export function chartPalette(root: Element): ChartPalette {
  const computed = getComputedStyle(root);
  const read = (token: string): string => {
    const value = computed.getPropertyValue(token).trim();
    return value === "" ? FALLBACK : value;
  };
  return {
    background: read(PALETTE_TOKENS.background),
    text: read(PALETTE_TOKENS.text),
    grid: read(PALETTE_TOKENS.grid),
    bull: read(PALETTE_TOKENS.bull),
    bear: read(PALETTE_TOKENS.bear),
    open: read(PALETTE_TOKENS.open),
    serverLine: read(PALETTE_TOKENS.serverLine),
    serverLevel: read(PALETTE_TOKENS.serverLevel),
    pivot: read(PALETTE_TOKENS.pivot),
    pivotGhost: read(PALETTE_TOKENS.pivotGhost),
  };
}
