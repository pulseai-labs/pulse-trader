// Rendered tests for the Strategy Library screen (r1.s1.w3, spec step 7).
//
// The command module is mocked (`vi.mock("../bindings")`) so the screen's
// behaviour is asserted against a payload SHAPE, not a live IPC bridge — the
// payload fixtures mirror the generated `LibraryOverview` types exactly. The
// empty-payload test is the anti-fabrication backstop: with the command
// returning nothing, no row and no number may render (`r1.s1` SPINE.md
// "Fakes"). Reuses `src/test/setup.ts` (cleanup + matchMedia) — not
// re-registered here.

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../bindings", () => ({
  commands: {
    // "env" keeps the credential banner hidden so library content is the only
    // status-ish thing in the tree.
    credentialStatus: vi.fn().mockResolvedValue("env"),
    libraryOverview: vi.fn(),
  },
}));

import { commands } from "../bindings";
import type {
  BusError,
  LibraryOverview,
  LibraryVersion,
  VersionStats,
} from "../bindings";
import { App, RouteContent } from "../App";
import { resolveRoute } from "../routes";

const overviewMock = vi.mocked(commands.libraryOverview);

function stats(expectancy: string, winRate: string, trades: number): VersionStats {
  return { expectancy, winRate, trades };
}

function version(
  id: string,
  parentId: string | null,
  versionStats: VersionStats | null,
  delta: string | null = null,
  createdBy = "human",
  agentName: string | null = null,
  hypothesis: string | null = null,
  certified = false,
  latestWalkForwardRunId: string | null = null,
): LibraryVersion {
  // The version's one persisted run, as both the catalogue's row and the
  // version's latest run (a fixture with nothing but an ordinary backtest).
  const runRow =
    versionStats === null
      ? null
      : {
          id: "run-2222-3333",
          createdAt: "2026-08-21T08:30:00.000Z",
          expectancy: versionStats.expectancy,
          trades: versionStats.trades,
        };
  return {
    id,
    parentId,
    createdAt: "2026-08-20T10:00:00.000Z",
    createdBy,
    agentName,
    hypothesis,
    certified,
    latestWalkForwardRunId,
    dsl: {
      name: "RSI Oversold",
      direction: "long",
      entry: ["rsi(14) < 30"],
      filters: [],
      exits: ["stop 5%", "take profit 2R"],
      risk: ["risk 1% per trade", "max leverage 3x"],
    },
    stats: versionStats,
    deltaVsParent: delta,
    recentRuns: runRow === null ? [] : [runRow],
    latestRun: runRow,
  };
}

const SEEDED: LibraryOverview = {
  strategies: [
    {
      id: "strat-alpha",
      name: "Alpha Wave",
      createdAt: "2026-08-01T09:00:00.000Z",
      pinnedVersionId: "v-alpha-2",
      versions: [
        version("v-alpha-1", null, stats("+0.3R", "46.2%", 38)),
        version("v-alpha-2", "v-alpha-1", stats("+0.42R", "48.3%", 64), "+0.12R"),
        version("v-alpha-3", "v-alpha-2", null),
      ],
    },
    {
      id: "strat-beta",
      name: "Beta Break",
      createdAt: "2026-08-10T09:00:00.000Z",
      pinnedVersionId: null,
      versions: [version("v-beta-1", null, null)],
    },
  ],
};

beforeEach(() => {
  overviewMock.mockResolvedValue({ status: "ok", data: { strategies: [] } });
});

describe("LibraryScreen (empty payload — the anti-fabrication backstop)", () => {
  it("renders the empty state naming the next action, and no strategy rows", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: { strategies: [] } });
    const { container } = render(<App />);

    expect(await screen.findByText(/strategy designer/i)).toBeTruthy();
    expect(container.querySelector(".scard")).toBeNull();
    expect(container.querySelector(".vnode")).toBeNull();
  });
});

