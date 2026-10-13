// The server connection status, polled (r3.s3.w5).
//
// The spec's d28: the status strip polls `server_status` every 15 s, so a
// server restart shows `down`, then `up`, without relaunching the app — the
// poll interval here is the strip's data source, and the Connect gate's
// trigger (a state that is not `up` renders the Connect screen).

import { useCallback, useEffect, useRef, useState } from "react";

import { commands } from "../bindings";
import type { ServerStatus } from "../bindings";

const POLL_MS = 15_000;

/**
 * The current `ServerStatus`, refreshed on a 15 s cadence, plus `refresh` for
 * the moments a poll would answer late — a successful Connect calls it so the
 * gate lifts immediately. `status` is `null` until the first read resolves;
 * callers render nothing rather than guess.
 *
 * `lastGoodAt` is the epoch-ms instant of the last poll that answered `up`
 * (r4.s5.w4, #338): the paper surfaces render `last seen <state> · <time> UTC`
 * from it while the status is anything but `up`, so no surface invents a time
 * of its own.
 */
export function useServerStatus(): {
  status: ServerStatus | null;
  refresh: () => void;
  lastGoodAt: number | null;
} {
  const [status, setStatus] = useState<ServerStatus | null>(null);
  const [lastGoodAt, setLastGoodAt] = useState<number | null>(null);
  // The ordering guard (#389, #K1): every read is minted a number, and an
  // answer applies unless a NEWER read's answer has ALREADY applied — `applied`
  // holds the highest read number that applied. A stalled read that answers
  // late still lands while nothing newer has applied: with `POLL_MS` equal to
  // the client's own 15 s request timeout, that late `down` is often the only
  // answer a stalled handshake ever produces, and the gate must see it. An
  // older answer that resolves after a newer one applied is still dropped.
  const issued = useRef(0);
  const applied = useRef(0);

  const refresh = useCallback(() => {
    const read = ++issued.current;
    commands
      .serverStatus()
      .then((result) => {
        if (read <= applied.current) {
          // A newer read's answer has already applied: this older one must not
          // overwrite it.
          return;
        }
        // The bus's `Result` shell (`typedError`): an `error` arm means the
        // call itself failed — keep the last known status and let the poll
        // retry, rather than rendering a state nobody reported.
        if (result.status === "ok") {
          applied.current = read;
          setStatus(result.data);
          // A poll that answered `down` succeeded but saw nothing good: the
          // last good read stays the last time the server was actually up.
          if (result.data.state === "up") {
            setLastGoodAt(Date.now());
          }
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

  return { status, refresh, lastGoodAt };
}
