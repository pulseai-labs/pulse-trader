// The Strategy Designer screen (r1.s1.w4) — the release's core journey's
// frontend half.
//
// The screen is a conversation: a user bubble per submitted natural-language
// target, one agent message per compose run whose step list renders the
// structured events the `compose_strategy` command streams over its
// per-invocation channel. The run's state machine — channel wiring, the
// event-to-step reduction, the running / finalize / error state — lives in
// `useComposeRun` (dispatch 2's extraction); this file renders it.
//
// What this screen deliberately does NOT do (the r1-honest composition):
//
//   - No live DSL-preview pane. The composer exposes no draft DSL mid-run —
//     `Finalized` is the first moment a version exists — so a "live · synced"
//     preview would be a fabricated intermediate state. The finalize summary
//     card, rendered from the command's RETURN value, is the honest
//     presentation.
//   - No cost pill, no session metering, no mutation flow (clone/draft/compare)
//     — none of that machinery exists this round; this compose creates a NEW
//     strategy + its initial version, and the card renders exactly the fields
//     the outcome carries (name, version id, created-by, LlmCall count, DSL
//     lines) and omits what it does not.
//   - No cancel button. Cancellation is wired to LEAVING, not to a control:
//     `useComposeRun`'s unmount cleanup calls `compose_cancel(runId)`, which
//     trips the run's latch in the backend registry so the composer refuses at
//     its next model turn and persists nothing. A visible cancel button would
//     need a run-state affordance and a confirmed-stopped state this round does
//     not build, and a dead one is the broken-link defect at button scale.
//   - No credential is requested, displayed, or echoed anywhere. The banner
//     (`w5`) states the no-credential condition globally; an unresolvable
//     credential surfaces HERE as the run's rendered `BusError` message —
//     no silent failure.

import { useComposeRun } from "../hooks/useComposeRun";
import type { AgentTurn, RunStatus, StrategySummary } from "../hooks/useComposeRun";

export default function DesignerScreen() {
  const { messages, target, setTarget, running, submit } = useComposeRun();

  return (
    <div className="designer">
      <div className="dsg-scroll">
        <div className="dsg-chat">
          {messages.length === 0 && (
            <div className="dsg-empty">
              <p className="dsg-empty-title">Describe a strategy in natural language.</p>
              <p className="dsg-empty-body">
                The composer builds it with its builder tools; each tool call streams in
                here as it happens, and the finished strategy is saved to your library
                as a new version.
              </p>
            </div>
          )}
          {messages.map((message, index) =>
            message.kind === "user" ? (
              <div key={index} className="dsg-msg dsg-msg-user">
                <div className="dsg-msg-meta">
                  <span>you</span>
                  <span className="dsg-when">{message.when}</span>
                </div>
                <div className="dsg-msg-body">{message.text}</div>
              </div>
            ) : (
              <div key={index} className="dsg-msg dsg-msg-agent">
                <div className="dsg-rail" />
                <div className="dsg-agent-body">
                  <div className="dsg-msg-meta">
                    <span className="dsg-agent-name">✦ Composer</span>
                    <span className="dsg-when">{message.when}</span>
                  </div>
                  <StepList turn={message.turn} />
                  {message.turn.summary !== undefined && (
                    <SummaryCard summary={message.turn.summary} />
                  )}
                  {message.turn.status === "error" && message.turn.error !== undefined && (
                    <div className="dsg-error" role="alert">
                      {message.turn.error}
                    </div>
                  )}
                  {message.turn.status === "cancelled" && (
                    <div className="dsg-error" role="alert">
                      The run was cancelled — this screen went away before it finished.
                    </div>
                  )}
                </div>
              </div>
            ),
          )}
        </div>
      </div>

      <div className="dsg-input">
        <textarea
          className="dsg-textarea"
          placeholder="Describe a strategy in natural language…"
          value={target}
          onChange={(event) => setTarget(event.target.value)}
          onKeyDown={(event) => {
            // `isComposing` guards the IME: for Japanese, Chinese and Korean
            // input the Enter that CONFIRMS a candidate reaches this handler
            // too, and submitting there would clear the textarea and start a
            // billable run on half-typed text.
            if (event.nativeEvent.isComposing) {
              return;
            }
            if (event.key === "Enter" && !event.shiftKey) {
              event.preventDefault();
              submit();
            }
          }}
        />
        <div className="dsg-input-foot">
          <span className="dsg-hint">enter to send · shift-enter for newline</span>
          <button type="button" className="dsg-send" disabled={running} onClick={submit}>
            {running ? (
              <>
                <span className="spinner-sm" /> Composer is running…
              </>
            ) : (
              "Send"
            )}
          </button>
        </div>
      </div>
    </div>
  );
}