describe("LibraryScreen (seeded payload)", () => {
  it("renders every strategy with an honest count line", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);

    expect(await screen.findByText("Alpha Wave")).toBeTruthy();
    expect(screen.getByText("Beta Break")).toBeTruthy();
    expect(screen.getByText("2 strategies · 4 versions")).toBeTruthy();
  });

  it("shows the version tree with parent-child edges when a card is expanded", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));

    // Scoped to the tree: the sidebar carries a "v3" tier badge and Beta's
    // collapsed card shows its own "v1" ordinal, neither of which is a node.
    const tree = container.querySelector(".vtree-wrap");
    expect(tree).not.toBeNull();
    const inTree = within(tree as HTMLElement);

    expect(await inTree.findByText("v1")).toBeTruthy();
    expect(inTree.getByText("v2")).toBeTruthy();
    expect(inTree.getByText("v3")).toBeTruthy();
    // One bezier edge per parent-child pair (2 in Alpha's chain).
    expect(container.querySelectorAll(".vtree-svg path").length).toBe(2);
  });

  it("renders an em dash and no number for a version with no run (A1)", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap");
    expect(tree).not.toBeNull();
    const node = within(tree as HTMLElement).getByText("v3").closest(".vnode");
    expect(node).not.toBeNull();

    const kpis = (node as HTMLElement).querySelector(".vnode-kpis");
    expect(kpis).not.toBeNull();
    expect((kpis as HTMLElement).textContent).toContain("—");
    expect((kpis as HTMLElement).textContent).not.toMatch(/\d/);
  });

  it("fills the third track's details pane from the selected version, with no coaching block (A3)", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap");
    expect(tree).not.toBeNull();
    fireEvent.click(within(tree as HTMLElement).getByText("v2"));

    const pane = document.getElementById("details-pane");
    expect(pane).not.toBeNull();
    const inPane = within(pane as HTMLElement);

    expect(await inPane.findByText("Alpha Wave")).toBeTruthy();
    expect(inPane.getByText("v2")).toBeTruthy();
    expect(inPane.getByText("rsi(14) < 30")).toBeTruthy();
    expect(inPane.getByText("stop 5%")).toBeTruthy();
    // The KPI block and the recent-run row both carry the run's expectancy —
    // at least one occurrence, scoped to the pane.
    expect(inPane.getAllByText("+0.42R").length).toBeGreaterThan(0);
    expect(inPane.getByText("48.3%")).toBeTruthy();

    // A3: the coaching block is NOT rendered at all — not rendered-empty.
    expect(screen.queryByText(/recent coaching/i)).toBeNull();
  });
});

describe("LibraryScreen (VersionNode keyboard accessibility — PR finding 1)", () => {
  it("renders a version node as a real <button>, reachable by its own accessible name", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));

    const node = screen.getByRole("button", { name: "Version v2" });
    expect(node.tagName).toBe("BUTTON");
  });

  it("is keyboard-focusable — the bug this regresses: a <div role=\"button\"> with no tabIndex cannot receive focus, so a keyboard-only user could never reach it", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const node = screen.getByRole("button", { name: "Version v2" });

    node.focus();
    expect(document.activeElement).toBe(node);
  });

  it("activating a focused node fills the details pane", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const node = screen.getByRole("button", { name: "Version v2" });

    // jsdom does not synthesize a real browser's native "Enter/Space on a
    // focused <button> dispatches click" default action, and
    // @testing-library/user-event (which patches that in) is not a
    // dependency of this project. This drives the same two steps a keyboard
    // user's Enter/Space press produces in a real browser — focus, then the
    // click a native <button> fires for it — which is exactly the behaviour
    // the fix relies on the browser for, rather than a hand-rolled key
    // handler.
    node.focus();
    expect(document.activeElement).toBe(node);
    fireEvent.click(node);

    const pane = document.getElementById("details-pane");
    expect(pane).not.toBeNull();
    const inPane = within(pane as HTMLElement);
    expect(await inPane.findByText("v2")).toBeTruthy();
    expect(inPane.getByText("rsi(14) < 30")).toBeTruthy();
  });

  it("exposes selection via aria-pressed, not only the CSS class", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const node = screen.getByRole("button", { name: "Version v2" });

    expect(node.getAttribute("aria-pressed")).toBe("false");
    fireEvent.click(node);
    expect(node.getAttribute("aria-pressed")).toBe("true");
  });
});

