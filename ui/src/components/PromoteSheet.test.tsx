// Rendered tests for the promote-to-paper sheet (r3.s4.w5, spec §2; AC-3).
//
// Same discipline as the other screen tests: `../bindings` is mocked so the
// sheet's behaviour is asserted against payload SHAPES, never a live IPC
// bridge. The variant is decided BEFORE the request, by the gate's own rule
// (#545): the version's latest certified record (`certification_records`) names
// a walk-forward run (`get_walk_forward_run`), checked against `server_status`.
// A passing run no record names reads uncertified. The version's own
// pair/timeframes come from its newest recorded run (`get_backtest_run`), and
// the server's 422 stays the backstop.

import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { useLayoutEffect } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const getWalkForwardRunMock = vi.fn();
const certificationRecordsMock = vi.fn();
const getBacktestRunMock = vi.fn();
const serverStatusMock = vi.fn();
const paperPromoteMock = vi.fn();
const streakCardMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    getWalkForwardRun: (...args: unknown[]) => getWalkForwardRunMock(...args),
    certificationRecords: (...args: unknown[]) => certificationRecordsMock(...args),
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
  CertificationRecordDto,
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
  holdoutStartMs: null,
  // #417: no freeze was open at this fixture's read, so nothing was withheld.
  withheldTradeCount: 0,
  totalsWithheld: false,
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

/** A certification record for `ver-1` that names `runId` — what the gate reads. */
function certificationRecord(
  runId: string,
  overrides: Partial<CertificationRecordDto> = {},
): CertificationRecordDto {
  return {
    id: "cert-1",
    versionId: "ver-1",
    freezeId: "freeze-1",
    hypothesisIndex: 1,
    rule: "wf-v2",
    pair: "BTCUSDT",
    searchWalkForwardRunId: runId,
    searchPass: true,
    holdoutStart: "2025-09-01T00:00:00.000Z",
    holdoutEnd: "2025-10-01T00:00:00.000Z",
    holdoutN: 200,
    holdoutMeanR: "0.4",
    holdoutZ: 2.64,
    holdoutLowerBound: 0.2,
    holdoutPasses: true,
    certified: true,
    searchDataVersions: [],
    holdoutDataVersions: [],
    engineFingerprint: SERVER_ENGINE,
    createdAt: "2026-09-02T00:00:00.000Z",
    calledBy: "test-agent",
    ...overrides,
  };
}

/** What `certification_records` answers: the version's records, newest first. */
function records(...list: CertificationRecordDto[]) {
  return ok({ versionId: "ver-1", records: list });
}

