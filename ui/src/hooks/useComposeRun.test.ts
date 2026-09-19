// Focused unit tests over the compose-run state machine (r1.s1.w4, dispatch 2).
//
// The rendered tests in `DesignerScreen.test.tsx` exercise this reduction
// through the whole screen; these drive `useComposeRun` directly so the
// event-to-step fold is asserted at the state it actually owns — including the
// repeated-tool-name case the rendered layer cannot isolate (two `add_filter`
// calls in one run must each keep their own outcome).
//
// Same discipline as the screen tests: `../bindings` is mocked so no IPC
// bridge is needed, events arrive through the REAL `Channel` the hook
// constructed, and the pending invoke resolves under the test's control.

import { act, renderHook } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Channel } from "@tauri-apps/api/core";

import type { BusEvent } from "../bindings";

const composeStrategyMock = vi.fn();
const composeCancelMock = vi.fn();

vi.mock("../bindings", () => ({
  commands: {
    composeStrategy: (...args: unknown[]) => composeStrategyMock(...args),
    composeCancel: (...args: unknown[]) => composeCancelMock(...args),
  },
}));

import { useComposeRun } from "./useComposeRun";

/** The hook's current snapshot, for helper signatures. */
type Hook = ReturnType<typeof useComposeRun>;

/** One streamed event, at `seq`, on the run under test. */
function event(seq: number, payload: BusEvent["payload"]): BusEvent {
  return { runId: "run-1", seq, payload };
}

/** The same, on a DIFFERENT run — the straggler case. */
function otherRunEvent(seq: number, payload: BusEvent["payload"]): BusEvent {
  return { runId: "run-2", seq, payload };
}

/** A `toolCallStarted` payload, the only event shape these tests need to vary. */
function started(name: string, argumentsPreview: string): BusEvent["payload"] {
  return { kind: "toolCallStarted", name, argumentsPreview };
}

/** A finalize return value carrying the fields the summary state keeps. */
function finalizeResult() {
  return {
    runId: "run-1",
    emitted: 3,
    cancelled: false,
    strategy: {
      strategyId: "strat-1",
      strategyName: "RSI Oversold",
      versionId: "ver-1",
      createdBy: "composer_llm",
      llmCallCount: 6,
      dsl: {
        direction: "long",
        entry: "rsi(14) < 30",
        filters: ["close > ema(200)"],
        exits: ["stop 5%"],
        risk: ["risk 1% per trade"],
      },
    },
  };
}

/** Type a target and send it, as the screen's input area does. */
function submitTarget(result: { current: Hook }, target: string) {
  act(() => {
    result.current.setTarget(target);
  });
  act(() => {
    result.current.submit();
  });
}

/** The newest agent turn — the run in flight. */
function lastTurn(result: { current: Hook }) {
  const last = result.current.messages[result.current.messages.length - 1];
  if (last === undefined || last.kind !== "agent") {
    throw new Error("no agent turn is open");
  }
  return last.turn;
}

// The pending invoke's resolver, captured per test by the mock implementation.
let resolveInvoke: (value: unknown) => void = () => {};

beforeEach(() => {
  composeStrategyMock.mockReset();
  composeCancelMock.mockReset();
  composeCancelMock.mockResolvedValue({ status: "ok", data: true });
  composeStrategyMock.mockImplementation(
    () =>
      new Promise((resolve) => {
        resolveInvoke = resolve;
      }),
  );
});