/** The header marker and word for each terminal status — one per status, no default.
 *
 * A two-way `streaming ? … : "✓ completed"` branch rendered "✓ completed" above
 * the error box for every failed and cancelled run, because both are simply
 * "not streaming". Each terminal state names itself instead.
 */
const TERMINAL_LABEL: Record<Exclude<RunStatus, "streaming">, string> = {
  finalized: "✓ completed",
  error: "✕ failed after",
  cancelled: "⊘ cancelled after",
};

/** The step list of one run, with its honest running/completed header. */
function StepList({ turn }: { turn: AgentTurn }) {
  if (turn.steps.length === 0 && turn.status !== "streaming") {
    return null;
  }
  // r3.s2.w5: the composer's prose notes fold into the list, but both counts
  // stay counts of TOOL CALLS — the finalize summary counts LLM calls, so a
  // note must never inflate what the headers report.
  const toolCount = turn.steps.filter((step) => step.kind === "tool").length;
  const calls = `${toolCount} tool ${toolCount === 1 ? "call" : "calls"}`;
  return (
    <div className="dsg-steps">
      <div className="dsg-steps-head">
        {turn.status === "streaming" ? (
          <>
            <span className="spinner-sm" /> streaming · {toolCount}{" "}
            {toolCount === 1 ? "step" : "steps"} so far
          </>
        ) : (
          <>
            {TERMINAL_LABEL[turn.status]} · {calls}
          </>
        )}
      </div>
      <div className="dsg-steps-body">
        {turn.steps.map((step, index) => {
          if (step.kind === "note") {
            // The composer's own scrubbed prose (r3.s2.w5) — a plain text
            // row between the tool steps, never a step with an outcome.
            return (
              <div key={index} className="dsg-note">
                {step.text}
              </div>
            );
          }
          const open = step.outcome === undefined && turn.status === "streaming";
          return (
            <div key={index} className={`dsg-step${open ? " is-running" : " is-done"}`}>
              <div className="dsg-step-marker">
                {step.outcome !== undefined ? (
                  "✓"
                ) : open ? (
                  <span className="spinner-sm" />
                ) : (
                  "·"
                )}
              </div>
              <div className="dsg-step-body">
                <div className="dsg-step-row">
                  <code className="dsg-step-name">{step.name}</code>
                  <code className="dsg-step-preview">{step.preview}</code>
                </div>
                {step.outcome !== undefined && (
                  <div className="dsg-step-outcome">{step.outcome}</div>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

/** One titled block of the summary card. */
function SscBlock({
  title,
  lines,
}: {
  title: string;
  lines: readonly string[];
}) {
  return (
    <div className="dsg-ssc-block">
      <div className="dsg-ssc-block-head">{title}</div>
      <div className="dsg-ssc-block-body">
        {lines.map((line, index) => (
          <code key={index}>{line}</code>
        ))}
      </div>
    </div>
  );
}

/** The finalize summary card — rendered from the command's return value only. */
function SummaryCard({ summary }: { summary: StrategySummary }) {
  return (
    <div className="dsg-ssc">
      <div className="dsg-ssc-head">
        <span className="dsg-ssc-tag">strategy preview</span>
        <span className="dsg-ssc-name">{summary.strategyName}</span>
        <span className="dsg-ssc-ver">{summary.versionId}</span>
      </div>
      <div className="dsg-ssc-grid">
        <SscBlock title="Setup" lines={[summary.dsl.direction]} />
        <SscBlock title="Entries" lines={[summary.dsl.entry]} />
        {summary.dsl.filters.length > 0 && (
          <SscBlock title="Filters" lines={summary.dsl.filters} />
        )}
        <SscBlock title="Exits" lines={summary.dsl.exits} />
        <SscBlock title="Risk" lines={summary.dsl.risk} />
      </div>
      <div className="dsg-ssc-foot">
        <span className="dsg-ssc-meta">
          {summary.createdBy} · {summary.llmCallCount} LLM{" "}
          {summary.llmCallCount === 1 ? "call" : "calls"}
        </span>
      </div>
    </div>
  );
}