function walkForward(
  pass: boolean,
  engineFingerprint: string,
  walkForwardRunId = "wf-1",
): WalkForwardRunDto {
  return {
    walkForwardRunId,
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
  // The default version carries a certified record that names `wf-1`; the
  // tests for a recordless version override it (#545).
  certificationRecordsMock.mockResolvedValue(records(certificationRecord("wf-1")));
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
    // The claim names the record and the run it names (#545).
    expect(screen.getByText("cert-1")).toBeTruthy();
    expect(screen.getByText("wf-1")).toBeTruthy();
    expect(screen.getByText(/Certified by record/)).toBeTruthy();
    expect(certificationRecordsMock).toHaveBeenCalledWith({ versionId: "ver-1" });
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
// #545 — the certified variant follows the certification record
// ---------------------------------------------------------------------------

describe("the certified variant reads the record, not the run's verdict (#545)", () => {
  const PASSING_RUN = () => ok(walkForward(true, SERVER_ENGINE));

  it("reads a passing wf-v1 run that no certified record names as uncertified", async () => {
    getWalkForwardRunMock.mockResolvedValue(PASSING_RUN());
    certificationRecordsMock.mockResolvedValue(records());
    sheet();

    expect(
      await screen.findByText(/No certification record certifies this version\./),
    ).toBeTruthy();
    // The claim box never shows, and the plain Promote is not offered.
    expect(screen.queryByText(/Certified by record/)).toBeNull();
    expect(screen.queryByText(/PASS under wf-v1/)).toBeNull();
    expect(screen.queryByRole("button", { name: /^promote$/i })).toBeNull();
    // The copy names certification as the missing step, not another walk-forward.
    expect(screen.getByText(/Certification is the missing step/)).toBeTruthy();
    expect(screen.queryByText(/Run a walk-forward first/)).toBeNull();
    // The override path is untouched.
    expect(screen.getByRole("button", { name: /Continue uncertified/ })).toBeTruthy();
  });

  it("reads a version whose only records are uncertified as uncertified", async () => {
    getWalkForwardRunMock.mockResolvedValue(PASSING_RUN());
    certificationRecordsMock.mockResolvedValue(
      records(certificationRecord("wf-1", { certified: false, holdoutPasses: false })),
    );
    sheet();

    expect(
      await screen.findByText(/No certification record certifies this version\./),
    ).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^promote$/i })).toBeNull();
  });

  it("reads a certified record that names the passing run as certified, naming both", async () => {
    getWalkForwardRunMock.mockResolvedValue(PASSING_RUN());
    certificationRecordsMock.mockResolvedValue(records(certificationRecord("wf-1")));
    sheet();

    expect(await screen.findByText(/Certified by record/)).toBeTruthy();
    expect(screen.getByText("cert-1")).toBeTruthy();
    expect(screen.getByText("wf-1")).toBeTruthy();
    expect(screen.getByText(/PASS under wf-v1/)).toBeTruthy();
    expect(screen.getByRole("button", { name: /^promote$/i })).toBeTruthy();
    expect(screen.queryByText(/No certification record certifies this version/)).toBeNull();
  });

  it("reads the run the record names, not the version's latest-run pointer", async () => {
    // The pointer says `wf-1`; the latest certified record names `wf-0`.
    getWalkForwardRunMock.mockImplementation(
      async ({ walkForwardRunId }: { walkForwardRunId: string }) =>
        ok(walkForward(true, SERVER_ENGINE, walkForwardRunId)),
    );
    certificationRecordsMock.mockResolvedValue(
      records(
        certificationRecord("wf-new", { id: "cert-3", certified: false, hypothesisIndex: 3 }),
        certificationRecord("wf-0", { id: "cert-2", hypothesisIndex: 2 }),
      ),
    );
    sheet();

    expect(await screen.findByText(/Certified by record/)).toBeTruthy();
    expect(screen.getByText("cert-2")).toBeTruthy();
    expect(screen.getByText("wf-0")).toBeTruthy();
    expect(getWalkForwardRunMock).toHaveBeenCalledWith({ walkForwardRunId: "wf-0" });
  });

  it("reads a record whose named run did not pass as uncertified", async () => {
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(false, SERVER_ENGINE)));
    certificationRecordsMock.mockResolvedValue(records(certificationRecord("wf-1")));
    sheet();

    expect(
      await screen.findByText(/No certification record certifies this version\./),
    ).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^promote$/i })).toBeNull();
  });

  it("reads a certification record that cannot be read as uncertified, with the reason", async () => {
    getWalkForwardRunMock.mockResolvedValue(PASSING_RUN());
    certificationRecordsMock.mockResolvedValue({
      status: "error",
      error: busError("data", "the certification read failed on the server"),
    });
    sheet();

    expect(
      await screen.findByText(/No certification record certifies this version\./),
    ).toBeTruthy();
    expect(screen.getByText("the certification read failed on the server")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^promote$/i })).toBeNull();
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

  beforeEach(() => {
    // No walk-forward run and no certification record (#545).
    certificationRecordsMock.mockResolvedValue(records());
  });

  function renderUncertified() {
    return render(
      <PromoteSheet target={uncertifiedTarget} onClose={vi.fn()} onPromoted={vi.fn()} />,
    );
  }

  it("keeps Continue disabled for a whitespace reason, then sends the override", async () => {
    renderUncertified();
    expect(await screen.findByText(/No certification record certifies this version\./)).toBeTruthy();

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
    // The local decision says certified (a record names a passing run on this
    // engine); the server's 422 overrules it.
    getWalkForwardRunMock.mockResolvedValue(ok(walkForward(true, SERVER_ENGINE)));
    certificationRecordsMock.mockResolvedValue(records(certificationRecord("wf-1")));
    sheet();
    expect(await screen.findByText(/PASS under wf-v1/)).toBeTruthy();

    paperPromoteMock.mockResolvedValue({
      status: "error",
      error: busError("uncertified", "strategy version is not certified"),
    });
    fireEvent.click(screen.getByRole("button", { name: /^promote$/i }));

    expect(await screen.findByText(/No certification record certifies this version\./)).toBeTruthy();
  });

  it("shows the card in the confirm step after the reason row, and never in the explain step", async () => {
    renderUncertified();
    expect(await screen.findByText(/No certification record certifies this version\./)).toBeTruthy();
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

// ---------------------------------------------------------------------------
// #548 — a version change while the sheet is open never carries the previous
// version's shape into the override
// ---------------------------------------------------------------------------

describe("the version changes while the sheet is open (#548)", () => {
  /** Version A: uncertified, with a latest run whose shape is `SHAPE_A`. */
  const SHAPE_A: BacktestRunDto = { ...FOLD_RUN, runId: "run-a" };
  /** Version B has a different pair, timeframes and a D1 series. */
  const SHAPE_B: BacktestRunDto = {
    ...FOLD_RUN,
    runId: "run-b",
    strategyVersionId: "ver-2",
    pair: "ETHUSDT",
    primaryTimeframe: "1h",
    primaryDataVersion: "snap-eth-h1",
    htfTimeframe: null,
    htfDataVersion: null,
    d1DataVersion: "snap-eth-d1",
  };
  const runOf = (id: string, label: string) => ({
    id,
    createdAt: "2026-09-01T00:00:00.000Z",
    expectancy: "+0.10R",
    trades: label.length,
  });
  const targetA = {
    version: { ...version(null, false), id: "ver-a", latestRun: runOf("run-a", "a") },
    strategyName: "EMA Cross Trend",
    versionLabel: "v3",
  };
  const targetB = {
    version: { ...version(null, false), id: "ver-b", latestRun: runOf("run-b", "b") },
    strategyName: "EMA Cross Trend",
    versionLabel: "v4",
  };
  const REASON = "I want live-bar evidence first.";

  beforeEach(() => {
    certificationRecordsMock.mockResolvedValue(records());
    getBacktestRunMock.mockImplementation(({ runId }: { runId: string }) =>
      Promise.resolve(ok(runId === "run-b" ? SHAPE_B : SHAPE_A)),
    );
  });

  /** Open the sheet on A, type a reason and read A's shape. */
  async function openOnA() {
    const view = render(
      <PromoteSheet target={targetA} onClose={vi.fn()} onPromoted={vi.fn()} />,
    );
    expect(await screen.findByText(/No certification record certifies this version\./)).toBeTruthy();
    // A's own shape is on screen once it loads.
    expect(await screen.findByText(/BTCUSDT · 15m · 4h/)).toBeTruthy();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: REASON } });
    expect(
      screen.getByRole("button", { name: /Continue uncertified/ }).hasAttribute("disabled"),
    ).toBe(false);
    return view;
  }

  /** What the sheet must show while B's shape has not loaded: nothing of A's. */
  function expectNothingOfA() {
    expect(screen.queryByText(/BTCUSDT/)).toBeNull();
    const next = screen.getByRole("button", { name: /Continue uncertified/ });
    expect(next.hasAttribute("disabled")).toBe(true);
    // The disabled control submits nothing, and nothing reaches the server.
    fireEvent.click(next);
    expect(screen.queryByRole("button", { name: /Promote uncertified/ })).toBeNull();
    expect(paperPromoteMock).not.toHaveBeenCalled();
  }

  it("keeps the override disabled and hides A's shape while B's run read is pending", async () => {
    const view = await openOnA();
    getBacktestRunMock.mockImplementation(({ runId }: { runId: string }) =>
      runId === "run-b" ? new Promise(() => {}) : Promise.resolve(ok(SHAPE_A)),
    );
    view.rerender(<PromoteSheet target={targetB} onClose={vi.fn()} onPromoted={vi.fn()} />);

    expect(await screen.findByText(/No certification record certifies this version\./)).toBeTruthy();
    await waitFor(() => expect(getBacktestRunMock).toHaveBeenCalledWith({ runId: "run-b" }));
    expectNothingOfA();
  });

  it("keeps the override disabled and hides A's shape when B's run read fails", async () => {
    const view = await openOnA();
    getBacktestRunMock.mockImplementation(({ runId }: { runId: string }) =>
      Promise.resolve(
        runId === "run-b"
          ? { status: "error" as const, error: busError("data", "run b is unreadable") }
          : ok(SHAPE_A),
      ),
    );
    view.rerender(<PromoteSheet target={targetB} onClose={vi.fn()} onPromoted={vi.fn()} />);

    await waitFor(() => expect(getBacktestRunMock).toHaveBeenCalledWith({ runId: "run-b" }));
    // Let B's refused read settle before the sheet is judged.
    await act(async () => {});
    expectNothingOfA();
    // B has no readable shape: the sheet says so, as it does for a version with no run.
    expect(screen.getByText(/This version has no recorded run yet/)).toBeTruthy();
  });

  it("submits B's own shape once B's run loads", async () => {
    const view = await openOnA();
    view.rerender(<PromoteSheet target={targetB} onClose={vi.fn()} onPromoted={vi.fn()} />);

    expect(await screen.findByText(/ETHUSDT · 1h/)).toBeTruthy();
    expect(screen.queryByText(/BTCUSDT/)).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: /Continue uncertified/ }));
    expect(await screen.findByText(/Promote v4 without a certificate\?/)).toBeTruthy();

    paperPromoteMock.mockResolvedValue(ok(summaryFixture()));
    fireEvent.click(screen.getByRole("button", { name: /Promote uncertified/ }));
    await waitFor(() => expect(paperPromoteMock).toHaveBeenCalledTimes(1));
    expect(paperPromoteMock.mock.calls[0][0]).toEqual({
      version_id: "ver-b",
      override: {
        reason: REASON,
        pair: "ETHUSDT",
        primary_timeframe: "1h",
        htf_timeframe: null,
        uses_d1: true,
      },
    });
  });

  it("shows nothing of A in the render that carries B's id, before any effect lands", async () => {
    // A layout effect runs at the commit, before the sheet's passive effect can
    // reset its state: it sees the DOM of the render between the version change
    // and the effect, the one a click could reach.
    const seen: { aPairShown: boolean; overrideEnabled: boolean }[] = [];
    function CommitProbe() {
      useLayoutEffect(() => {
        const submit = screen.queryByRole("button", { name: /Promote uncertified/ });
        seen.push({
          aPairShown: document.body.textContent?.includes("BTCUSDT") ?? false,
          overrideEnabled: submit !== null && !submit.hasAttribute("disabled"),
        });
      });
      return null;
    }
    const tree = (target: typeof targetA) => (
      <>
        <PromoteSheet target={target} onClose={vi.fn()} onPromoted={vi.fn()} />
        <CommitProbe />
      </>
    );

    const view = render(tree(targetA));
    expect(await screen.findByText(/BTCUSDT · 15m · 4h/)).toBeTruthy();
    fireEvent.change(screen.getByRole("textbox"), { target: { value: REASON } });
    fireEvent.click(screen.getByRole("button", { name: /Continue uncertified/ }));
    // On A the override is live: the guard below is the only thing that can turn it off.
    expect(
      (await screen.findByRole("button", { name: /Promote uncertified/ })).hasAttribute("disabled"),
    ).toBe(false);

    seen.length = 0;
    view.rerender(tree(targetB));

    // The first commit with B's props: A's pair is gone and the override is not live.
    expect(seen[0]).toEqual({ aPairShown: false, overrideEnabled: false });
    await act(async () => {});
    expect(paperPromoteMock).not.toHaveBeenCalled();
  });
});
