// Regression tests for the window-chrome controls and the "New strategy"
// affordance a PR review found inert (r1.s1 PR review, `tauri.conf.json`'s
// `decorations: false` makes the app-drawn `.titlebar` the window's ONLY
// chrome, so a dead close/minimize button or an unbound `⌘N` hint is not
// cosmetic -- it is a control the user has no other way to reach).
//
// `getCurrentWindow` is mocked the same shape `../bindings` is mocked
// elsewhere in this suite (a `vi.mock` factory ahead of the mocked module's
// first import): `@tauri-apps/api/window`'s real implementation talks to
// `window.__TAURI_INTERNALS__.invoke`, which has no jsdom stub (`test/setup.ts`
// only stubs `transformCallback`, for the unrelated `Channel` case), so the
// real module cannot run under jsdom at all.

import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const closeMock = vi.fn().mockResolvedValue(undefined);
const minimizeMock = vi.fn().mockResolvedValue(undefined);

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({
    close: closeMock,
    minimize: minimizeMock,
  }),
}));

import { Sidebar, WindowChrome } from "./AppShell";
import type { ServerStatus } from "../bindings";

function setHash(hash: string) {
  window.location.hash = hash;
}

describe("WindowChrome traffic lights", () => {
  beforeEach(() => {
    closeMock.mockClear();
    minimizeMock.mockClear();
  });

  it("marks the titlebar a deep drag region, and its title block no longer redundantly", () => {
    const { container } = render(<WindowChrome />);
    const titlebar = container.querySelector(".titlebar");
    const titleBlock = container.querySelector(".title-block");
    expect(titlebar?.getAttribute("data-tauri-drag-region")).toBe("deep");
    expect(titleBlock?.hasAttribute("data-tauri-drag-region")).toBe(false);
  });

  it("renders close and minimize as real buttons that call the mocked window API", () => {
    render(<WindowChrome />);
    const closeBtn = screen.getByRole("button", { name: "Close window" });
    const minimizeBtn = screen.getByRole("button", { name: "Minimize window" });
    expect(closeBtn.tagName).toBe("BUTTON");
    expect(minimizeBtn.tagName).toBe("BUTTON");

    fireEvent.click(closeBtn);
    expect(closeMock).toHaveBeenCalledTimes(1);
    expect(minimizeMock).not.toHaveBeenCalled();

    fireEvent.click(minimizeBtn);
    expect(minimizeMock).toHaveBeenCalledTimes(1);
  });

  it("renders the zoom control disabled -- the window is not resizable", () => {
    render(<WindowChrome />);
    const zoomBtn = screen.getByRole("button", {
      name: "Zoom (this window is a fixed size)",
    }) as HTMLButtonElement;
    expect(zoomBtn.disabled).toBe(true);
  });

  it("renders no Layout button -- no layout feature exists (r1 dead-control convention)", () => {
    render(<WindowChrome />);
    expect(screen.queryByRole("button", { name: /layout/i })).toBeNull();
  });
});

describe("WindowChrome server status strip", () => {
  function status(role: string | null): ServerStatus {
    return {
      state: "up",
      binary_version: "0.1.3",
      engine_fingerprint: null,
      reason: null,
      role,
    };
  }

  it("shows the QA badge when the connected server reports role qa", () => {
    render(<WindowChrome serverStatus={status("qa")} />);
    expect(screen.getByText("QA")).toBeTruthy();
  });

  it("shows nothing new for prod or an absent role", () => {
    const { rerender } = render(<WindowChrome serverStatus={status("prod")} />);
    expect(screen.queryByText("QA")).toBeNull();
    rerender(<WindowChrome serverStatus={status(null)} />);
    expect(screen.queryByText("QA")).toBeNull();
  });
});

