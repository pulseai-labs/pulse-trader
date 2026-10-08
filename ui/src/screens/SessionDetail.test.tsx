// Rendered tests for the session detail (r3.s4.w5, spec §4; AC-3).
//
// `../bindings` is mocked (the LibraryScreen precedent); the session stream is
// driven through the REAL `Channel` the hook constructed, exactly as
// `useComposeRun.test.ts` drives the compose channel. The panels render only
// server values: the shadow verdict and its `bar_count` come from the summary
// and the frames the UI actually received, the OOS texts are the server's own,
// and a stopped session is read-only.

import { act, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Channel } from "@tauri-apps/api/core";

const paperSessionMock = vi.fn();
const paperSessionTradesMock = vi.fn();
const paperSessionEventsMock = vi.fn();
const paperStopMock = vi.fn();
const paperShadowCheckMock = vi.fn();
const serverStatusMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    paperSession: (...args: unknown[]) => paperSessionMock(...args),
    paperSessionTrades: (...args: unknown[]) => paperSessionTradesMock(...args),
    paperSessionEvents: (...args: unknown[]) => paperSessionEventsMock(...args),
    paperStop: (...args: unknown[]) => paperStopMock(...args),
    paperShadowCheck: (...args: unknown[]) => paperShadowCheckMock(...args),
    serverStatus: (...args: unknown[]) => serverStatusMock(...args),
  },
}));

import type {
  PaperSessionSummary,
  PaperShadowCheck,
  PaperStreamEvent,
  PaperTrades,
} from "../bindings";
import SessionDetail from "./SessionDetail";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const ENGINE = "engine-current-0123456789abcdef";

function summary(over: Partial<PaperSessionSummary> = {}): PaperSessionSummary {
  return {
    id: "s-1",
    strategy_version_id: "ver-1",
    pair: "BTCUSDT",
    primary_timeframe: "15m",
    htf_timeframe: "4h",
    uses_d1: false,
    graduation: {
      graduation: "certified",
      walk_forward_run_id: "wf-1",
      data_versions: [{ timeframe: "15m", data_version: "snap-m15-0001" }],
    },
    fixture: false,
    promoted_by: "operator",
    status: { state: "running" },
    epochs: [ENGINE],
    last_bar_open_time: "1738368900000",
    closed_trade_count: 4,
    open_position: null,
    shadow_checks: [],
    certification_stale: false,
    comparison: {
      status: "pending",
      n: 4,
      of: 20,
      engine_builds: 1,
      certification_stale: false,
    },
    ...over,
  };
}

function identical(closedTrades: number): PaperShadowCheck {
  return {
    engine_fingerprint: ENGINE,
    result: { verdict: "identical", closed_trades: closedTrades, open_position: false },
  };
}

function drifted(): PaperShadowCheck {
  return {
    engine_fingerprint: ENGINE,
    result: {
      verdict: "drift",
      first_divergence: "closed-trade 3 differs",
      live: '{"exit_price":"64102.5","exit_reason":"trail","realized_r":"0.61"}',
      shadow: '{"exit_price":"64388.0","exit_reason":"tp1","realized_r":"1.00"}',
    },
  };
}

const TRADES: PaperTrades = {
  closed_trades: [
    {
      side: "long",
      qty: "0.02",
      entry_price: "64000",
      entry_fill_time: "2026-10-01T00:00:00.000Z",
      exit_price: "64400",
      exit_fill_time: "2026-10-01T01:00:00.000Z",
      exit_reason: "take_profit",
      realized_r: "0.61",
    },
  ],
  open_position: null,
};

/** The channel the detail's hook handed to `paper_session_events`. */
let streamChannel: Channel<PaperStreamEvent> | null = null;

function ok<T>(data: T) {
  return { status: "ok" as const, data };
}

function frame(seq: number, type: string, payload: object): PaperStreamEvent {
  return { kind: "frame", seq, type, payload: JSON.stringify({ type, seq, ...payload }) };
}

beforeEach(() => {
  vi.clearAllMocks();
  streamChannel = null;
  serverStatusMock.mockResolvedValue(
    ok({ state: "up", binary_version: "0.3.0", engine_fingerprint: ENGINE, reason: null, role: null }),
  );
  paperSessionTradesMock.mockResolvedValue(ok(TRADES));
  paperShadowCheckMock.mockResolvedValue(
    ok({ verdict: "identical", closed_trades: 4, open_position: false }),
  );
  paperSessionEventsMock.mockImplementation(
    (_id: string, _afterSeq: number | null, channel: Channel<PaperStreamEvent>) => {
      streamChannel = channel;
      // The stream is long-lived: the promise settles only on unmount/refusal.
      return new Promise(() => {});
    },
  );
});

function renderDetail(over: Partial<PaperSessionSummary> = {}) {
  paperSessionMock.mockResolvedValue(ok(summary(over)));
  return render(<SessionDetail sessionId="s-1" />);
}

// ---------------------------------------------------------------------------
// The certificate and engine rows
// ---------------------------------------------------------------------------

