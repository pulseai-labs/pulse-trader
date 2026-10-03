// The Deployment Dashboard (r3.s4.w5, spec §4) — every paper session on the
// server, the kill switch, and the selected session's detail in the shell's
// third track.
//
// The cards render only what `paper_sessions()` answers: the badges (derived
// from each summary, never a tracked flag), the pair and timeframes, the
// closed-trade count, the OOS mini-verdict, and a drift alert for a session
// whose live epoch drifted. The kill switch (A7) sits behind a confirm and
// reports its own result — every stopped id and EVERY failure with its typed
// code; it never reports success over a failure.
//
// The detail opens in the `#details-pane` host the shell renders for a route
// declaring `details` (the Library's portal convention). A fresh promotion
// asks for its session's detail through the handoff module, so the sheet's
// "navigate to the new session" lands here.

import { useCallback, useEffect, useState } from "react";
import { createPortal } from "react-dom";

import { commands } from "../bindings";
import type { PaperSessionSummary, StopAllResult } from "../bindings";
import { CertBadge } from "../components/CertBadge";
import { SessionBadge, sessionBadgeState } from "../components/SessionBadge";
import { useRefetchOnFocus } from "../hooks/useRefetchOnFocus";
import { takeRequestedSession } from "../paper/handoff";
import SessionDetail from "./SessionDetail";

/** The details-track host `App.tsx` renders when the route declares the track. */
const DETAILS_PANE_ID = "details-pane";

/** The certificate badge a session card states. */
function cardCertKind(summary: PaperSessionSummary): "certified" | "uncertified" | "fixture" | "stale" {
  if (summary.certification_stale) return "stale";
  if (summary.fixture) return "fixture";
  if (summary.graduation.graduation === "override") return "uncertified";
  return "certified";
}

/** The live epoch's latest shadow check, when the log recorded one. */
function liveShadow(summary: PaperSessionSummary) {
  return summary.shadow_checks.at(-1) ?? null;
}

/** The OOS mini-verdict a card shows. */
function oosMini(summary: PaperSessionSummary): string {
  const comparison = summary.comparison;
  switch (comparison.status) {
    case "not_applicable":
      return "OOS: n/a";
    case "pending":
      return `OOS pending ${comparison.n}/${comparison.of}`;
    case "within":
      return `OOS ${comparison.live_mean_r}R within`;
    case "below":
      return `OOS ${comparison.live_mean_r}R below`;
    case "above":
      return `OOS ${comparison.live_mean_r}R above`;
  }
}

