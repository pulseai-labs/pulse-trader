// The promote-to-paper sheet (r3.s4.w5, spec §2) — certified / stale /
// uncertified, from `promote-sheet.jsx`.
//
// **The variant is decided BEFORE the request**, from the server's own reads:
// `get_walk_forward_run` (does the version's latest run pass, and under which
// engine?) plus `server_status` (this server's fingerprint). The server's 422
// stays the backstop: `certified_under_other_engine` re-renders the sheet as
// stale, `uncertified` as uncertified.
//
// Stale means NO override and no promote button — E2 is not overridable, and
// the only action is "Re-run walk-forward", which opens the Backtest Lab on
// this version. Uncertified is two steps with a required (trimmed non-empty)
// reason; the confirm step sends the override with the version's OWN pair and
// timeframes, read from its newest recorded run (`get_backtest_run`).
//
// Nothing on this sheet is invented: the run id, the verdict, the snapshots
// and the pair/timeframes all come from the server; "10,000 USDT fixed" is the
// recorded A4 starting equity, stated as the fixed default it is.

import { useCallback, useEffect, useState } from "react";

import { commands } from "../bindings";
import type { BacktestRunDto, LibraryVersion, PromoteOverride, WalkForwardRunDto } from "../bindings";
import { openBacktestLab } from "../paper/handoff";
import { CertBadge } from "./CertBadge";

/** What the sheet promotes: one persisted version and how to label it. */
export interface PromoteTarget {
  /** The version's record (id, latest run, latest walk-forward run). */
  readonly version: LibraryVersion;
  /** The strategy's display name. */
  readonly strategyName: string;
  /** The version's display label (`v2`, `v3.1`, …). */
  readonly versionLabel: string;
}

/** The decided variant, with the data each body renders. */
type Variant =
  | { kind: "loading" }
  | { kind: "error"; message: string }
  | { kind: "certified"; run: WalkForwardRunDto }
  | { kind: "stale"; certifiedUnder: string | null; serverEngine: string | null; note: string | null }
  | { kind: "uncertified"; note: string | null };

interface PromoteSheetProps {
  target: PromoteTarget;
  onClose: () => void;
  /** Called with the new session's id, so the caller can open its detail. */
  onPromoted?: (sessionId: string) => void;
}

