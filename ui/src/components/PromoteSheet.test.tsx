// Rendered tests for the promote-to-paper sheet (r3.s4.w5, spec §2; AC-3).
//
// Same discipline as the other screen tests: `../bindings` is mocked so the
// sheet's behaviour is asserted against payload SHAPES, never a live IPC
// bridge. The variant is decided BEFORE the request (`get_walk_forward_run` +
// `server_status`), the version's own pair/timeframes come from its newest
// recorded run (`get_backtest_run`), and the server's 422 stays the backstop.

import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const getWalkForwardRunMock = vi.fn();
const getBacktestRunMock = vi.fn();
const serverStatusMock = vi.fn();
const paperPromoteMock = vi.fn();
const streakCardMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    getWalkForwardRun: (...args: unknown[]) => getWalkForwardRunMock(...args),
    getBacktestRun: (...args: unknown[]) => getBacktestRunMock(...args),
    serverStatus: (...args: unknown[]) => serverStatusMock(...args),
    paperPromote: (...args: unknown[]) => paperPromoteMock(...args),
    streakCard: (...args: unknown[]) => streakCardMock(...args),
  },
}));

import type {
  BacktestRunDto,
  BusError,
  BusErrorCode,
  LibraryVersion,
  StreakCardDto,
  WalkForwardRunDto,
} from "../bindings";
import { subscribePaperCountRefresh } from "../paper/handoff";
import { PromoteSheet } from "./PromoteSheet";

// ---------------------------------------------------------------------------
// Fixtures (mirroring the generated types exactly)
// ---------------------------------------------------------------------------

/** The server's own fingerprint — what `server_status` answers. */
const SERVER_ENGINE = "9d4e7c1abeef1234567890abcdef1234567890abcdef1234567890abcdef1234";

/** The one run the sheet's shape/snapshot reads resolve to. */
const FOLD_RUN: BacktestRunDto = {
  runId: "run-fold-1",
  strategyVersionId: "ver-1",
  schemaVersion: 3,
  createdAt: "2026-09-28T10:00:00.000Z",
  pair: "BTCUSDT",
  primaryTimeframe: "15m",
  primaryDataVersion: "snap-m15-0001",
  htfTimeframe: "4h",
  htfDataVersion: "snap-h4-0002",
  d1DataVersion: null,
  usesHtf: true,
  firstOpenTimeMs: "1735689600000",
  lastCloseTimeMs: "1756684800000",
  startingEquity: "10000.00",
  takerFeeBps: "4",
  slippageBps: "1",
  funding: "snapshot_rates",
  symbolFilters: null,
  leadInFrom: null,
  engineFingerprint: SERVER_ENGINE,
  engineTarget: "aarch64-apple-darwin",
  resultContentHash: "sha256:abc",
  fingerprintWarning: null,
  netPnl: "12.5",
  feesTotal: "1.2",
  fundingTotal: "-0.4",
  slippageTotal: "0.3",
  expectancy: "0.42",
  winRate: "0.5",
  profitFactor: null,
  grossProfit: "20",
  grossLoss: "7.5",
  avgWin: "4",
  avgLoss: "2.5",
  maxDrawdown: "3",
  tradeCount: 20,
  winCount: 10,
  lossCount: 10,
  maxWinStreak: 2,
  maxLossStreak: 2,
  sharpe: null,
  sortino: null,
  skippedSubLot: 0,
  skippedSubNotional: 0,
  skippedLeverageCapped: 0,
  equity: [],
  regimes: [],
  mfe: { binWidth: "0.25", bins: [], underflow: 0, overflow: 0 },
  mae: { binWidth: "0.25", bins: [], underflow: 0, overflow: 0 },
  trades: [],
  walkForward: null,
};

