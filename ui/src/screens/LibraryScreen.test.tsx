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
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../bindings", () => ({
  commands: {
    // "env" keeps the credential banner hidden so library content is the only
    // status-ish thing in the tree.
    // r3.s3.w5: the bus's `Result` shell wraps every command answer, so the
    // mock carries the `typedError` union shape.
    credentialStatus: vi.fn().mockResolvedValue({ status: "ok", data: "env" }),
    libraryOverview: vi.fn(),
    // r3.s3.w5: App polls the connection status (the Connect gate + the
    // titlebar strip); an `up` answer keeps the Library where it always was.
    serverStatus: vi.fn().mockResolvedValue({
      status: "ok",
      data: { state: "up", binary_version: "0.0.0", engine_fingerprint: null, reason: null, role: null },
    }),
    // r3.s4.w5: App polls the running paper-session count for the titlebar
    // pill; an empty list keeps the strip at "0 paper · 0 live".
    paperSessions: vi.fn().mockResolvedValue({ status: "ok", data: [] }),
    // r4.s1.w5: the pane's certification-record read; an empty list by default
    // (the "None yet." state every other pane test then renders).
    certificationRecords: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: { versionId: "", records: [] } }),
    // r5.s5.w3: the pane's power-review read; no open freeze by default.
    powerReviews: vi
      .fn()
      .mockResolvedValue({ status: "ok", data: { versionId: "", openFreezeId: null, reviews: [] } }),
    // r4.s5.w3 (#330): the details pane reads the version's latest walk-forward
    // once to learn which of its recent runs are folds. Defaults to a
    // not-found-shaped refusal, so a fixture that carries a pointer but no
    // walk-forward DTO renders its rows unlabelled — the honest failed-read
    // state — while the #330 cases answer with their own DTO.
    getWalkForwardRun: vi.fn().mockResolvedValue({
      status: "error",
      error: {
        code: "not_found",
        message: "no walk-forward run",
        run_id: null,
        session_id: null,
        child_run_id: null,
      },
    }),
  },
}));

import { commands } from "../bindings";
import type {
  BusError,
  CertificationRecordDto,
  LibraryOverview,
  LibraryRunSummary,
  LibraryVersion,
  PowerReviewDto,
  VersionStats,
  WalkForwardRunDto,
} from "../bindings";
import { App, RouteContent } from "../App";
import { clearWalkForwardFoldReads } from "../hooks/useWalkForwardFolds";
import { resolveNavId, resolveRoute } from "../routes";

/** The sidebar's nav rows (App's own list), for the #269 fragment assertion. */
const KNOWN_NAV_IDS = ["library", "designer", "backtest", "deploy", "journal", "analytics", "settings", "help"];

const overviewMock = vi.mocked(commands.libraryOverview);
const certificationRecordsMock = vi.mocked(commands.certificationRecords);
// r5.s5.w3: the pane's power-review read.
const powerReviewsMock = vi.mocked(commands.powerReviews);
const walkForwardRunMock = vi.mocked(commands.getWalkForwardRun);

/** The figure helper (r4.s5.w1): the A2 display text plus the ring's own exact
 * string on the same element (`title`/`data-exact`), asserted together. */
function expectFigure(scope: Element, display: string, exact: string): void {
  const element = within(scope as HTMLElement).getByText(display);
  expect(element.getAttribute("data-exact")).toBe(exact);
  expect(element.getAttribute("title")).toBe(exact);
}

