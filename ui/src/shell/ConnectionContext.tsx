// The app's one connection poll, shared with the screens that render it
// (r4.s3.w3).
//
// `App` owns `useServerStatus` — the 15 s poll whose every tick is a fresh
// handshake — for the gate and the titlebar strip. The Settings screen needs the
// same status and the same refresh, and a second `useServerStatus` would be a
// second handshake per tick that can disagree with the first. So the value is
// passed down through this context instead: one poll, one answer, one place a
// screen can ask for a fresh read.
//
// It also carries `openConnect`: the action "Change server" performs — show the
// Connect screen over the live app. It is App state (not a route) because the
// Connect screen is the gate's screen, and the gate is not a destination.

import { createContext, useContext } from "react";
import type { ReactNode } from "react";

import type { ServerStatus } from "../bindings";

export interface ConnectionValue {
  /** The latest status the poll answered. */
  status: ServerStatus;
  /** Ask the status read to run now (used after a connect or a disconnect). */
  refresh: () => void;
  /** Open the Connect screen over the live app ("Change server"). */
  openConnect: () => void;
}

const ConnectionContext = createContext<ConnectionValue | null>(null);

export function ConnectionProvider({
  value,
  children,
}: {
  value: ConnectionValue;
  children: ReactNode;
}) {
  return <ConnectionContext.Provider value={value}>{children}</ConnectionContext.Provider>;
}

/** The connection the surrounding app is on. Throws outside a provider — a
 * screen that renders without one is a wiring bug, not a state to mask. */
export function useConnection(): ConnectionValue {
  const value = useContext(ConnectionContext);
  if (value === null) {
    throw new Error("useConnection must be used inside <ConnectionProvider>");
  }
  return value;
}
