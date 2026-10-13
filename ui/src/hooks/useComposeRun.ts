// The compose-run state machine (r1.s1.w4) — the channel wiring, the
// event-to-step reduction, and the running / finalize / error state behind the
// Strategy Designer screen. Extracted from `DesignerScreen.tsx` (dispatch 2)
// so the streaming logic is unit-testable directly and the screen is markup
// plus wiring.
//
// One channel per invocation (grill A2): the channel IS the correlation
// between a submitted target and its agent turn. Each tool call opens a step
// the moment its `toolCallStarted` arrives and its outcome appends when the
// matching `toolCallResult` does — that one-by-one fold is `d1`'s observable.
// The finalize summary comes from the command's RETURN value, not the event
// channel; `started` mutates no step, and `finished` closes the last open step
// with its closing line (#317) — a SUCCESSFUL `finalize_strategy` never emits a
// `toolCallResult` of its own. A correctable refusal rides the result's own
// `outcome` text (#322), never a separate error channel.
//
// The channel alone is NOT enough correlation, which is why `inFlight` below
// exists. Three things it fixes, all of them cases where the channel's identity
// and the run's identity come apart:
//
//   1. **A late event patches the wrong turn.** The old fold patched whichever
//      agent turn was newest. A straggler from run A arriving after run B has
//      started would therefore append a step to B. Every event now carries a
//      `runId`; the run binds it on its first event and drops anything else.
//   2. **A dropped event goes unnoticed.** `seq` is contiguous from 0. A gap
//      means the step list no longer describes the run, so the turn ends as a
//      stream error instead of silently rendering a partial history as complete.
//   3. **Unmounting did not stop the run.** A `Channel`'s callback stays
//      registered with Tauri for the life of the webview, so navigating away
//      mid-compose left every backend send SUCCEEDING: the failed-send guard
//      never tripped and the remaining billable LLM calls ran to completion and
//      persisted a strategy nobody was waiting for. Cancellation is now
//      explicit — the cleanup calls `composeCancel(runId)`, which trips the
//      run's latch in the backend registry.

import { useEffect, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";

import { commands } from "../bindings";
import type { BusEvent, ComposeResult } from "../bindings";

/** The finalize payload, minus the null a cancelled run carries. */
export type StrategySummary = NonNullable<ComposeResult["strategy"]>;

/**
 * One streamed composer step: a tool call — opened by `toolCallStarted`,
 * closed by its result — or a `note`, the composer's own scrubbed prose
 * (r3.s2.w5, Q5 option 1), folded in WHERE it arrived between the tool steps.
 */
export type StepEntry =
  | {
      kind: "tool";
      name: string;
      preview: string;
      outcome: string | undefined;
      /**
       * The correctable refusal's own messages, when the outcome IS one
       * (r4.s5.w3, #322): the failure mark's reason line. Absent on every
       * success, so the success path's shape is unchanged.
       */
      failure?: string[];
      /**
       * Set on a call of a tool an EARLIER call of which was refused (#322):
       * the composer correcting itself reads as a retry, not a fresh step.
       */
      retry?: true;
    }
  | { kind: "note"; text: string };

export type RunStatus = "streaming" | "finalized" | "cancelled" | "error";

/** One agent message's run state — the step list and how it ended. */
export interface AgentTurn {
  status: RunStatus;
  steps: StepEntry[];
  summary: StrategySummary | undefined;
  error: string | undefined;
}

export type Message =
  | { kind: "user"; when: string; text: string }
  | { kind: "agent"; when: string; turn: AgentTurn };

/** A short local timestamp for the message metas. */
function clockLabel(): string {
  return new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/** A `FieldError`-shaped entry of a serialized correctable refusal — the one
 * shape `errors_to_content` sends, checked field by field so a success summary
 * that happens to parse as JSON is never read as a failure (#322). */
function isFieldError(value: unknown): value is { message: string } {
  if (typeof value !== "object" || value === null) {
    return false;
  }
  return (
    "path" in value &&
    typeof value.path === "string" &&
    "code" in value &&
    typeof value.code === "string" &&
    "message" in value &&
    typeof value.message === "string"
  );
}

/**
 * The messages of a correctable refusal, or `null` when the outcome is the
 * tool's success summary (#322).
 *
 * The stream carries no boolean for this: `toolCallResult`'s `outcome` IS the
 * answer. A success is the tool's own summary text; a refusal is one of the two
 * JSON shapes the composer sends — the serialized `FieldError` list
 * (`[{"path","code","message"}, …]`, `errors_to_content`) or the unknown-tool
 * object (`{"error":"unknown tool …"}`). Parsed and shape-checked, never guessed
 * from the wording.
 */
function toolFailureMessages(outcome: string): string[] | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(outcome);
  } catch {
    return null;
  }
  if (Array.isArray(parsed)) {
    const messages: string[] = [];
    for (const entry of parsed) {
      if (!isFieldError(entry)) {
        return null;
      }
      messages.push(entry.message);
    }
    return messages.length === 0 ? null : messages;
  }
  if (
    typeof parsed === "object" &&
    parsed !== null &&
    "error" in parsed &&
    typeof parsed.error === "string"
  ) {
    return [parsed.error];
  }
  return null;
}

