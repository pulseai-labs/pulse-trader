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
