// Focused tests over the paper-session stream hook (r3.s4.w5, spec §4; AC-3).
//
// The rendered tests in `SessionDetail.test.tsx` exercise this reduction
// through the whole screen; these drive `usePaperSession` directly so the
// frame fold is asserted at the state it actually owns — including the
// duplicate-seq drop and the unmount cancellation the rendered layer cannot
// isolate. `../bindings` is mocked; frames arrive through the REAL `Channel`
// the hook constructed (the `useComposeRun.test.ts` precedent).

import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Channel } from "@tauri-apps/api/core";

const paperSessionMock = vi.fn();
const paperSessionEventsMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    paperSession: (...args: unknown[]) => paperSessionMock(...args),
    paperSessionEvents: (...args: unknown[]) => paperSessionEventsMock(...args),
  },
}));

import type { PaperSessionSummary, PaperStreamEvent } from "../bindings";
import { usePaperSession } from "./usePaperSession";

const ENGINE = "engine-current-0123456789abcdef";

function summary(): PaperSessionSummary {
  return {
    id: "s-1",
    strategy_version_id: "ver-1",
    pair: "BTCUSDT",
    primary_timeframe: "15m",
    htf_timeframe: null,
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
      status: "pending",
      n: 0,
      of: 20,
      engine_builds: 1,
      certification_stale: false,
    },
  };
}

function ok<T>(data: T) {
  return { status: "ok" as const, data };
}

function frame(seq: number, type: string, payload: object = {}): PaperStreamEvent {
  return { kind: "frame", seq, type, payload: JSON.stringify({ type, seq, ...payload }) };
}

/** The channel the hook handed to `paper_session_events`. */
let streamChannel: Channel<PaperStreamEvent> | null = null;

function push(event: PaperStreamEvent) {
  act(() => {
    streamChannel?.onmessage?.(event);
  });
}

beforeEach(() => {
  vi.clearAllMocks();
  streamChannel = null;
  paperSessionMock.mockResolvedValue(ok(summary()));
  paperSessionEventsMock.mockImplementation(
    (_id: string, _afterSeq: number | null, channel: Channel<PaperStreamEvent>) => {
      streamChannel = channel;
      return new Promise(() => {});
    },
  );
});