/** Append a result to the LAST open tool step — of `name` when one is given
 * (names can repeat; a note never captures an outcome), of any tool otherwise:
 * the closing `finished` line names no tool and belongs to the step the composer
 * was on (`finalize_strategy`), which a successful run never closes with a
 * `toolCallResult` (#317). */
function attachOutcome(
  steps: StepEntry[],
  name: string | null,
  outcome: string,
  failure?: string[],
): StepEntry[] {
  let target = -1;
  for (let i = steps.length - 1; i >= 0; i -= 1) {
    const step = steps[i];
    if (
      step.kind === "tool" &&
      step.outcome === undefined &&
      (name === null || step.name === name)
    ) {
      target = i;
      break;
    }
  }
  if (target === -1) {
    return steps;
  }
  const copy = steps.slice();
  const step = copy[target];
  if (step.kind === "tool") {
    copy[target] =
      failure === undefined ? { ...step, outcome } : { ...step, outcome, failure };
  }
  return copy;
}

/** Fold one streamed event's payload into a run's turn — pure. */
function applyEvent(turn: AgentTurn, payload: BusEvent["payload"]): AgentTurn {
  if (payload.kind === "toolCallStarted") {
    // #322: a call of a tool an earlier call of which was refused is the
    // composer's correction — the same step shape, marked as the retry.
    const retried = turn.steps.some(
      (step) => step.kind === "tool" && step.name === payload.name && step.failure !== undefined,
    );
    return {
      ...turn,
      steps: [
        ...turn.steps,
        {
          kind: "tool",
          name: payload.name,
          preview: payload.argumentsPreview,
          outcome: undefined,
          ...(retried ? { retry: true as const } : {}),
        },
      ],
    };
  }
  if (payload.kind === "toolCallResult") {
    const failure = toolFailureMessages(payload.outcome);
    return {
      ...turn,
      steps: attachOutcome(
        turn.steps,
        payload.name,
        payload.outcome,
        failure === null ? undefined : failure,
      ),
    };
  }
  if (payload.kind === "finished") {
    // #317: the composer's closing line is the last step's own result line.
    return { ...turn, steps: attachOutcome(turn.steps, null, payload.message) };
  }
  if (payload.kind === "assistantText") {
    // The composer's own prose (r3.s2.w5): a note entry in stream order.
    return { ...turn, steps: [...turn.steps, { kind: "note", text: payload.text }] };
  }
  return turn;
}

/**
 * The highest `seq` a run's FIRST event may carry.
 *
 * The backend numbers a run's events contiguously from 0, where 0 is `Started`.
 * `Started` mutates no step, so losing only it is survivable and 1 is accepted as
 * a baseline. Anything higher means events were already dropped before the first
 * one arrived, and a baseline taken there would silently swallow that prefix —
 * a `toolCallResult` at seq 2 seen first would discard its unmatched outcome and
 * the run would still render as completed.
 */
const MAX_FIRST_SEQ = 1;

/** One submitted run: which turn it owns, and where its stream is. */
interface InFlight {
  /** The index of this run's agent turn in `messages` — fixed at submit. */
  index: number;
  /** The run id, bound from the first event that arrives (`null` until then). */
  runId: string | null;
  /** The `seq` the next accepted event must carry, or `null` before the first. */
  nextSeq: number | null;
  /** Set once a gap is seen: the step list is no longer a faithful history. */
  broken: boolean;
  /**
   * Set when the screen unmounted while this run had no id yet.
   *
   * The unmount cleanup can only cancel a run it can NAME, and the id is minted
   * backend-side and first observable on the `Started` event. Leaving immediately
   * after submitting therefore beats the id: the cleanup has nothing to send, and
   * without this flag the run would go on burning billable calls and persist a
   * strategy. The flag makes the cancel fire the moment the id arrives instead.
   */
  abandoned: boolean;
}

