//! The coach turn's transport posture and chat knobs — ONE place, in the adapter
//! ring, for both coach surfaces (#164, PR #165 review R5/R6/R7/R10).
//!
//! `pulse coach` (the debug verb) and the desktop `coach_turn` bus command ask the
//! same question of the same model and must ask it the same way. They did not: the
//! CLI borrowed `cli::llm`'s reasoning cap and the desktop borrowed the COMPOSER's
//! config, so the coach's cap and temperature were numbers nobody had chosen for the
//! coach. Fixing that by having the product ring import from `mod cli` would trade
//! one drift for a worse dependency — the desktop is the product surface and `cli`
//! is a developer one — so the shared knobs live here, beside the transport they
//! configure, and both surfaces import them. (`capturing` moved out of `mod cli`
//! for the same reason at r1.s4.w2.)
//!
//! **The effective per-turn generation budget is `min(cap, timeout × throughput)`**
//! — [`COACH_MAX_TOKENS`] and [`COACH_TIMEOUT_SECS`] are a coupled pair, not two
//! independent knobs. glm-5.3-flash generated 5 615–9 074 output tokens at ~186
//! tok/s on the #164 turns and 8 093 tokens in 59s (~137 tok/s) on the review's,
//! so throughput is a RUNTIME property no test can pin: raising the cap without
//! raising the timeout buys nothing past what the timeout allows, and raising the
//! timeout without raising the cap buys nothing past the cap.
//!
//! **Why the transport fires before the turn guard.** The coach turn is wrapped in
//! `agent::DEFAULT_TURN_TIMEOUT` (120s), and this transport's timeout is
//! deliberately SHORTER. A transport timeout comes back as a typed
//! `TransportFailure` THROUGH the redacting-logging decorator, which has already
//! written the call's ledger row; the guard-owned `ProviderTimeout` fires outside
//! the decorator and abandons a call the operator has been billed for, with no
//! `llm_call` row to show for it (#129 tracks that structural gap). The advertised
//! turn budget is the guard; the usable GENERATION budget is this timeout; the
//! difference is [`COACH_LEDGER_WRITE_MARGIN_SECS`], the room the ledger write
//! needs to finish inside the guard once the response is in hand.

use std::time::Duration;

use crate::adapters::llm::openai_compat::{OLLAMA_MODEL_ID, OpenAiCompatProvider};
use crate::agent::DEFAULT_TURN_TIMEOUT;
use crate::domain::{LlmBackend, LlmConfig};

/// The coach's RESPONSE TOKEN CAP — the coach's own answer to the question, not the
/// CLI reasoning constant it used to borrow (#164).
///
/// glm-5.3-flash reasons BEFORE it calls a tool, and its thinking tokens are billed
/// against this cap. On the real run `23e890d0` the reasoning alone ran 7 176 and
/// 9 074 output tokens; under the old 4 096 the provider returned
/// `finish_reason: "length"` with an empty `content` and NO tool call, which the
/// taxonomy could only record as `ZeroCalls` — a cap that was too small, reading as
/// a model that declined. 16 384 clears the worst turn measured with headroom, and
/// the endpoint accepts it.
///
/// Separate from `cli::llm::REASONING_MAX_TOKENS` on purpose: `llm-check` sends a
/// one-sentence prompt and the coach sends a whole backtest, so one number cannot
/// be right for both. Changing it moves the coach's REQUEST FINGERPRINT (the
/// single-flight key feeds `max_tokens`), which is deliberate: a turn asked under a
/// different cap is a different request.
pub(crate) const COACH_MAX_TOKENS: u32 = 16_384;

/// The coach's sampling temperature — 0.0, the most deterministic setting the wire
/// offers, for BOTH surfaces.
///
/// The CLI already sent 0.0 while the desktop rail sent the composer's 0.2 (it was
/// wired to `compose_config`), so the two surfaces asked the same question two ways
/// and neither was chosen on purpose. Like the cap, this feeds the request
/// fingerprint.
pub(crate) const COACH_TEMPERATURE: f32 = 0.0;

/// The coach's REQUEST TIMEOUT, in seconds — longer than the 60s every other
/// surface sends, and shorter than the turn guard by exactly
/// [`COACH_LEDGER_WRITE_MARGIN_SECS`].
///
/// A coach turn that spends its whole [`COACH_MAX_TOKENS`] budget generates for
/// well over a minute, so the 60s posture would cut off exactly the reasoning the
/// larger cap exists to permit. It stays under `agent::DEFAULT_TURN_TIMEOUT` so the
/// TRANSPORT is what fires on a slow turn — see the module doc for why a typed
/// `TransportFailure` with its ledger row beats a guard-owned `ProviderTimeout`
/// that abandons a billable call (#129).
pub(crate) const COACH_TIMEOUT_SECS: u64 = 100;

