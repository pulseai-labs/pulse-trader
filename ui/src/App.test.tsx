// Rendered tests over the shell (r1.s1.w6, spec step 7's layer, finding C6).
//
// These are the PRIMARY evidence for round-3 requirements 1 and 2 -- exactly
// one nav row is `.is-active`, and no row is a dead link -- both rendering
// facts a pure-function suite could not assert. `check-shell-navigation.sh`
// (AC-2) is a backstop against these being deleted, not the proof itself.
//
// `commands.credentialStatus` is mocked so the credential banner (w5) has a
// deterministic, real (non-"none") DOM to assert against without depending on
// a live Tauri IPC bridge, which does not exist under `jsdom`.

import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("./bindings", () => ({
  commands: {
    credentialStatus: vi.fn().mockResolvedValue("none"),
    // r1.s1.w3: the default landing is now the Library screen, which reads
    // `libraryOverview` on mount — mocked to an empty payload so these shell
    // tests keep asserting the shell (the screen's own behaviour lives in
    // `screens/LibraryScreen.test.tsx`).
    libraryOverview: vi.fn().mockResolvedValue({ status: "ok", data: { strategies: [] } }),
    // r1.s4.w3 (#141): the Backtest Lab is reachable from the shell, so the
    // remount regression below drives its one command through the shell's own
    // active-operation provider.
    runBacktestVersion: vi.fn(),
    coachTurn: vi.fn(),
    coachDecide: vi.fn(),
  },
}));

import { App } from "./App";
import { RouteContent } from "./App";
import { commands } from "./bindings";
import type { BacktestRunDto } from "./bindings";
import type { Route } from "./routes";

function setHash(hash: string) {
  window.location.hash = hash;
}