describe("Sidebar 'New strategy'", () => {
  beforeEach(() => {
    setHash("");
  });

  afterEach(() => {
    setHash("");
  });

  it("navigates to the Designer route when activated", () => {
    render(<Sidebar />);
    fireEvent.click(screen.getByRole("button", { name: /new strategy/i }));
    expect(window.location.hash).toBe("#/designer");
  });

  it("navigates to the Designer route on the advertised ⌘N shortcut", () => {
    render(<Sidebar />);
    fireEvent.keyDown(window, { key: "n", metaKey: true });
    expect(window.location.hash).toBe("#/designer");
  });
});

// ---------------------------------------------------------------------------
// r4.s5.w4 — the sidebar version and the strip's no-wrap rule
// ---------------------------------------------------------------------------

/** The Node runtime vitest runs on, named rather than accessed through an
 * inline cast. This toolchain ships no `@types/node` (see package.json), so
 * the runtime globals are reached through `globalThis` — the
 * `styles/backtest.test.ts` precedent. */
interface NodeRuntime {
  process: { cwd(): string; getBuiltinModule(id: string): unknown };
}

const nodeRuntime = globalThis as unknown as NodeRuntime;

interface FsLike {
  readFileSync(path: string, encoding: "utf8"): string;
}

/** The ported sheet the strip's rules live in, read from disk: vitest stubs
 * CSS imports, so pinning a rule means reading the file. */
function readSharedCss(): string {
  const fs = nodeRuntime.process.getBuiltinModule("node:fs") as FsLike;
  return fs.readFileSync(`${nodeRuntime.process.cwd()}/ui/src/styles/shared.css`, "utf8");
}

/** The body of the first `selector { ... }` rule in `css`, or null when the
 * selector is absent. These rules hold no nested braces, so a plain slice to
 * the first `}` is exact. */
function ruleBody(css: string, selector: string): string | null {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = new RegExp(`${escaped}\\s*\\{([^}]*)\\}`).exec(css);
  return match === null ? null : match[1];
}

function upStatus(version: string): ServerStatus {
  return {
    state: "up",
    binary_version: version,
    engine_fingerprint: null,
    reason: null,
    role: null,
  };
}

function downStatus(): ServerStatus {
  return {
    state: "down",
    binary_version: null,
    engine_fingerprint: null,
    reason: null,
    role: null,
  };
}

describe("the sidebar version label (#310)", () => {
  it("reads the version from the status handshake, never a hard-coded string (#310)", () => {
    const { rerender } = render(<Sidebar serverStatus={upStatus("0.1.3")} />);
    // The handshake answered `up`: the version comes from it, and the mode
    // word states the app's actual connection (a server, not `local`).
    expect(screen.getByText("v0.1.3 · server")).toBeTruthy();

    // No good handshake (down, and nothing read at all): the em dash, not a
    // stale version.
    rerender(<Sidebar serverStatus={downStatus()} />);
    expect(screen.getByText("—")).toBeTruthy();
    rerender(<Sidebar />);
    expect(screen.getByText("—")).toBeTruthy();
  });
});

describe("the title strip's no-wrap rule (#319)", () => {
  it("carries each strip text's full words as its title (#319)", () => {
    render(<WindowChrome serverStatus={upStatus("0.1.2")} paperRunning={3} />);

    const serverText = screen.getByText("server up · 0.1.2");
    expect(serverText.className).toContain("ts-text");
    expect(serverText.getAttribute("title")).toBe("server up · 0.1.2");

    const paperText = screen.getByText("3 paper · 0 live");
    expect(paperText.className).toContain("ts-text");
    expect(paperText.getAttribute("title")).toBe("3 paper · 0 live");
  });

  it("pins the nowrap and ellipsis rules in shared.css (#319)", () => {
    const sheet = readSharedCss();
    const stripRule = ruleBody(sheet, ".title-status");
    expect(stripRule).not.toBeNull();
    expect(stripRule).toContain("white-space: nowrap");

    const textRule = ruleBody(sheet, ".title-status .ts-text");
    expect(textRule).not.toBeNull();
    expect(textRule).toContain("overflow: hidden");
    expect(textRule).toContain("text-overflow: ellipsis");
    expect(textRule).toContain("white-space: nowrap");
  });
});