describe("useComposeRun", () => {
  it("submitting a target invokes the command with it and opens a streaming turn", () => {
    const { result } = renderHook(() => useComposeRun());
    expect(result.current.messages).toHaveLength(0);

    submitTarget(result, "RSI oversold bounce on BTC");

    expect(composeStrategyMock).toHaveBeenCalledTimes(1);
    expect(composeStrategyMock.mock.calls[0][0]).toBe("RSI oversold bounce on BTC");
    // The input is cleared for the next message; the run is the busy state.
    expect(result.current.target).toBe("");
    expect(result.current.running).toBe(true);
    expect(result.current.messages[0]).toMatchObject({
      kind: "user",
      text: "RSI oversold bounce on BTC",
    });
    expect(result.current.messages[1]).toMatchObject({
      kind: "agent",
      turn: { status: "streaming", steps: [] },
    });
  });

  it("submitting while a run streams is a no-op — one run at a time", () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "first");
    submitTarget(result, "second");

    expect(composeStrategyMock).toHaveBeenCalledTimes(1);
    // Only the first run's user bubble + agent turn exist.
    expect(result.current.messages).toHaveLength(2);
  });

  it("folds streamed events into the running turn one by one", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    // First step opens the moment its ToolCallStarted arrives.
    await act(async () => {
      channel.onmessage?.(
        event(1, {
          kind: "toolCallStarted",
          name: "add_entry_signal",
          argumentsPreview: "rsi(14) < 30",
        }),
      );
    });
    expect(lastTurn(result).steps).toEqual([
      { name: "add_entry_signal", preview: "rsi(14) < 30", outcome: undefined },
    ]);

    // Its result closes the same step.
    await act(async () => {
      channel.onmessage?.(
        event(2, { kind: "toolCallResult", name: "add_entry_signal", outcome: "entry signal added" }),
      );
    });
    expect(lastTurn(result).steps[0]?.outcome).toBe("entry signal added");

    // A repeated tool name opens a SECOND step of that name; each result must
    // attach to the LAST open one, leaving the earlier step untouched.
    await act(async () => {
      channel.onmessage?.(
        event(3, { kind: "toolCallStarted", name: "add_filter", argumentsPreview: "close > ema(200)" }),
      );
    });
    await act(async () => {
      channel.onmessage?.(
        event(4, { kind: "toolCallStarted", name: "add_filter", argumentsPreview: "volume > ma(20)" }),
      );
    });
    expect(lastTurn(result).steps.map((step) => step.preview)).toEqual([
      "rsi(14) < 30",
      "close > ema(200)",
      "volume > ma(20)",
    ]);

    await act(async () => {
      channel.onmessage?.(
        event(5, { kind: "toolCallResult", name: "add_filter", outcome: "filter added (volume)" }),
      );
    });
    expect(lastTurn(result).steps[2]?.outcome).toBe("filter added (volume)");
    expect(lastTurn(result).steps[1]?.outcome).toBeUndefined();

    await act(async () => {
      channel.onmessage?.(
        event(6, { kind: "toolCallResult", name: "add_filter", outcome: "filter added (trend)" }),
      );
    });
    expect(lastTurn(result).steps[1]?.outcome).toBe("filter added (trend)");
  });

  it("a finalized run keeps the returned summary and clears the running state", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    await act(async () => {
      channel.onmessage?.(
        event(1, {
          kind: "toolCallStarted",
          name: "add_entry_signal",
          argumentsPreview: "rsi(14) < 30",
        }),
      );
    });
    await act(async () => {
      resolveInvoke({ status: "ok", data: finalizeResult() });
    });

    const turn = lastTurn(result);
    expect(turn.status).toBe("finalized");
    expect(turn.summary?.versionId).toBe("ver-1");
    expect(result.current.running).toBe(false);
  });

  it("a cancelled run ends cancelled with no summary — a normal ending, not an error", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");

    await act(async () => {
      resolveInvoke({ status: "ok", data: { runId: "run-1", emitted: 1, cancelled: true, strategy: null } });
    });

    const turn = lastTurn(result);
    expect(turn.status).toBe("cancelled");
    expect(turn.summary).toBeUndefined();
    expect(turn.error).toBeUndefined();
    expect(result.current.running).toBe(false);
  });

  it("a rejected command records its message on the turn — no silent failure", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");

    await act(async () => {
      resolveInvoke({
        status: "error",
        error: {
          code: "llm",
          message: "no usable LLM credential found (searched: env, config dir, .env, app data dir)",
        },
      });
    });

    const turn = lastTurn(result);
    expect(turn.status).toBe("error");
    expect(turn.error).toContain("no usable LLM credential found");
    expect(result.current.running).toBe(false);
  });

  // -------------------------------------------------------------------------
  // Stream identity and lifecycle. The channel alone does not correlate a run
  // with its turn: a straggler can outlive its run, an event can go missing,
  // and unmounting used to leave the backend composing into a channel nobody
  // read. Each test below pins one of those.
  // -------------------------------------------------------------------------

  it("binds the run on its first event and drops events from another run", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    // The real stream opens with `Started` at seq 0; the run binds `run-1` here.
    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      channel.onmessage?.(event(1, started("add_entry_signal", "rsi(14) < 30")));
    });
    expect(lastTurn(result).steps).toHaveLength(1);

    // A late event from a DIFFERENT run must not append a step to this one.
    await act(async () => {
      channel.onmessage?.(otherRunEvent(2, started("add_filter", "close > ema(200)")));
    });
    expect(lastTurn(result).steps).toHaveLength(1);
    expect(lastTurn(result).steps[0]?.preview).toBe("rsi(14) < 30");
    expect(lastTurn(result).status).toBe("streaming");
  });

  it("a gap in seq ends the run as a stream error rather than rendering a partial list", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      channel.onmessage?.(event(1, started("add_entry_signal", "rsi(14) < 30")));
    });
    // seq 2 never arrives.
    await act(async () => {
      channel.onmessage?.(event(3, started("add_filter", "close > ema(200)")));
    });

    const turn = lastTurn(result);
    expect(turn.status).toBe("error");
    expect(turn.error).toContain("expected event 2, received 3");
    // The event that exposed the gap is not folded in — the list stops where
    // it stopped being trustworthy.
    expect(turn.steps).toHaveLength(1);
  });

  it("a broken stream keeps its error even when the command returns success", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      channel.onmessage?.(event(5, started("add_filter", "close > ema(200)")));
    });
    await act(async () => {
      resolveInvoke({ status: "ok", data: finalizeResult() });
    });

    const turn = lastTurn(result);
    expect(turn.status).toBe("error");
    expect(turn.error).toContain("expected event 1, received 5");
    expect(turn.summary).toBeUndefined();
    expect(result.current.running).toBe(false);
  });

  it("unmounting mid-run cancels the backend run by id", async () => {
    const { result, unmount } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    // The run id is only knowable from an event; `Started` carries it.
    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });

    unmount();

    expect(composeCancelMock).toHaveBeenCalledTimes(1);
    expect(composeCancelMock.mock.calls[0][0]).toBe("run-1");
  });

  it("unmounting BEFORE the first event still cancels, once the id arrives", async () => {
    // The id is minted backend-side and first observable on `Started`, so leaving
    // straight after submitting beats it. The cleanup has nothing to name yet; the
    // cancel has to fire when the id finally arrives, or the run bills on.
    const { result, unmount } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    unmount();
    expect(composeCancelMock).not.toHaveBeenCalled();

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });

    expect(composeCancelMock).toHaveBeenCalledTimes(1);
    expect(composeCancelMock.mock.calls[0][0]).toBe("run-1");
  });

  it("a straggler from a settled run cannot capture the next run", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "first");
    const channelA = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;
    await act(async () => {
      channelA.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      resolveInvoke({ status: "ok", data: finalizeResult() });
    });

    // Run B starts; A's delayed event lands BEFORE B's first. Reading the current
    // record here would bind B to A's runId and then reject every real B event.
    submitTarget(result, "second");
    const channelB = composeStrategyMock.mock.calls[1][1] as Channel<BusEvent>;
    await act(async () => {
      channelA.onmessage?.(event(9, started("add_filter", "stale")));
    });
    await act(async () => {
      channelB.onmessage?.(otherRunEvent(0, { kind: "started" }));
    });
    await act(async () => {
      channelB.onmessage?.(otherRunEvent(1, started("add_entry_signal", "rsi(14) < 30")));
    });

    const turn = lastTurn(result);
    expect(turn.status).toBe("streaming");
    expect(turn.steps).toHaveLength(1);
    expect(turn.steps[0]?.preview).toBe("rsi(14) < 30");
  });

  it("a first event past the tolerated baseline is a gap, not a new baseline", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    // seq 2 first: `Started` AND a `toolCallStarted` were dropped. Taking 2 as the
    // baseline would silently swallow that prefix and let the run render complete.
    await act(async () => {
      channel.onmessage?.(
        event(2, { kind: "toolCallResult", name: "add_filter", outcome: "filter added" }),
      );
    });

    const turn = lastTurn(result);
    expect(turn.status).toBe("error");
    expect(turn.error).toContain("received 2");
  });

  it("declaring a stream gap also cancels the backend run", async () => {
    const { result } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      channel.onmessage?.(event(4, started("add_filter", "close > ema(200)")));
    });

    // Without this the user sees only a stream error, retries, and the abandoned
    // run persists a duplicate strategy behind their back.
    expect(lastTurn(result).status).toBe("error");
    expect(composeCancelMock).toHaveBeenCalledTimes(1);
    expect(composeCancelMock.mock.calls[0][0]).toBe("run-1");
  });

  it("unmounting with no run in flight cancels nothing", () => {
    const { unmount } = renderHook(() => useComposeRun());
    unmount();
    expect(composeCancelMock).not.toHaveBeenCalled();
  });

  it("unmounting after a run finished cancels nothing", async () => {
    const { result, unmount } = renderHook(() => useComposeRun());
    submitTarget(result, "t");
    const channel = composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      resolveInvoke({ status: "ok", data: finalizeResult() });
    });

    unmount();

    expect(composeCancelMock).not.toHaveBeenCalled();
  });
});
