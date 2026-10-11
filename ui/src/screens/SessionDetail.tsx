// The session detail (r3.s4.w5, spec §4; the money view r4.s4.w2) — the
// certificate and engine rows, the stopped note, the running total, the marked
// open position, the orders and fills, the shadow and OOS panels, the closed
// trades and the inline stop.
//
// Every value rendered here is the server's: the summary and the trades come
// from the reads, the live updates from `usePaperSession`'s stream, the OOS
// texts are the server's own sentences (including the `not_applicable`
// reason, rendered verbatim), and the shadow's "identical over N bars" bar
// count is the `shadow_checked` frame's own `bar_count` — shown only when a
// frame actually delivered one. A verdict without a frame renders the
// identity WITHOUT a count; the summary's `closed_trades` is never called a
// bar count. The money figures (r4.s4.w2) cross as the server's exact decimal
// text and are rendered as they come — this screen never adds, subtracts,
// rounds or re-formats one, and the "n of m" counts are the server's own two
// numbers, not a computed count.
//
// A stopped session is read-only (A4): no stop button, no shadow check.

import { useEffect, useRef, useState } from "react";

import { commands } from "../bindings";
import type { PaperSessionSummary, PaperShadowCheck, PaperTrades } from "../bindings";
import { CertBadge } from "../components/CertBadge";
import { SessionBadge, sessionBadgeState } from "../components/SessionBadge";
import { usePaperSession } from "../hooks/usePaperSession";

/** The em dash a null renders — a statement that no value exists. */
const EM_DASH = "—";

/** How the certificate row states a session's graduation. */
function certKind(summary: PaperSessionSummary): "certified" | "uncertified" | "fixture" | "stale" {
  if (summary.certification_stale) return "stale";
  if (summary.fixture) return "fixture";
  if (summary.graduation.graduation === "override") return "uncertified";
  return "certified";
}