describe("usePaperSession", () => {
  it("reduces frames and ignores a duplicate seq", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());

    push(frame(1, "bar_processed"));
    await waitFor(() => expect(result.current.lastSeq).toBe(1));
    expect(result.current.lastType).toBe("bar_processed");

    push(frame(2, "fill"));
    await waitFor(() => expect(result.current.lastSeq).toBe(2));

    // The resumed stream's boundary replay: seq 2 again must change nothing.
    push(frame(2, "shadow_checked", { bar_count: 999 }));
    expect(result.current.lastSeq).toBe(2);
    expect(result.current.lastType).toBe("fill");
    expect(result.current.shadowBars).toBeNull();

    push(frame(3, "shadow_checked", { bar_count: 96 }));
    await waitFor(() => expect(result.current.shadowBars).toBe(96));
    expect(result.current.lastSeq).toBe(3);
  });

  it("re-fetches the summary on a processed bar", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());
    expect(paperSessionMock).toHaveBeenCalledTimes(1);

    // Every processed bar moves the mark and the running total (G4), so the
    // summary (and the trades that follow it) re-reads per bar.
    push(frame(1, "bar_processed"));
    await waitFor(() => expect(paperSessionMock).toHaveBeenCalledTimes(2));
  });

  it("re-fetches the summary on each event that moves it", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());
    expect(paperSessionMock).toHaveBeenCalledTimes(1);

    const types = ["fill", "funding", "shadow_checked", "stop", "engine_upgraded"];
    for (const [index, type] of types.entries()) {
      push(frame(index + 1, type));
      await waitFor(() =>
        expect(paperSessionMock).toHaveBeenCalledTimes(index + 2),
      );
    }

    // An `order` is a signal only: it folds, and re-fetches nothing.
    push(frame(types.length + 1, "order"));
    await waitFor(() => expect(result.current.lastSeq).toBe(types.length + 1));
    expect(paperSessionMock).toHaveBeenCalledTimes(types.length + 1);
  });

  it("collapses a replayed bar backlog into at most two summary reads", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    expect(paperSessionMock).toHaveBeenCalledTimes(1);

    // A long session's backlog: every replayed bar is a refetch trigger, and
    // all of them arrive while the mount read is still in flight.
    for (let n = 1; n <= 250; n += 1) push(frame(n, "bar_processed"));
    expect(paperSessionMock).toHaveBeenCalledTimes(1);

    // The in-flight read settles: the whole burst costs exactly ONE trailing
    // read, not one read per frame.
    await waitFor(() => expect(paperSessionMock).toHaveBeenCalledTimes(2));
    // Nothing arrives after it: the trailing read is the last one.
    await act(async () => {});
    expect(paperSessionMock).toHaveBeenCalledTimes(2);
    expect(result.current.summary).not.toBeNull();
  });

  it("drops an older session's read when it answers after the newer one", async () => {
    let oldDelivered = false;
    paperSessionMock
      .mockImplementationOnce(async () => {
        // Answers after the new session's read has already been applied.
        await Promise.resolve();
        await Promise.resolve();
        oldDelivered = true;
        return ok({ ...summary(), id: "s-1" });
      })
      .mockImplementationOnce(async () => ok({ ...summary(), id: "s-2" }));

    const { result, rerender } = renderHook(
      ({ id }: { id: string }) => usePaperSession(id),
      { initialProps: { id: "s-1" } },
    );
    expect(paperSessionMock).toHaveBeenCalledTimes(1);

    // The screen switches sessions while the first read is still pending.
    rerender({ id: "s-2" });
    await waitFor(() => expect(result.current.summary?.id).toBe("s-2"));

    // The previous session's read answers now — last — and must not win.
    await waitFor(() => expect(oldDelivered).toBe(true));
    await act(async () => {});
    expect(result.current.summary?.id).toBe("s-2");
  });

  it("drops the live-epoch bar count when a new epoch opens", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());

    push(frame(1, "shadow_checked", { bar_count: 96 }));
    await waitFor(() => expect(result.current.shadowBars).toBe(96));

    push(frame(2, "engine_upgraded"));
    await waitFor(() => expect(result.current.shadowBars).toBeNull());
  });

  it("handles TokenRefused as a terminal refusal", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());

    push({ kind: "tokenRefused", reason: "the presented token has been revoked" });
    await waitFor(() => expect(result.current.stream).toBe("refused"));
    expect(result.current.error).toMatch(/revoked/);

    // A terminal stream folds nothing further.
    push(frame(1, "fill"));
    expect(result.current.lastSeq).toBeNull();
  });

  it("cancels on unmount — no further fetches, no further folds", async () => {
    const { result, unmount } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());
    const fetches = paperSessionMock.mock.calls.length;

    unmount();
    push(frame(1, "shadow_checked", { bar_count: 96 }));
    push(frame(2, "stop"));

    expect(paperSessionMock.mock.calls.length).toBe(fetches);
    expect(result.current.lastSeq).toBeNull();
  });

  it("runs the queued trailing refresh after a failed read, and clears the error on the next good one (#371)", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());
    expect(paperSessionMock).toHaveBeenCalledTimes(1);

    // Read 2 fails, and a frame arrives WHILE IT IS IN FLIGHT: the queued
    // trailing refresh must still run — with the old guard the failure dropped
    // it, so a stopped session needed a reload to recover.
    let failRead2!: (reason: unknown) => void;
    paperSessionMock.mockImplementationOnce(
      // The executor form, not `Promise.withResolvers`: this tree's `lib` is
      // ES2022 (tsconfig.json), and the typecheck stage is part of the gate.
      () =>
        new Promise((_resolve, reject) => {
          failRead2 = reject;
        }),
    );
    push(frame(1, "fill"));
    await waitFor(() => expect(paperSessionMock).toHaveBeenCalledTimes(2));
    push(frame(2, "stop"));
    await act(async () => {
      failRead2(new Error("the read failed"));
    });

    // The trailing read runs and is the one that recovers the view.
    await waitFor(() => expect(paperSessionMock).toHaveBeenCalledTimes(3));
    await waitFor(() => expect(result.current.error).toBeNull());
    expect(result.current.summary).not.toBeNull();
  });
});
