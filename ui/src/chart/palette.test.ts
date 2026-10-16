// The chart palette's own tests (r5.s2.w2) — the ten aliases the adapter draws with.
//
// Two things are worth proving here and nothing else is:
//
//  1. the aliases are READ off the element the chart lives in (never literals in
//     TypeScript), so a theme flip changes them without touching code;
//  2. an unresolvable token falls back to the platform's own `CanvasText`, which
//     is not a new colour (PDR-0031) — never to a hex or an hsl literal.
//
// The third case reads the screen sheet back and asserts the aliases are what
// they claim to be: aliases of existing hues, and never `--focus` (the ruling is
// explicit that focus blue is not an indicator colour).

import { describe, expect, it } from "vitest";

import { PALETTE_TOKENS, chartPalette } from "./palette";

/** The Node runtime globals vitest always runs on (the same pattern
 *  `format/no-float-money.test.ts` uses — this toolchain ships no `@types/node`). */
const nodeRuntime = (globalThis as unknown as {
  process: { cwd(): string; getBuiltinModule(id: string): unknown };
}).process;

interface FsLike {
  readFileSync(path: string, encoding: "utf8"): string;
}
const fs = nodeRuntime.getBuiltinModule("node:fs") as FsLike;

/** The ten aliases, in the order the sheet declares them. */
const ALIASES = Object.values(PALETTE_TOKENS);

/** A container with every alias set on it, as the loaded sheet would. */
function themed(): HTMLElement {
  const el = document.createElement("div");
  for (const token of ALIASES) {
    el.style.setProperty(token, `hsl(200 50% 50%) /* ${token} */`);
  }
  document.body.append(el);
  return el;
}

describe("chartPalette", () => {
  it("reads every alias off the element the chart lives in", () => {
    const el = themed();
    const palette = chartPalette(el);
    for (const [key, token] of Object.entries(PALETTE_TOKENS)) {
      expect(palette[key as keyof typeof palette]).toContain(token);
    }
  });

  it("falls back to the platform's own CanvasText when an alias is unresolvable", () => {
    // No sheet loaded (a bare element): the value is empty, and an empty value
    // must not become a colour this codebase invented.
    const el = document.createElement("div");
    const palette = chartPalette(el);
    for (const value of Object.values(palette)) {
      expect(value).toBe("CanvasText");
    }
  });

  it("names aliases of existing hues, and never the focus colour", () => {
    // The sheet is the one place a chart colour is decided; this reads it back.
    const sheet = fs.readFileSync(
      `${nodeRuntime.cwd()}/ui/src/styles/chart-tab.css`,
      "utf8",
    );
    const declared = [...sheet.matchAll(/(--chart-[a-z-]+):\s*([^;]+);/g)];
    expect(declared.length).toBe(ALIASES.length);
    for (const [, name, value] of declared) {
      expect(ALIASES).toContain(name);
      // An alias, not a colour: `var(--something)`.
      const aliased = /^var\((--[a-z0-9-]+)\)$/.exec(value.trim());
      expect(aliased, `${name} is not a bare alias: ${value.trim()}`).not.toBeNull();
      expect(aliased?.[1]).not.toBe("--focus");
    }
  });

  it("carries no colour literal in TypeScript", () => {
    const source = fs.readFileSync(`${nodeRuntime.cwd()}/ui/src/chart/palette.ts`, "utf8");
    const code = source
      .split("\n")
      .filter((line) => !line.trim().startsWith("//") && !line.trim().startsWith("*"))
      .join("\n");
    expect(/#[0-9a-fA-F]{3,8}\b/.test(code)).toBe(false);
    expect(/\b(?:hsl|hsla|rgb|rgba)\s*\(/.test(code)).toBe(false);
  });
});