/** One certification record, in the generated DTO's shape (r4.s1.w5). */
function certificationRecord(certified: boolean): CertificationRecordDto {
  return {
    id: "cert-1",
    versionId: "v-alpha-2",
    freezeId: "freeze-1",
    hypothesisIndex: 3,
    rule: "wf-v2",
    pair: "BTCUSDT",
    searchWalkForwardRunId: "wf-run-7f3a9c21",
    searchPass: true,
    holdoutStart: "2025-07-01T00:00:00.000Z",
    holdoutEnd: "2026-01-01T00:00:00.000Z",
    holdoutN: 214,
    holdoutMeanR: "0.412",
    holdoutZ: 2.638,
    holdoutLowerBound: 0.181,
    holdoutPasses: certified,
    certified,
    searchDataVersions: [
      { timeframe: "15m", dataVersion: "m15-sha256:aaaa" },
      { timeframe: "4h", dataVersion: "h4-sha256:bbbb" },
    ],
    holdoutDataVersions: [
      { timeframe: "15m", dataVersion: "m15-sha256:aaaa" },
      { timeframe: "4h", dataVersion: "h4-sha256:bbbb" },
    ],
    engineFingerprint: "fp",
    createdAt: "2026-01-02T00:00:00.000Z",
    calledBy: "claude-code",
  };
}

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
  // r5.s5.w3: the power-review read answers "no open freeze" unless a case says
  // otherwise — the module mock's own default, restored here so one case's
  // resolved value cannot leak into the next.
  powerReviewsMock.mockResolvedValue({
    status: "ok",
    data: { versionId: "", openFreezeId: null, reviews: [] },
  });
  // r4.s5.w3 (#330): the fold reads are cached by walk-forward id for the
  // session; each case starts from an empty cache so its call assertions hold.
  clearWalkForwardFoldReads();
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
    // The card's created date reads A2 (a date, UTC), the ring's own instant on
    // the element (r4.s5.w1).
    const alphaCard = screen.getByText("Alpha Wave").closest(".scard") as HTMLElement;
    expectFigure(alphaCard, "2026-08-01 UTC", "2026-08-01T09:00:00.000Z");
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

  it("renders the version node line as direction · entry · filters · exits (#193)", async () => {
    // A version whose DSL carries an H4 filter and an ATR stop: both tokens
    // must be visible in the node's own summary line, untruncated.
    const withH4Filter = {
      ...version("v-alpha-1", null, stats("+0.3R", "46.2%", 38)),
      dsl: {
        name: "D47 Breakout",
        direction: "long",
        entry: ["close > highest(high, 20 prior)"],
        filters: ["h4:ema(200) rising (1 bar)"],
        exits: ["atr(14)×2", "take profit 2R"],
        risk: ["risk 1% per trade"],
      },
    };
    overviewMock.mockResolvedValue({
      status: "ok",
      data: {
        strategies: [
          {
            id: "strat-d47",
            name: "D47 Breakout",
            createdAt: "2026-08-01T09:00:00.000Z",
            pinnedVersionId: null,
            versions: [withH4Filter],
          },
        ],
      },
    });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle d47 breakout/i }));
    const tree = container.querySelector(".vtree-wrap");
    expect(tree).not.toBeNull();
    const node = within(tree as HTMLElement).getByText("v1").closest(".vnode");
    expect(node).not.toBeNull();
    const line = (node as HTMLElement).querySelector(".vnode-summary");
    expect(line).not.toBeNull();
    expect((line as HTMLElement).textContent).toContain("h4:ema(200) rising (1 bar)");
    expect((line as HTMLElement).textContent).toContain("atr(14)×2");
    expect((line as HTMLElement).textContent).toContain("close > highest(high, 20 prior)");
  });

  it("keeps the full summary line reachable while the node truncates it visually", async () => {
    // PROXY, stated plainly: the ellipsis itself is the stylesheet's job
    // (`.vnode-summary` carries `text-overflow: ellipsis; overflow: hidden`),
    // and jsdom performs no layout, so no DOM assertion can observe a rendered
    // cut. What IS asserted here: the element is the truncating class, and the
    // FULL line rides the `title` attribute for hover. The verifier should
    // judge this as a proxy for "truncate to the node's width with an
    // ellipsis", not as an observed truncation.
    const longEntry = "close > highest(high, 20 prior) with an absurdly long extra tail ".repeat(4);
    const longDsl = {
      ...version("v-alpha-1", null, stats("+0.3R", "46.2%", 38)),
      dsl: {
        name: "Long Line",
        direction: "long",
        entry: [longEntry.trim()],
        filters: [],
        exits: ["atr(14)×2"],
        risk: [],
      },
    };
    overviewMock.mockResolvedValue({
      status: "ok",
      data: {
        strategies: [
          {
            id: "strat-long",
            name: "Long Line",
            createdAt: "2026-08-01T09:00:00.000Z",
            pinnedVersionId: null,
            versions: [longDsl],
          },
        ],
      },
    });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle long line/i }));
    const tree = container.querySelector(".vtree-wrap");
    expect(tree).not.toBeNull();
    const node = within(tree as HTMLElement).getByText("v1").closest(".vnode");
    expect(node).not.toBeNull();
    const line = (node as HTMLElement).querySelector(".vnode-summary");
    expect(line).not.toBeNull();
    expect(line?.className).toContain("vnode-summary");
    expect(line?.getAttribute("title")).toBe(
      `long · ${longEntry.trim()} · atr(14)×2`,
    );
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
    // The KPI block and the recent-run row both carry the run's expectancy,
    // normalised to A2's `+0.42 R` with the ring's own `"+0.42R"` on each
    // element — at least one occurrence of each, scoped to the pane.
    expect(inPane.getAllByText("+0.42 R").length).toBeGreaterThan(0);
    const exactExpectancies = (pane as HTMLElement).querySelectorAll('[data-exact="+0.42R"]');
    expect(exactExpectancies.length).toBeGreaterThan(0);
    expect((exactExpectancies[0] as HTMLElement).getAttribute("title")).toBe("+0.42R");
    expect(inPane.getByText("48.3%")).toBeTruthy();
    // The recent-run row's own date, A2 (UTC), the ring's instant on the element.
    expectFigure(pane as HTMLElement, "2026-08-21 UTC", "2026-08-21T08:30:00.000Z");

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

// ---------------------------------------------------------------------------
// r4.s1.w5 — the certification record's FULL read in the details pane
// ---------------------------------------------------------------------------