describe("the certificate and engine rows", () => {
  it("names the certifying run and the epochs, and notes a span of >1 build", async () => {
    renderDetail({
      epochs: ["engine-old-aaaa", ENGINE],
      comparison: {
        status: "pending",
        n: 4,
        of: 20,
        engine_builds: 2,
        certification_stale: true,
      },
      certification_stale: true,
    });
    expect(await screen.findByText("wf-1")).toBeTruthy();
    expect(screen.getByText(/started on engine-old-aaaa/)).toBeTruthy();
    expect(screen.getByText(/server now engine-current-0123456789abcdef/)).toBeTruthy();
    expect(screen.getByText("spans 2 engine builds")).toBeTruthy();
    expect(screen.getByText("stale")).toBeTruthy();
  });

  it("omits the span note when the session ran under one build", async () => {
    renderDetail();
    expect(await screen.findByText("wf-1")).toBeTruthy();
    expect(screen.queryByText(/spans \d+ engine build/)).toBeNull();
  });

  it("shows the override reason on an override session", async () => {
    renderDetail({
      graduation: { graduation: "override", reason: "low-vol week", at: "2026-10-01T00:00:00.000Z" },
    });
    expect(await screen.findByText("low-vol week")).toBeTruthy();
    expect(screen.getByText("uncertified")).toBeTruthy();
  });

  it("shows the fixture note on a fixture-certified session", async () => {
    renderDetail({ fixture: true });
    expect(await screen.findByText(/Seeded certify-fixture strategy/)).toBeTruthy();
    expect(screen.getByText("fixture")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// The equity placeholder (spec §4's "Out of this item")
// ---------------------------------------------------------------------------

describe("the equity placeholder", () => {
  it("renders in the chart's place, with no digits and no currency symbol", async () => {
    renderDetail();
    const placeholder = await screen.findByText(/carries no equity series/);
    expect(placeholder.textContent ?? "").not.toMatch(/[0-9$€£¥]/);
  });
});

// ---------------------------------------------------------------------------
// The shadow panel (AC-3: identical + drift)
// ---------------------------------------------------------------------------

describe("the shadow panel", () => {
  it("shows 'identical over N bars' only when the shadow_checked frame carried N", async () => {
    renderDetail({ shadow_checks: [identical(4)] });
    expect(await screen.findByText(/Shadow backtest: identical/)).toBeTruthy();
    expect(screen.queryByText(/over \d+ bars/)).toBeNull();

    act(() => {
      streamChannel?.onmessage?.(frame(1, "shadow_checked", { bar_count: 96, result: {} }));
    });

    expect(await screen.findByText("Shadow backtest: identical over 96 bars")).toBeTruthy();
  });

  it("shows the drift's own sentence, both sides, and the defect-report copy", async () => {
    renderDetail({ shadow_checks: [drifted()] });
    expect(await screen.findByText("closed-trade 3 differs")).toBeTruthy();
    expect(screen.getByText(/64102\.5/)).toBeTruthy();
    expect(screen.getByText(/64388\.0/)).toBeTruthy();
    expect(screen.getByRole("button", { name: /Copy defect report/ })).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// The OOS panel (AC-3: all five verdicts)
// ---------------------------------------------------------------------------

describe("the OOS panel", () => {
  it("renders the server's not-applicable text verbatim", async () => {
    const reason = "OOS comparison: n/a, certified on fixture data";
    renderDetail({
      comparison: {
        status: "not_applicable",
        reason,
        engine_builds: 1,
        certification_stale: false,
      },
    });
    expect(await screen.findByText(reason)).toBeTruthy();
  });

  it("renders pending with the server's own of", async () => {
    renderDetail();
    expect(await screen.findByText("Pending: 4 of 20 trades")).toBeTruthy();
  });

  it("renders within, below and above with the fold range", async () => {
    for (const [status, word] of [
      ["within", "within"],
      ["below", "below"],
      ["above", "above"],
    ] as const) {
      const { unmount } = renderDetail({
        comparison: {
          status,
          live_mean_r: "0.34",
          n: 20,
          fold_min: "0.18",
          fold_max: "0.51",
          engine_builds: 1,
          certification_stale: false,
        },
      });
      expect(await screen.findByText(new RegExp(`Expectancy .* ${word} the OOS range`))).toBeTruthy();
      expect(screen.getByText("0.18")).toBeTruthy();
      expect(screen.getByText("0.51")).toBeTruthy();
      unmount();
    }
  });
});

// ---------------------------------------------------------------------------
// Stopped is read-only (A4)
// ---------------------------------------------------------------------------

describe("a stopped session", () => {
  it("renders the stop actor and offers no stop and no shadow check", async () => {
    renderDetail({
      status: { state: "stopped", stopped_by: { token: { label: "operator-token" } } },
    });
    expect(await screen.findByText(/Stopped by operator-token/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /^Stop session$/ })).toBeNull();
    expect(screen.queryByRole("button", { name: /Run shadow check/ })).toBeNull();
    expect(screen.getByText(/Read-only/)).toBeTruthy();
  });

  it("stops a running session behind the inline confirm", async () => {
    renderDetail();
    const stop = await screen.findByRole("button", { name: /^Stop session$/ });
    paperStopMock.mockResolvedValue(
      ok({ session_id: "s-1", stopped_without_shadow: false }),
    );
    fireEvent.click(stop);
    expect(screen.getByText("Stop this session? It stays listed, read-only.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: /Stop session/ }));
    await waitFor(() => expect(paperStopMock).toHaveBeenCalledWith("s-1"));
  });
});

// ---------------------------------------------------------------------------
// The trade log
// ---------------------------------------------------------------------------

describe("the trade log", () => {
  it("renders the closed trades with their R", async () => {
    renderDetail();
    const log = await screen.findByRole("region", { name: /Trade log/ });
    expect(within(log).getByText("0.61")).toBeTruthy();
    expect(within(log).getByText(/take_profit/)).toBeTruthy();
  });
});