export default function DeploymentDashboard() {
  const [sessions, setSessions] = useState<PaperSessionSummary[] | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [sweeping, setSweeping] = useState(false);
  const [sweep, setSweep] = useState<StopAllResult | null>(null);
  const [paneHost, setPaneHost] = useState<HTMLElement | null>(null);

  const load = useCallback(async (alive: () => boolean = () => true) => {
    try {
      const result = await commands.paperSessions();
      if (!alive()) return;
      if (result.status === "ok") {
        setSessions(result.data);
        setError(null);
      } else {
        setError(result.error.message);
      }
    } catch {
      if (alive()) setError("The paper-session read failed.");
    }
  }, []);

  useEffect(() => {
    let alive = true;
    void load(() => alive);
    return () => {
      alive = false;
    };
  }, [load]);

  useRefetchOnFocus(load);

  useEffect(() => {
    setPaneHost(document.getElementById(DETAILS_PANE_ID));
    // A fresh promotion asks for its session's detail (the sheet's own call).
    const requested = takeRequestedSession();
    if (requested !== null) setSelected(requested);
  }, []);

  const running = (sessions ?? []).filter((session) => session.status.state !== "stopped");
  const driftedCheck = (sessions ?? [])
    .filter((session) => session.status.state !== "stopped")
    .map((session) => liveShadow(session))
    .find((check) => check?.result.verdict === "drift");

  return (
    <>
      {driftedCheck !== undefined && driftedCheck !== null && driftedCheck.result.verdict === "drift" && (
        <div className="alert-bar" role="alert">
          <b>Shadow drift on a live session</b>
          <span>{driftedCheck.result.first_divergence}</span>
        </div>
      )}

      <div className="deploy-toolbar">
        <h1>Paper sessions</h1>
        <span className="dt-sub">on the server · keeps running while this Mac sleeps</span>
        <button
          className="btn-prim"
          disabled={sweeping || running.length === 0}
          onClick={() => setConfirming(true)}
        >
          Stop all…
        </button>
      </div>

      {error !== null && (
        <p className="ds-empty bear" role="alert">
          {error}
        </p>
      )}

      {sweep !== null && (
        <div className="sweep-report" role="status" aria-label="stop-all result">
          {sweep.stopped.length > 0 && <span className="mono">stopped: {sweep.stopped.join(", ")}</span>}
          {sweep.failures.length > 0 ? (
            <span className="mono bear">
              {sweep.failures.length} failed:{" "}
              {sweep.failures.map((failure) => `${failure.id} (${failure.code})`).join(", ")}
            </span>
          ) : (
            <span className="mono">no failures</span>
          )}
        </div>
      )}

      <div className="deploy-scroll">
        {sessions === null ? (
          <div className="ds-empty">Reading the paper sessions…</div>
        ) : sessions.length === 0 ? (
          <div className="ds-empty">No paper sessions yet. Promote a version from the Library.</div>
        ) : (
          <div className="deploy-cards">
            {sessions.map((session) => (
              <div
                key={session.id}
                className={`dcard${selected === session.id ? " is-selected" : ""}`}
              >
                <header className="dc-head">
                  <span className="dc-pair-tag">{session.pair}</span>
                  <CertBadge kind={cardCertKind(session)} />
                  <SessionBadge state={sessionBadgeState(session)} />
                </header>
                <div className="dc-body">
                  <span className="dc-tf mono">
                    {session.pair} · {session.primary_timeframe}
                    {session.htf_timeframe === null ? "" : ` · ${session.htf_timeframe}`}
                    {session.uses_d1 ? " · D1" : ""}
                  </span>
                  <span className="dc-trades mono">{session.closed_trade_count} closed trades</span>
                  <span className="dc-oos mono">{oosMini(session)}</span>
                </div>
                <footer className="dc-foot">
                  <button className="btn-sec-sm" onClick={() => setSelected(session.id)}>
                    Details
                  </button>
                </footer>
              </div>
            ))}
          </div>
        )}
      </div>

      {confirming && (
        <div className="sheet-scrim" onClick={() => setConfirming(false)}>
          <div className="sheet" role="alertdialog" aria-modal="true" onClick={(event) => event.stopPropagation()}>
            <div className="sh-body">
              <div className="sh-box tone-bear">
                <div>
                  <b>
                    Stop all {running.length} paper session{running.length === 1 ? "" : "s"} on the
                    server?
                  </b>
                  <span className="sh-box-sub">
                    This is the server-side kill switch. Every running session stops at once and
                    stays listed, read-only. There is no undo; promote again to start new sessions.
                  </span>
                </div>
              </div>
            </div>
            <footer className="sh-foot">
              <span className="sh-foot-note">App only. Agents can't stop sessions.</span>
              <button className="btn-sec" onClick={() => setConfirming(false)}>
                Cancel
              </button>
              <button
                className="btn-danger"
                onClick={() => {
                  setConfirming(false);
                  setSweeping(true);
                  void commands
                    .paperStopAll()
                    .then((result) => {
                      if (result.status === "ok") {
                        setSweep(result.data);
                      } else {
                        setError(result.error.message);
                      }
                      return load();
                    })
                    .catch(() => setError("The stop-all call itself failed."))
                    .finally(() => setSweeping(false));
                }}
              >
                Stop all sessions
              </button>
            </footer>
          </div>
        </div>
      )}

      {paneHost !== null &&
        selected !== null &&
        createPortal(
          // Keyed by id: a switch remounts the detail, so a read still pending
          // for the previous session cannot land in the new one.
          <SessionDetail key={selected} sessionId={selected} onChanged={() => void load()} />,
          paneHost,
        )}
    </>
  );
}