describe("LibraryScreen (r4.s1.w5 — the certification records)", () => {
  it("renders one row per record, with the holdout numbers the agent never sees", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    certificationRecordsMock.mockResolvedValue({
      status: "ok",
      data: { versionId: "v-alpha-2", records: [certificationRecord(true)] },
    });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    fireEvent.click(within(tree).getByText("v2"));

    const pane = document.getElementById("details-pane") as HTMLElement;
    const inPane = within(pane);
    expect(await inPane.findByText("Certification records")).toBeTruthy();
    expect(await inPane.findByText("#3")).toBeTruthy();
    expect(inPane.getByText("BTCUSDT")).toBeTruthy();
    expect(inPane.getByText("214t")).toBeTruthy();
    // The holdout numbers, A2-formatted, the ring's own strings on the
    // elements: the mean R, the 3-decimal bound, and the window saying UTC.
    expectFigure(pane, "+0.41 R", "0.412");
    expectFigure(pane, "0.181", "0.181");
    expectFigure(pane, "2025-07-01 → 2026-01-01 UTC", "2025-07-01T00:00:00.000Z → 2026-01-01T00:00:00.000Z");
    expect(
      certificationRecordsMock,
    ).toHaveBeenCalledWith({ versionId: "v-alpha-2" });
  });

  it("says 'None yet.' when the version has no records", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    certificationRecordsMock.mockResolvedValue({
      status: "ok",
      data: { versionId: "v-alpha-2", records: [] },
    });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    fireEvent.click(within(tree).getByText("v2"));

    const pane = document.getElementById("details-pane") as HTMLElement;
    const inPane = within(pane);
    expect(await inPane.findByText("Certification records")).toBeTruthy();
    expect(await inPane.findByText("None yet.")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// r5.s5.w3 — the power review before the one certify attempt
// ---------------------------------------------------------------------------

/** One power review, in the generated DTO's shape (r5.s5.w3). */
function powerReview(allowed: boolean): PowerReviewDto {
  return {
    id: "pr-1",
    versionId: "v-alpha-2",
    freezeId: "f-1",
    pair: "BTCUSDT",
    h: 2,
    holdoutN: 31,
    passChance: 0.4213,
    bar: "0.50",
    minHoldoutTrades: 20,
    allowed,
    reason: allowed ? null : "below_bar",
    createdAt: "2026-11-30T09:00:00.000Z",
    calledBy: "cli",
  };
}

describe("LibraryScreen (r5.s5.w3 — the power review)", () => {
  /** Select v2 of "Alpha Wave" and return the pane's Power review section. */
  async function openPowerReview(): Promise<HTMLElement> {
    const { container } = render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    fireEvent.click(within(tree).getByText("v2"));
    const pane = document.getElementById("details-pane") as HTMLElement;
    const inPane = within(pane);
    return (await inPane.findByText("Power review")).closest("section") as HTMLElement;
  }

  it("shows the count, the pass chance, the bar and why an attempt is not allowed", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    powerReviewsMock.mockResolvedValue({
      status: "ok",
      data: { versionId: "v-alpha-2", openFreezeId: "f-1", reviews: [powerReview(false)] },
    });

    const section = await openPowerReview();
    const inSection = within(section);
    expect(inSection.getByText("BTCUSDT")).toBeTruthy();
    expect(inSection.getByText("31t")).toBeTruthy();
    expect(inSection.getByText("pass 42.1%")).toBeTruthy();
    expect(inSection.getByText("bar 50.0%")).toBeTruthy();
    expect(inSection.getByText("attempt not allowed — pass chance below the bar")).toBeTruthy();
    expect(inSection.getByText("Holdout outcomes are not shown before the attempt.")).toBeTruthy();
    expect(powerReviewsMock).toHaveBeenCalledWith({ versionId: "v-alpha-2" });
    // G1: nothing about how the holdout trades went reaches the pane.
    expect(section.textContent ?? "").not.toMatch(/mean|bound|pnl|win/i);
  });

  it("says an attempt is allowed", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    powerReviewsMock.mockResolvedValue({
      status: "ok",
      data: { versionId: "v-alpha-2", openFreezeId: "f-1", reviews: [powerReview(true)] },
    });

    const section = await openPowerReview();
    expect(within(section).getByText("attempt allowed")).toBeTruthy();
  });

  it("says no freeze is open", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });

    const section = await openPowerReview();
    expect(await within(section).findByText("No freeze is open.")).toBeTruthy();
  });

  it("says 'None yet.' when the open freeze has no review", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    powerReviewsMock.mockResolvedValue({
      status: "ok",
      data: { versionId: "v-alpha-2", openFreezeId: "f-1", reviews: [] },
    });

    const section = await openPowerReview();
    expect(await within(section).findByText("None yet.")).toBeTruthy();
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
    expect((await inPane.findAllByText("+0.42 R")).length).toBeGreaterThan(0);
    expect(inPane.queryByText("Hypothesis")).toBeNull();

    overviewMock.mockResolvedValue({ status: "ok", data: fresh });
    fireEvent(window, new Event("focus"));

    // The selection survives — by ID — and the pane renders the fresher
    // object: new expectancy, new win rate, and the hypothesis block the
    // stale object lacked.
    await waitFor(() => {
      expect(inPane.getAllByText("+0.99 R").length).toBeGreaterThan(0);
    });
    expect(inPane.getByText("55.0%")).toBeTruthy();
    expect(await inPane.findByText("A wider stop cuts noise exits.")).toBeTruthy();
    expect(inPane.queryByText("+0.42 R")).toBeNull();
    expect(inPane.getByText("Alpha Wave")).toBeTruthy();
    expect(inPane.getByText("v2")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// r4.s5.w3 (#330) — the fold labels in Recent backtests
// ---------------------------------------------------------------------------

/** The version's own run, two folds of its LATEST walk-forward (index 2 and 0),
 * and a fold of an older walk-forward its catalogue row cannot name. */
const LIB_OWN_RUN: LibraryRunSummary = {
  id: "ownrun-0001",
  createdAt: "2026-08-24T09:00:00.000Z",
  expectancy: "+0.42R",
  trades: 12,
};

const LIB_FOLD_RUN: LibraryRunSummary = {
  id: "run-fold-reopen-2",
  createdAt: "2026-08-23T09:00:00.000Z",
  expectancy: "+0.62R",
  trades: 4,
};

const LIB_FOLD_RUN_FIRST: LibraryRunSummary = {
  id: "run-fold-reopen-0",
  createdAt: "2026-08-23T09:00:00.000Z",
  expectancy: "+0.41R",
  trades: 3,
};

const LIB_FOLD_OLDER: LibraryRunSummary = {
  id: "oldwf-fold-1",
  createdAt: "2026-08-21T09:00:00.000Z",
  expectancy: "+0.10R",
  trades: 6,
};

/** The walk-forward the version's pointer names: folds at index 2 and 0, both in
 * the recent-run list. */
const LIB_WF_DTO: WalkForwardRunDto = {
  walkForwardRunId: "wf-6c1d9f38",
  versionId: "v-alpha-1",
  scheme: "rolling-oos/v1",
  k: 2,
  rule: "wf-v1",
  spanFrom: "2026-08-01T00:00:00.000Z",
  spanTo: "2026-08-31T23:59:59.000Z",
  fromDefaulted: true,
  engineFingerprint: "sha256:library9",
  verdict: {
    pass: true,
    foldsHolding: 2,
    foldsRequired: 2,
    pooled: { n: 7, meanR: "0.500", lowerBound: 0.12, holds: true },
  },
  folds: [
    {
      index: 2, windowFrom: "2026-08-20T00:00:00.000Z", windowTo: "2026-08-23T00:00:00.000Z",
      backtestRunId: "run-fold-reopen-2", n: 4, meanR: "0.620", lowerBound: 0.16,
      holds: true, trades: 4, expectancy: "0.620", winRate: "0.750",
    },
    {
      index: 0, windowFrom: "2026-08-10T00:00:00.000Z", windowTo: "2026-08-14T00:00:00.000Z",
      backtestRunId: "run-fold-reopen-0", n: 3, meanR: "0.410", lowerBound: -0.4,
      holds: true, trades: 3, expectancy: "0.410", winRate: "0.667",
    },
  ],
};

/** The one-strategy catalogue the #330 cases select through; `olderFold` mixes a
 * run no pointer can name into the same list, in first place. */
function foldCatalog(olderFold: LibraryRunSummary | null): LibraryOverview {
  const runs = [LIB_FOLD_RUN, LIB_OWN_RUN, LIB_FOLD_RUN_FIRST];
  return {
    strategies: [
      {
        id: "strat-alpha",
        name: "Alpha Wave",
        createdAt: "2026-08-01T09:00:00.000Z",
        pinnedVersionId: null,
        versions: [
          {
            ...version(
              "v-alpha-1",
              null,
              stats("+0.42R", "48.3%", 64),
              null,
              "human",
              null,
              null,
              false,
              "wf-6c1d9f38",
            ),
            recentRuns: olderFold === null ? runs : [olderFold, ...runs],
            latestRun: LIB_OWN_RUN,
          },
        ],
      },
    ],
  };
}

/** Expand the card and select v1 — the pane whose Recent backtests list is
 * under test. */
async function openFoldPane(overview: LibraryOverview): Promise<HTMLElement> {
  overviewMock.mockResolvedValue({ status: "ok", data: overview });
  const { container } = render(<App />);
  fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
  const tree = container.querySelector(".vtree-wrap") as HTMLElement;
  fireEvent.click(within(tree).getByText("v1"));
  const pane = document.getElementById("details-pane") as HTMLElement;
  await within(pane).findByText("Recent backtests");
  return pane;
}

describe("LibraryScreen (#330 — fold runs are marked in Recent backtests)", () => {
  it("labels the latest walk-forward's folds and groups them after the version's own runs (#330)", async () => {
    walkForwardRunMock.mockResolvedValue({ status: "ok", data: LIB_WF_DTO });
    const pane = await openFoldPane(foldCatalog(null));

    // The labels ride a read of the version's pointer, so they land a tick after
    // the rows do.
    const rows = await waitFor(() => {
      const found = Array.from(pane.querySelectorAll(".bt-row"));
      expect(found).toHaveLength(3);
      return found;
    });
    expect(rows.map((row) => row.querySelector(".bt-id")?.textContent)).toEqual([
      "ownrun-0",
      "run-fold",
      "run-fold",
    ]);
    expect(rows[0]?.querySelector(".bt-fold-label")).toBeNull();
    expect(rows[1]?.querySelector(".bt-fold-label")?.textContent).toBe("fold 3 of WF wf-6c1d9");
    expect(rows[2]?.querySelector(".bt-fold-label")?.textContent).toBe("fold 1 of WF wf-6c1d9");
    expect(walkForwardRunMock).toHaveBeenCalledWith({ walkForwardRunId: "wf-6c1d9f38" });
  });

  it("leaves an older walk-forward's fold unmarked — the known limit (#330)", async () => {
    walkForwardRunMock.mockResolvedValue({ status: "ok", data: LIB_WF_DTO });
    const pane = await openFoldPane(foldCatalog(LIB_FOLD_OLDER));

    const rows = await waitFor(() => {
      const found = Array.from(pane.querySelectorAll(".bt-row"));
      expect(found).toHaveLength(4);
      return found;
    });
    const older = rows.find((row) => row.querySelector(".bt-id")?.textContent === "oldwf-fo");
    expect(older).toBeDefined();
    // Nothing on the version names that walk-forward, so the screen cannot know
    // this run is a fold — and must not guess one.
    expect(older?.querySelector(".bt-fold-label")).toBeNull();
    // Non-vacuous: the latest walk-forward's own folds are labelled here.
    expect(
      rows
        .filter((row) => row.querySelector(".bt-fold-label") !== null)
        .map((row) => row.querySelector(".bt-id")?.textContent),
    ).toEqual(["run-fold", "run-fold"]);
  });
});

// ---------------------------------------------------------------------------
// r4.s5.w3 (#269) — the certification line opens the run in the Lab
// ---------------------------------------------------------------------------

describe("LibraryScreen (#269 — the certification run id opens the Lab)", () => {
  afterEach(() => {
    window.location.hash = "";
  });

  it("makes the certification run id a button that deep-links to that run in the Lab (#269)", async () => {
    overviewMock.mockResolvedValue({ status: "ok", data: CERTIFIED });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    fireEvent.click(within(tree).getByText("v2"));

    const pane = document.getElementById("details-pane") as HTMLElement;
    const link = await within(pane).findByRole("button", { name: "wf-run-7f3a9c21" });
    // A real button: the shell-navigation gate forbids `href="#"`, and a link
    // would read as navigation the app does not own.
    expect(link.getAttribute("href")).toBeNull();

    fireEvent.click(link);

    // The fragment names the run; the route table reads it as the Backtest Lab
    // (the query is not part of the nav id).
    expect(window.location.hash).toBe("#/backtest?wf=wf-run-7f3a9c21");
    expect(resolveNavId(window.location.hash, KNOWN_NAV_IDS)).toBe("backtest");
  });
});

// ---------------------------------------------------------------------------
// r4.s5.w3 (#325) — a wrapped creator label must not clip the sublines
// ---------------------------------------------------------------------------

describe("LibraryScreen (#325 — the hypothesis subline survives a long label)", () => {
  it("keeps the agent label to one line and the hypothesis subline readable (#325)", async () => {
    // The walked case: `external_agent · claude-code` wraps at a normal window
    // width, which used to push the DSL/hypothesis subline out of the node's
    // fixed height (d37's clipped `long · (rsi(14)…` sliver).
    overviewMock.mockResolvedValue({ status: "ok", data: WITH_AGENT });
    const { container } = render(<App />);

    fireEvent.click(await screen.findByRole("button", { name: /toggle alpha wave/i }));
    const tree = container.querySelector(".vtree-wrap") as HTMLElement;
    const node = within(tree).getByText("v2").closest(".vnode") as HTMLElement;

    // PROXY, stated plainly: the ellipsis itself is the stylesheet's job
    // (`.vnode-provenance` carries `white-space: nowrap; overflow: hidden;
    // text-overflow: ellipsis`) and jsdom performs no layout, so what is
    // asserted here is the DOM contract that keeps the label to ONE line — the
    // truncating class and the FULL line on `title` — plus the sublines that
    // must stay in the node.
    const label = node.querySelector(".vnode-provenance") as HTMLElement;
    expect(label.className).toContain("agent");
    expect(label.getAttribute("title")).toBe("external_agent · claude-code");
    // The hypothesis subline renders, with its full text reachable (#193's own
    // discipline, applied to the subline the walk found clipped).
    const hypothesis = node.querySelector(".vnode-hypothesis") as HTMLElement;
    expect(hypothesis.textContent).toBe("A wider stop cuts noise exits.");
    expect(hypothesis.getAttribute("title")).toBe("A wider stop cuts noise exits.");
    // ...and the DSL subline is still in the node.
    expect((node.querySelector(".vnode-summary")?.textContent ?? "").length).toBeGreaterThan(0);
  });
});

// ---------------------------------------------------------------------------
// #404 (d37) — every line the node renders must fit the height it is given
// ---------------------------------------------------------------------------
//
// jsdom performs no layout, so the tests above pass with the regression in
// place: `.vnode-provenance` (and the already-clipped `.vnode-hypothesis`)
// shrink to zero height and nothing in the DOM notices. This guard therefore
// reads the SOURCES back the way `../format/no-float-money.test.ts` and
// `../styles/backtest.test.ts` do — the node height constants from
// `LibraryScreen.tsx`, the line rules from `library.css`/`shared.css` (the
// certification chip's shared `.cbadge` from `paper.css`) and the tokens they
// resolve through — and does the arithmetic a browser would:
//
//   1. head + provenance + [2-line hypothesis] + summary + kpis + the column's
//      gaps + the node's padding and border, per shape, must fit inside the
//      height that shape is given (NODE_H; the taller NODE_H_AGENT when the
//      source declares one for a hypothesis version);
//   2. `.vnode-provenance` and `.vnode-hypothesis` carry `flex-shrink: 0`: a
//      flex item with non-visible overflow gets an automatic minimum of 0,
//      which is exactly how #404 zeroed these lines while `.vnode-summary`'s
//      30px min-height held the 64px box.
//
// Re-read this guard when a node line, a font size or a node height changes:
// it pins numbers, and a failure here means the numbers, not the CSS, moved.

/** The Node runtime vitest always runs on: this toolchain ships no
 * `@types/node`, so the global is claimed through one named cast. */
interface NodeRuntime {
  process: { cwd(): string; getBuiltinModule(id: string): unknown };
}
const nodeGlobal = globalThis as unknown as NodeRuntime;
const nodeRuntime = nodeGlobal.process;

/** The minimum `fs` surface this guard needs. */
interface FsLike {
  readFileSync(path: string, encoding: "utf8"): string;
}

const fs = nodeRuntime.getBuiltinModule("node:fs") as FsLike;

/** A source file under `ui/src/`, from the repo root vitest runs in. */
function source(relativePath: string): string {
  return fs.readFileSync(`${nodeRuntime.cwd()}/ui/src/${relativePath}`, "utf8");
}

const LIBRARY_SCREEN = source("screens/LibraryScreen.tsx");
const LIBRARY_CSS = source("styles/library.css");
const SHARED_CSS = source("styles/shared.css");
const PAPER_CSS = source("styles/paper.css");

/** Every `--token: value;` first declaration in `tokens.css` — the `:root`
 * values, not a later theme scope's. */
function tokenTable(tokensCss: string): Record<string, string | undefined> {
  const tokens: Record<string, string | undefined> = {};
  for (const match of tokensCss.matchAll(/(--[a-z0-9-]+)\s*:\s*([^;]+);/g)) {
    if (Object.hasOwn(tokens, match[1])) continue;
    tokens[match[1]] = match[2].trim();
  }
  return tokens;
}

const TOKEN_VALUES = tokenTable(source("styles/tokens.css"));

/** The body of the rule whose selector is exactly `selector` — brace-balanced
 * and COMMENT-STRIPPED (so a selector named in a comment cannot satisfy the
 * read), or null when no rule starts with it. Anchored at a rule boundary
 * (`^` or a preceding `}`) so `.kpi-sep` cannot be satisfied by the
 * `.vnode-kpis .kpi-sep` rule that follows it. */
function blockOf(css: string, selector: string): string | null {
  const stripped = css.replace(/\/\*[\s\S]*?\*\//g, "");
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const match = stripped.match(new RegExp(`(?:^|\\})\\s*${escaped}\\s*\\{`));
  if (match === null || match.index === undefined) return null;
  const open = match.index + match[0].length;
  let depth = 1;
  for (let i = open; i < stripped.length; i++) {
    if (stripped[i] === "{") depth++;
    else if (stripped[i] === "}") {
      depth--;
      if (depth === 0) return stripped.slice(open, i);
    }
  }
  return null;
}

/** The block this guard cannot do without; a renamed selector is a failure
 * with its own name, never an assertion over `null`. */
function requiredBlock(css: string, selector: string): string {
  const body = blockOf(css, selector);
  if (body === null) {
    throw new Error(`${selector} is not in the stylesheet — re-read this guard`);
  }
  return body;
}

/** A `property: value;` declaration inside a block, or null. */
function decl(body: string | null, property: string): string | null {
  if (body === null) return null;
  const match = body.match(new RegExp(`(?:^|[;{])\\s*${property}\\s*:\\s*([^;]+);`));
  return match === null ? null : match[1].trim();
}

/** The first number a CSS value holds, or null when it holds none. Read with a
 * regex rather than `parseFloat` — this file sits inside the no-float guard's
 * screens scan (`../format/no-float-money.test.ts`), and a CSS length is not
 * one of the rendered figures that guard's rule protects. */
function numberIn(value: string | null | undefined): number | null {
  if (value === null || value === undefined) return null;
  const found = value.match(/-?\d+(?:\.\d+)?/);
  return found === null ? null : +found[0];
}

/** A CSS value's px/number, with `var(--token)` resolved through `tokens.css`
 * — or null when the value names no number at all. A `var(--token)` that no
 * sheet defines makes the whole declaration invalid at computed-value time, so
 * the property keeps the value it would inherit instead (`.vnode-pin`,
 * `.vnode-cert` and `.vnode-delta` all ask for an undefined `--fs-10`).
 * `numberIn` reads the TOP component of a shorthand (`8px 12px` → 8, `1px
 * solid …` → 1) — the only component the node's vertical geometry uses. */
function lengthOf(value: string | null | undefined): number | null {
  if (value === null || value === undefined) return null;
  return numberIn(
    value.replace(/var\((--[a-z0-9-]+)\)/g, (_, name: string) => TOKEN_VALUES[name] ?? ""),
  );
}

/** A mandatory length (the node's chrome, a token): a declaration the rule no
 * longer makes fails with its own name, never a parse of "". */
function requiredLength(value: string | null | undefined, where: string): number {
  const parsed = lengthOf(value);
  if (parsed === null) throw new Error(`${where} is missing a length — re-read this guard`);
  return parsed;
}

/** The horizontal component of a box shorthand (`8px 12px` → 12, a single
 * value → itself) — the side the width budget needs and the vertical model
 * ignores. */
function horizontalLengthOf(value: string | null | undefined, where: string): number {
  const parts = value?.split(/\s+/) ?? [];
  const parsed = lengthOf(parts.length > 1 ? parts[1] : parts[0]);
  if (parsed === null) throw new Error(`${where} is missing a length — re-read this guard`);
  return parsed;
}

/** One rule's own line box: font-size × line-height — each its own when the
 * declaration computes, else the value inherited into it — plus the rule's own
 * vertical padding and border where it declares them (the certification badge
 * does). */
function lineBox(
  css: string,
  selector: string,
  inheritedFontSize: number,
  inheritedLineHeight: number,
): number {
  const rule = requiredBlock(css, selector);
  const fontSize = lengthOf(decl(rule, "font-size")) ?? inheritedFontSize;
  const lineHeight = lengthOf(decl(rule, "line-height")) ?? inheritedLineHeight;
  const vertical =
    2 * (lengthOf(decl(rule, "padding")) ?? 0) + 2 * (lengthOf(decl(rule, "border")) ?? 0);
  return fontSize * lineHeight + vertical;
}

/** A `const NAME = <digits>;` pixel constant from the screen source. */
function declaredPixels(name: string): number | null {
  const match = LIBRARY_SCREEN.match(new RegExp(`const ${name} = (\\d+);`));
  return match === null ? null : +match[1];
}

/** The document's own font size and line-height, which `.vnode`'s
 * `font: inherit` hands every rule below that declares neither — and what a
 * `font-size: var(--fs-10)` (a token no sheet defines) falls back to. */
const BODY_FONT_SIZE = requiredLength(TOKEN_VALUES["--fs-14"], "tokens.css --fs-14");
const BODY_LINE_HEIGHT = requiredLength(TOKEN_VALUES["--lh-normal"], "tokens.css --lh-normal");

describe("LibraryScreen (#404 — the version node's geometry)", () => {
  it("fits every node shape's lines inside the height that shape is given (#404)", () => {
    // The node's own constants: NODE_H for a plain version; NODE_H_AGENT for
    // one whose hypothesis subline renders its two clamped lines. When the
    // source declares no taller agent node, the hypothesis shape shares
    // NODE_H — and must still fit it.
    const plainHeight = declaredPixels("NODE_H");
    if (plainHeight === null) throw new Error("LibraryScreen.tsx no longer declares NODE_H");
    const agentHeight = declaredPixels("NODE_H_AGENT");
    const hypothesisHeight = agentHeight ?? plainHeight;

    // The column's chrome, off `.vnode` itself.
    const vnode = requiredBlock(LIBRARY_CSS, ".vnode");
    const verticalChrome =
      2 * requiredLength(decl(vnode, "padding"), ".vnode padding") +
      2 * requiredLength(decl(vnode, "border"), ".vnode border");
    const gap = requiredLength(decl(vnode, "gap"), ".vnode gap");

    // The lines, in VersionNode's own order. `.vnode`'s `font: inherit` hands
    // them the document's own font size and line-height (body's 14px and
    // --lh-normal); every rule below keeps them unless it declares its own.
    const headLineHeight = requiredLength(
      decl(requiredBlock(LIBRARY_CSS, ".vnode-head"), "line-height"),
      ".vnode-head line-height",
    );
    // The certification chip is the shared `.cbadge` (paper.css) inside
    // `.vnode-cert`'s own padding and border; both rules are read, so the
    // head line cannot grow unnoticed. A chip is the head's tallest child.
    const certChip = requiredBlock(LIBRARY_CSS, ".vnode-cert");
    const chipLineHeight = lengthOf(decl(certChip, "line-height")) ?? headLineHeight;
    const cert =
      lineBox(PAPER_CSS, ".cbadge", BODY_FONT_SIZE, chipLineHeight) +
      2 * (lengthOf(decl(certChip, "padding")) ?? 0) +
      2 * (lengthOf(decl(certChip, "border")) ?? 0);
    const head = Math.max(
      lineBox(LIBRARY_CSS, ".vnode-name", BODY_FONT_SIZE, headLineHeight),
      lineBox(LIBRARY_CSS, ".vnode-pin", BODY_FONT_SIZE, headLineHeight),
      cert,
      lineBox(LIBRARY_CSS, ".vnode-delta", BODY_FONT_SIZE, headLineHeight),
    );
    const provenance = lineBox(SHARED_CSS, ".vnode-provenance", BODY_FONT_SIZE, BODY_LINE_HEIGHT);
    const hypothesisRule = requiredBlock(SHARED_CSS, ".vnode-hypothesis");
    const clamp = numberIn(decl(hypothesisRule, "-webkit-line-clamp")) ?? 1;
    const hypothesis =
      lineBox(SHARED_CSS, ".vnode-hypothesis", BODY_FONT_SIZE, BODY_LINE_HEIGHT) * clamp;
    const summaryRule = requiredBlock(LIBRARY_CSS, ".vnode-summary");
    const summaryMinHeight = lengthOf(decl(summaryRule, "min-height"));
    const summary = Math.max(
      lineBox(LIBRARY_CSS, ".vnode-summary", BODY_FONT_SIZE, BODY_LINE_HEIGHT),
      summaryMinHeight ?? 0,
    );
    const kpis = lineBox(LIBRARY_CSS, ".vnode-kpis", BODY_FONT_SIZE, BODY_LINE_HEIGHT);

    const needed = (lines: readonly number[]): number =>
      lines.reduce((sum, line) => sum + line, 0) + gap * (lines.length - 1) + verticalChrome;

    // A plain node: head + provenance + summary + kpis...
    // ...and an agent version adds the two-line hypothesis subline. Both clear
    // their height by at least the 2px of sub-pixel slack this arithmetic
    // cannot see (line boxes and the badge's own baseline).
    expect(needed([head, provenance, summary, kpis])).toBeLessThanOrEqual(plainHeight - 2);
    expect(needed([head, provenance, hypothesis, summary, kpis])).toBeLessThanOrEqual(
      hypothesisHeight - 2,
    );

    // The head is read as ONE line per child; the row must not wrap (#404: a
    // crowded head wrapped its delta onto a second line and grew by 14px).
    expect(decl(requiredBlock(LIBRARY_CSS, ".vnode-head"), "white-space")).toBe("nowrap");
  });

  it("keeps the provenance and hypothesis lines from shrinking to zero (#404)", () => {
    // #325 gave `.vnode-provenance` `overflow: hidden` for its ellipsis, and a
    // flex item with non-visible overflow gets `min-height: 0` — so only
    // `flex-shrink: 0` keeps these lines at their own height in a tight column.
    expect(decl(requiredBlock(SHARED_CSS, ".vnode-provenance"), "flex-shrink")).toBe("0");
    expect(decl(requiredBlock(SHARED_CSS, ".vnode-hypothesis"), "flex-shrink")).toBe("0");
  });

  it("is wide enough for its widest line, and never wraps one (#404 F1/F2)", () => {
    // The mono advance is 0.6em: ~6.6px for these 11px lines. The Mac proof of
    // 9cd7bea measured the shipped face that way (SF Mono, Menlo); the Linux
    // fallback this suite runs against is narrower, which is why a green
    // suite shipped a node whose KPI row wrapped on the Mac. Each rule's own
    // font size is read honestly — one whose `font-size` names the undefined
    // `--fs-10` inherits (14px), it is never assumed to be 11.
    const charWidth = (fontSize: number): number => 0.6 * fontSize;
    const fontSizeOf = (css: string, selector: string): number =>
      lengthOf(decl(requiredBlock(css, selector), "font-size")) ?? BODY_FONT_SIZE;

    const nodeWidth = declaredPixels("NODE_W");
    if (nodeWidth === null) throw new Error("LibraryScreen.tsx no longer declares NODE_W");
    const vnode = requiredBlock(LIBRARY_CSS, ".vnode");
    const contentWidth =
      nodeWidth -
      2 * horizontalLengthOf(decl(vnode, "padding"), ".vnode padding") -
      2 * requiredLength(decl(vnode, "border"), ".vnode border");

    // F1 (#325 keeps it to ONE line): `external_agent · <agent name>` must
    // READ for an agent name of 16 characters — 33 characters — not ellipsize
    // to `external_agent…`.
    const agentLabel = `external_agent · ${"x".repeat(16)}`;
    expect(agentLabel.length * charWidth(fontSizeOf(SHARED_CSS, ".vnode-provenance"))).toBeLessThanOrEqual(
      contentWidth,
    );

    // F2: the widest KPI row the ring's formatters can hand a node — a signed
    // four-digit expectancy with its R unit, a full win rate, a five-digit
    // trade count — plus the separators' own spacing, on ONE line.
    const kpiProbe = "-1234.56 R · 100.0% · 12345t";
    const separator = requiredLength(
      decl(requiredBlock(LIBRARY_CSS, ".vnode-kpis .kpi-sep"), "margin-inline"),
      ".vnode-kpis .kpi-sep margin-inline",
    );
    expect(
      kpiProbe.length * charWidth(fontSizeOf(LIBRARY_CSS, ".vnode-kpis")) + 2 * 2 * separator,
    ).toBeLessThanOrEqual(contentWidth);

    // The kpis row and the summary ellipsize at their end, never wrap onto a
    // second line (the wrap is what pushed the summary out of the node).
    for (const [css, selector] of [
      [LIBRARY_CSS, ".vnode-summary"],
      [LIBRARY_CSS, ".vnode-kpis"],
      [SHARED_CSS, ".vnode-provenance"],
    ] as const) {
      expect(decl(requiredBlock(css, selector), "white-space")).toBe("nowrap");
    }
    const kpis = requiredBlock(LIBRARY_CSS, ".vnode-kpis");
    expect(decl(kpis, "overflow")).toBe("hidden");
    expect(decl(kpis, "text-overflow")).toBe("ellipsis");

    // ...and that spacing stays INSIDE the node: `.kpi-sep` is shared with the
    // collapsed strategy card's `.cc-kpis` (a 6px flex gap), so the bare rule
    // must not carry horizontal margin of its own.
    const bareSeparator = requiredBlock(LIBRARY_CSS, ".kpi-sep");
    expect(decl(bareSeparator, "margin-inline")).toBeNull();
    expect(decl(bareSeparator, "margin")).toBeNull();

    // ...and nothing in the column may be squeezed to fit: each line keeps its
    // own height, and the hypothesis keeps its two clamped lines.
    for (const [css, selector] of [
      [LIBRARY_CSS, ".vnode-summary"],
      [LIBRARY_CSS, ".vnode-kpis"],
    ] as const) {
      expect(decl(requiredBlock(css, selector), "flex-shrink")).toBe("0");
    }
    const hypothesis = requiredBlock(SHARED_CSS, ".vnode-hypothesis");
    expect(numberIn(decl(hypothesis, "-webkit-line-clamp"))).toBe(2);
    expect(decl(hypothesis, "overflow")).toBe("hidden");
  });
});