/// The seconds reserved between [`COACH_TIMEOUT_SECS`] and the turn guard for the
/// guard-wrapped ledger write that follows a response.
///
/// `run_coach_turn` calls the provider and persists the `LlmCall` INSIDE the same
/// `DEFAULT_TURN_TIMEOUT` guard, so a response that arrives at the very edge of the
/// transport timeout still needs time to be priced, redacted and written. Without
/// that room a SUCCESSFUL slow call settles as `ProviderTimeout` with
/// `llm_call_id = NULL` — the billed call with no row, again. 20s is the reserve;
/// `the_coach_timeout_leaves_the_ledger_write_room_inside_the_turn_guard` is what
/// keeps the two numbers honest when either moves.
pub(crate) const COACH_LEDGER_WRITE_MARGIN_SECS: u64 = 20;

/// The budget arithmetic, checked at COMPILE time: the request timeout plus the
/// ledger-write margin must fit inside the turn guard.
///
/// A test would catch a bad edit too, and one below does; this catches it in the
/// build, which is where a three-constant relationship nobody re-derives by hand
/// belongs. `DEFAULT_TURN_TIMEOUT` is the outer bound, so it is the term that must
/// not be exceeded rather than one of the two that must fit.
const _: () =
    assert!(COACH_TIMEOUT_SECS + COACH_LEDGER_WRITE_MARGIN_SECS <= DEFAULT_TURN_TIMEOUT.as_secs());

/// The coach chat config — the ONE place both coach surfaces read their chat knobs
/// from (`cli::coach::run_coach` and `tauri::commands::coach_turn`).
///
/// MODEL resolves the config `[llm].model` override → the shipped
/// [`OLLAMA_MODEL_ID`] fallback; the CAP and the TEMPERATURE are the coach's own
/// ([`COACH_MAX_TOKENS`] / [`COACH_TEMPERATURE`]). The composer keeps its own
/// config untouched — a coach turn and a composer step are not the same size of
/// question.
///
/// The fallback is the ADAPTER's model const rather than `cli::compose`'s so that
/// nothing in the product path reaches into the CLI ring for it; `agent::config`'s
/// `every_model_id_site_agrees_with_the_shipped_config` (#126) holds all three
/// compiled-in ids equal to the shipped `[llm].model`, so this is the same value
/// the composer falls back to, not a second opinion about it.
pub(crate) fn coach_config(model_override: Option<&str>) -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: model_override.unwrap_or(OLLAMA_MODEL_ID).to_owned(),
        temperature: COACH_TEMPERATURE,
        max_tokens: COACH_MAX_TOKENS,
    }
}