describe("the library route entry (the real ROUTES table)", () => {
  it("mounts the screen through RouteContent", async () => {
    const route = resolveRoute("/library");
    expect(route).toBeDefined();

    render(<RouteContent route={route} />);
    expect(await screen.findByText("Strategy Library")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// r2.s1.w4 — C1 provenance + C2 refetch on focus
// ---------------------------------------------------------------------------

/** The seeded tree plus one `external_agent` child of v-alpha-2 — the shape
 * `pulse mcp` writes through the repositories. */
const WITH_AGENT: LibraryOverview = {
  strategies: [
    {
      id: "strat-alpha",
      name: "Alpha Wave",
      createdAt: "2026-08-01T09:00:00.000Z",
      pinnedVersionId: null,
      versions: [
        version("v-alpha-1", null, stats("+0.3R", "46.2%", 38)),
        version(
          "v-agent-1",
          "v-alpha-1",
          stats("+0.42R", "48.3%", 64),
          null,
          "external_agent",
          "claude-code",
          "A wider stop cuts noise exits.",
        ),
      ],
    },
  ],
};

describe("LibraryScreen (C1 — provenance and hypothesis, r2.s1.w4)", () => {
  it("renders `external_agent · <agent name>` and the hypothesis subline for an agent version, the bare label for a human one", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: WITH_AGENT });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap");
    expect(tree).not.toBeNull();
    const inTree = within(tree as HTMLElement);

    const agentNode = inTree.getByText("v2").closest(".vnode") as HTMLElement;
    expect(agentNode.querySelector(".vnode-provenance")?.textContent).toBe(
      "external_agent · claude-code",
    );
    expect(agentNode.querySelector(".vnode-hypothesis")?.textContent).toBe(
      "A wider stop cuts noise exits.",
    );

    const humanNode = inTree.getByText("v1").closest(".vnode") as HTMLElement;
    expect(humanNode.querySelector(".vnode-provenance")?.textContent).toBe("human");
    expect(humanNode.querySelector(".vnode-hypothesis")).toBeNull();
  });

  it("shows the hypothesis in full in the details pane", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: WITH_AGENT });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap");
    fireEvent.click(within(tree as HTMLElement).getByText("v2"));

    const pane = document.getElementById("details-pane");
    expect(pane).not.toBeNull();
    const inPane = within(pane as HTMLElement);
    expect(await inPane.findByText("Hypothesis")).toBeTruthy();
    expect(inPane.getByText("A wider stop cuts noise exits.")).toBeTruthy();
  });

  it("renders no hypothesis block for a human version", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: WITH_AGENT });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap");
    fireEvent.click(within(tree as HTMLElement).getByText("v1"));

    const pane = document.getElementById("details-pane");
    expect(pane).not.toBeNull();
    const inPane = within(pane as HTMLElement);
    await inPane.findByText("Alpha Wave");
    expect(inPane.queryByText("Hypothesis")).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// r2.s3.w4 — d22: the certification badge + the pane's certification line
// ---------------------------------------------------------------------------

/** The seeded tree with v-alpha-2 certified by a walk-forward run — the shape
 * the Library serves after the accept gate (or a direct walk-forward) lands a
 * passing run on the version. */
const CERTIFIED: LibraryOverview = {
  strategies: [
    {
      id: "strat-alpha",
      name: "Alpha Wave",
      createdAt: "2026-08-01T09:00:00.000Z",
      pinnedVersionId: null,
      versions: [
        version("v-alpha-1", null, stats("+0.3R", "46.2%", 38)),
        version(
          "v-alpha-2",
          "v-alpha-1",
          stats("+0.42R", "48.3%", 64),
          "+0.12R",
          "human",
          null,
          null,
          true,
          "wf-run-7f3a9c21",
        ),
        version("v-alpha-3", "v-alpha-2", null),
      ],
    },
  ],
};

describe("LibraryScreen (d22 — certification badge, r2.s3.w4)", () => {
  it("renders the certified badge on the certified node and nothing on the others", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap");
    expect(tree).not.toBeNull();
    const inTree = within(tree as HTMLElement);

    const certifiedNode = inTree.getByText("v2").closest(".vnode") as HTMLElement;
    expect(within(certifiedNode).getByText("certified")).toBeTruthy();

    for (const label of ["v1", "v3"]) {
      const node = inTree.getByText(label).closest(".vnode") as HTMLElement;
      expect(within(node).queryByText("certified")).toBeNull();
    }
  });

  it("the details pane reads 'certified' and names the walk-forward run", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    fireEvent.click(within(tree).getByText("v2"));

    const pane = document.getElementById("details-pane") as HTMLElement;
    const inPane = within(pane);
    expect(await inPane.findByText("Certification")).toBeTruthy();
    expect(inPane.getByText("certified")).toBeTruthy();
    expect(inPane.getByText("wf-run-7f3a9c21")).toBeTruthy();
  });

  it("the details pane reads 'uncertified' with no run id when the pointer is absent", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    fireEvent.click(within(tree).getByText("v1"));

    const pane = document.getElementById("details-pane") as HTMLElement;
    const inPane = within(pane);
    expect(await inPane.findByText("Certification")).toBeTruthy();
    expect(inPane.getByText("uncertified")).toBeTruthy();
    expect(inPane.queryByText("wf-run-7f3a9c21")).toBeNull();
  });
});

