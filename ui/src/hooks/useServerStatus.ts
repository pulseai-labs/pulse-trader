// The server connection status, polled (r3.s3.w5).
//
// The spec's d28: the status strip polls `server_status` every 15 s, so a
// server restart shows `down`, then `up`, without relaunching the app — the
// poll interval here is the strip's data source, and the Connect gate's
// trigger (a state that is not `up` renders the Connect screen).

import { useCallback, useEffect, useState } from "react";

import { commands } from "../bindings";
import type { ServerStatus } from "../bindings";

const POLL_MS = 15_000;

/**
 * The current `ServerStatus`, refreshed on a 15 s cadence, plus `refresh` for
 * the moments a poll would answer late — a successful Connect calls it so the
 * gate lifts immediately. `status` is `null` until the first read resolves;
 * callers render nothing rather than guess.
 */
export function useServerStatus(): {
  status: ServerStatus | null;
  refresh: () => void;
} {
  const [status, setStatus] = useState<ServerStatus | null>(null);

  const refresh = useCallback(() => {
    commands
      .serverStatus()
      .then((result) => {
        // The bus's `Result` shell (`typedError`): an `error` arm means the
        // call itself failed — keep the last known status and let the poll
        // retry, rather than rendering a state nobody reported.
        if (result.status === "ok") {
          setStatus(result.data);
        }
      })
      .catch(() => {
        // A failed status read is a DOWN server as far as the UI can know —
        // but fabricating that state would lie about what happened, so the
        // read simply stays at its last value; the next poll retries.
      });
  }, []);

  useEffect(() => {
    refresh();
    const timer = window.setInterval(refresh, POLL_MS);
    return () => window.clearInterval(timer);
  }, [refresh]);

  return { status, refresh };
}