/// The coach's transport: ONE upstream attempt per turn (PR #128, finding H1) at
/// the coach's own [`COACH_TIMEOUT_SECS`].
///
/// `run_turn` records one exchange and names one ledger row, and it neither retries
/// nor nudges (grill L3). The adapter's default posture retries a transient 429/5xx
/// twice, which would put three upstream attempts — and their cost — behind that one
/// record. The composer and `llm-check` keep the retrying default: neither records
/// one exchange per attempt.
///
/// A function rather than an inline `match` at each surface because the posture is
/// otherwise unobservable AND unenforced: `OpenAiCompatProvider` cannot make a
/// caller ask for one attempt, so the seam that CAN be asserted is this one. Both
/// coach sites build through it, and `tests/tauri_coach.rs`'s source scan is what
/// keeps a future edit from constructing a provider inline again.
pub(crate) fn coach_provider(api_key: &str, base_url: Option<&str>) -> OpenAiCompatProvider {
    OpenAiCompatProvider::single_attempt_with_timeout(
        api_key.to_owned(),
        base_url,
        Duration::from_secs(COACH_TIMEOUT_SECS),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        COACH_LEDGER_WRITE_MARGIN_SECS, COACH_MAX_TOKENS, COACH_TEMPERATURE, COACH_TIMEOUT_SECS,
        DEFAULT_TURN_TIMEOUT, coach_config, coach_provider,
    };
    use crate::domain::LlmBackend;

    /// The coach's OUTPUT CAP is its own constant, and the coach config is the one
    /// place both surfaces read it from (#164). The old wiring answered the same
    /// question twice — the CLI took `cli::llm::REASONING_MAX_TOKENS` (4096) and the
    /// desktop took the composer's `compose_config` (4096, temperature 0.2) — and
    /// a reasoning model that spends 7k-9k output tokens before its tool call was
    /// cut off by both, which the taxonomy could only record as `ZeroCalls`.
    #[test]
    fn the_coach_config_carries_the_coach_output_cap() {
        let config = coach_config(None);
        assert_eq!(config.backend, LlmBackend::Ollama);
        assert_eq!(
            config.max_tokens, COACH_MAX_TOKENS,
            "the coach reads its own cap, not the CLI reasoning constant"
        );
        assert_eq!(
            COACH_MAX_TOKENS, 16_384,
            "the cap is the value #164's real turns qualified"
        );
        assert!(
            config.max_tokens > crate::cli::llm::REASONING_MAX_TOKENS,
            "and it is bigger than the cap that produced the empty completions"
        );
        // Bit-compared: the wire carries this f32 exactly, and an approximate
        // assertion would pass for a temperature the model would not sample at.
        assert_eq!(config.temperature.to_bits(), COACH_TEMPERATURE.to_bits());
        assert_eq!(
            COACH_TEMPERATURE.to_bits(),
            0.0_f32.to_bits(),
            "one deterministic posture for both surfaces"
        );
    }

    /// MODEL still resolves the config `[llm].model` override → the shipped const
    /// fallback, exactly as the composer's does: the cap is what forks, not the
    /// model resolution. `agent::config`'s identity test (#126) is what keeps this
    /// fallback equal to the composer's.
    #[test]
    fn the_coach_config_prefers_the_configured_model() {
        assert_eq!(coach_config(Some("kimi-k2.6")).model, "kimi-k2.6");
        assert_eq!(
            coach_config(None).model,
            crate::adapters::llm::openai_compat::OLLAMA_MODEL_ID,
            "no override falls back to the shipped model id"
        );
    }

    /// The coach's transport posture is chosen HERE, so it is proven here (PR #128,
    /// finding H1). `OpenAiCompatProvider` cannot enforce it — a caller reaching for
    /// `new` still retries — which is exactly why the composition site is the thing
    /// worth asserting.
    #[test]
    fn the_coach_provider_makes_one_attempt_per_turn_at_the_coach_timeout() {
        for base_url in [None, Some("https://example.test/v1")] {
            let provider = coach_provider("k", base_url);
            assert_eq!(
                provider.max_retries(),
                0,
                "the coach path attempts once, base-url override or not"
            );
            assert_eq!(
                provider.timeout_secs(),
                COACH_TIMEOUT_SECS,
                "and waits out a full reasoning budget either way"
            );
        }
    }

    /// The COMPOSER did not move when the coach forked (review V1).
    ///
    /// The coach's cap and temperature are the whole change; the composer's are the
    /// baseline that change is measured against, and `compose_config`'s own test
    /// pins only `max_tokens >= 4096` and says nothing about temperature — so a
    /// later edit that raised the composer to the coach's numbers, or dropped its
    /// 0.2 to the coach's 0.0, would pass every gate while making the ledger's
    /// composer rows a different question than the ones already recorded.
    #[test]
    fn the_composer_config_did_not_move_when_the_coach_forked() {
        let composer = crate::cli::compose::compose_config(None);
        assert_eq!(
            composer.max_tokens, 4096,
            "the composer keeps the 4096 cap the coach outgrew"
        );
        assert_eq!(
            composer.temperature.to_bits(),
            0.2_f32.to_bits(),
            "and keeps its 0.2 sampling temperature"
        );
        assert!(
            COACH_MAX_TOKENS > composer.max_tokens,
            "the fork exists because a coach turn is a bigger question than a \
             composer step"
        );
    }

    /// The transport timeout and the turn guard are ONE budget with a reserve in it,
    /// so they are asserted together rather than separately (review R1/R11).
    ///
    /// `<` alone would pass at 119s, which leaves a second for a priced, redacted
    /// ledger write and turns a slow SUCCESS into a `ProviderTimeout` with no row.
    /// The margin is what the sum has to respect.
    #[test]
    fn the_coach_timeout_leaves_the_ledger_write_room_inside_the_turn_guard() {
        assert_eq!(COACH_TIMEOUT_SECS, 100);
        assert_eq!(COACH_LEDGER_WRITE_MARGIN_SECS, 20);
        assert!(
            COACH_TIMEOUT_SECS + COACH_LEDGER_WRITE_MARGIN_SECS <= DEFAULT_TURN_TIMEOUT.as_secs(),
            "the transport must return early enough for the guard-wrapped ledger \
             write to finish inside the turn guard"
        );
    }
}