describe("LibraryScreen (C2 — refetch on focus, r2.s1.w4)", () => {
  it("refetches the overview when the window regains focus", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);
    await screen.findByText("Alpha Wave");
    const callsOnMount = overviewMock.mock.calls.length;

    fireEvent(window, new Event("focus"));

    await waitFor(() => {
      expect(overviewMock.mock.calls.length).toBeGreaterThan(callsOnMount);
    });
  });

  it("refetches the overview when the document becomes visible again", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);
    await screen.findByText("Alpha Wave");
    const callsOnMount = overviewMock.mock.calls.length;

    Object.defineProperty(document, "visibilityState", {
      value: "visible",
      configurable: true,
    });
    fireEvent(document, new Event("visibilitychange"));

    await waitFor(() => {
      expect(overviewMock.mock.calls.length).toBeGreaterThan(callsOnMount);
    });
  });
});

// ---------------------------------------------------------------------------
// F5 (r2.s1 review) — the two stale-state defects in `load`
// ---------------------------------------------------------------------------

function busError(message: string): { status: "error"; error: BusError } {
  return {
    status: "error",
    error: { code: "data", message, run_id: null, session_id: null, child_run_id: null },
  };
}

describe("LibraryScreen (F5 — a recovered read clears the error)", () => {
  it("a failed refetch no longer hides the library once the backend recovers", async () => {
    // The mount read succeeds; a later focus refetch fails, then the next
    // focus refetch succeeds — the screen must render the recovered payload,
    // not stay hidden behind the dead read's error line.
    overviewMock
      .mockResolvedValueOnce({ status: "ok", data: SEEDED })
      .mockResolvedValueOnce(busError("the store read failed"))
      .mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);
    await screen.findByText("Alpha Wave");

    fireEvent(window, new Event("focus"));
    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.queryByText("Alpha Wave")).toBeNull();

    // `useRefetchOnFocus` throttles both signals to one call per second —
    // jump the clock past the window so this second focus actually refetches.
    const nowSpy = vi.spyOn(Date, "now").mockReturnValue(Number.MAX_SAFE_INTEGER);
    fireEvent(window, new Event("focus"));
    nowSpy.mockRestore();

    expect(await screen.findByText("Alpha Wave")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("a mount-time failure is cleared by the first successful refetch", async () => {
    overviewMock
      .mockResolvedValueOnce(busError("the store read failed"))
      .mockResolvedValue({ status: "ok", data: SEEDED });
    render(<App />);
    expect(await screen.findByRole("alert")).toBeTruthy();

    fireEvent(window, new Event("focus"));
    expect(await screen.findByText("Alpha Wave")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

describe("LibraryScreen (F5 — the details pane re-renders from the fresh payload)", () => {
  it("a focus refetch swaps the selected version's object — new stats, runs and hypothesis, never the stale row", async () => {
    // The same tree, but v-alpha-2's payload moved on: fresh stats (so the
    // pane's KPI + recent-run row change), an external_agent provenance and a
    // hypothesis the stale object did not carry.
    const fresh: LibraryOverview = {
      strategies: [
        {
          ...SEEDED.strategies[0],
          versions: [
            SEEDED.strategies[0].versions[0],
            version(
              "v-alpha-2",
              "v-alpha-1",
              stats("+0.99R", "55.0%", 70),
              "+0.69R",
              "external_agent",
              "claude-code",
              "A wider stop cuts noise exits.",
            ),
            SEEDED.strategies[0].versions[2],
          ],
        },
        SEEDED.strategies[1],
      ],
    };
    overviewMock.mockResolvedValue({ status: "ok", data: SEEDED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    fireEvent.click(within(tree).getByText("v2"));

    const pane = document.getElementById("details-pane") as HTMLElement;
    const inPane = within(pane);
    // The STALE object is what the pane shows first — KPI and run row both.
    expect((await inPane.findAllByText("+0.42R")).length).toBeGreaterThan(0);
    expect(inPane.queryByText("Hypothesis")).toBeNull();

    overviewMock.mockResolvedValue({ status: "ok", data: fresh });
    fireEvent(window, new Event("focus"));

    // The selection survives — by ID — and the pane renders the fresher
    // object: new expectancy, new win rate, and the hypothesis block the
    // stale object lacked.
    await waitFor(() => {
      expect(inPane.getAllByText("+0.99R").length).toBeGreaterThan(0);
    });
    expect(inPane.getByText("55.0%")).toBeTruthy();
    expect(await inPane.findByText("A wider stop cuts noise exits.")).toBeTruthy();
    expect(inPane.queryByText("+0.42R")).toBeNull();
    expect(inPane.getByText("Alpha Wave")).toBeTruthy();
    expect(inPane.getByText("v2")).toBeTruthy();
  });
});