export function useComposeRun() {
  const [messages, setMessages] = useState<Message[]>([]);
  const [target, setTarget] = useState("");
  const [running, setRunning] = useState(false);

  // Refs, not state: a `Channel`'s callback is registered once at submit and must
  // keep working across renders, and none of this is rendered.
  const inFlight = useRef<InFlight | null>(null);
  const unmounted = useRef(false);

  /** Ask the backend to stop a run. Fire-and-forget: the screen may be gone. */
  const cancelRun = (runId: string) => {
    void commands.composeCancel(runId).catch(() => {
      // Nothing to render, and nothing to retry — the run also ends on its own
      // guard if this never lands.
    });
  };

  // Cancellation-on-unmount, for real. A `Channel`'s callback stays registered
  // with Tauri for the life of the WEBVIEW, so an SPA navigation makes no send
  // fail and the failed-send guard never trips: without an explicit cancel the
  // run keeps making billable calls and persists a strategy nobody awaits.
  useEffect(() => {
    // Reset on SETUP, not only on cleanup. React StrictMode runs setup/cleanup
    // twice on mount in development; without this the first cleanup would latch
    // `unmounted` true forever and the second setup would leave it there, so
    // every event and every terminal result would be dropped by `patchTurn` —
    // the backend work would still happen while the UI sat frozen on
    // "streaming".
    unmounted.current = false;
    return () => {
      unmounted.current = true;
      const run = inFlight.current;
      if (run === null) {
        return;
      }
      if (run.runId === null) {
        // Left before the id was knowable. `handleEvent` fires the cancel the
        // moment the first event names the run.
        run.abandoned = true;
        return;
      }
      cancelRun(run.runId);
    };
  }, []);

  /** Patch the turn at `index`, leaving every other run untouched. */
  const patchTurn = (index: number, patch: (turn: AgentTurn) => AgentTurn) => {
    if (unmounted.current) {
      return;
    }
    setMessages((current) => {
      const message = current[index];
      if (message === undefined || message.kind !== "agent") {
        return current;
      }
      const copy = current.slice();
      copy[index] = { ...message, turn: patch(message.turn) };
      return copy;
    });
  };

  /**
   * Fold one streamed event into the run it belongs to — or drop it.
   *
   * Takes the run's OWN record rather than reading `inFlight.current`. A channel
   * outlives the run that made it, so a straggler from a settled run A can arrive
   * after run B has started; reading the current record would let A's event bind
   * B's `runId` to A's, after which every genuine B event is rejected as foreign
   * and B finalizes with an empty step list.
   */
  const handleEvent = (run: InFlight, event: BusEvent) => {
    if (run.runId === null) {
      run.runId = event.runId;
      if (run.abandoned) {
        // The screen left before this id existed; this is the first moment the
        // run can be named, so cancel it now and fold nothing.
        cancelRun(run.runId);
        return;
      }
    } else if (run.runId !== event.runId) {
      return;
    }
    if (run.abandoned || run.broken) {
      return;
    }

    // A gap. The steps rendered so far are a partial history and nothing
    // downstream can say which are missing, so the run ends as a stream error
    // rather than presenting an incomplete list as the whole story — and the
    // backend is cancelled, because a user who sees only the error will retry,
    // and an uncancelled run would persist a duplicate strategy behind it.
    const expected = run.nextSeq ?? 0;
    const gapped =
      run.nextSeq === null ? event.seq > MAX_FIRST_SEQ : event.seq !== run.nextSeq;
    if (gapped) {
      run.broken = true;
      patchTurn(run.index, (turn) => ({
        ...turn,
        status: "error",
        error: `stream error: expected event ${expected}, received ${event.seq} — the step list below is incomplete`,
      }));
      if (run.runId !== null) {
        cancelRun(run.runId);
      }
      return;
    }
    run.nextSeq = event.seq + 1;
    patchTurn(run.index, (turn) => applyEvent(turn, event.payload));
  };

  const submit = () => {
    const text = target.trim();
    if (text === "" || running) {
      return;
    }
    setTarget("");
    setRunning(true);

    // The agent turn's index is deterministic: `submit` is the only thing that
    // appends, it appends the user message then the agent message, and the
    // `running` guard means no other run is in flight to move them.
    const index = messages.length + 1;
    const run: InFlight = {
      index,
      runId: null,
      nextSeq: null,
      broken: false,
      abandoned: false,
    };
    inFlight.current = run;

    // The channel's callback closes over THIS run's record, so a straggler
    // arriving after the run settles can never be mistaken for the next one's.
    const channel = new Channel<BusEvent>();
    channel.onmessage = (event) => {
      handleEvent(run, event);
    };

    const when = clockLabel();
    setMessages((current) => [
      ...current,
      { kind: "user", when, text },
      { kind: "agent", when, turn: { status: "streaming", steps: [], summary: undefined, error: undefined } },
    ]);

    void commands
      .composeStrategy(text, channel)
      .then((result) => {
        // A run whose stream broke keeps its stream error: the command's return
        // value describes work the rendered step list cannot account for, so
        // overwriting the error with `✓ completed` would hide the gap.
        if (run.broken) {
          return;
        }
        if (result.status === "ok") {
          patchTurn(index, (turn) => ({
            ...turn,
            status: result.data.cancelled ? "cancelled" : "finalized",
            summary: result.data.strategy ?? undefined,
          }));
        } else {
          patchTurn(index, (turn) => ({
            ...turn,
            status: "error",
            error: result.error.message,
          }));
        }
      })
      .catch((error: unknown) => {
        // A non-serialized failure (an unreachable command, a broken bridge)
        // still renders — the no-silent-failure rule covers this path too.
        patchTurn(index, (turn) => ({
          ...turn,
          status: "error",
          error: error instanceof Error ? error.message : String(error),
        }));
      })
      .finally(() => {
        setRunning(false);
        if (inFlight.current === run) {
          inFlight.current = null;
        }
      });
  };

  return { messages, target, setTarget, running, submit };
}
