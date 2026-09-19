// Rendered tests over the Strategy Designer screen (r1.s1.w4, spec step 8).
//
// The bindings module is mocked (the same discipline as `App.test.tsx`) so the
// screen is driven without a live Tauri IPC bridge: `composeStrategy` resolves
// under the test's control and the events arrive through the REAL `Channel`
// object the screen constructed — fed one at a time, with an assertion after
// EACH event, because "steps appear one by one" is the streaming claim (`d1`'s
// observable) and asserting only the end state would prove nothing about it.

import { act, fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Channel } from "@tauri-apps/api/core";

import type { BusEvent } from "../bindings";

const composeStrategyMock = vi.fn();
// The screen's unmount cleanup cancels an in-flight run, so every test that
// leaves one open reaches this on teardown.
const composeCancelMock = vi.fn((..._args: unknown[]) =>
  Promise.resolve({ status: "ok", data: true }),
);

vi.mock("../bindings", () => ({
  commands: {
    composeStrategy: (...args: unknown[]) => composeStrategyMock(...args),
    composeCancel: (...args: unknown[]) => composeCancelMock(...args),
  },
}));

import DesignerScreen from "./DesignerScreen";
import { resolveRoute } from "../routes";

/** One streamed event, at `seq`, on the run under test. */
function event(seq: number, payload: BusEvent["payload"]): BusEvent {
  return { runId: "run-1", seq, payload };
}

/** A finalize return value carrying the fields the summary card renders. */
function finalizeResult() {
  return {
    runId: "run-1",
    emitted: 14,
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
        exits: ["stop 5%", "take profit 2R"],
        risk: ["risk 1% per trade", "max leverage 3x"],
      },
    },
  };
}

/** Type a target into the composer input and send it (Enter, as the hint promises). */
function submitTarget(target: string) {
  fireEvent.change(screen.getByRole("textbox"), { target: { value: target } });
  fireEvent.keyDown(screen.getByRole("textbox"), { key: "Enter", shiftKey: false });
}

/** The channel the screen handed to the command — the mock captured it. */
function capturedChannel(): Channel<BusEvent> {
  expect(composeStrategyMock).toHaveBeenCalledTimes(1);
  return composeStrategyMock.mock.calls[0][1] as Channel<BusEvent>;
}

// The pending invoke's resolver, captured per test by the mock implementation.
let resolveInvoke: (value: unknown) => void = () => {};

beforeEach(() => {
  composeStrategyMock.mockReset();
  composeCancelMock.mockClear();
  composeStrategyMock.mockImplementation(
    () =>
      new Promise((resolve) => {
        resolveInvoke = resolve;
      }),
  );
});

