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

  it("re-fetches the summary on fill, funding, shadow_checked, stop and engine_upgraded", async () => {
    const { result } = renderHook(() => usePaperSession("s-1"));
    await waitFor(() => expect(result.current.summary).not.toBeNull());
    expect(paperSessionMock).toHaveBeenCalledTimes(1);

    const types = [
      "bar_processed",
      "order",
      "fill",
      "funding",
      "shadow_checked",
      "stop",
      "engine_upgraded",
    ];
    types.forEach((type, index) => push(frame(index + 1, type)));
    // bar_processed and order change no summary field: five re-fetches.
    await waitFor(() => expect(paperSessionMock).toHaveBeenCalledTimes(6));
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
});
