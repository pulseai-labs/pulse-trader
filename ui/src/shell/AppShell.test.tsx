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

import { fireEvent, render, screen, within } from "@testing-library/react";
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
import type { ServerStatus, ServerStatusState } from "../bindings";
import { RECONNECTING_NOTE } from "./ShellStatus";

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

// ---------------------------------------------------------------------------
// r7.s2.w4 — the shell aligned to canvas v9 (PDR-0049 decision 7)
// ---------------------------------------------------------------------------

function v9Status(state: ServerStatusState, role: string | null = null): ServerStatus {
  return {
    state,
    binary_version: state === "up" ? "0.1.18" : null,
    engine_fingerprint: state === "up" ? "f0dbdf5748d38d5c" : null,
    reason: null,
    role,
  };
}

const ADDRESS = "http://draco-desk:17620";
const LONG_ADDRESS = "http://pulse-trader-production-server.tailnet-example.ts.net:17620";
const LONG_NAME = "pulse-trader-production-server.tailnet-example.ts.net";

describe("the shell aligned to canvas v9 (r7.s2.w4)", () => {
  beforeEach(() => {
    localStorage.removeItem("pt-sidebar");
  });

  afterEach(() => {
    localStorage.removeItem("pt-sidebar");
  });

  it("renders the status strip as the titlebar's own child, not inside the title block (#503)", () => {
    const { container, rerender } = render(
      <WindowChrome docTitle="Chart" serverStatus={v9Status("up")} />,
    );
    const titlebar = container.querySelector(".titlebar");
    expect(titlebar).not.toBeNull();
    expect(Array.from(titlebar?.children ?? []).map((c) => c.classList[0])).toEqual([
      "traffic",
      "title-block",
      "title-status",
      "title-right",
    ]);
    expect(container.querySelector(".title-block .title-status")).toBeNull();

    rerender(<WindowChrome docTitle="Chart" serverStatus={v9Status("up")} hasSidebar />);
    expect(
      Array.from(container.querySelector(".titlebar")?.children ?? []).map((c) => c.classList[0]),
    ).toEqual(["traffic", "icon-btn", "title-block", "title-status", "title-right"]);
  });

  it("shows the server pill with the server name and its connection state (#503)", () => {
    const { container, rerender } = render(
      <WindowChrome serverStatus={v9Status("up")} serverAddress={ADDRESS} />,
    );
    const pill = screen.getByRole("button", { name: "draco-desk connected" });
    expect(pill.classList.contains("conn-pill")).toBe(true);
    expect(pill.classList.contains("is-connected")).toBe(true);
    expect(pill.getAttribute("title")).toBe("Server connection");
    expect(pill.getAttribute("aria-expanded")).toBe("false");
    expect(pill.querySelector(".dot")).not.toBeNull();
    expect(pill.querySelector(".cp-name")?.textContent).toBe("draco-desk");
    expect(pill.querySelector(".cp-state")?.textContent).toBe("connected");
    expect(screen.queryByText(/server up/)).toBeNull();
    expect(container.querySelector(".ts-text")).toBeNull();

    rerender(<WindowChrome serverStatus={v9Status("not_connected")} />);
    const offline = screen.getByRole("button", { name: "server disconnected" });
    expect(offline.classList.contains("is-disconnected")).toBe(true);
  });

  it("shows a spinner and reconnecting… while the server is not answering", () => {
    const { container } = render(
      <WindowChrome serverStatus={v9Status("down")} serverAddress={ADDRESS} />,
    );
    const pill = screen.getByRole("button", { name: "draco-desk reconnecting…" });
    expect(pill.classList.contains("is-reconnecting")).toBe(true);
    expect(pill.querySelector(".spinner-sm")).not.toBeNull();
    fireEvent.click(pill);
    expect(container.querySelector(".cpop-note")?.textContent).toBe(RECONNECTING_NOTE);
  });

  it("shows the paper pill with the running count, and an em dash while the count is unknown", () => {
    const { container, rerender } = render(
      <WindowChrome serverStatus={v9Status("up")} paperRunning={2} />,
    );
    const paper = container.querySelector(".ts-paper");
    expect(paper?.textContent).toBe("2 paper · 0 live");
    expect(paper?.getAttribute("title")).toBe(
      "Paper sessions running on the server. Live trading is not available yet.",
    );
    rerender(<WindowChrome serverStatus={v9Status("up")} />);
    expect(container.querySelector(".ts-paper")?.textContent).toBe("— paper · 0 live");
  });

  it("shows the LLM cost MTD pill as not built yet, never a number", () => {
    const { container } = render(<WindowChrome serverStatus={v9Status("up")} />);
    expect(container.querySelector(".ts-cost")?.textContent).toBe(
      "LLM cost this month: not built yet MTD",
    );
    expect(container.querySelector(".ts-cost .not-built .sr-only")?.textContent).toBe(
      "LLM cost this month: ",
    );
    expect(container.querySelector(".ts-cost .not-built")?.getAttribute("title")).toBe(
      "LLM cost this month — not built yet",
    );
    expect(container.querySelector(".titlebar")?.textContent).not.toContain("$");
  });

  it("opens the connection popover with the address, server version and engine, the token label not built yet", () => {
    const { container } = render(
      <WindowChrome serverStatus={v9Status("up")} serverAddress={ADDRESS} />,
    );
    const pill = screen.getByRole("button", { name: "draco-desk connected" });
    fireEvent.click(pill);
    expect(pill.getAttribute("aria-expanded")).toBe("true");
    const pop = container.querySelector(".conn-pop") as HTMLElement;
    expect(pop).not.toBeNull();
    expect(pop.getAttribute("data-tauri-drag-region")).toBe("false");
    expect(Array.from(pop.querySelectorAll("dt")).map((n) => n.textContent)).toEqual([
      "address",
      "server",
      "engine",
      "token",
    ]);
    expect(Array.from(pop.querySelectorAll("dd")).map((n) => n.textContent)).toEqual([
      "draco-desk:17620",
      "pulse 0.1.18",
      "f0dbdf5",
      "The token label: not built yet",
    ]);
    expect(pop.parentElement?.classList.contains("title-status")).toBe(true);
    expect(pop.closest(".ts-pills")).toBeNull();
    expect(pop.querySelector(".cpop-note")).toBeNull();
    const link = within(pop).getByRole("link", { name: "Connection settings →" });
    expect(link.getAttribute("href")).toBe("#/settings");

    fireEvent.click(pill);
    expect(container.querySelector(".conn-pop")).toBeNull();
  });

  it("hides and shows the sidebar from the titlebar toggle and ⌘\\, remembered across mounts", () => {
    const first = render(<WindowChrome hasSidebar serverStatus={v9Status("up")} />);
    const hide = screen.getByRole("button", { name: "Hide sidebar" });
    expect(hide.getAttribute("aria-pressed")).toBe("false");
    expect(hide.getAttribute("title")).toBe("Hide sidebar  ⌘\\");

    fireEvent.click(hide);
    expect(first.container.querySelector(".window")?.classList.contains("sb-off")).toBe(true);
    const show = screen.getByRole("button", { name: "Show sidebar" });
    expect(show.getAttribute("aria-pressed")).toBe("true");
    expect(show.classList.contains("is-off")).toBe(true);
    expect(localStorage.getItem("pt-sidebar")).toBe("off");

    first.unmount();
    const second = render(<WindowChrome hasSidebar serverStatus={v9Status("up")} />);
    expect(second.container.querySelector(".window")?.classList.contains("sb-off")).toBe(true);

    fireEvent.keyDown(window, { key: "\\", metaKey: true });
    expect(second.container.querySelector(".window")?.classList.contains("sb-off")).toBe(false);
    expect(localStorage.getItem("pt-sidebar")).toBe("on");
  });

  it("renders no sidebar toggle on a chrome without a sidebar", () => {
    localStorage.setItem("pt-sidebar", "off");
    const { container } = render(<WindowChrome />);
    expect(screen.queryByRole("button", { name: /sidebar/i })).toBeNull();
    expect(container.querySelector(".window")?.classList.contains("sb-off")).toBe(false);
    expect(container.querySelector(".title-status")).toBeNull();
  });

  it("shows the Library strategy count and the Deployment paper count, and no count on any other row", () => {
    const { container, rerender } = render(
      <Sidebar serverStatus={v9Status("up")} strategyCount={5} paperRunning={2} />,
    );
    const library = () => screen.getByRole("link", { name: /strategy library/i });
    const deploy = () => screen.getByRole("link", { name: /deployment dashboard/i });
    expect(library().querySelector(".nav-count")?.textContent).toBe("5");
    expect(deploy().querySelector(".nav-count")?.textContent).toBe("2");
    expect(container.querySelectorAll(".nav-count")).toHaveLength(2);

    rerender(<Sidebar serverStatus={v9Status("up")} strategyCount={0} />);
    expect(library().querySelector(".nav-count")?.textContent).toBe("0");
    expect(container.querySelectorAll(".nav-count")).toHaveLength(1);

    rerender(<Sidebar serverStatus={v9Status("up")} />);
    expect(container.querySelectorAll(".nav-count")).toHaveLength(0);
  });

  it("renders the status card: the server row from the handshake, the agent and LLM cost rows not built yet", () => {
    function rows(root: HTMLElement): string[][] {
      return Array.from(root.querySelectorAll(".status-card .status-row")).map((row) => [
        row.querySelector(".status-key")?.textContent ?? "",
        row.querySelector(".status-val")?.textContent ?? "",
      ]);
    }
    const notBuilt = [
      ["Agent", "Agent last seen: not built yet"],
      ["LLM cost · MTD", "LLM cost this month: not built yet"],
    ];
    const { container, rerender } = render(
      <Sidebar serverStatus={v9Status("up")} serverAddress={ADDRESS} />,
    );
    expect(rows(container)).toEqual([["draco-desk", "f0dbdf5"], ...notBuilt]);

    rerender(<Sidebar serverStatus={v9Status("down")} serverAddress={ADDRESS} />);
    expect(rows(container)).toEqual([["draco-desk", "retrying"], ...notBuilt]);
    expect(container.querySelector(".status-card .status-row .spinner-sm")).not.toBeNull();

    rerender(<Sidebar serverStatus={v9Status("not_connected")} serverAddress={ADDRESS} />);
    expect(rows(container)).toEqual([["draco-desk", "offline"], ...notBuilt]);
  });

  it("keeps a long server name whole in the DOM and in the popover (#503, CR4)", () => {
    const { container } = render(
      <WindowChrome serverStatus={v9Status("up")} serverAddress={LONG_ADDRESS} />,
    );
    expect(container.querySelector(".cp-name")?.textContent).toBe(LONG_NAME);
    const pill = screen.getByRole("button", { name: `${LONG_NAME} connected` });
    fireEvent.click(pill);
    const pop = container.querySelector(".conn-pop") as HTMLElement;
    expect(pop.querySelector("dd")?.textContent).toBe(`${LONG_NAME}:17620`);
  });

  it("keeps every strip pill on one line, bounds the name, clips only the pill row, and sizes the sidebar track from --sb-w (#319, #503)", () => {
    const sheet = readSharedCss();
    const strip = ruleBody(sheet, ".title-status");
    expect(strip).toContain("white-space: nowrap");
    expect(strip).toContain("position: relative");
    expect(strip).not.toContain("overflow");

    const pills = ruleBody(sheet, ".ts-pills");
    expect(pills).toContain("overflow: hidden");
    expect(pills).toContain("min-width: 0");

    const name = ruleBody(sheet, ".conn-pill .cp-name");
    expect(name).toContain("max-width: 220px");
    expect(name).toContain("text-overflow: ellipsis");

    const tsPill = ruleBody(sheet, ".ts-pill");
    expect(tsPill).toContain("white-space: nowrap");
    expect(tsPill).toContain("flex-shrink: 0");

    const connPill = ruleBody(sheet, ".conn-pill");
    expect(connPill).toContain("white-space: nowrap");
    expect(connPill).toContain("flex-shrink: 0");
    expect(connPill).not.toContain("text-overflow");

    expect(ruleBody(sheet, ".conn-pop")).toContain("white-space: normal");
    expect(ruleBody(sheet, ".layout")).toContain("grid-template-columns: var(--sb-w) 1fr 360px");
    expect(ruleBody(sheet, ".window.sb-off")).toContain("--sb-w: 0px");
    expect(ruleBody(sheet, ".not-built")).not.toBeNull();
    expect(sheet).not.toContain(".ts-text");
  });
});
