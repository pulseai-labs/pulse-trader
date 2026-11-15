// The title bar's status strip and the sidebar's status card, aligned to canvas v9
// (r7.s2.w4; `app-shell.jsx` `ConnStrip` and `Sidebar`'s `.status-card`). Every value
// is an existing read (`server_status`, `paper_sessions`, `server_address`). LLM cost,
// agent last-seen and the token label have no backend in r7, so they render
// `NotBuiltYet` in the canvas's place.

import { useState } from "react";

import type { ServerStatus, ServerStatusState } from "../bindings";
import { NotBuiltYet } from "../components/NotBuiltYet";
import { EM_DASH } from "../format/numbers";

interface DotProps {
  /** A CSS colour value — usually one of `tokens.css`'s semantic custom properties. */
  color: string;
  /** Whether the dot pulses (`shared.css`'s `@keyframes pulse`). */
  pulse?: boolean;
}

/** A small status dot (`app-shell.jsx`'s `Dot`), used by the strip and the status card. */
export function Dot({ color, pulse = false }: DotProps) {
  return (
    <span
      className="dot"
      style={{
        width: 6,
        height: 6,
        borderRadius: 99,
        background: color,
        flexShrink: 0,
        display: "inline-block",
        boxShadow: pulse ? `0 0 0 0 ${color}` : "none",
        animation: pulse ? "pulse 1.6s var(--ease-out) infinite" : "none",
      }}
    />
  );
}

export type ConnectionState = "connected" | "reconnecting" | "disconnected";

export const CONNECTION_LABELS: Readonly<Record<ConnectionState, string>> = {
  connected: "connected",
  reconnecting: "reconnecting…",
  disconnected: "disconnected",
};

/** The canvas's three states, mapped as the Settings section maps them. */
export function connectionState(state: ServerStatusState): ConnectionState {
  if (state === "up") {
    return "connected";
  }
  if (state === "down") {
    return "reconnecting";
  }
  return "disconnected";
}

/** The server's name: the stored URL's host name, else the word `server`. */
export function serverName(address: string | null | undefined): string {
  if (address === null || address === undefined) {
    return "server";
  }
  try {
    const hostname = new URL(address).hostname;
    return hostname === "" ? "server" : hostname;
  } catch {
    return "server";
  }
}

/** The stored URL's `host:port`, else an em dash. */
export function serverHostPort(address: string | null | undefined): string {
  if (address === null || address === undefined) {
    return EM_DASH;
  }
  try {
    const host = new URL(address).host;
    return host === "" ? EM_DASH : host;
  } catch {
    return EM_DASH;
  }
}

/** The first seven characters of the engine fingerprint, else an em dash. */
export function engineShort(fingerprint: string | null): string {
  return fingerprint === null ? EM_DASH : fingerprint.slice(0, 7);
}

export const RECONNECTING_NOTE =
  "The server is not answering. Retrying every 15 s; nothing to do on your side.";

function StateMark({ state }: { state: ConnectionState }) {
  if (state === "reconnecting") {
    return <span className="spinner-sm" />;
  }
  return <Dot color={state === "disconnected" ? "var(--bear-1)" : "var(--bull-1)"} />;
}

interface ConnPopoverProps {
  status: ServerStatus;
  serverAddress: string | null;
  state: ConnectionState;
  label: string;
  name: string;
  onClose: () => void;
}

function ConnPopover({ status, serverAddress, state, label, name, onClose }: ConnPopoverProps) {
  return (
    <div className="conn-pop" data-tauri-drag-region="false" onMouseLeave={onClose}>
      <div className="cpop-head">
        <StateMark state={state} />
        <b>{name}</b>
        <span className={`cpop-state is-${state}`}>{label}</span>
      </div>
      {state === "reconnecting" && <p className="cpop-note">{RECONNECTING_NOTE}</p>}
      <dl className="cpop-dl">
        <dt>address</dt>
        <dd>{serverHostPort(serverAddress)}</dd>
        <dt>server</dt>
        <dd>{status.binary_version === null ? EM_DASH : `pulse ${status.binary_version}`}</dd>
        <dt>engine</dt>
        <dd>{engineShort(status.engine_fingerprint)}</dd>
        <dt>token</dt>
        <dd>
          <NotBuiltYet what="The token label" />
        </dd>
      </dl>
      <a className="cpop-link" href="#/settings" onClick={onClose}>
        Connection settings →
      </a>
    </div>
  );
}