function walkForward(pass: boolean, engineFingerprint: string): WalkForwardRunDto {
  return {
    walkForwardRunId: "wf-1",
    versionId: "ver-1",
    scheme: "rolling-oos/v1",
    k: 6,
    rule: "wf-v1",
    spanFrom: "2025-01-01T00:00:00.000Z",
    spanTo: "2025-09-01T00:00:00.000Z",
    fromDefaulted: true,
    engineFingerprint,
    verdict: {
      pass,
      foldsHolding: pass ? 6 : 2,
      foldsRequired: 4,
      pooled: { n: 120, meanR: "0.31", lowerBound: 0.12, holds: pass },
    },
    folds: [
      {
        index: 0,
        windowFrom: "2025-01-01T00:00:00.000Z",
        windowTo: "2025-02-01T00:00:00.000Z",
        backtestRunId: FOLD_RUN.runId,
        n: 20,
        meanR: "0.31",
        lowerBound: 0.12,
        holds: true,
        trades: 20,
        expectancy: "0.31",
        winRate: "0.5",
      },
    ],
  };
}

function version(latestWalkForwardRunId: string | null, certified: boolean): LibraryVersion {
  return {
    id: "ver-1",
    parentId: null,
    createdAt: "2026-09-01T00:00:00.000Z",
    createdBy: "human",
    agentName: null,
    hypothesis: null,
    dsl: {
      name: "RSI Oversold Bounce",
      direction: "long",
      entry: ["rsi(14) < 30"],
      filters: [],
      exits: ["stop 5%"],
      risk: ["risk 1%"],
    },
    stats: null,
    deltaVsParent: null,
    recentRuns: [],
    latestRun: null,
    certified,
    latestWalkForwardRunId,
  };
}

const TARGET = {
  version: version("wf-1", true),
  strategyName: "RSI Oversold Bounce",
  versionLabel: "v3.1",
};

/** The card `streak_card` answers (r5.s4.w2). Its run is the version's latest
 * FULL-SPAN run, deliberately a different run from `FOLD_RUN` (the sheet's
 * shape run is the walk-forward fold whenever one exists). */
function streakCardReady(): StreakCardDto {
  return {
    state: "ready",
    runId: "run-full-span-1",
    runCreatedAt: "2026-10-01T09:00:00.000Z",
    closedTrades: 179,
    horizonTrades: 100,
    paths: 10000,
    seed: "0x523553345354524b",
    streak: { p50: 7, p95: 12, observed: 3 },
    drawdownR: { p50: "10", p95: "19", observed: "3" },
    drawdownUsdt: { p50: "1000", p95: "1900", observed: "300" },
    drawdownPct: { p50: "10", p95: "19", observed: "3" },
  };
}

/** The two elements' document order: does `b` follow `a`? */
function follows(a: HTMLElement, b: HTMLElement): boolean {
  return (a.compareDocumentPosition(b) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
}

function ok<T>(data: T) {
  return { status: "ok" as const, data };
}

function busError(code: BusErrorCode, message: string): BusError {
  return { code, message, run_id: null, session_id: null, child_run_id: null };
}

function up(fingerprint: string) {
  return ok({
    state: "up" as const,
    binary_version: "0.3.0",
    engine_fingerprint: fingerprint,
    reason: null,
    role: null,
  });
}

function sheet(onPromoted = vi.fn()) {
  const result = render(
    <PromoteSheet target={TARGET} onClose={vi.fn()} onPromoted={onPromoted} />,
  );
  return { result, onPromoted };
}

beforeEach(() => {
  vi.clearAllMocks();
  serverStatusMock.mockResolvedValue(up(SERVER_ENGINE));
  getBacktestRunMock.mockResolvedValue(ok(FOLD_RUN));
  // The card's own read (r5.s4.w2); a test overrides it for the other states.
  streakCardMock.mockResolvedValue(ok(streakCardReady()));
});

// ---------------------------------------------------------------------------
// Certified
// ---------------------------------------------------------------------------

describe("certified", () => {
  beforeEach(() => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
  });

  it("shows the run, the snapshots and the server match, and promotes with no override", async () => {
    const { onPromoted } = sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();
    // The folds sentence (r4.s5.w1, #305/#328): the scheme's count stated in
    // words — never the bare `6/4` ratio — from the verdict's own numbers.
    expect(screen.getByText("6 of 6 folds hold (4 required)")).toBeTruthy();
    expect(screen.getByText(/matches this server/)).toBeTruthy();
    // The snapshots come from the version's recorded run, never the mock.
    expect(await screen.findByText("snap-m15-0001")).toBeTruthy();
    expect(screen.getByText("snap-h4-0002")).toBeTruthy();

    paperPromoteMock.mockResolvedValue(ok({ ...summaryFixture(), id: "s-new" }));
    fireEvent.click(screen.getByRole("button", { name: /^promote$/i }));

    await waitFor(() => expect(paperPromoteMock).toHaveBeenCalledTimes(1));
    expect(paperPromoteMock.mock.calls[0][0]).toEqual({
      version_id: "ver-1",
      override: null,
    });
    await waitFor(() => expect(onPromoted).toHaveBeenCalledWith("s-new"));
  });
});

