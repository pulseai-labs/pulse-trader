// The Connect gate (r3.s3.w5): the screen that replaces the app while no
// server connection stands.
//
// Two sources name a refusal, and both render verbatim: the `ServerStatus`
// the parent passes (a `token_refused`/`skew` from ANY command flips the
// connection state to refused, and this screen is where the UI lands), and
// the `ConnectOutcome` a submit just produced. The token field is a real
// password input — the token is pasted from `pulse token issue` or the
// `pulse mcp login` flow; nothing here stores it beyond the Connect command
// itself, which persists it to the connection file server-side of this
// component's concerns.
//
// Design-system note: this screen composes the shell's existing token
// classes (`pane`, `status-card`, `field`, `btn`) — no new colors, no scale
// transforms (the window-config gate), no invented sample data.

import { useState } from "react";
import type { FormEvent } from "react";

import { commands } from "../bindings";
import type { ConnectOutcome, ServerStatus } from "../bindings";

/**
 * What a Connect attempt answered, reduced to the display strings the
 * outcome enum carries. `connected` is the parent's cue to re-poll status.
 */
function outcomeReason(outcome: ConnectOutcome): string {
  switch (outcome.outcome) {
    case "connected":
      return "";
    case "unreachable":
      return outcome.reason;
    case "token_refused":
      return outcome.reason;
    case "skew":
      return `server speaks API v${outcome.server_api_version}, this app speaks v1 — update one side before connecting`;
  }
}

export default function ConnectScreen({
  status,
  onConnected,
}: {
  /** The connection state that made this screen render. */
  status: ServerStatus;
  /** Called after a `connected` outcome so the parent re-polls and un-gates. */
  onConnected: () => void;
}) {
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [error, setError] = useState<string | null>(
    status.state === "refused" ? (status.reason ?? "the connection was refused") : null,
  );
  const [busy, setBusy] = useState(false);

  const connect = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const answer = await commands.serverConnect(url.trim(), token);
      // The bus's `Result` shell (`typedError`): a real transport failure
      // (not-connected etc.) lands in the `error` arm and shows verbatim.
      if (answer.status === "error") {
        setError(answer.error.message);
        return;
      }
      const reason = outcomeReason(answer.data);
      if (reason === "") {
        onConnected();
      } else {
        setError(reason);
      }
    } catch (err) {
      // The bus's typed error — show its message, never a stack.
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <main className="content">
      <section className="pane" aria-label="Connect to the server">
        <h2>Connect to the server</h2>
        <p className="status-card">
          PulseTrader runs against an always-on server. Paste the server URL
          and the agent token the server operator issued you.
        </p>
        <form onSubmit={(event) => void connect(event)}>
          <label className="field" htmlFor="connect-url">
            Server URL
            <input
              id="connect-url"
              value={url}
              onChange={(event) => setUrl(event.target.value)}
              placeholder="http://draco-desk:17620"
              autoComplete="url"
              required
            />
          </label>
          <label className="field" htmlFor="connect-token">
            Token
            <input
              id="connect-token"
              type="password"
              value={token}
              onChange={(event) => setToken(event.target.value)}
              placeholder="pt_…"
              autoComplete="off"
              required
            />
          </label>
          <button className="btn" type="submit" disabled={busy || url === "" || token === ""}>
            {busy ? "Connecting…" : "Connect"}
          </button>
        </form>
        {error !== null && (
          <p className="error-card" role="alert">
            {error}
          </p>
        )}
      </section>
    </main>
  );
}
