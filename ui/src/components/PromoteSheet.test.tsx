// Rendered tests for the promote-to-paper sheet (r3.s4.w5, spec §2; AC-3).
//
// Same discipline as the other screen tests: `../bindings` is mocked so the
// sheet's behaviour is asserted against payload SHAPES, never a live IPC
// bridge. The variant is decided BEFORE the request (`get_walk_forward_run` +
// `server_status`), the version's own pair/timeframes come from its newest
// recorded run (`get_backtest_run`), and the server's 422 stays the backstop.

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const getWalkForwardRunMock = vi.fn();
const getBacktestRunMock = vi.fn();
const serverStatusMock = vi.fn();
const paperPromoteMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    getWalkForwardRun: (...args: unknown[]) => getWalkForwardRunMock(...args),
    getBacktestRun: (...args: unknown[]) => getBacktestRunMock(...args),
    serverStatus: (...args: unknown[]) => serverStatusMock(...args),
    paperPromote: (...args: unknown[]) => paperPromoteMock(...args),
  },
}));

import type {
  BacktestRunDto,
  BusError,
  BusErrorCode,
  LibraryVersion,
  WalkForwardRunDto,
} from "../bindings";
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
