// The Connect screen (r4.s3.w3): every state of `docs/design/mock/connect.jsx`,
// rendered from what the server actually answered.
//
// Two sources name a refusal, and both render: the `ServerStatus` the parent
// passes (a `token_refused`/`skew` from ANY command flips the connection state
// to refused, and this screen is where the UI lands) — derived from the prop on
// every render, so a refusal that arrives after mount shows too — and the
// `ConnectOutcome` a submit just produced, which wins while it stands as the
// newer fact.
//
// The three-step list is the spec's own rule (Approach 1): a step is marked done
// or failed **only from the one `server_connect` outcome**. No timer advances a
// step — the mock's `setTimeout` phase machine is not ported — and while the call
// is in flight every row is pending. The only clock on this screen is the
// connected card's short auto-open, which the spec blesses.
//
// The token is a real password input, is trimmed before it is sent, and is never
// rendered again: it appears in no status field, no event and no other element,
// and the `server_address` read the card uses returns the URL only.
//
// Design-system note: the screen composes the ported primitives (`kr-field`,
// `kr-input`, `kr-btn`, `btn-prim`, `btn-sec-sm`, `mono`, `spinner-sm`) plus this
// screen's own class names from `styles/connection.css` — no new colour.

import { useCallback, useEffect, useRef, useState } from "react";
import type { FormEvent, ReactNode } from "react";

import { commands } from "../bindings";
import type { ConnectOutcome, ServerStatus } from "../bindings";

/**
 * The one storage sentence (spec §Approach 1). It is written ONCE, here —
 * r4.s3.w2 moved the token into the macOS Keychain and edited exactly this
 * line. Nothing else in the UI names storage, and no storage mechanism is named
 * anywhere in the frontend — the Keychain account name itself never appears
 * under `ui/src` (AC-8).
 */
export const STORAGE_NOTE =
  "Stored on this Mac: the server address, and a revocable app token in the macOS Keychain. Nothing else.";

/** The command a trader runs on the server to issue (or re-issue) an app token. */
const ISSUE_TOKEN_COMMAND = "pulse token issue --scope app --label <name>";

/** The API generation this app speaks — the skew panel's right-hand side. */
const APP_API_VERSION = 1;

/** The connected card's short auto-open, in seconds (the mock's is three). */
const AUTO_OPEN_SECONDS = 3;

/** One step row's state. */
type StepState = "done" | "failed" | "pending";

/**
 * The three rows' states, derived from the ONE `server_connect` outcome and
 * nothing else:
 *
 * - `null` (in flight, or no attempt): every row pending — no timer marks a row.
 * - `connected`: all three done.
 * - `unreachable`: the server was never reached — row 1 failed, the rest were
 *   never attempted.
 * - `token_refused`: the server answered and refused — row 1 done, the token
 *   check failed.
 * - `skew`: the handshake completed but the versions disagree — row 3 failed.
 */
function stepStates(outcome: ConnectOutcome | null): readonly [StepState, StepState, StepState] {
  if (outcome === null) {
    return ["pending", "pending", "pending"];
  }
  switch (outcome.outcome) {
    case "connected":
      return ["done", "done", "done"];
    case "unreachable":
      return ["failed", "pending", "pending"];
    case "token_refused":
      return ["done", "failed", "pending"];
    case "skew":
      return ["done", "done", "failed"];
  }
}

/**
 * Whether a refusal is the server saying "this token is not app-scoped". The
 * server names the presented scope in its refusal, so the agent-token case is
 * recognised from that reason — never guessed from the token's shape.
 */
const PRESENTED_SCOPE = /the presented token is ([a-z][a-z-]*)-scoped/;

function isAgentRefusal(reason: string): boolean {
  const scope = PRESENTED_SCOPE.exec(reason)?.[1];
  return scope !== undefined && scope !== "app";
}

/** The mock's `CIcon`: the small check / x marks the steps and panels use. */
function StepMark({ state }: { state: StepState }) {
  if (state === "done") {
    return (
      <span className="step-mark" data-state="done" aria-hidden="true">
        <svg viewBox="0 0 16 16" width={14} height={14}>
          <path d="M3 8.5 L6.5 11.5 L13 4.5" />
        </svg>
      </span>
    );
  }
  if (state === "failed") {
    return (
      <span className="step-mark" data-state="failed" aria-hidden="true">
        <svg viewBox="0 0 16 16" width={14} height={14}>
          <path d="M4 4 L12 12 M12 4 L4 12" />
        </svg>
      </span>
    );
  }
  return <span className="step-mark" data-state="pending" aria-hidden="true" />;
}