interface TitleStatusProps {
  status: ServerStatus;
  serverAddress: string | null;
  paperRunning: number | undefined;
}

/** The title bar's centred strip: the server pill (with its popover), the QA badge,
 * the running-paper pill and the LLM cost MTD pill. */
export function TitleStatus({ status, serverAddress, paperRunning }: TitleStatusProps) {
  const [open, setOpen] = useState(false);
  const state = connectionState(status.state);
  const label = CONNECTION_LABELS[state];
  const name = serverName(serverAddress);
  return (
    <div className="title-status">
      <div className="ts-pills">
        <button
          type="button"
          className={`conn-pill is-${state}`}
          title="Server connection"
          aria-label={`${name} ${label}`}
          aria-expanded={open}
          onClick={() => setOpen((current) => !current)}
        >
          <StateMark state={state} />
          <span className="cp-name">{name}</span>
          <span className="cp-state">{label}</span>
        </button>
        {status.role === "qa" && (
          <span className="qa-badge" title="Connected to a QA server">
            QA
          </span>
        )}
        <span
          className="ts-pill ts-paper"
          title="Paper sessions running on the server. Live trading is not available yet."
        >
          <Dot
            color={paperRunning === undefined ? "var(--text-4)" : "var(--bull-1)"}
            pulse={state === "connected" && paperRunning !== undefined}
          />
          <span className="mono">{paperRunning ?? EM_DASH}</span>
          {" paper "}
          <span className="dim">· 0 live</span>
        </span>
        <span className="ts-pill ts-cost">
          <svg
            width="11"
            height="11"
            viewBox="0 0 16 16"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.5"
            strokeLinejoin="round"
            aria-hidden="true"
          >
            <path d="M9 1.5 L3 9 H8 L7 14.5 L13 7 H8 Z" />
          </svg>
          <NotBuiltYet what="LLM cost this month" />{" "}
          <span className="dim">MTD</span>
        </span>
      </div>
      {open && (
        <ConnPopover
          status={status}
          serverAddress={serverAddress}
          state={state}
          label={label}
          name={name}
          onClose={() => setOpen(false)}
        />
      )}
    </div>
  );
}

interface StatusCardProps {
  status: ServerStatus | undefined;
  serverAddress: string | null | undefined;
}

/** The sidebar's foot card: the server row from the handshake, and the agent and
 * LLM cost rows, which are not built yet. */
export function StatusCard({ status, serverAddress }: StatusCardProps) {
  const state: ConnectionState =
    status === undefined ? "disconnected" : connectionState(status.state);
  const serverValue =
    state === "connected"
      ? engineShort(status?.engine_fingerprint ?? null)
      : state === "reconnecting"
        ? "retrying"
        : "offline";
  return (
    <div className="status-card">
      <div className="status-row">
        <StateMark state={state} />
        <span className="status-key">{serverName(serverAddress)}</span>
        <span className="status-val mono">{serverValue}</span>
      </div>
      <div className="status-row">
        <Dot color="var(--text-4)" />
        <span className="status-key">Agent</span>
        <span className="status-val">
          <NotBuiltYet what="Agent last seen" />
        </span>
      </div>
      <div className="status-row">
        <Dot color="var(--text-4)" />
        <span className="status-key">LLM cost · MTD</span>
        <span className="status-val">
          <NotBuiltYet what="LLM cost this month" />
        </span>
      </div>
    </div>
  );
}