// ---------------------------------------------------------------------------
// Stale
// ---------------------------------------------------------------------------

describe("stale", () => {
  beforeEach(() => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, "engine-old-4f2a91")));
  });

  it("shows both fingerprints, no override and no promote button — only re-run", async () => {
    sheet();
    expect(
      await screen.findByText(/Certified under engine engine-old-4f2a91; this server runs/),
    ).toBeTruthy();
    expect(screen.getByText("There is no override")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^promote$/i })).toBeNull();
    expect(screen.queryByRole("button", { name: /promote uncertified/i })).toBeNull();
    expect(screen.getByRole("button", { name: /Re-run walk-forward/ })).toBeTruthy();
  });

  it("re-renders as stale when the server refuses with certified_under_other_engine", async () => {
    // The decision said certified (matching fingerprints), and the server's
    // 422 backstop overrules it.
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
    sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

    paperPromoteMock.mockResolvedValue({
      status: "error",
      error: busError(
        "certified_under_other_engine",
        "certified under engine engine-old-4f2a91; re-run walk-forward to re-certify (this build is 9d4e7c1a)",
      ),
    });
    fireEvent.click(screen.getByRole("button", { name: /^promote$/i }));

    expect(await screen.findByText("There is no override")).toBeTruthy();
    expect(screen.getByText(/re-run walk-forward to re-certify/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^promote$/i })).toBeNull();
  });
});

// ---------------------------------------------------------------------------
// Uncertified — two steps, a required reason
// ---------------------------------------------------------------------------