describe("<App /> shell navigation", () => {
  beforeEach(() => {
    setHash("");
  });

  afterEach(() => {
    setHash("");
  });

  it("opens on Strategy Library rather than a blank pane", async () => {
    render(<App />);
    const activeRows = await screen.findAllByRole("link", { name: /strategy library/i });
    expect(activeRows).toHaveLength(1);
    expect(activeRows[0].className).toContain("is-active");
  });

  it("marks exactly one nav row active, and it is the one the fragment selects", async () => {
    setHash("#/settings");
    render(<App />);
    const settingsRow = await screen.findByRole("link", { name: /settings/i });
    expect(settingsRow.className).toContain("is-active");

    const nav = settingsRow.closest("nav");
    expect(nav).not.toBeNull();
    // Every OTHER row in the same nav list must not be active.
    const others = within(nav as HTMLElement)
      .getAllByRole("link")
      .filter((row) => row !== settingsRow);
    for (const row of others) {
      expect(row.className).not.toContain("is-active");
    }
  });

  it("gives every nav row a real fragment href -- none is a dead '#' link", async () => {
    render(<App />);
    const rows = await screen.findAllByRole("link");
    expect(rows.length).toBeGreaterThan(0);
    for (const row of rows) {
      const href = row.getAttribute("href");
      expect(href).not.toBe("#");
      expect(href).toMatch(/^#\//);
    }
  });

  // r1.s3.w4: the Backtest Lab's ROUTES entry makes the existing nav row live
  // through `isNavBuilt` alone — the "Soon" badge disappears with no nav-side
  // edit, while still-unbuilt rows keep theirs (the flip is a derivation, not
  // a blanket badge removal).
  it("shows the Backtest Lab nav row live once its ROUTES entry lands", async () => {
    render(<App />);
    const backtestRow = await screen.findByRole("link", { name: /backtest lab/i });
    expect(within(backtestRow).queryByText("Soon")).toBeNull();
    const deployRow = screen.getByRole("link", { name: /deployment dashboard/i });
    expect(within(deployRow).getByText("Soon")).toBeTruthy();
  });

  it("keeps the credential banner mounted regardless of the active route", async () => {
    setHash("#/settings");
    render(<App />);
    expect(await screen.findByRole("status")).toBeTruthy();
  });

  it("renders the unbuilt pane, not fabricated content, for a destination with no route", async () => {
    setHash("#/settings");
    render(<App />);
    expect(await screen.findByText(/not built/i)).toBeTruthy();
  });
});

describe("RouteContent (given a resolved route, independent of which nav id is active)", () => {
  it("mounts the route's element when one is declared", () => {
    const route: Route = {
      path: "/x",
      title: "X",
      element: () => <div>Real screen content</div>,
    };
    render(<RouteContent route={route} />);
    expect(screen.getByText("Real screen content")).toBeTruthy();
  });

  it("renders the unbuilt pane when the route has no element", () => {
    render(<RouteContent route={undefined} />);
    expect(screen.getByText(/not built/i)).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// r1.s4.w3 / #141 — the operation survives the route
// ---------------------------------------------------------------------------

/** A catalog with one real version, so the Lab has something to run. */
const ONE_VERSION = {
  strategies: [
    {
      id: "strat-1",
      name: "Alpha Wave",
      createdAt: "2026-09-01T09:00:00.000Z",
      pinnedVersionId: null,
      versions: [
        {
          id: "v-1",
          parentId: null,
          createdAt: "2026-09-01T09:00:00.000Z",
          dsl: {
            name: "RSI Oversold",
            direction: "long",
            entry: ["rsi(14) < 30"],
            filters: [],
            exits: ["stop 5%"],
            risk: ["risk 1% per trade"],
          },
          stats: null,
          deltaVsParent: null,
          recentRuns: [],
          latestRun: null,
          createdBy: "human",
          agentName: null,
          hypothesis: null,
          certified: false,
          latestWalkForwardRunId: null,
        },
      ],
    },
  ],
};

/** The minimum of a run DTO these shell tests read back. */
const LANDED_RUN = {
  runId: "run-landed-while-away",
  strategyVersionId: "v-1",
  schemaVersion: 3,
  createdAt: "2026-09-05T10:00:00.000Z",
  pair: "BTCUSDT",
  primaryTimeframe: "15m",
  primaryDataVersion: "v7",
  htfTimeframe: null,
  htfDataVersion: null,
  firstOpenTimeMs: "1",
  lastCloseTimeMs: "2",
  startingEquity: "10000",
  takerFeeBps: "4",
  slippageBps: "2",
  funding: "snapshot_rates",
  leadInFrom: null,
  engineFingerprint: "sha256:abc",
  engineTarget: "aarch64-apple-darwin",
  resultContentHash: "sha256:def",
  fingerprintWarning: null,
  netPnl: "1.00",
  feesTotal: "0",
  fundingTotal: "0",
  slippageTotal: "0",
  expectancy: "0.5",
  winRate: "0.5",
  profitFactor: null,
  grossProfit: "1",
  grossLoss: "0",
  avgWin: "1",
  avgLoss: "0",
  maxDrawdown: "0",
  tradeCount: 2,
  winCount: 1,
  lossCount: 1,
  maxWinStreak: 1,
  maxLossStreak: 1,
  sharpe: null,
  sortino: null,
  skippedSubLot: 0,
  skippedSubNotional: 0,
  skippedLeverageCapped: 0,
  equity: [{ timeMs: "1", equity: "10000" }],
  regimes: [
    { regime: "trending_up", tradeCount: 1, netPnl: "1" },
    { regime: "trending_down", tradeCount: 1, netPnl: "0" },
    { regime: "ranging", tradeCount: 0, netPnl: "0" },
    { regime: "unknown", tradeCount: 0, netPnl: "0" },
  ],
  mfe: { binWidth: "0.25", bins: [], underflow: 0, overflow: 0 },
  mae: { binWidth: "0.25", bins: [], underflow: 0, overflow: 0 },
  trades: [],
} as unknown as BacktestRunDto;

describe("<App /> keeps an operation alive across navigation (#141)", () => {
  beforeEach(() => {
    setHash("");
    vi.mocked(commands.libraryOverview).mockResolvedValue({ status: "ok", data: ONE_VERSION });
    vi.mocked(commands.runBacktestVersion).mockReset();
  });

  afterEach(() => {
    setHash("");
  });

  it("reattaches the run started before a navigation, and refuses a second one meanwhile", async () => {
    // A run that will not settle until this test says so — the whole window in
    // which navigating away used to lose the operation.
    let settle!: (value: { status: "ok"; data: BacktestRunDto }) => void;
    const pending = new Promise<{ status: "ok"; data: BacktestRunDto }>((resolve) => {
      settle = resolve;
    });
    vi.mocked(commands.runBacktestVersion).mockReturnValue(
      pending as ReturnType<typeof commands.runBacktestVersion>,
    );

    setHash("#/backtest");
    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /run backtest/i }));
    expect(commands.runBacktestVersion).toHaveBeenCalledTimes(1);

    // Navigate away and back. The screen unmounts and remounts; the operation
    // does neither.
    await act(async () => {
      setHash("#/library");
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });
    await act(async () => {
      setHash("#/backtest");
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });

    // Reattached: still running, and NOTHING was invoked again by the remount.
    expect(await screen.findByText(/running the backtest/i)).toBeTruthy();
    expect(commands.runBacktestVersion).toHaveBeenCalledTimes(1);

    // A second run for the same version, before the first settles, is refused in
    // the UI — the button is not a live trigger while its operation is in flight.
    const runButton = screen.getByRole("button", { name: /running|run backtest/i });
    fireEvent.click(runButton);
    expect(commands.runBacktestVersion).toHaveBeenCalledTimes(1);

    // The result lands and the reattached screen renders it.
    await act(async () => {
      settle({ status: "ok", data: LANDED_RUN });
      await pending;
    });
    expect(await screen.findByText("run-landed-while-away")).toBeTruthy();
    expect(commands.runBacktestVersion).toHaveBeenCalledTimes(1);
  });

  it("displays a result that settled while the screen was unmounted, without re-running", async () => {
    let settle!: (value: { status: "ok"; data: BacktestRunDto }) => void;
    const pending = new Promise<{ status: "ok"; data: BacktestRunDto }>((resolve) => {
      settle = resolve;
    });
    vi.mocked(commands.runBacktestVersion).mockReturnValue(
      pending as ReturnType<typeof commands.runBacktestVersion>,
    );

    setHash("#/backtest");
    render(<App />);
    fireEvent.click(await screen.findByRole("button", { name: /run backtest/i }));

    // Leave, THEN let it settle — the case the old component-local state dropped
    // on the floor.
    await act(async () => {
      setHash("#/library");
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });
    await act(async () => {
      settle({ status: "ok", data: LANDED_RUN });
      await pending;
    });
    await act(async () => {
      setHash("#/backtest");
      window.dispatchEvent(new HashChangeEvent("hashchange"));
    });

    expect(await screen.findByText("run-landed-while-away")).toBeTruthy();
    expect(commands.runBacktestVersion).toHaveBeenCalledTimes(1);
  });
});
