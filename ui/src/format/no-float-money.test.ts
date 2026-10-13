// The no-float guard (r4.s5.w1, A1): the screens and components render the
// server's decimal text through the format module, never through a float.
// This file reads their sources back and fails if `toFixed(` appears at all,
// or if `parseFloat(` / `parseInt(` / `Number(` appears outside a line (or a
// comment block) marked `// chart geometry`, or outside the one allow-listed
// line named below.
//
// Two escape hatches, both narrow and deliberately explicit:
//
//  1. `// chart geometry` — a DTO string that must become a number to place a
//     pixel. Nothing else may hide behind it.
//  2. ONE entry in ALLOWED, naming the file, the exact line and the reason.
//     Matching is file AND exact trimmed line, so editing the line re-opens
//     the question. The pattern itself is never weakened for it.

import { describe, expect, it } from "vitest";

// Reading the sources: this toolchain deliberately ships no `@types/node` (see
// package.json), so the Node builtins are reached through `globalThis` and one
// cast — the same pattern `ui/src/styles/backtest.test.ts` uses.

/** The Node runtime globals vitest always runs on. */
const nodeRuntime = (globalThis as unknown as {
  process: { cwd(): string; getBuiltinModule(id: string): unknown };
}).process;

/** The minimum `fs` surface this guard needs. */
interface FsLike {
  existsSync(path: string): boolean;
  readdirSync(path: string, options: { withFileTypes: true }): { name: string; isDirectory(): boolean }[];
  readFileSync(path: string, encoding: "utf8"): string;
}

/** The minimum `path` surface this guard needs. */
interface PathLike {
  join(...parts: string[]): string;
}

const fs = nodeRuntime.getBuiltinModule("node:fs") as FsLike;
const path = nodeRuntime.getBuiltinModule("node:path") as PathLike;

/** The four directories the AC-5 census covers, from this file's own location
 * (`ui/src/format/`), so the scan does not depend on the process cwd. */
const SCANNED_DIRS = ["screens", "components", "paper", "shell"] as const;

const MARKER = "// chart geometry";

/** A float conversion under any spelling this codebase uses. */
const FLOAT_CALL = /(?:\bparseFloat|\bparseInt|\bNumber)\s*\(/;

/** `toFixed(` — always a violation, marker or not. */
const TO_FIXED = /\btoFixed\s*\(/;

/** The one allow-listed line: the folds input's integer count. `/^\d+$/` is
 * what makes the parse exact; it is not a displayed money or price value. */
const ALLOWED = [
  {
    file: "ui/src/screens/BacktestLabScreen.tsx",
    line: "const k = Number.parseInt(text, 10);",
    reason: "the fold-count input: an integer behind its own /^\\d+$/ guard, not a displayed figure",
  },
];

interface Source {
  path: string;
  text: string;
}
interface Violation {
  path: string;
  line: number;
  text: string;
  reason: string;
}

/** Whether the marker sits on this line or in the contiguous comment block
 * directly above it. */
function markedAt(lines: readonly string[], index: number): boolean {
  if (lines[index].includes(MARKER)) return true;
  for (let i = index - 1; i >= 0; i -= 1) {
    const trimmed = lines[i].trim();
    if (trimmed.startsWith("//") || trimmed.startsWith("*") || trimmed.startsWith("/*")) {
      if (lines[i].includes(MARKER)) return true;
      continue;
    }
    return false;
  }
  return false;
}

/** The scanner, as one function over in-memory sources so the synthetic
 * self-checks below can drive it directly. */
export function scan(sources: readonly Source[]): Violation[] {
  const violations: Violation[] = [];
  for (const source of sources) {
    const lines = source.text.split("\n");
    lines.forEach((line, index) => {
      if (TO_FIXED.test(line)) {
        violations.push({
          path: source.path,
          line: index + 1,
          text: line.trim(),
          reason: "toFixed( — a float-rounded figure; render it through the format module",
        });
        return;
      }
      if (!FLOAT_CALL.test(line)) return;
      const allowed = ALLOWED.some(
        (entry) => entry.file === source.path && entry.line === line.trim(),
      );
      if (allowed) return;
      if (markedAt(lines, index)) return;
      violations.push({
        path: source.path,
        line: index + 1,
        text: line.trim(),
        reason: "unmarked float conversion — mark the line `// chart geometry` or use the format module",
      });
    });
  }
  return violations;
}

/** Locate `ui/src` from wherever vitest started (the repo root when `npm run
 * test` runs from there, `ui/` when it runs with that as its root), and give
 * every path the same repo-relative prefix the allow-list uses. */
function locateUiSrc(): string {
  const candidates = [
    path.join(nodeRuntime.cwd(), "ui/src"),
    path.join(nodeRuntime.cwd(), "src"),
  ];
  for (const candidate of candidates) {
    if (fs.existsSync(path.join(candidate, "screens"))) return candidate;
  }
  throw new Error(`the guard could not locate ui/src from ${nodeRuntime.cwd()}`);
}

/** Every `.ts`/`.tsx` under the four scanned directories. */
function sourcesUnder(): Source[] {
  const uiSrc = locateUiSrc();
  const sources: Source[] = [];
  for (const dir of SCANNED_DIRS) {
    const walk = (absolute: string, relative: string) => {
      for (const entry of fs.readdirSync(absolute, { withFileTypes: true })) {
        const childAbsolute = path.join(absolute, entry.name);
        const childRelative = `${relative}/${entry.name}`;
        if (entry.isDirectory()) {
          walk(childAbsolute, childRelative);
        } else if (/\.tsx?$/.test(entry.name)) {
          sources.push({ path: childRelative, text: fs.readFileSync(childAbsolute, "utf8") });
        }
      }
    };
    walk(path.join(uiSrc, dir), `ui/src/${dir}`);
  }
  return sources;
}

describe("the no-float guard detects", () => {
  it("flags an unmarked conversion at the right line", () => {
    const violations = scan([
      { path: "ui/src/screens/Example.tsx", text: "const a = 1;\nconst probe = parseFloat(x);" },
    ]);
    expect(violations).toEqual([
      {
        path: "ui/src/screens/Example.tsx",
        line: 2,
        text: "const probe = parseFloat(x);",
        reason:
          "unmarked float conversion — mark the line `// chart geometry` or use the format module",
      },
    ]);
  });

  it("clears a conversion the marker covers, on the line or in the block above", () => {
    expect(
      scan([
        {
          path: "ui/src/screens/Example.tsx",
          text: [
            "// chart geometry: the y position only",
            "const y = num(value);",
            "const z = parseFloat(value); // chart geometry",
          ].join("\n"),
        },
      ]),
    ).toEqual([]);
  });

  it("flags toFixed( even when the marker is present", () => {
    const violations = scan([
      { path: "ui/src/screens/Example.tsx", text: "const s = x.toFixed(2); // chart geometry" },
    ]);
    expect(violations).toHaveLength(1);
    expect(violations[0].reason).toContain("toFixed(");
  });
});

describe("the real tree", () => {
  it("is clean over the screens and components", () => {
    const sources = sourcesUnder();
    // Non-vacuity: a scan that read nothing proves nothing.
    expect(sources.length).toBeGreaterThanOrEqual(10);
    expect(scan(sources)).toEqual([]);
  });
});