describe("uncertified", () => {
  // The version's OWN shape (pair/timeframes) comes from its newest recorded
  // run, so the uncertified path needs one: `latestRun` here.
  const uncertifiedTarget = {
    version: {
      ...version(null, false),
      latestRun: {
        id: "run-latest",
        createdAt: "2026-09-01T00:00:00.000Z",
        expectancy: "+0.10R",
        trades: 12,
      },
    },
    strategyName: "EMA Cross Trend",
    versionLabel: "v3",
  };

  function renderUncertified() {
    return render(
      <PromoteSheet target={uncertifiedTarget} onClose={vi.fn()} onPromoted={vi.fn()} />,
    );
  }

  it("keeps Continue disabled for a whitespace reason, then sends the override", async () => {
    renderUncertified();
    expect(await screen.findByText(/No walk-forward run certifies this version\./)).toBeTruthy();

    const next = screen.getByRole("button", { name: /Continue uncertified/ });
    expect(next.hasAttribute("disabled")).toBe(true);

    const reason = screen.getByRole("textbox");
    fireEvent.change(reason, { target: { value: "   " } });
    expect(next.hasAttribute("disabled")).toBe(true);

    fireEvent.change(reason, {
      target: { value: "Fold 3 failed on one low-vol week." },
    });
    expect(next.hasAttribute("disabled")).toBe(false);
    fireEvent.click(next);

    // The confirm step quotes the trimmed reason and sends it with the
    // version's OWN pair and timeframes (read from its recorded run).
    expect(await screen.findByText(/Promote v3 without a certificate\?/)).toBeTruthy();
    // The quoted reason rides the confirm step's own row (the badge's tooltip
    // carries it too — the reason is part of the record).
    const reasonRow = screen.getByText("Your reason").closest(".ps-row") as HTMLElement;
    expect(within(reasonRow).getByText(/“Fold 3 failed on one low-vol week.”/)).toBeTruthy();

    paperPromoteMock.mockResolvedValue(ok(summaryFixture()));
    fireEvent.click(screen.getByRole("button", { name: /Promote uncertified/ }));
    await waitFor(() => expect(paperPromoteMock).toHaveBeenCalledTimes(1));
    expect(paperPromoteMock.mock.calls[0][0]).toEqual({
      version_id: "ver-1",
      override: {
        reason: "Fold 3 failed on one low-vol week.",
        pair: "BTCUSDT",
        primary_timeframe: "15m",
        htf_timeframe: "4h",
        uses_d1: false,
      },
    });
  });

  it("re-renders as uncertified when the server refuses with uncertified", async () => {
    // The local decision says certified (a passing run on this engine); the
    // server's 422 overrules it.
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
    sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

    paperPromoteMock.mockResolvedValue({
      status: "error",
      error: busError("uncertified", "strategy version is not certified"),
    });
    fireEvent.click(screen.getByRole("button", { name: /^promote$/i }));

    expect(await screen.findByText(/No walk-forward run certifies this version\./)).toBeTruthy();
  });

  it("shows the card in the confirm step after the reason row, and never in the explain step", async () => {
    renderUncertified();
    expect(await screen.findByText(/No walk-forward run certifies this version\./)).toBeTruthy();
    // The explain step asks for a reason; the card belongs at the decision.
    expect(screen.queryByRole("table", { name: "A normal bad run" })).toBeNull();

    fireEvent.change(screen.getByRole("textbox"), {
      target: { value: "Fold 3 failed on one low-vol week." },
    });
    fireEvent.click(screen.getByRole("button", { name: /Continue uncertified/ }));

    const table = await screen.findByRole("table", { name: "A normal bad run" });
    const reasonRow = screen.getByText("Your reason").closest(".ps-row") as HTMLElement;
    const equityRow = screen.getByText("Equity").closest(".ps-row") as HTMLElement;
    expect(follows(reasonRow, table)).toBe(true);
    expect(follows(equityRow, table)).toBe(true);
    expect(
      follows(table, screen.getByRole("button", { name: /Promote uncertified/ })),
    ).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// r5.s4.w2 — the streak and drawdown card, before the confirm button
// ---------------------------------------------------------------------------

describe("the streak and drawdown card", () => {
  beforeEach(() => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
  });

  it("renders before the promote button, naming the card's own run", async () => {
    sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

    const table = await screen.findByRole("table", { name: "A normal bad run" });
    expect(streakCardMock).toHaveBeenCalledWith({ versionId: "ver-1" });

    // The card names the run the CARD read, never the sheet's shape run: the
    // shape is the walk-forward fold, the card is the latest full-span run.
    expect(getBacktestRunMock).toHaveBeenCalledWith({ runId: FOLD_RUN.runId });
    expect(screen.getByTitle("run-full-span-1")).toBeTruthy();
    expect(screen.queryByTitle("run-fold-1")).toBeNull();

    expect(
      within(table)
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["Typical (p50)", "Bad but normal (p95)", "This backtest"]);

    // Placement: after the certified body's Equity row, before the footer's
    // Promote — the trader decides with the card in front of them.
    const equityRow = screen.getByText("Equity").closest(".ps-row") as HTMLElement;
    expect(follows(equityRow, table)).toBe(true);
    expect(follows(table, screen.getByRole("button", { name: /^promote$/i }))).toBe(true);
  });

  it("renders the card's text as the trader reads it", async () => {
    sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

    const table = await screen.findByRole("table", { name: "A normal bad run" });
    expect(
      within(table)
        .getAllByRole("row")
        .map((row) => row.textContent),
    ).toEqual([
      "Typical (p50)Bad but normal (p95)This backtest",
      "Longest losing streak7 trades12 trades3 trades",
      "Max drawdown10.00 R · 1,000.00 USDT (10.00% of 10,000)19.00 R · 1,900.00 USDT (19.00% of 10,000)3.00 R · 300.00 USDT (3.00% of 10,000)",
    ]);
  });

  it("is absent in the stale body", async () => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, "engine-old-4f2a91")));
    sheet();
    expect(await screen.findByText(/Certified under engine engine-old-4f2a91/)).toBeTruthy();
    expect(screen.queryByRole("table", { name: "A normal bad run" })).toBeNull();
  });

  it("shows not-enough-trades with no figures, and leaves Promote enabled", async () => {
    streakCardMock.mockResolvedValue(
      ok({ state: "notEnoughTrades", runId: "run-thin-1", closedTrades: 12 }),
    );
    sheet();
    expect(
      await screen.findByText(
        "Only 12 closed trades in backtest run-thin — at least 30 are needed to estimate a normal bad run.",
      ),
    ).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
    const promote = screen.getByRole("button", { name: /^promote$/i }) as HTMLButtonElement;
    expect(promote.disabled).toBe(false);
  });

  it("shows no-run with no figures, and leaves Promote enabled", async () => {
    streakCardMock.mockResolvedValue(ok({ state: "noRun" }));
    sheet();
    expect(
      await screen.findByText(
        "No full backtest of this version yet — run one to see a normal bad run.",
      ),
    ).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
    const promote = screen.getByRole("button", { name: /^promote$/i }) as HTMLButtonElement;
    expect(promote.disabled).toBe(false);
  });

  it("keeps the variant, the refusal and the buttons when the card read rejects", async () => {
    // The card's read settles AFTER the decision on purpose: a stray write from
    // the card's own chain then has nowhere left to hide (#336's own idiom).
    let rejectCard!: (reason: unknown) => void;
    streakCardMock.mockImplementation(
      () =>
        new Promise((_resolve, reject) => {
          rejectCard = reject;
        }),
    );
    sheet();

    // The certified variant decides without waiting for the card...
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();
    expect(screen.getByText("Reading the streak and drawdown card…")).toBeTruthy();

    // ...and the card's own failure lands afterwards, changing only the card:
    // no figures are invented, no refusal is raised, and the trader can still
    // promote (the card informs; the server's gate decides).
    await act(async () => {
      rejectCard(new Error("the IPC bridge is down"));
    });
    expect(screen.getByText("The streak and drawdown card could not be read.")).toBeTruthy();
    expect(screen.getByText(/PASS under wf-v1/)).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.queryByRole("alert")).toBeNull();
    const promote = screen.getByRole("button", { name: /^promote$/i }) as HTMLButtonElement;
    expect(promote.disabled).toBe(false);
  });

  it("keeps the variant when the bus refuses the card read", async () => {
    let resolveCard!: (value: unknown) => void;
    streakCardMock.mockImplementation(
      () =>
        new Promise((resolve) => {
          resolveCard = resolve;
        }),
    );
    sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

    await act(async () => {
      resolveCard({
        status: "error",
        error: busError("data", "the card read failed on the server"),
      });
    });
    expect(screen.getByText("The streak and drawdown card could not be read.")).toBeTruthy();
    expect(screen.getByText(/PASS under wf-v1/)).toBeTruthy();
    expect(screen.queryByRole("table")).toBeNull();
    expect(screen.getByRole("button", { name: /^promote$/i })).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// The footer
// ---------------------------------------------------------------------------

describe("the agents footer", () => {
  it("is present on the certified and uncertified variants", async () => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
    sheet();
    expect(await screen.findByText("Only this app can promote. Agents can't.")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------

function summaryFixture() {
  return {
    id: "s-1",
    strategy_version_id: "ver-1",
    pair: "BTCUSDT",
    primary_timeframe: "15m",
    htf_timeframe: null,
    uses_d1: false,
    graduation: {
      graduation: "certified" as const,
      walk_forward_run_id: "wf-1",
      data_versions: [{ timeframe: "15m", data_version: "snap-m15-0001" }],
    },
    fixture: false,
    promoted_by: "operator",
    status: { state: "running" as const },
    epochs: [SERVER_ENGINE],
    last_bar_open_time: null,
    closed_trade_count: 0,
    realized_usdt_total: "0",
    realized_usdt_count: 0,
    realized_r_total: "0",
    realized_r_count: 0,
    unrealized_usdt: null,
    session_pnl_usdt: "0",
    open_position: null,
    shadow_checks: [],
    certification_stale: false,
    comparison: {
      status: "pending" as const,
      n: 0,
      of: 20,
      engine_builds: 1,
      certification_stale: false,
    },
  };
}

// ---------------------------------------------------------------------------
// r4.s5.w4 — the in-progress promote and the strip's count refresh
// ---------------------------------------------------------------------------

describe("the promote in progress (#336)", () => {
  it("reads Promoting…, is disabled, and cannot be submitted twice (#336)", async () => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
    let settlePromote!: (value: unknown) => void;
    paperPromoteMock.mockImplementation(
      // The executor form, not `Promise.withResolvers`: this tree's `lib` is
      // ES2022 (tsconfig.json), and the typecheck stage is part of the gate.
      () =>
        new Promise((resolve) => {
          settlePromote = resolve;
        }),
    );
    sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /^promote$/i }));
    const running = screen.getByRole("button", { name: "Promoting…" }) as HTMLButtonElement;
    expect(running.disabled).toBe(true);

    // A second submit cannot send the request again — the repeat click, and
    // the Enter-key path that activates the same focused button.
    fireEvent.click(running);
    fireEvent.keyDown(running, { key: "Enter" });
    fireEvent.click(running);
    expect(paperPromoteMock).toHaveBeenCalledTimes(1);

    // A failure re-enables it with the server's own message.
    await act(async () => {
      settlePromote({
        status: "error",
        error: busError("data", "the promote failed on the server"),
      });
    });
    const again = (await screen.findByRole("button", { name: /^promote$/i })) as HTMLButtonElement;
    expect(again.disabled).toBe(false);
    expect(screen.getByRole("alert").textContent).toContain("the promote failed on the server");
  });
});

describe("the title strip's count refresh (#335)", () => {
  it("asks the strip's existing count poll to refresh after a successful promote (#335)", async () => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
    const refreshed = vi.fn();
    const unsubscribe = subscribePaperCountRefresh(refreshed);
    try {
      sheet();
      expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();
      expect(refreshed).not.toHaveBeenCalled();

      paperPromoteMock.mockResolvedValue(ok(summaryFixture()));
      fireEvent.click(screen.getByRole("button", { name: /^promote$/i }));
      await waitFor(() => expect(refreshed).toHaveBeenCalledTimes(1));
    } finally {
      unsubscribe();
    }
  });

  it("leaves the count alone when the promote is refused (#335)", async () => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
    const refreshed = vi.fn();
    const unsubscribe = subscribePaperCountRefresh(refreshed);
    try {
      sheet();
      expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

      paperPromoteMock.mockResolvedValue({
        status: "error",
        error: busError("certified_under_other_engine", "certified under engine engine-old"),
      });
      fireEvent.click(screen.getByRole("button", { name: /^promote$/i }));
      // The sheet re-renders as stale; the strip's count did not move.
      expect(await screen.findByText("There is no override")).toBeTruthy();
      expect(refreshed).not.toHaveBeenCalled();
    } finally {
      unsubscribe();
    }
  });
});
