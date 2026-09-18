// Focused unit tests for the C2 hook (r2.s1.w4).
//
// The desktop refetches on the two DOM signals a WebView actually delivers —
// `window` `focus` and `document` `visibilitychange` to "visible" — throttled to
// one call per interval and unsubscribed on unmount. DOM events only: no Tauri
// window-event API is involved, so the tests drive plain DOM events and assert
// the callback's call count, nothing else.

import { act, renderHook } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";

import { useRefetchOnFocus } from "./useRefetchOnFocus";

/** jsdom owns no visibility lifecycle — the tests pin the state they mean. */
function setVisibility(state: DocumentVisibilityState) {
  Object.defineProperty(document, "visibilityState", { value: state, configurable: true });
}

afterEach(() => {
  setVisibility("visible");
});

describe("useRefetchOnFocus (r2.s1.w4 C2)", () => {
  it("calls refetch when the window regains focus", () => {
    const refetch = vi.fn();
    renderHook(() => useRefetchOnFocus(refetch));

    act(() => {
      window.dispatchEvent(new Event("focus"));
    });

    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it("calls refetch when the document becomes visible, and not while hidden", () => {
    const refetch = vi.fn();
    renderHook(() => useRefetchOnFocus(refetch));

    setVisibility("hidden");
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(refetch).not.toHaveBeenCalled();

    setVisibility("visible");
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it("throttles to one call per interval across both signals", () => {
    vi.useFakeTimers();
    try {
      const refetch = vi.fn();
      renderHook(() => useRefetchOnFocus(refetch, 1000));

      act(() => {
        window.dispatchEvent(new Event("focus"));
        setVisibility("visible");
        document.dispatchEvent(new Event("visibilitychange"));
      });
      expect(refetch).toHaveBeenCalledTimes(1);

      act(() => {
        vi.advanceTimersByTime(1001);
        window.dispatchEvent(new Event("focus"));
      });
      expect(refetch).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("unsubscribes on unmount — neither signal reaches the callback afterwards", () => {
    const refetch = vi.fn();
    const { unmount } = renderHook(() => useRefetchOnFocus(refetch));
    unmount();

    act(() => {
      window.dispatchEvent(new Event("focus"));
      setVisibility("visible");
      document.dispatchEvent(new Event("visibilitychange"));
    });

    expect(refetch).not.toHaveBeenCalled();
  });
});