export default function SessionDetail({
  sessionId,
  onChanged,
}: {
  sessionId: string;
  onChanged?: () => void;
}) {
  const view = usePaperSession(sessionId);
  const [trades, setTrades] = useState<PaperTrades | null>(null);
  const [serverEngine, setServerEngine] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);

  const tradesRequest = useRef(0);

  // The trades follow the summary: every refetch (a frame's trigger, a stop)
  // swaps the summary object, and the log is re-read with it. Only the newest
  // request's answer may land (r4.s4.w2): an older read that resolves after a
  // newer one must not overwrite the newer log.
  useEffect(() => {
    let alive = true;
    const request = ++tradesRequest.current;
    void commands.paperSessionTrades(sessionId).then((result) => {
      if (alive && request === tradesRequest.current && result.status === "ok") {
        setTrades(result.data);
      }
    });
    return () => {
      alive = false;
    };
  }, [sessionId, view.summary]);

  useEffect(() => {
    void commands.serverStatus().then((result) => {
      if (result.status === "ok") setServerEngine(result.data.engine_fingerprint);
    });
  }, []);

  const summary = view.summary;
  if (summary === null) {
    return (
      <aside className="drawer" aria-label="Session detail">
        <div className="ds-empty">
          {view.error ?? (view.stream === "refused" ? "connection refused, reconnect from Connect" : "Reading the session…")}
        </div>
      </aside>
    );
  }

  const stopped = summary.status.state === "stopped";
  const startedOn = summary.epochs[0] ?? null;
  // The certificate's own facts, narrowed from the graduation the server sent.
  const reason = summary.graduation.graduation === "override" ? summary.graduation.reason : null;
  const certifyingRun =
    summary.graduation.graduation === "certified" ? summary.graduation.walk_forward_run_id : null;
  const stoppedBy = summary.status.state === "stopped" ? summary.status.stopped_by : null;
  const stopActor =
    stoppedBy === null
      ? null
      : stoppedBy.token !== undefined
        ? stoppedBy.token.label
        : `stop_all by ${stoppedBy.stop_all.issuer}`;

  return (
    <aside className="drawer" aria-label="Session detail">
      <header className="drawer-head">
        <div className="dh-left">
          <span className="dc-pair-tag">{summary.pair}</span>
          <div className="dh-info">
            <span className="dh-name mono">{summary.strategy_version_id}</span>
            <span className="dh-ver mono">
              {summary.primary_timeframe}
              {summary.htf_timeframe === null ? "" : ` · ${summary.htf_timeframe}`}
              {summary.uses_d1 ? " · D1" : ""}
            </span>
          </div>
        </div>
        <SessionBadge state={sessionBadgeState(summary)} />
      </header>

      {/* The stream's own state (spec §4): a refusal is terminal — the screen
          says so and stops folding; `live` means frames are arriving. */}
      {view.stream === "refused" && (
        <p className="stream-note bear" role="status">
          connection refused, reconnect from Connect
        </p>
      )}
      {view.stream === "error" && (
        <p className="stream-note bear" role="status">
          {view.error ?? "the session stream failed"}
        </p>
      )}

      <div className="drawer-scroll">
        <section className="drawer-section dmeta">
          <div className="dm-row">
            <span className="dm-k">certificate</span>
            <span className="dm-v">
              <CertBadge kind={certKind(summary)} reason={reason} />
              {certifyingRun !== null && (
                <>
                  <span className="mono">{certifyingRun}</span>
                  <span className="dim">· wf-v1</span>
                </>
              )}
            </span>
          </div>
          {reason !== null && (
            <div className="dm-reason">
              <span>Override reason</span>“<b>{reason}</b>”
            </div>
          )}
          {summary.fixture && (
            <div className="dm-fixture">
              Seeded certify-fixture strategy. It exists to exercise certification and paper
              sessions end to end; its results are not an edge.
            </div>
          )}
          <div className="dm-row">
            <span className="dm-k">engine</span>
            <span className="mono dm-v">
              started on {startedOn ?? EM_DASH} · server now {serverEngine ?? EM_DASH}
            </span>
          </div>
          {/* #340: the newest consumed bar, as the server's own epoch-ms text
              (r4.s5 owns human units). */}
          <div className="dm-row">
            <span className="dm-k">last bar</span>
            <span className="mono dm-v">{summary.last_bar_open_time ?? EM_DASH}</span>
          </div>
          {summary.comparison.engine_builds > 1 && (
            <div className="dm-row">
              <span className="dm-k">span</span>
              <span className="dm-v">spans {summary.comparison.engine_builds} engine builds</span>
            </div>
          )}
          {stopped && (
            <div className="dm-stopped">
              Stopped by {stopActor ?? EM_DASH}. Read-only; promote the version again to start a
              new session.
            </div>
          )}
        </section>

        {/* The running total (grill Q2; r4.s4.w2): the realized totals with
            their "n of m" counts, the open position's unrealized USDT (or
            "flat") and their sum. Every figure is the server's own text, and
            the screen still computes no money — so the mock's equity chart is
            still absent, with the note that says exactly that. */}
        <section className="drawer-section">
          <header className="ds-head">
            <h3>Running total</h3>
          </header>
          <div className="pos-stats">
            <div className="ps-item">
              <span className="ps-lab">realized</span>
              <span className="ps-val mono">{summary.realized_usdt_total}</span>
              <span className="ps-count">
                {summary.realized_usdt_count} of {summary.closed_trade_count} trades
              </span>
            </div>
            <div className="ps-item">
              <span className="ps-lab">realized R</span>
              <span className="ps-val mono">{summary.realized_r_total}</span>
              <span className="ps-count">
                {summary.realized_r_count} of {summary.closed_trade_count} trades
              </span>
            </div>
            <div className="ps-item">
              <span className="ps-lab">unrealized</span>
              <span className="ps-val mono">
                {summary.open_position === null ? "flat" : (summary.unrealized_usdt ?? EM_DASH)}
              </span>
            </div>
            <div className="ps-item">
              <span className="ps-lab">session P&L</span>
              <span className="ps-val mono">{summary.session_pnl_usdt}</span>
            </div>
          </div>
          <p className="equity-placeholder">
            No equity chart: the paper API carries no equity series, and this screen computes no
            money.
          </p>
        </section>

        <section className="drawer-section">
          <header className="ds-head">
            <h3>Open position</h3>
            {summary.open_position === null && <span className="mono dim">flat</span>}
          </header>
          {summary.open_position === null ? (
            <div className="ds-empty">No open position.</div>
          ) : (
            <>
              <div className="pos-stats">
                <div className="ps-item">
                  <span className="ps-lab">side</span>
                  <span className="ps-val">{summary.open_position.side}</span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">entry</span>
                  <span className="ps-val mono">{summary.open_position.entry_price}</span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">size</span>
                  <span className="ps-val mono">{summary.open_position.qty}</span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">opened</span>
                  <span className="ps-val mono">{summary.open_position.entry_fill_time}</span>
                </div>
                {/* The mark and the money figures (r4.s4.w2): the server's own
                    text, the dash where the wire carries none. */}
                <div className="ps-item">
                  <span className="ps-lab">mark</span>
                  <span className="ps-val mono">{summary.open_position.mark_price ?? EM_DASH}</span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">mark time</span>
                  <span className="ps-val mono">{summary.open_position.mark_time ?? EM_DASH}</span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">unrealized</span>
                  <span className="ps-val mono">
                    {summary.open_position.unrealized_usdt ?? EM_DASH}
                  </span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">unrealized R</span>
                  <span className="ps-val mono">
                    {summary.open_position.unrealized_r ?? EM_DASH}
                  </span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">funding</span>
                  <span className="ps-val mono">{summary.open_position.funding_usdt}</span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">entry fee</span>
                  <span className="ps-val mono">{summary.open_position.entry_fee_usdt}</span>
                </div>
                {/* The live stops (#341): present only while a running engine
                    answers the read; a null renders the dash, and their
                    absence carries the one note below. */}
                <div className="ps-item">
                  <span className="ps-lab">initial stop</span>
                  <span className="ps-val mono">
                    {summary.open_position.initial_stop ?? EM_DASH}
                  </span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">stop</span>
                  <span className="ps-val mono">{summary.open_position.stop_price ?? EM_DASH}</span>
                </div>
                <div className="ps-item">
                  <span className="ps-lab">take profit</span>
                  <span className="ps-val mono">
                    {summary.open_position.take_profit_price ?? EM_DASH}
                  </span>
                </div>
              </div>
              {summary.open_position.initial_stop === null && (
                <p className="ds-empty">stops shown only while the session runs</p>
              )}
            </>
          )}
        </section>

        <section className="drawer-section">
          <header className="ds-head">
            <h3>Shadow backtest</h3>
            <span className="mono dim">replays recorded bars</span>
          </header>
          <ShadowPanel check={summary.shadow_checks.at(-1) ?? null} shadowBars={view.shadowBars} />
          {!stopped && (
            <button
              className="btn-sec-sm"
              disabled={checking}
              onClick={() => {
                setChecking(true);
                void commands
                  .paperShadowCheck(sessionId)
                  .then((result) => {
                    if (result.status === "error") setNotice(result.error.message);
                    view.reload();
                    onChanged?.();
                  })
                  .catch(() => setNotice("The shadow check call itself failed."))
                  .finally(() => setChecking(false));
              }}
            >
              Run shadow check
            </button>
          )}
        </section>

        <section className="drawer-section">
          <header className="ds-head">
            <h3>Out-of-sample comparison</h3>
          </header>
          <OosPanel summary={summary} />
        </section>

        {/* #344: every order and fill the log holds, latest first — the entry
            fills the old log dropped. Every value is the server's own text. */}
        <section className="drawer-section" aria-label="Orders and fills">
          <header className="ds-head">
            <h3>Orders and fills</h3>
            <span className="mono dim">latest first</span>
          </header>
          {trades === null || trades.entries.length === 0 ? (
            <div className="ds-empty">No orders or fills yet.</div>
          ) : (
            <div className="table-scroll">
              <table className="bt-trades">
                <thead>
                  <tr>
                    <th scope="col">kind</th>
                    <th scope="col">time</th>
                    <th scope="col">side</th>
                    <th scope="col">qty</th>
                    <th scope="col">price</th>
                    <th scope="col">reason</th>
                  </tr>
                </thead>
                <tbody>
                  {trades.entries
                    .slice()
                    .reverse()
                    .map((entry, index) => (
                      <tr key={`${entry.kind}-${entry.at}-${index}`}>
                        <td className="mono">{entry.kind}</td>
                        <td className="mono">
                          {entry.kind === "fill" ? entry.fill_time : entry.at}
                        </td>
                        <td className="mono">{entry.side}</td>
                        <td className="mono">{entry.qty}</td>
                        <td className="mono">{entry.kind === "fill" ? entry.price : EM_DASH}</td>
                        <td className="mono">
                          {entry.kind === "fill" ? (entry.exit_reason ?? EM_DASH) : EM_DASH}
                        </td>
                      </tr>
                    ))}
                </tbody>
              </table>
            </div>
          )}
        </section>

        <section className="drawer-section" aria-label="Trade log">
          <header className="ds-head">
            <h3>Closed trades</h3>
            <span className="mono dim">latest first</span>
          </header>
          {trades === null || trades.closed_trades.length === 0 ? (
            <div className="ds-empty">No closed trades yet.</div>
          ) : (
            <div className="table-scroll">
              <table className="bt-trades">
                <thead>
                  <tr>
                    <th scope="col">entry</th>
                    <th scope="col">exit</th>
                    <th scope="col">side</th>
                    <th scope="col">qty</th>
                    <th scope="col">entry px</th>
                    <th scope="col">exit px</th>
                    <th scope="col">reason</th>
                    <th scope="col">R</th>
                    <th scope="col">USDT</th>
                    <th scope="col">fee in</th>
                    <th scope="col">fee out</th>
                    <th scope="col">funding</th>
                  </tr>
                </thead>
                <tbody>
                  {trades.closed_trades
                    .slice()
                    .reverse()
                    .map((trade) => (
                      <tr key={`${trade.exit_fill_time}-${trade.exit_price}`}>
                        <td className="mono">{trade.entry_fill_time ?? EM_DASH}</td>
                        <td className="mono">{trade.exit_fill_time}</td>
                        <td className="mono">{trade.side}</td>
                        <td className="mono">{trade.qty}</td>
                        <td className="mono">{trade.entry_price ?? EM_DASH}</td>
                        <td className="mono">{trade.exit_price}</td>
                        <td className="mono">{trade.exit_reason}</td>
                        <td className="mono">{trade.realized_r ?? EM_DASH}</td>
                        <td className="mono">{trade.realized_usdt ?? EM_DASH}</td>
                        <td className="mono">{trade.entry_fee_usdt ?? EM_DASH}</td>
                        <td className="mono">{trade.exit_fee_usdt}</td>
                        <td className="mono">{trade.funding_usdt}</td>
                      </tr>
                    ))}
                </tbody>
              </table>
            </div>
          )}
        </section>

        {notice !== null && (
          <p className="ds-empty bear" role="alert">
            {notice}
          </p>
        )}
      </div>

      {!stopped && (
        <footer className="drawer-foot">
          {confirming ? (
            <>
              <span className="df-q">Stop this session? It stays listed, read-only.</span>
              <button className="btn-sec-sm" onClick={() => setConfirming(false)}>
                Cancel
              </button>
              <button
                className="btn-bear-solid-sm"
                onClick={() => {
                  setConfirming(false);
                  void commands
                    .paperStop(sessionId)
                    .then((result) => {
                      if (result.status === "error") setNotice(result.error.message);
                      view.reload();
                      onChanged?.();
                    })
                    .catch(() => setNotice("The stop call itself failed."));
                }}
              >
                Stop session
              </button>
            </>
          ) : (
            <>
              <span className="df-note">Agents can't stop sessions. Only this app can.</span>
              <button className="btn-bear-sec-sm" onClick={() => setConfirming(true)}>
                Stop session
              </button>
            </>
          )}
        </footer>
      )}
    </aside>
  );
}

