// Rendered tests for the Deployment Dashboard (r3.s4.w5, spec §4; AC-3).
//
// `../bindings` is mocked (the LibraryScreen precedent). The cards come from
// `paper_sessions()`, the drift alert from the latest live-epoch shadow check,
// and the kill switch behind a confirm that reports every failure with its
// code — never a bare success.

import { fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";

const paperSessionsMock = vi.fn();
const paperStopAllMock = vi.fn();
const paperStopMock = vi.fn();
const paperSessionMock = vi.fn();
const paperSessionTradesMock = vi.fn();
const paperSessionEventsMock = vi.fn();
const paperShadowCheckMock = vi.fn();
const serverStatusMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    paperSessions: (...args: unknown[]) => paperSessionsMock(...args),
    paperStopAll: (...args: unknown[]) => paperStopAllMock(...args),
    paperStop: (...args: unknown[]) => paperStopMock(...args),
    paperSession: (...args: unknown[]) => paperSessionMock(...args),
    paperSessionTrades: (...args: unknown[]) => paperSessionTradesMock(...args),
    paperSessionEvents: (...args: unknown[]) => paperSessionEventsMock(...args),
    paperShadowCheck: (...args: unknown[]) => paperShadowCheckMock(...args),
    serverStatus: (...args: unknown[]) => serverStatusMock(...args),
  },
}));

import type { PaperSessionSummary, PaperShadowCheck } from "../bindings";
import DeploymentDashboard from "./DeploymentDashboard";

const ENGINE = "engine-current-0123456789abcdef";

function identical(): PaperShadowCheck {
  return {
    engine_fingerprint: ENGINE,
    result: { verdict: "identical", closed_trades: 4, open_position: false },
  };
}

function drifted(): PaperShadowCheck {
  return {
    engine_fingerprint: ENGINE,
    result: {
      verdict: "drift",
      first_divergence: "closed-trade 3 differs",
      live: '{"exit_price":"64102.5"}',
      shadow: '{"exit_price":"64388.0"}',
    },
  };
}

function session(over: Partial<PaperSessionSummary> = {}): PaperSessionSummary {
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
    shadow_checks: [identical()],
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

function ok<T>(data: T) {
  return { status: "ok" as const, data };
}

/** The shell's details track, which the dashboard portals into. */
function detailsHost(): HTMLElement {
  let host = document.getElementById("details-pane");
  if (host === null) {
    host = document.createElement("aside");
    host.id = "details-pane";
    document.body.appendChild(host);
  }
  return host;
}

beforeEach(() => {
  vi.clearAllMocks();
  detailsHost();
  serverStatusMock.mockResolvedValue(
    ok({ state: "up", binary_version: "0.3.0", engine_fingerprint: ENGINE, reason: null }),
  );
  paperSessionMock.mockImplementation((id: string) =>
    Promise.resolve(ok(session({ id }))),
  );
  paperSessionTradesMock.mockResolvedValue(ok({ closed_trades: [], open_position: null }));
  paperSessionEventsMock.mockImplementation(() => new Promise(() => {}));
  paperShadowCheckMock.mockResolvedValue(
    ok({ verdict: "identical", closed_trades: 0, open_position: false }),
  );
});

// ---------------------------------------------------------------------------
// The cards
// ---------------------------------------------------------------------------

describe("the session cards", () => {
  it("renders one card per session with its badges, pair, timeframes and trades", async () => {
    paperSessionsMock.mockResolvedValue(
      ok([
        session(),
        session({
          id: "s-2",
          pair: "ETHUSDT",
          primary_timeframe: "1h",
          htf_timeframe: null,
          closed_trade_count: 12,
          status: { state: "stopped", stopped_by: { token: { label: "operator" } } },
        }),
      ]),
    );
    render(<DeploymentDashboard />);

    expect(await screen.findByText("BTCUSDT · 15m · 4h")).toBeTruthy();
    expect(screen.getByText("ETHUSDT · 1h")).toBeTruthy();
    expect(screen.getByText("12 closed trades")).toBeTruthy();
    expect(screen.getAllByText("certified").length).toBeGreaterThan(0);
    expect(screen.getByText("stopped")).toBeTruthy();
  });

  it("shows the drift alert and badge on a drifted live epoch", async () => {
    paperSessionsMock.mockResolvedValue(ok([session({ shadow_checks: [identical(), drifted()] })]));
    render(<DeploymentDashboard />);

    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(screen.getByText("closed-trade 3 differs")).toBeTruthy();
    expect(screen.getByText("drift")).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// The kill switch (A7)
// ---------------------------------------------------------------------------

describe("stop all", () => {
  it("confirms with the count, then lists every failure with its code", async () => {
    paperSessionsMock.mockResolvedValue(
      ok([session(), session({ id: "s-2", pair: "ETHUSDT" })]),
    );
    paperStopAllMock.mockResolvedValue(
      ok({
        stopped: ["s-1"],
        failures: [{ id: "s-2", code: "data" }],
      }),
    );
    render(<DeploymentDashboard />);
    expect(await screen.findByText("BTCUSDT · 15m · 4h")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /Stop all/ }));
    expect(await screen.findByText(/Stop all 2 paper sessions/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: /Stop all sessions/ }));
    await waitFor(() => expect(paperStopAllMock).toHaveBeenCalledTimes(1));

    const report = await screen.findByRole("status", { name: /stop-all result/i });
    expect(within(report).getByText(/s-1/)).toBeTruthy();
    expect(within(report).getByText(/s-2/)).toBeTruthy();
    expect(within(report).getByText(/data/)).toBeTruthy();
  });

  it("never claims success when a session failed", async () => {
    paperSessionsMock.mockResolvedValue(ok([session()]));
    paperStopAllMock.mockResolvedValue(
      ok({ stopped: [], failures: [{ id: "s-1", code: "data" }] }),
    );
    render(<DeploymentDashboard />);
    await screen.findByText("BTCUSDT · 15m · 4h");

    fireEvent.click(screen.getByRole("button", { name: /Stop all/ }));
    fireEvent.click(await screen.findByRole("button", { name: /Stop all sessions/ }));

    const report = await screen.findByRole("status", { name: /stop-all result/i });
    expect(within(report).queryByText(/^stopped:/i)).toBeNull();
    expect(within(report).getByText(/1 failed/)).toBeTruthy();
  });
});

// ---------------------------------------------------------------------------
// The detail track
// ---------------------------------------------------------------------------

describe("the session detail", () => {
  it("opens the selected session in the shell's details track", async () => {
    paperSessionsMock.mockResolvedValue(ok([session()]));
    render(<DeploymentDashboard />);
    fireEvent.click(await screen.findByRole("button", { name: /Details/ }));

    const pane = within(detailsHost());
    expect(await pane.findByText("wf-1")).toBeTruthy();
  });
});
