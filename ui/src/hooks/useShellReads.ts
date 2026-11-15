// The shell's two reads (r7.s2.w4): the Library nav count (`shell_info`) and the
// server's stored address (`server_address`). Each hook calls its command inside
// `Promise.resolve().then(...)` so that a missing command rejects instead of
// throwing during render.

import { useEffect, useState } from "react";

import { commands } from "../bindings";

/** The status strip's cadence (`useServerStatus`'s 15 s), so the counts age together. */
const SHELL_POLL_MS = 15_000;

/** The Library row's count: `shell_info.strategyCount` (non-archived, as the Library lists them),
 * polled every 15 s; reset and re-read when `epoch` (an accepted connect) changes. A failed read keeps
 * the last count, and `null` (nothing read yet) renders no count. */
export function useStrategyCount(epoch: number): number | null {
  const [count, setCount] = useState<number | null>(null);
  useEffect(() => {
    let live = true;
    setCount(null);
    const read = () => {
      Promise.resolve()
        .then(() => commands.shellInfo())
        .then((result) => {
          if (live && result.status === "ok") {
            setCount(result.data.strategyCount);
          }
        })
        .catch(() => {
          // Keep the last count; the next poll retries.
        });
    };
    read();
    const timer = window.setInterval(read, SHELL_POLL_MS);
    return () => {
      live = false;
      window.clearInterval(timer);
    };
  }, [epoch]);
  return count;
}

/** The stored server URL (`server_address`), read once per `epoch`; `null` when none is stored or the read fails. */
export function useServerAddress(epoch: number): string | null {
  const [address, setAddress] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    setAddress(null);
    Promise.resolve()
      .then(() => commands.serverAddress())
      .then((result) => {
        if (live && result.status === "ok") {
          setAddress(result.data);
        }
      })
      .catch(() => {
        // No address: the strip says "server".
      });
    return () => {
      live = false;
    };
  }, [epoch]);
  return address;
}
