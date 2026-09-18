// Refetch-on-focus (r2.s1.w4 C2).
//
// The desktop's data can change while the app sits unfocused — `pulse mcp`
// writes versions and runs straight through the repositories, so nothing in
// the WebView learns of them until it asks again. The two DOM signals a
// WebView delivers on activation are `window`'s `focus` and `document`'s
// `visibilitychange` back to "visible"; both feed one throttled `refetch`.
//
// DOM events ONLY, by spec: no Tauri window-event API (so `capabilities/*.json`
// is unchanged) and no polling — the WebView stays silent until the OS says the
// trader is looking at it again.

import { useEffect, useRef } from "react";

/**
 * Call `refetch` when the app window regains focus or becomes visible again.
 *
 * Throttled to at most one call per `minIntervalMs` across BOTH signals —
 * activation often delivers focus and visibilitychange together, and a burst
 * must still be one read. `refetch` is invoked for its side effect; its return
 * value (a promise, for the async reads both screens pass) is deliberately not
 * awaited. The listeners are removed on unmount.
 */
export function useRefetchOnFocus(
  refetch: () => void | Promise<void>,
  minIntervalMs = 1000,
): void {
  // The throttle window lives in a ref: re-subscribing (a new `refetch`
  // identity) must not reset it, or a re-render storm would double as a
  // throttle bypass.
  const lastFired = useRef(0);

  useEffect(() => {
    const fire = () => {
      const now = Date.now();
      if (now - lastFired.current < minIntervalMs) {
        return;
      }
      lastFired.current = now;
      void refetch();
    };
    const onFocus = () => fire();
    const onVisibility = () => {
      if (document.visibilityState === "visible") {
        fire();
      }
    };

    window.addEventListener("focus", onFocus);
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      window.removeEventListener("focus", onFocus);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [refetch, minIntervalMs]);
}