/** The shadow verdict, with the bar count only when a frame delivered one. */
function ShadowPanel({ check, shadowBars }: { check: PaperShadowCheck | null; shadowBars: number | null }) {
  if (check === null) {
    return <div className="ds-empty">No shadow check recorded yet.</div>;
  }
  const result = check.result;
  if (result.verdict === "identical") {
    return (
      <div className="shadow-ok">
        <div>
          <b>
            {shadowBars === null
              ? "Shadow backtest: identical"
              : `Shadow backtest: identical over ${shadowBars} bars`}
          </b>
          <span>
            Every fill, exit and fee matches a replay of the bars this session recorded.{" "}
            {result.closed_trades} closed trades compared.
          </span>
        </div>
      </div>
    );
  }
  return (
    <div className="shadow-drift" role="alert">
      <div className="sd-head">
        <b>Session and shadow backtest differ</b>
      </div>
      <p>{result.first_divergence}</p>
      <div className="sd-table">
        <span className="sd-lab">session</span>
        <span className="mono sd-side">{result.live}</span>
        <span className="sd-lab">shadow</span>
        <span className="mono sd-side">{result.shadow}</span>
      </div>
      <div className="sd-actions">
        <button
          className="alert-cta"
          onClick={() => {
            const report = [
              `session ${check.engine_fingerprint}`,
              `first difference: ${result.first_divergence}`,
              `session side: ${result.live}`,
              `shadow side: ${result.shadow}`,
            ].join("\n");
            void navigator.clipboard?.writeText(report);
          }}
        >
          Copy defect report
        </button>
        <span>includes the engine {check.engine_fingerprint} and both records</span>
      </div>
    </div>
  );
}

/** The OOS comparison, rendered from the server's own verdict. */
function OosPanel({ summary }: { summary: PaperSessionSummary }) {
  const comparison = summary.comparison;
  switch (comparison.status) {
    case "not_applicable":
      return <div className="ds-empty">{comparison.reason}</div>;
    case "pending":
      return (
        <div className="oos-pending">
          Pending: {comparison.n} of {comparison.of} trades
        </div>
      );
    case "within":
    case "below":
    case "above": {
      const word = comparison.status;
      return (
        <>
          <p className="oos-verdict">
            Expectancy {comparison.live_mean_r}R · {word} the OOS range
          </p>
          <div className="rbar">
            <span className="rb-edge mono">{comparison.fold_min}</span>
            <span className="rb-track" />
            <span className="rb-edge mono">{comparison.fold_max}</span>
          </div>
          <p className="ds-empty">from the certifying run's fold expectancies ({comparison.n} counted trades)</p>
        </>
      );
    }
  }
}
