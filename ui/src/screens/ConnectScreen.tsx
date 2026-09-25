// The Connect gate (r3.s3.w5): the screen that replaces the app while no
// server connection stands.
//
// Two sources name a refusal, and both render verbatim: the `ServerStatus`
// the parent passes (a `token_refused`/`skew` from ANY command flips the
// connection state to refused, and this screen is where the UI lands) — derived
// from the prop on every render, so a refusal that arrives after mount shows
// too — and the `ConnectOutcome` a submit just produced, which wins while it
// stands as the newer fact. The token field is a real password input — the
// token is the APP-scoped one from `pulse token issue --scope app` (the token
// this app's own routes need; an agent token, the kind `pulse mcp login`
// stores for MCP clients, is refused at connect rather than saved); nothing
// here stores it beyond the Connect command itself, which persists it to the
// connection file server-side of this component's concerns.
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
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // The refusal the STATUS carries is derived from the prop on EVERY render,
  // not seeded once on mount: a mounted screen's poll can flip to `refused`
  // while the operator is reading the form, and that reason has to appear.
  const statusReason =
    status.state === "refused" ? (status.reason ?? "the connection was refused") : null;
  // A reason a submit just produced is the newer fact, so it wins while it
  // stands (`connect` clears it before each attempt).
  const error = submitError ?? statusReason;

  const connect = async (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setSubmitError(null);
    try {
      // BOTH fields are trimmed: a pasted token carrying whitespace would
      // otherwise be hashed padded — a 401 `token_refused` that reads like a
      // wrong token — or rejected outright as a bad header value. The login
      // path (`pulse mcp login`) trims for the same reason.
      const answer = await commands.serverConnect(url.trim(), token.trim());
      // The bus's `Result` shell (`typedError`): a real transport failure
      // (not-connected etc.) lands in the `error` arm and shows verbatim.
      if (answer.status === "error") {
        setSubmitError(answer.error.message);
        return;
      }
      const reason = outcomeReason(answer.data);
      if (reason === "") {
        onConnected();
      } else {
        setSubmitError(reason);
      }
    } catch (err) {
      // The bus's typed error — show its message, never a stack.
      setSubmitError(err instanceof Error ? err.message : String(err));
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
          and the app token the server operator issued you — the app needs a
          token issued with <code className="mono">--scope app</code>, not the
          agent token an MCP client uses. A token that cannot do app work is
          refused here rather than saved.
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
          <button
            className="btn"
            type="submit"
            // The guard judges what `connect` would actually send: the trimmed
            // values, so a whitespace-only field cannot arm the button.
            disabled={busy || url.trim() === "" || token.trim() === ""}
          >
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
