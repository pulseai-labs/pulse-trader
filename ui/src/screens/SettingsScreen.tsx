// The Settings screen's "Server connection" section (r4.s3.w3, spec §Approach 2).
//
// The mock's other sections (AI provider, Agents, Appearance, Data, About) are
// out of scope for this item: nothing is rendered for them, and no placeholder
// stands in their place.
//
// Every fact here is one the app actually has: the state comes from the shared
// polled `ServerStatus`, the address from the `server_address` command (the
// stored URL only — the token never crosses to the frontend), and the version
// and fingerprint from the status the last handshake answered with. No number is
// invented for a field the wire does not carry.
//
// Disconnect keeps the connection file's lifecycle in Rust (the same
// `server_disconnect` the command bus has always had) and, per the Dispatch-3
// amendment, shows the disconnected state and the revoke sentence at once —
// from local knowledge of what just happened, never from a poll that has not
// answered yet. The way to Connect from there is the same App action as "Change
// server": the Connect screen opens over the live app.

import { useEffect, useState } from "react";

import { commands } from "../bindings";
import { useConnection } from "../shell/ConnectionContext";

/** The command that revokes the token on the server (the note names it). */
const REVOKE_COMMAND = "pulse token revoke <label>";

/** What the section calls each state, from the polled status. */
const STATE_LABELS = {
  connected: "connected",
  reconnecting: "reconnecting…",
  disconnected: "disconnected",
} as const;

type SectionState = keyof typeof STATE_LABELS;

/** The one-line note under the chip (no measured numbers — none are reported). */
const STATE_NOTES: Record<SectionState, string> = {
  connected: "The last status poll's handshake answered.",
  reconnecting: "The server is not answering the poll; it retries every 15 s.",
  disconnected: "No connection is stored on this Mac.",
};

/** Map the polled status to the section's three states. */
function sectionState(status: string): SectionState {
  if (status === "up") {
    return "connected";
  }
  if (status === "down") {
    return "reconnecting";
  }
  // `not_connected` and `refused` both mean the same thing here: no usable
  // connection is stored (a refusal also gates the app at once, so this screen
  // is usually not mounted for it).
  return "disconnected";
}

export default function SettingsScreen() {
  const { status, refresh, openConnect } = useConnection();
  const [address, setAddress] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [disconnected, setDisconnected] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // The stored address. Re-read when the connection goes away (a confirmed
  // Disconnect deletes the file), so the row never shows an address the Mac no
  // longer holds.
  useEffect(() => {
    let cancelled = false;
    commands
      .serverAddress()
      .then((answer) => {
        if (!cancelled && answer.status === "ok") {
          setAddress(answer.data);
        }
      })
      .catch(() => {
        // No address to show; the section still states what it knows.
      });
    return () => {
      cancelled = true;
    };
  }, [disconnected]);

  const disconnect = async () => {
    setBusy(true);
    setError(null);
    try {
      const answer = await commands.serverDisconnect();
      if (answer.status === "error") {
        setError(answer.error.message);
        return;
      }
      // Straight away, from what just happened: no poll has to answer first.
      setDisconnected(true);
      setConfirming(false);
      setAddress(null);
      // The app's own picture of the connection changed too; refresh it so the
      // titlebar strip cannot claim a connection this Mac no longer holds.
      refresh();
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const state: SectionState = disconnected ? "disconnected" : sectionState(status.state);
  const factsKnown = status.state === "up" && !disconnected;

  return (
    <section className="settings-section" aria-label="Settings">
      <div className="settings-head">
        <h2>Server connection</h2>
        <p>This Mac stores the server address and one app token. Everything else lives on the server.</p>
      </div>

      <div className="srv-card">
        <div className="srv-head">
          <span className="srv-icon" aria-hidden="true">
            <svg viewBox="0 0 16 16" width={14} height={14}>
              <rect x="2" y="2.5" width="12" height="4.5" rx="1" />
              <rect x="2" y="9" width="12" height="4.5" rx="1" />
            </svg>
          </span>
          <span className="srv-state" data-state={state}>
            {STATE_LABELS[state]}
          </span>
        </div>
        <p className="srv-state-note">{STATE_NOTES[state]}</p>

        <dl className="srv-facts">
          <div>
            <dt>Server address</dt>
            <dd className="mono">{address ?? "—"}</dd>
          </div>
          <div>
            <dt>Server version</dt>
            <dd className="mono">
              {factsKnown && status.binary_version !== null
                ? `pulse ${status.binary_version}`
                : "—"}
            </dd>
          </div>
          <div>
            <dt>Engine fingerprint</dt>
            <dd className="mono">
              {factsKnown && status.engine_fingerprint !== null ? status.engine_fingerprint : "—"}
            </dd>
          </div>
        </dl>

        <div className="srv-foot">
          <span className="srv-foot-note">The token itself is never shown after connecting.</span>
          <button type="button" className="btn-sec-sm" onClick={openConnect}>
            Change server
          </button>
          <button
            type="button"
            className="btn-bear-sec-sm"
            disabled={state === "disconnected" || busy}
            onClick={() => setConfirming(true)}
          >
            Disconnect
          </button>
        </div>

        {confirming && (
          <div className="srv-confirm" role="group" aria-label="Confirm disconnect">
            <div>
              <b>Forget the token on this Mac?</b>
              <p>
                You&apos;ll need a new app token to reconnect. Paper sessions keep running on the
                server.
              </p>
            </div>
            <button type="button" className="btn-sec-sm" onClick={() => setConfirming(false)}>
              Cancel
            </button>
            <button
              type="button"
              className="btn-bear-solid-sm"
              disabled={busy}
              onClick={() => void disconnect()}
            >
              Forget the token
            </button>
          </div>
        )}

        {error !== null && (
          <p className="error-card" role="alert">
            {error}
          </p>
        )}

        {disconnected && (
          <p className="srv-revoked" role="status">
            This Mac no longer holds the token. To revoke it on the server, run{" "}
            <code className="mono">{REVOKE_COMMAND}</code> there.
          </p>
        )}
      </div>
    </section>
  );
}