export function PromoteSheet({ target, onClose, onPromoted }: PromoteSheetProps) {
  const [variant, setVariant] = useState<Variant>({ kind: "loading" });
  const [shape, setShape] = useState<BacktestRunDto | null>(null);
  const [step, setStep] = useState<"explain" | "confirm">("explain");
  const [reason, setReason] = useState("");
  const [busy, setBusy] = useState(false);
  const [refusal, setRefusal] = useState<string | null>(null);

  const { id: versionId, latestWalkForwardRunId, latestRun } = target.version;

  /**
   * Decide the variant, then read the version's newest recorded run for its
   * pair/timeframes (and, when certified, the snapshots it certified on).
   */
  useEffect(() => {
    let alive = true;
    setVariant({ kind: "loading" });
    setStep("explain");
    setRefusal(null);

    const decide = async () => {
      const status = await commands.serverStatus();
      const serverEngine =
        status.status === "ok" ? status.data.engine_fingerprint : null;

      /** The version's own shape: a walk-forward fold run when one exists,
       * else its latest ordinary run. */
      const loadShape = async (runId: string | null) => {
        if (runId === null) return;
        const loaded = await commands.getBacktestRun({ runId });
        if (alive && loaded.status === "ok") setShape(loaded.data);
      };

      if (latestWalkForwardRunId === null) {
        if (alive) setVariant({ kind: "uncertified", note: null });
        await loadShape(latestRun?.id ?? null);
        return;
      }
      const run = await commands.getWalkForwardRun({
        walkForwardRunId: latestWalkForwardRunId,
      });
      if (!alive) return;
      if (run.status === "error") {
        // The certificate cannot be read: there is nothing to certify with,
        // and the server's own gate stays the authority on the promotion.
        setVariant({ kind: "uncertified", note: run.error.message });
        await loadShape(latestRun?.id ?? null);
        return;
      }
      const dto = run.data;
      if (!dto.verdict.pass) {
        setVariant({ kind: "uncertified", note: null });
      } else if (serverEngine !== null && dto.engineFingerprint === serverEngine) {
        setVariant({ kind: "certified", run: dto });
      } else {
        setVariant({
          kind: "stale",
          certifiedUnder: dto.engineFingerprint,
          serverEngine,
          note: null,
        });
      }
      await loadShape(dto.folds[0]?.backtestRunId ?? latestRun?.id ?? null);
    };

    void decide().catch(() => {
      if (alive) {
        setVariant({ kind: "error", message: "The promotion decision could not be read." });
      }
    });
    return () => {
      alive = false;
    };
  }, [versionId, latestWalkForwardRunId, latestRun]);

  /** The snapshots the certification named — the shape run's data versions. */
  const snapshots =
    shape === null
      ? []
      : [shape.primaryDataVersion, shape.htfDataVersion, shape.d1DataVersion].filter(
          (version): version is string => version !== null,
        );

  const trimmed = reason.trim();

  const promote = useCallback(
    async (override: PromoteOverride | null) => {
      setBusy(true);
      setRefusal(null);
      try {
        const result = await commands.paperPromote({ version_id: versionId, override });
        if (result.status === "ok") {
          onPromoted?.(result.data.id);
          return;
        }
        // The server's backstop: re-render in the variant its refusal names,
        // with the server's own message (it names both fingerprints for E2).
        if (result.error.code === "certified_under_other_engine") {
          setVariant({
            kind: "stale",
            certifiedUnder: null,
            serverEngine: null,
            note: result.error.message,
          });
          return;
        }
        if (result.error.code === "uncertified") {
          setVariant({ kind: "uncertified", note: result.error.message });
          setStep("explain");
          return;
        }
        setRefusal(result.error.message);
      } catch {
        setRefusal("The promotion call itself failed.");
      } finally {
        setBusy(false);
      }
    },
    [versionId, onPromoted],
  );

  const head = (
    <header className="sh-head">
      <div>
        <span className="sh-eyebrow">Promote to paper</span>
        <h2>
          {target.strategyName} <span className="dc-ver mono">{target.versionLabel}</span>
        </h2>
        <span className="sh-sub mono">
          {shape === null
            ? "the version's recorded run sets the pair and timeframes"
            : `${shape.pair} · ${shape.primaryTimeframe}${shape.htfTimeframe === null ? "" : ` · ${shape.htfTimeframe}`}`}
        </span>
      </div>
      <button className="drawer-close" aria-label="Close" onClick={onClose}>
        ×
      </button>
    </header>
  );

  const footer = (
    <footer className="sh-foot">
      <span className="sh-foot-note">Only this app can promote. Agents can't.</span>
      {variant.kind === "certified" && (
        <>
          <button className="btn-sec" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn-prim"
            disabled={busy}
            onClick={() => {
              void promote(null);
            }}
          >
            Promote
          </button>
        </>
      )}
      {variant.kind === "stale" && (
        <>
          <button className="btn-sec" onClick={onClose}>
            Cancel
          </button>
          <button className="btn-prim" onClick={() => openBacktestLab(versionId)}>
            Re-run walk-forward
          </button>
        </>
      )}
      {variant.kind === "uncertified" && step === "explain" && (
        <>
          <button className="btn-sec" onClick={onClose}>
            Cancel
          </button>
          <button
            className="btn-sec"
            disabled={trimmed.length === 0 || shape === null}
            onClick={() => setStep("confirm")}
          >
            Continue uncertified…
          </button>
        </>
      )}
      {variant.kind === "uncertified" && step === "confirm" && (
        <>
          <button className="btn-sec" onClick={() => setStep("explain")}>
            Back
          </button>
          <button
            className="btn-warn-solid"
            disabled={busy || shape === null}
            onClick={() => {
              if (shape === null) return;
              void promote({
                reason: trimmed,
                pair: shape.pair,
                primary_timeframe: shape.primaryTimeframe,
                htf_timeframe: shape.htfTimeframe,
                uses_d1: shape.d1DataVersion !== null,
              });
            }}
          >
            Promote uncertified
          </button>
        </>
      )}
    </footer>
  );

  return (
    <div className="sheet-scrim" onClick={onClose}>
      <div
        className="sheet"
        role="dialog"
        aria-modal="true"
        aria-label="Promote to paper"
        onClick={(event) => event.stopPropagation()}
      >
        {head}
        {variant.kind === "loading" && (
          <div className="sh-body">
            <div className="sh-box tone-neutral">Reading the version's certificate…</div>
          </div>
        )}
        {variant.kind === "error" && (
          <div className="sh-body">
            <div className="sh-box tone-bear">{variant.message}</div>
          </div>
        )}
        {variant.kind === "certified" && (
          <div className="sh-body">
            <div className="sh-box tone-bull">
              <div>
                <b>
                  Certified by walk-forward run{" "}
                  <span className="mono">{variant.run.walkForwardRunId}</span>, PASS under wf-v1
                </b>
                <span className="mono sh-box-sub">
                  {variant.run.verdict.foldsHolding}/{variant.run.verdict.foldsRequired} folds hold
                </span>
              </div>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Certified on</span>
              <span className="ps-snaps">
                {snapshots.length === 0 ? (
                  <span className="dim">not readable</span>
                ) : (
                  snapshots.map((snapshot) => (
                    <span key={snapshot} className="ps-snap mono">
                      {snapshot}
                    </span>
                  ))
                )}
              </span>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Engine</span>
              <span className="mono ps-eng">
                {variant.run.engineFingerprint} <span className="bull">· matches this server</span>
              </span>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Equity</span>
              <span className="ps-eng">10,000 USDT fixed</span>
            </div>
            <p className="sh-note">
              The session runs on the server and keeps running while this Mac sleeps. A shadow
              backtest replays its bars to check it trade for trade.
            </p>
          </div>
        )}
        {variant.kind === "stale" && (
          <div className="sh-body">
            <div className="sh-box tone-bear">
              <div>
                {variant.certifiedUnder !== null ? (
                  <b>
                    Certified under engine {variant.certifiedUnder}; this server runs{" "}
                    {variant.serverEngine ?? "another build"}.
                  </b>
                ) : (
                  <b>{variant.note}</b>
                )}
                <span className="sh-box-sub">
                  A certificate from another engine build can't vouch for this one.
                </span>
              </div>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Badge</span>
              <span className="ps-snaps">
                <CertBadge kind="stale" />
              </span>
            </div>
            <p className="sh-note">There is no override</p>
          </div>
        )}
        {variant.kind === "uncertified" && step === "explain" && (
          <div className="sh-body">
            <div className="sh-box tone-neutral">
              <div>
                <b>No walk-forward run certifies this version.</b>
                <span className="sh-box-sub">
                  Run a walk-forward first and it promotes as certified, with an out-of-sample range
                  to compare the session against.
                </span>
              </div>
            </div>
            {variant.note !== null && <p className="sh-note">{variant.note}</p>}
            <div className="sh-override">
              <label className="sh-reason">
                <span>
                  Why paper-trade an uncertified strategy? <span className="req">required</span>
                </span>
                <textarea
                  rows={3}
                  value={reason}
                  onChange={(event) => setReason(event.target.value)}
                  placeholder="e.g. Fold 3 failed on one low-vol week. I want live-bar evidence before re-tuning."
                />
              </label>
              <span className="sh-hint">Saved with the session and shown on its badge, for good.</span>
              {shape === null && (
                <span className="sh-hint">
                  This version has no recorded run yet — run a backtest first, so the session's pair
                  and timeframes are its own.
                </span>
              )}
            </div>
          </div>
        )}
        {variant.kind === "uncertified" && step === "confirm" && shape !== null && (
          <div className="sh-body">
            <div className="sh-box tone-warn">
              <div>
                <b>Promote {target.versionLabel} without a certificate?</b>
                <span className="sh-box-sub">
                  Check what this session will and won't have before you start it.
                </span>
              </div>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Badge</span>
              <span className="ps-snaps">
                <CertBadge kind="uncertified" reason={trimmed} />
                <span className="dim ps-perm">permanent · cannot be removed later</span>
              </span>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Your reason</span>
              <span className="ps-quote">“{trimmed}”</span>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Comparison</span>
              <span className="ps-eng">
                Shadow backtest only. There is no out-of-sample range to judge it against.
              </span>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Session</span>
              <span className="ps-eng mono">
                {shape.pair} · {shape.primaryTimeframe}
                {shape.htfTimeframe === null ? "" : ` · ${shape.htfTimeframe}`}
              </span>
            </div>
            <div className="ps-row">
              <span className="ps-lab">Equity</span>
              <span className="ps-eng">10,000 USDT fixed</span>
            </div>
          </div>
        )}
        {refusal !== null && (
          <div className="sh-body">
            <p className="sh-note bear" role="alert">
              {refusal}
            </p>
          </div>
        )}
        {footer}
      </div>
    </div>
  );
}
