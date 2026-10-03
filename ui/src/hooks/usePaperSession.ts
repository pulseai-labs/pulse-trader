// The paper-session stream hook (r3.s4.w5, spec §4).
//
// One channel per invocation (`paper_session_events`), the `useComposeRun`
// precedent: the hook reads the summary once, subscribes, folds frames into
// the view, and re-reads the summary on the three events that change it —
// `shadow_checked`, `stop` and `engine_upgraded`. A frame whose seq is at or
// below the last one is DROPPED: the server resumes from `Last-Event-ID`, and
// a resumed stream may replay its boundary, so the duplicate must not reach
// the view twice.
//
// `shadowBars` is the latest LIVE-epoch `shadow_checked` frame's `bar_count` —
// the only place that count exists (`ShadowResult` itself carries just the
// closed-trade count), and it is cleared when an `engine_upgraded` frame opens
// a new epoch, because the previous epoch's count is not the live one's.
//
// A terminal `TokenRefused` moves the view to `refused` and stops folding —
// the screen shows "connection refused, reconnect from Connect".
//
// On unmount the hook cancels: the alive flag drops late frames and the
// promise's answer, and the channel's callback is released. The backend reader
// stops on its next failed send — there is no cancel command among the eight,
// and a channel that nobody reads is the only signal it has.

import { useCallback, useEffect, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";

import { commands } from "../bindings";
import type { PaperSessionSummary, PaperStreamEvent } from "../bindings";

/** How the stream stands. */
export type PaperStreamState = "connecting" | "live" | "refused" | "error";

/** What a session's detail renders. */
export interface PaperSessionView {
  /** The latest summary the server answered, or `null` before the first read. */
  summary: PaperSessionSummary | null;
  /** The stream's state. */
  stream: PaperStreamState;
  /** The last frame's sequence, or `null` before the first one. */
  lastSeq: number | null;
  /** The last frame's event type. */
  lastType: string | null;
  /** The live epoch's latest `shadow_checked` frame's bar count. */
  shadowBars: number | null;
  /** The last error or refusal reason, when one was reported. */
  error: string | null;
  /** Re-read the summary now (a command the screen just issued). */
  reload: () => void;
}

/** The event types that change the summary. */
const REFETCH_TYPES: Record<string, true> = {
  shadow_checked: true,
  stop: true,
  engine_upgraded: true,
};

export function usePaperSession(id: string): PaperSessionView {
  const [summary, setSummary] = useState<PaperSessionSummary | null>(null);
  const [stream, setStream] = useState<PaperStreamState>("connecting");
  const [lastSeq, setLastSeq] = useState<number | null>(null);
  const [lastType, setLastType] = useState<string | null>(null);
  const [shadowBars, setShadowBars] = useState<number | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const alive = useRef(true);
  const seq = useRef<number | null>(null);
  const terminal = useRef(false);

  const loadSummary = useCallback(async () => {
    try {
      const result = await commands.paperSession(id);
      if (!alive.current) return;
      if (result.status === "ok") {
        setSummary(result.data);
        setError(null);
      } else {
        setError(result.error.message);
      }
    } catch {
      if (alive.current) setError("The session read failed.");
    }
  }, [id]);

  useEffect(() => {
    void loadSummary();
  }, [loadSummary, reloadKey]);

  useEffect(() => {
    alive.current = true;
    seq.current = null;
    terminal.current = false;
    setLastSeq(null);
    setLastType(null);
    setShadowBars(null);
    setStream("connecting");
    setError(null);

    const channel = new Channel<PaperStreamEvent>();
    channel.onmessage = (event) => {
      if (!alive.current || terminal.current) return;
      if (event.kind === "tokenRefused") {
        terminal.current = true;
        setStream("refused");
        setError(event.reason);
        return;
      }
      if (seq.current !== null && event.seq <= seq.current) return;
      seq.current = event.seq;
      setStream("live");
      setLastSeq(event.seq);
      setLastType(event.type);
      if (event.type === "engine_upgraded") {
        setShadowBars(null);
      }
      if (event.type === "shadow_checked") {
        const payload = JSON.parse(event.payload) as { bar_count?: number };
        if (typeof payload.bar_count === "number") setShadowBars(payload.bar_count);
      }
      if (REFETCH_TYPES[event.type] === true) void loadSummary();
    };

    void commands
      .paperSessionEvents(id, null, channel)
      .then((result) => {
        if (!alive.current) return;
        if (result.status === "error") {
          setStream("error");
          setError(result.error.message);
        }
      })
      .catch(() => {
        if (alive.current) {
          setStream("error");
          setError("The session stream failed.");
        }
      });

    return () => {
      // The unmount cancel: the flag drops every late frame and the promise's
      // answer, and the callback is released. The backend reader stops on its
      // next failed send (there is no cancel command among the eight).
      alive.current = false;
      channel.onmessage = () => {};
    };
  }, [id, loadSummary]);

  const reload = useCallback(() => {
    setReloadKey((key) => key + 1);
  }, []);

  return { summary, stream, lastSeq, lastType, shadowBars, error, reload };
}