/** A copyable command block — the mock's `CmdBlock`. */
function CmdBlock({ cmd }: { cmd: string }) {
  const [copied, setCopied] = useState(false);
  const copy = () => {
    // jsdom has no clipboard; a missing or failing write must never break the
    // click (the command is on screen and selectable either way).
    const write = navigator.clipboard?.writeText(cmd);
    void write?.catch(() => {});
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1400);
  };
  return (
    <div className="cmd">
      <code className="cmd-text mono">{cmd}</code>
      <button type="button" className="cmd-copy" onClick={copy}>
        {copied ? "Copied" : "Copy"}
      </button>
    </div>
  );
}

/** The mock's `Problem` layout: a titled alert with the recovery text inside. */
function Problem({
  tone,
  title,
  children,
}: {
  tone: "bear" | "warn";
  title: string;
  children: ReactNode;
}) {
  return (
    <div className={`problem tone-${tone}`} role="alert">
      <div className="problem-title">{title}</div>
      <div className="problem-body">{children}</div>
    </div>
  );
}

/** The recovery panel for a failed outcome (never `connected`). */
function ProblemPanel({ outcome, attemptedUrl }: { outcome: ConnectOutcome; attemptedUrl: string }) {
  switch (outcome.outcome) {
    case "token_refused":
      return isAgentRefusal(outcome.reason) ? (
        <Problem tone="warn" title="This is an agent token, for MCP clients">
          <p>
            Agent tokens (<code className="mono">pt_…</code>) let Claude Code or Codex read,
            compose and backtest. They can&apos;t drive this app. Use an app token (
            <code className="mono">pt_…</code>).
          </p>
          <p className="problem-reason">{outcome.reason}</p>
          <CmdBlock cmd={ISSUE_TOKEN_COMMAND} />
        </Problem>
      ) : (
        <Problem tone="bear" title="The server refused this token">
          <p>
            It was revoked on the server, or it was mistyped. Issue a new app token and paste it
            above.
          </p>
          <p className="problem-reason">{outcome.reason}</p>
          <CmdBlock cmd={ISSUE_TOKEN_COMMAND} />
        </Problem>
      );
    case "unreachable": {
      const host = attemptedUrl.replace(/^https?:\/\//, "");
      return (
        <Problem tone="bear" title={`Can't reach ${host}`}>
          <p>
            No response. Check Tailscale is up on this Mac and that{" "}
            <code className="mono">pulse serve</code> is running on the server.
          </p>
          <ul className="problem-checks">
            <li>Tailscale on this Mac shows the server as online</li>
            <li>The address matches the server&apos;s tailnet IP</li>
            <li>
              <code className="mono">pulse serve</code> is listening on port 8420
            </li>
          </ul>
          <p className="problem-reason">{outcome.reason}</p>
          <CmdBlock cmd={`tailscale ping ${host.split(":")[0]}`} />
        </Problem>
      );
    }
    case "skew":
      return (
        <Problem tone="warn" title="The server runs a different version">
          <p>
            Update the app or the server so both run the same version. Nothing was changed on
            either side.
          </p>
          <div className="problem-vers">
            <div>
              <span>Server</span>
              <b className="mono">API v{outcome.server_api_version}</b>
            </div>
            <div>
              <span>This app</span>
              <b className="mono">API v{APP_API_VERSION}</b>
            </div>
          </div>
        </Problem>
      );
    case "connected":
      // The card replaces the form on success; there is no problem panel.
      return null;
  }
}

export default function ConnectScreen({
  status,
  onConnected,
  onBack,
}: {
  /** The connection state that made this screen render. */
  status: ServerStatus;
  /** Called when the trader opens the app (the auto-open and the button). */
  onConnected: () => void;
  /**
   * Present only when the screen was opened over a LIVE connection ("Change
   * server" in Settings) — the gate itself has nowhere to go back to.
   */
  onBack?: () => void;
}) {
  const [url, setUrl] = useState("");
  const [token, setToken] = useState("");
  const [show, setShow] = useState(false);
  const [address, setAddress] = useState<string | null>(null);
  const [submitError, setSubmitError] = useState<string | null>(null);
  // The one attempt: its URL (the step list's first row names it) and, once the
  // single `server_connect` call has answered, its outcome. `outcome === null`
  // while the call is in flight.
  const [attempt, setAttempt] = useState<{ url: string; outcome: ConnectOutcome | null } | null>(
    null,
  );
  const [openIn, setOpenIn] = useState<number | null>(null);
  const opened = useRef(false);
  const prefilled = useRef(false);

  const busy = attempt !== null && attempt.outcome === null;

  /** Read the stored connection's address (the URL only — never the token). */
  const readAddress = useCallback(() => {
    commands
      .serverAddress()
      .then((answer) => {
        if (answer.status === "ok") {
          setAddress(answer.data);
        }
      })
      .catch(() => {
        // No address to show; the form still works without one.
      });
  }, []);

  useEffect(() => {
    readAddress();
  }, [readAddress]);

  // The idle form starts at the stored address when there is one — the trader
  // only has to paste a token. Once only, so a cleared field stays cleared.
  useEffect(() => {
    if (prefilled.current || address === null) {
      return;
    }
    prefilled.current = true;
    setUrl((current) => (current === "" ? address : current));
  }, [address]);

  // The connected card's short auto-open. The single clock on this screen: it
  // opens the app, it never marks a step.
  useEffect(() => {
    if (openIn === null) {
      return;
    }
    if (openIn <= 0) {
      if (!opened.current) {
        opened.current = true;
        onConnected();
      }
      return;
    }
    const timer = window.setTimeout(() => {
      setOpenIn((remaining) => (remaining === null ? null : remaining - 1));
    }, 1000);
    return () => window.clearTimeout(timer);
  }, [openIn, onConnected]);

  const connect = async (event: FormEvent) => {
    event.preventDefault();
    const submitted = url.trim();
    setSubmitError(null);
    setAttempt({ url: submitted, outcome: null });
    try {
      // BOTH fields are trimmed: a pasted token carrying whitespace would
      // otherwise be hashed padded — a 401 `token_refused` that reads like a
      // wrong token — or rejected outright as a bad header value.
      const answer = await commands.serverConnect(submitted, token.trim());
      if (answer.status === "error") {
        // A real transport failure (not-connected etc.), not a connect outcome:
        // there is no outcome to mark the steps from, so the attempt is cleared.
        setAttempt(null);
        setSubmitError(answer.error.message);
        return;
      }
      setAttempt({ url: submitted, outcome: answer.data });
      if (answer.data.outcome === "connected") {
        // The connect just rewrote the stored connection; re-read so the card
        // shows the address that is actually stored now.
        readAddress();
        setOpenIn(AUTO_OPEN_SECONDS);
      }
    } catch (err) {
      setAttempt(null);
      setSubmitError(err instanceof Error ? err.message : String(err));
    }
  };

  const settled = attempt?.outcome ?? null;
  const connected = settled?.outcome === "connected" ? settled : null;
  const statusReason =
    status.state === "refused" ? (status.reason ?? "the connection was refused") : null;
  // The newest refusing fact wins: a settled submit outcome, else the poll's.
  const problem: ConnectOutcome | null =
    settled !== null && settled.outcome !== "connected"
      ? settled
      : connected === null && statusReason !== null
        ? { outcome: "token_refused", reason: statusReason }
        : null;
  const reconnect = status.state === "refused" || settled?.outcome === "token_refused";
  const attemptedUrl = attempt?.url ?? url.trim();

  return (
    <main className="content">
      <section className="connect" aria-label="Connect to the server">
        {connected !== null ? (
          <div className="connect-card">
            <span className="connect-ok" aria-hidden="true">
              <svg viewBox="0 0 16 16" width={18} height={18}>
                <path d="M3 8.5 L6.5 11.5 L13 4.5" />
              </svg>
            </span>
            <span className="connect-eyebrow">Connected</span>
            <h1 className="connect-title">Connected to your Pulse server</h1>
            <p className="connect-sub">
              Everything you open from here runs on the server. Close the lid whenever you like;
              paper sessions keep running.
            </p>
            <dl className="connect-facts">
              <div>
                <dt>Server version</dt>
                <dd className="mono">pulse {connected.binary_version}</dd>
              </div>
              <div>
                <dt>Engine fingerprint</dt>
                <dd className="mono">{connected.engine_fingerprint}</dd>
              </div>
              <div>
                <dt>Server address</dt>
                <dd className="mono">{address ?? "—"}</dd>
              </div>
            </dl>
            <div className="connect-actions">
              <button type="button" className="btn-prim" onClick={onConnected}>
                Open the app →
              </button>
              {openIn !== null && openIn > 0 && (
                <span className="connect-auto mono">opening in {openIn} s</span>
              )}
            </div>
          </div>
        ) : (
          <>
            <span className={`connect-eyebrow${reconnect ? " is-refused" : ""}`}>
              {reconnect ? "Connection refused" : "First launch"}
            </span>
            <h1 className="connect-title">
              {reconnect ? "Reconnect to your Pulse server" : "Connect to your Pulse server"}
            </h1>
            <p className="connect-sub">
              {reconnect
                ? "The server stopped accepting this Mac's token. Paste a new app token to carry on; nothing on the server was touched."
                : "This app is a thin client. Strategies, market data, the AI provider key and paper sessions all live on the server you run. This Mac stores only the address and a revocable app token."}
            </p>

            <form className="connect-form" onSubmit={(event) => void connect(event)}>
              <label className="kr-lab" htmlFor="connect-url">
                Server URL
                <span className="kr-hint">Tailscale address · port 8420</span>
              </label>
              <div className="kr-field">
                <input
                  id="connect-url"
                  className="kr-input"
                  value={url}
                  onChange={(event) => setUrl(event.target.value)}
                  placeholder="http://100.x.y.z:8420"
                  autoComplete="url"
                  spellCheck={false}
                  disabled={busy}
                  required
                />
              </div>

              <label className="kr-lab" htmlFor="connect-token">
                App token
              </label>
              <div className="kr-field">
                <input
                  id="connect-token"
                  className="kr-input"
                  type={show ? "text" : "password"}
                  value={token}
                  onChange={(event) => setToken(event.target.value)}
                  placeholder="pt_…"
                  autoComplete="off"
                  spellCheck={false}
                  disabled={busy}
                  required
                />
                {token !== "" && (
                  <button
                    type="button"
                    className={`kr-btn${show ? " is-on" : ""}`}
                    aria-pressed={show}
                    onClick={() => setShow((current) => !current)}
                  >
                    {show ? "Hide" : "Reveal"}
                  </button>
                )}
              </div>

              <div className="connect-help">
                <span>Get an app token by running this on the server:</span>
                <CmdBlock cmd={ISSUE_TOKEN_COMMAND} />
              </div>

              <div className="connect-submit">
                <button
                  type="submit"
                  className="btn-prim"
                  // The guard judges what `connect` would send: the trimmed
                  // values, so a whitespace-only field cannot arm the button.
                  disabled={busy || url.trim() === "" || token.trim() === ""}
                >
                  {busy ? (
                    <>
                      <span className="spinner-sm" /> Connecting…
                    </>
                  ) : (
                    "Connect"
                  )}
                </button>
                <span className="connect-store">{STORAGE_NOTE}</span>
              </div>
            </form>

            {attempt !== null && (
              <ol className="connect-steps" aria-label="Connection steps">
                {(
                  [
                    ["reach", `Reach ${attempt.url}`],
                    ["token", "Verify the app token"],
                    ["versions", "Check versions"],
                  ] as const
                ).map(([id, label], index) => {
                  const states = stepStates(attempt.outcome);
                  const state = states[index] ?? "pending";
                  return (
                    <li key={id} className="connect-step" data-step={id} data-state={state}>
                      <StepMark state={state} />
                      <span>{label}</span>
                    </li>
                  );
                })}
              </ol>
            )}

            {submitError !== null && (
              <p className="error-card" role="alert">
                {submitError}
              </p>
            )}

            {problem !== null && <ProblemPanel outcome={problem} attemptedUrl={attemptedUrl} />}

            {onBack !== undefined && (
              <div className="connect-back">
                {/* Disabled while a connect is in flight (D4): Back unmounts
                    this screen, and a success arriving after that would switch
                    servers without the acceptance callback ever running. */}
                <button type="button" className="btn-sec-sm" onClick={onBack} disabled={busy}>
                  Back to the app
                </button>
              </div>
            )}
          </>
        )}
      </section>
    </main>
  );
}
