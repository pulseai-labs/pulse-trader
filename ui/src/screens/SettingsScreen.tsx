// The Settings screen's "Server connection" section (r4.s3.w3, spec §Approach 2), and
// the "About" section r5.s2.w2 adds beneath it.
//
// The mock's other sections (AI provider, Agents, Appearance, Data) are still out of
// scope: nothing is rendered for them, and no placeholder stands in their place. The
// About section is the one exception the Chart tab owes: the chart library's licence
// asks every app that uses it to credit TradingView with a link to tradingview.com
// (grill Q6; PDR-0004), and this is that line.
//
// Every fact here is one the app actually has: the state comes from the shared
// polled `ServerStatus`, the address from the `server_address` command (the
// stored URL only — the token never crosses to the frontend), the version
// and fingerprint from the status the last handshake answered with, and the About
// section's app version from the app's own bundle (`getVersion`). No number is
// invented for a field the wire does not carry; a version that cannot be read renders
// the em dash rather than a literal copied out of `package.json`, which would go stale
// on the next version bump.
//
// Disconnect keeps the connection file's lifecycle in Rust (the same
// `server_disconnect` the command bus has always had) and, per the Dispatch-3
// amendment, shows the disconnected state and the revoke sentence at once —
// from local knowledge of what just happened, never from a poll that has not
// answered yet. The way to Connect from there is the same App action as "Change
// server": the Connect screen opens over the live app.

import { useEffect, useState } from "react";

import { getVersion } from "@tauri-apps/api/app";

import { commands } from "../bindings";
import { EM_DASH } from "../format/numbers";
import { useConnection } from "../shell/ConnectionContext";

/** The command that revokes the token on the server (the note names it). */
const REVOKE_COMMAND = "pulse token revoke --label <label>";

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
  return (
    <>
      <ServerConnectionSection />
      <AboutSection />
    </>
  );
}

// ---------------------------------------------------------------------------
// About (r5.s2.w2) — the chart library's credit, and the app's own version
// ---------------------------------------------------------------------------

/**
 * The app's own bundle version, or `null` while it cannot be read. Read from the app
 * (`@tauri-apps/api/app`), never from a literal: `package.json`'s version and the
 * shipped bundle's are two numbers that drift apart, and the one the trader can act on
 * ("which build am I running?") is the bundle's.
 */
function useAppVersion(): string | null {
  const [version, setVersion] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    getVersion()
      .then((value) => {
        if (!cancelled) setVersion(value);
      })
      .catch(() => {
        // No version to show. The row says so with the em dash; the About section is
        // not the app's version gate.
      });
    return () => {
      cancelled = true;
    };
  }, []);
  return version;
}

function AboutSection() {
  const version = useAppVersion();
  return (
    <section className="settings-section" aria-label="About">
      <div className="settings-head">
        <h2>About</h2>
        <p>PulseTrader runs your strategies on your own server; this Mac app is the window onto it.</p>
      </div>

      <div className="srv-card">
        <dl className="srv-facts">
          <div>
            <dt>App version</dt>
            <dd className="mono">{version === null ? EM_DASH : `PulseTrader ${version}`}</dd>
          </div>
        </dl>

        {/* The chart library's credit (grill Q6). It stays even if a later build hides
            the logo the library draws on the chart itself. The link opens in a new
            tab: this app has no opener plugin and no navigation handler of its own, so
            a same-window link would replace the whole app view with tradingview.com. */}
        <p className="srv-foot-note">
          Charts by TradingView Lightweight Charts™ —{" "}
          <a href="https://www.tradingview.com/" target="_blank" rel="noopener noreferrer">
            tradingview.com
          </a>
        </p>
      </div>
    </section>
  );
}

function ServerConnectionSection() {
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
          {/* #386: never disabled — from the disconnected state this IS the way
              back to a connection. */}
          <button
            type="button"
            className="btn-sec-sm"
            onClick={openConnect}
          >
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