describe("<DesignerScreen /> streaming compose", () => {
  it("invokes the compose command with the submitted target", () => {
    render(<DesignerScreen />);
    submitTarget("RSI oversold bounce on BTC");

    expect(composeStrategyMock).toHaveBeenCalledTimes(1);
    expect(composeStrategyMock.mock.calls[0][0]).toBe("RSI oversold bounce on BTC");
    // The user's own words appear in the conversation, not just in the call.
    expect(screen.getByText("RSI oversold bounce on BTC")).toBeTruthy();
  });

  it("renders the composer's steps ONE BY ONE as events arrive, then the summary on finalize", async () => {
    render(<DesignerScreen />);
    submitTarget("RSI oversold bounce on BTC");
    const channel = capturedChannel();

    // Started: the run is open and streaming — before any step exists.
    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    expect(await screen.findByText(/streaming/i)).toBeTruthy();
    expect(screen.queryByText(/completed/i)).toBeNull();

    // First step opens: the tool name and its arguments preview are visible
    // the moment ToolCallStarted arrives — not batched at the end.
    await act(async () => {
      channel.onmessage?.(
        event(1, {
          kind: "toolCallStarted",
          name: "add_entry_signal",
          argumentsPreview: "rsi(14) < 30",
        }),
      );
    });
    expect(await screen.findByText(/add_entry_signal/)).toBeTruthy();
    expect(screen.getByText(/rsi\(14\) < 30/)).toBeTruthy();

    // The step's outcome appends to the SAME step when its result arrives.
    await act(async () => {
      channel.onmessage?.(
        event(2, {
          kind: "toolCallResult",
          name: "add_entry_signal",
          outcome: "entry signal added",
        }),
      );
    });
    expect(await screen.findByText(/entry signal added/)).toBeTruthy();

    // A second step appears beside the first — one by one is the claim.
    await act(async () => {
      channel.onmessage?.(
        event(3, {
          kind: "toolCallStarted",
          name: "add_filter",
          argumentsPreview: "close > ema(200)",
        }),
      );
    });
    expect(await screen.findByText(/add_filter/)).toBeTruthy();

    // ...and closes. A run's `seq` is contiguous, so this fixture carries every
    // beat the backend would actually send: skipping one is now a stream error,
    // which is the point of `useComposeRun`'s gap check.
    await act(async () => {
      channel.onmessage?.(
        event(4, { kind: "toolCallResult", name: "add_filter", outcome: "filter added" }),
      );
    });

    // Finished closes the stream; the summary card comes from the RETURN value.
    await act(async () => {
      channel.onmessage?.(event(5, { kind: "finished", message: "finalized: RSI Oversold" }));
    });
    await act(async () => {
      resolveInvoke({ status: "ok", data: finalizeResult() });
    });

    expect(await screen.findByText("RSI Oversold")).toBeTruthy();
    expect(screen.getByText(/ver-1/)).toBeTruthy();
    expect(screen.getByText(/composer_llm/)).toBeTruthy();
    expect(screen.getByText(/6 LLM calls/)).toBeTruthy();
    // The compact DSL summary renders the version's own lines. The filter line
    // legitimately appears TWICE now — as the step's preview and in the summary
    // card's Filters block — so assert presence, not uniqueness.
    expect(screen.getByText(/stop 5%/)).toBeTruthy();
    expect(screen.getAllByText(/close > ema\(200\)/).length).toBeGreaterThan(0);
    // The run is marked complete once its outcome is in.
    expect(screen.getByText(/completed/i)).toBeTruthy();
  });

  it("renders a rejected command's error message in the conversation — no silent failure", async () => {
    render(<DesignerScreen />);
    submitTarget("RSI oversold bounce on BTC");
    const channel = capturedChannel();

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      resolveInvoke({
        status: "error",
        error: {
          code: "llm",
          message: "no usable LLM credential found (searched: env, config dir, .env, app data dir)",
        },
      });
    });

    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("no usable LLM credential found");
    // The failed run stays visible in the conversation, not swallowed.
    expect(screen.getByText(/add_entry|composer/i)).toBeTruthy();
  });

  it("disables the send control honestly while a run is streaming", async () => {
    render(<DesignerScreen />);
    submitTarget("RSI oversold bounce on BTC");
    const channel = capturedChannel();

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });

    const send = await screen.findByRole("button", { name: /running/i });
    expect((send as HTMLButtonElement).disabled).toBe(true);

    // The run finishing re-enables it.
    await act(async () => {
      channel.onmessage?.(event(1, { kind: "finished", message: "done" }));
    });
    await act(async () => {
      resolveInvoke({ status: "ok", data: finalizeResult() });
    });
    const sendAfter = await screen.findByRole("button", { name: /send/i });
    expect((sendAfter as HTMLButtonElement).disabled).toBe(false);
  });

  it("ignores Enter while an IME is composing", () => {
    render(<DesignerScreen />);
    const input = screen.getByRole("textbox");
    fireEvent.change(input, { target: { value: "移動平均" } });

    // The Enter that CONFIRMS an IME candidate reaches this handler too. Acting
    // on it would clear the textarea and start a billable run on partial text.
    fireEvent.keyDown(input, { key: "Enter", shiftKey: false, isComposing: true });
    expect(composeStrategyMock).not.toHaveBeenCalled();
    expect((input as HTMLTextAreaElement).value).toBe("移動平均");

    // The Enter that follows, once composition has ended, does submit.
    fireEvent.keyDown(input, { key: "Enter", shiftKey: false });
    expect(composeStrategyMock).toHaveBeenCalledTimes(1);
  });

  it("names each terminal state rather than calling every ended run completed", async () => {
    render(<DesignerScreen />);
    submitTarget("RSI oversold bounce on BTC");
    const channel = capturedChannel();

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
    await act(async () => {
      channel.onmessage?.(
        event(1, {
          kind: "toolCallStarted",
          name: "add_entry_signal",
          argumentsPreview: "rsi(14) < 30",
        }),
      );
    });
    expect(screen.getByText(/streaming/)).toBeTruthy();

    // A run that emitted steps and then FAILED used to render "✓ completed"
    // directly above its own error box, because every non-streaming status took
    // the success arm of a two-way branch.
    await act(async () => {
      resolveInvoke({
        status: "error",
        error: { code: "llm", message: "upstream returned 503" },
      });
    });

    expect(screen.queryByText(/completed/)).toBeNull();
    expect(screen.getByText(/failed after/)).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toContain("upstream returned 503");
  });

  it("names a cancelled run cancelled, not completed", async () => {
    render(<DesignerScreen />);
    submitTarget("RSI oversold bounce on BTC");
    const channel = capturedChannel();

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });
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
      resolveInvoke({
        status: "ok",
        data: { runId: "run-1", emitted: 2, cancelled: true, strategy: null },
      });
    });

    expect(screen.queryByText(/completed/)).toBeNull();
    expect(screen.getByText(/cancelled after/)).toBeTruthy();
  });

  it("cancels the backend run when the screen unmounts mid-compose", async () => {
    const { unmount } = render(<DesignerScreen />);
    submitTarget("RSI oversold bounce on BTC");
    const channel = capturedChannel();

    await act(async () => {
      channel.onmessage?.(event(0, { kind: "started" }));
    });

    unmount();

    expect(composeCancelMock).toHaveBeenCalledTimes(1);
    expect(composeCancelMock.mock.calls[0][0]).toBe("run-1");
  });
});

describe("the designer route entry", () => {
  it("mounts the screen from ROUTES at /designer", () => {
    const route = resolveRoute("/designer");
    expect(route).toBeDefined();
    expect(route?.element).toBeDefined();
    const Element = route?.element;
    if (Element === undefined) {
      throw new Error("the designer route declares no element");
    }
    render(<Element />);
    expect(screen.getByRole("textbox")).toBeTruthy();
    expect(screen.getByPlaceholderText(/describe a strategy/i)).toBeTruthy();
  });
});
