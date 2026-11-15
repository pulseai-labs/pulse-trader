// The Chart screen's line cap (r7.s2.w2, #502 C5): every function in the screen's files stays at or
// under 80 lines of logic (`.claude/memory-bank/03-code-patterns.md`, User-global defaults).
//
// The count is read off the sources, the way `ui/src/chart/palette.test.ts` reads the sheet. Blank
// lines, comment lines and the returned JSX are markup and notes, not logic, so they do not count.

import { describe, expect, it } from "vitest";

/** The Node runtime globals vitest always runs on (this toolchain ships no `@types/node`). */
const nodeRuntime = (globalThis as unknown as {
  process: { cwd(): string; getBuiltinModule(id: string): unknown };
}).process;

interface FsLike {
  readFileSync(path: string, encoding: "utf8"): string;
  readdirSync(path: string): string[];
}
const fs = nodeRuntime.getBuiltinModule("node:fs") as FsLike;

/** Each top-level function's logic lines: the lines from its `function` line to the closing `}` at column 0
 *  (both included), less blank lines, comment lines (trimmed start `//`, `/*` or `*`) and the returned JSX (from a line that is
 *  exactly `  return (` to the next line that is exactly `  );`, both included). */
function logicLines(source: string): Map<string, number> {
  const counts = new Map<string, number>();
  const lines = source.split("\n");
  let name: string | null = null;
  let count = 0;
  let inJsx = false;
  for (const line of lines) {
    if (name === null) {
      const start = /^(?:export )?(?:default )?function (\w+)/.exec(line);
      if (start === null) continue;
      name = start[1];
      count = 0;
      inJsx = false;
    }
    if (inJsx) {
      if (line === "  );") inJsx = false;
    } else if (line === "  return (") {
      inJsx = true;
    } else {
      const trimmed = line.trim();
      const note = trimmed.startsWith("//") || trimmed.startsWith("/*") || trimmed.startsWith("*");
      if (trimmed !== "" && !note) count += 1;
    }
    if (line === "}") {
      counts.set(name, count);
      name = null;
    }
  }
  return counts;
}

/** The screen's own source files: the screen and every non-test `.ts` and `.tsx` in `screens/chart/`. */
function screenFiles(): string[] {
  const root = `${nodeRuntime.cwd()}/ui/src/screens`;
  const inChart = fs
    .readdirSync(`${root}/chart`)
    .filter((file) => /\.tsx?$/.test(file) && !file.includes(".test."))
    .map((file) => `chart/${file}`);
  return ["ChartScreen.tsx", ...inChart].map((file) => `${root}/${file}`);
}

describe("the chart screen under the line cap (r7.s2.w2, #502 C5)", () => {
  it("measures logic lines without blank lines, comments or the returned JSX", () => {
    const fixture = [
      "// header",
      "function sample() {",
      "  // a comment",
      "  const a = 1;",
      "",
      "  /* block */",
      "  return (",
      "    <div>",
      "      {a}",
      "    </div>",
      "  );",
      "}",
    ].join("\n");
    expect(logicLines(fixture)).toEqual(new Map([["sample", 3]]));
  });

  it("keeps every function in the chart screen files at or under 80 logic lines", () => {
    const found: string[] = [];
    for (const file of screenFiles()) {
      const counts = logicLines(fs.readFileSync(file, "utf8"));
      for (const [name, count] of counts) {
        found.push(name);
        expect(count, `${file} ${name}`).toBeLessThanOrEqual(80);
      }
    }
    expect(found).toEqual(
      expect.arrayContaining([
        "ChartScreen",
        "useReadState",
        "useSelectionState",
        "useMarkState",
        "useChartCanvas",
        "useMarkWriter",
        "useZoomKeys",
        "useRunFragment",
        "useMarksRead",
        "useChartRead",
        "useChartDraw",
        "chartHandlers",
        "goToHandler",
        "chartView",
        "tradeRows",
        "overlayRows",
        "holdoutTag",
        "legendBar",
        "ChartToolbar",
        "MarkBar",
        "DrawRail",
        "ChartPane",
        "OhlcLegend",
        "ChartNotes",
        "OtherTimeframeNote",
        "SelectedTradeTable",
        "TradesTable",
        "MarksTable",
        "ChartDataTables",
        "CandleTable",
        "refusalText",
        "requestFor",
        "readRun",
      ]),
    );
  });
});
