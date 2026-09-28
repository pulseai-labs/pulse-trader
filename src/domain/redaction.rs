//! The PURE text-redaction kernel (moved into the domain ring by r1.s4.w2,
//! `pulseai-labs/pulse-trader#150`; authored at VS-1.3.1 work-1.04 — NFR-6, ADR-0016,
//! README C7).
//!
//! **Why it lives here now.** [`Redactor`] is a total function over text. It holds
//! no credential, reads no environment, no file and no Keychain, opens no socket,
//! and depends on nothing under `crate::adapters`. Its only inputs are the caller's
//! tagged secret VALUES and the strings to scrub. It sat in `adapters::llm` for
//! historical reasons, and the application ring's coach turn imported it from there
//! — which made ADR-0015's "exactly ONE deliberate adapters import" into two. Moving
//! the pure half inward removes the second exception without weakening anything:
//! provider concerns (the transport, the price table, the ledger write) stay in
//! `src/adapters/llm/`, and credential HANDLING stays in `src/adapters/llm/` and
//! `src/adapters/secrets.rs` (ADR-0012 / ADR-0016).
//!
//! **The behaviour is unchanged, deliberately and provably.** Nothing in this file
//! was rewritten in the move: the same two rules (caller-tagged secret VALUES,
//! longest-first; then structurally API-key-shaped word tokens through the shared
//! [`looks_like_secret_token`](crate::domain::looks_like_secret_token) kernel), the
//! same placeholder, the same hand-written `Debug` that never prints a tagged
//! secret, the same recursive JSON-leaf scrub. `tests/coach_redaction.rs`'s canaries
//! and `tests/credential_redaction.rs` are the proof, and neither was edited.
//!
//! **Redaction still happens BEFORE persist** (ADR-0016): the decorator in
//! `src/adapters/llm/redacting_logging.rs` calls the inner provider with the REAL
//! bytes and passes a COPY through here on its way into the ledger. Moving the
//! function did not move the call site, so the audit trail is byte-identical.

use serde_json::Value;

use crate::domain::llm::{Message, ToolCall};

/// The placeholder substituted for every redacted span in the persisted copy.
pub(crate) const REDACTED: &str = "«REDACTED»";

/// The byte bound on provider-derived detail that reaches a persisted row or
/// a log (r3.s1.w5, #172). Moved here from the adapter: this is the ONE bound,
/// owned by the seam that applies it AFTER scrubbing.
pub const TRANSPORT_DETAIL_MAX_BYTES: usize = 4096;

/// The explicit marker a cut detail ends in, so a reader can tell the provider
/// body was truncated rather than complete. Applied only by
/// [`scrub_then_bound`] — `tests/llm_scrub_seam.rs`'s structural guard fails
/// any second application outside this file.
pub const DETAIL_TRUNCATED: &str = "[truncated]";

/// The ONE seam provider-derived detail crosses from the provider to storage
/// (r3.s1.w5, #172's sixth recurrence; ADR-0016): redact the WHOLE string
/// first — the exact-match replacement needs the credential intact to find it
/// — then bound the scrubbed result at a UTF-8 boundary ending in
/// [`DETAIL_TRUNCATED`].
///
/// The order is the invariant. Bounding before scrubbing — what the adapter
/// did until this item — cuts a credential straddling the boundary into a
/// fragment neither the exact-value nor the structural redaction recognizes,
/// and the fragment is persisted in the immutable row. Every caller that
/// persists or logs provider-derived detail goes through here; nothing may
/// truncate provider detail anywhere else (`tests/llm_scrub_seam.rs`'s
/// structural guard).
#[must_use]
pub fn scrub_then_bound(redactor: &Redactor, detail: &str, max_bytes: usize) -> String {
    let scrubbed = redactor.redact(detail);
    bound_at_utf8_boundary(scrubbed, max_bytes)
}

/// Cut `text` to at most `max_bytes` at a UTF-8 char boundary, ending a cut in
/// [`DETAIL_TRUNCATED`]. A string that already fits is returned unchanged.
fn bound_at_utf8_boundary(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let Some(room) = max_bytes.checked_sub(DETAIL_TRUNCATED.len()) else {
        // A bound smaller than the marker itself carries only the marker.
        return DETAIL_TRUNCATED.to_owned();
    };
    let mut end = room;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(DETAIL_TRUNCATED);
    text
}

/// A scoped, data-driven secret scrubber for the PERSISTED copy of a prompt +
/// completion (NFR-6, README C7, audit ch1).
///
/// v1 strips exactly two things and nothing else:
/// - **API-key-shaped tokens** — a conservative structural pattern (a known key
///   prefix — `sk-`/`sk_`/`pk-`/`ghp_`/`gho_`/`xox`/`akia` — or a long
///   mixed-alphanumeric run), shared with the compose-time scrub via the
///   [`looks_like_secret_token`](crate::domain) kernel so the at-rest copy is
///   never weaker than compose-time (slice-close FIX C). This is detection LOGIC,
///   not secret DATA, so it lives in code.
/// - **Caller-declared tagged secrets** — the exact secret VALUES the caller
///   marks as sensitive (e.g. a live key value), supplied as DATA via
///   [`from_config`](Self::from_config) so no production secret is a public-Rust
///   literal (decision 4).
///
/// It deliberately does **not** strip bare numbers / balances / R-multiples: they
/// share no lexical shape, so a numeric regex would erase the very context the
/// coach (1.3.3) needs. Structural balance/account-ID redaction is deferred to
/// 1.3.2/1.3.3, done at prompt-composition time where the field is known.
#[derive(Clone, Default)]
pub struct Redactor {
    tagged_secrets: Vec<String>,
}

// Hand-written `Debug` that NEVER prints the tagged secret VALUES (they are raw
// secrets — e.g. the live GLM API key is tagged in via `from_config` at the CLI
// composition root). A derived `Debug` would dump them in the clear through any
// `dbg!`/`tracing::debug!(?redactor)`/Debug-deriving wrapper — defeating the whole
// leak-at-rest purpose (slice-close close-audit finding R1-2).
impl std::fmt::Debug for Redactor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Redactor")
            .field(
                "tagged_secrets",
                &format_args!("<{} redacted>", self.tagged_secrets.len()),
            )
            .finish()
    }
}

impl Redactor {
    /// Build a `Redactor` from loaded config DATA: the exact secret VALUES the
    /// caller has tagged as sensitive. API-key-shaped stripping is always on (it
    /// is structural, not data). Production loads the ruleset from the private
    /// config; a test passes a small explicit list. [`Default`] is the minimal
    /// safe redactor (API-key stripping only, no tagged secrets).
    #[must_use]
    pub fn from_config(mut tagged_secrets: Vec<String>) -> Self {
        // Longest-first: the sequential exact-substring replacement in `redact` is
        // order-sensitive, so a shorter tagged secret that is a substring of a
        // longer one must be scrubbed AFTER the longer one — otherwise the short
        // match eats part of the long secret and leaves its tail exposed in the
        // persisted copy (close-review Codex C4). Descending length guarantees the
        // longest match wins.
        tagged_secrets.sort_by_key(|s| std::cmp::Reverse(s.len()));
        Self { tagged_secrets }
    }

    /// Redact one string for the PERSISTED copy: every caller-tagged secret value
    /// and every API-key-shaped token is replaced with the redaction placeholder;
    /// all other text — including numbers/balances — is preserved verbatim.
    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        // 1. Caller-tagged secret VALUES (exact substring) first.
        let mut scrubbed = text.to_owned();
        for secret in &self.tagged_secrets {
            if !secret.is_empty() {
                scrubbed = scrubbed.replace(secret.as_str(), REDACTED);
            }
        }
        // 2. Structural API-key-shaped tokens, preserving every separator.
        Self::redact_api_keys(&scrubbed)
    }

    /// Recursively redact every string leaf of a JSON [`Value`] for the PERSISTED
    /// copy (NFR-6, #81). Object/array structure, object KEYS, and non-string
    /// scalars (numbers/bools/null) are preserved verbatim; each string leaf is
    /// passed through [`redact`](Self::redact) (the tagged-secret + api-key-shape
    /// rules). Used to scrub `Assistant.tool_calls[i].arguments` — a
    /// `serde_json::Value` — before it lands in the ledger copy (defense-in-depth
    /// per audit F2: the composer's builder-tool args are non-secret strategy
    /// params, so this is a general backstop, not a live leak fix).
    #[must_use]
    pub fn redact_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.redact(text)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.redact_value(item)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, val)| (key.clone(), self.redact_value(val)))
                    .collect(),
            ),
            // Numbers / bools / null carry no string leaf — preserved verbatim (we
            // do NOT strip numbers; that would erase the coach's context).
            other => other.clone(),
        }
    }

    /// Replace API-key-shaped word tokens with the placeholder, keeping every
    /// separator (whitespace/punctuation) verbatim so formatting + numbers survive.
    fn redact_api_keys(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut word = String::new();
        for ch in text.chars() {
            if Self::is_word_char(ch) {
                word.push(ch);
            } else {
                Self::flush_word(&mut out, &mut word);
                out.push(ch);
            }
        }
        Self::flush_word(&mut out, &mut word);
        out
    }

    /// Append `word` to `out` — as the placeholder if it is API-key-shaped, else
    /// verbatim — then clear it.
    fn flush_word(out: &mut String, word: &mut String) {
        if word.is_empty() {
            return;
        }
        if crate::domain::looks_like_secret_token(word) {
            out.push_str(REDACTED);
        } else {
            out.push_str(word);
        }
        word.clear();
    }

    /// A token character: ASCII alphanumeric plus `-`/`_` (the shape of `sk-…`
    /// keys and base64/hex tokens). A `.` is a separator, so `12345.67` stays two
    /// pure-digit words (never key-shaped) and survives.
    fn is_word_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
    }

    /// Redact a whole prompt: each message's text content is scrubbed while its
    /// structure (roles, tool-call ids) is preserved.
    pub(crate) fn redact_messages(&self, messages: &[Message]) -> Vec<Message> {
        messages.iter().map(|m| self.redact_message(m)).collect()
    }

    /// Redact one message's text content field(s).
    pub(crate) fn redact_message(&self, message: &Message) -> Message {
        match message {
            Message::System { content } => Message::System {
                content: self.redact(content),
            },
            Message::User { content } => Message::User {
                content: self.redact(content),
            },
            Message::Assistant {
                content,
                tool_calls,
            } => Message::Assistant {
                content: content.as_ref().map(|c| self.redact(c)),
                // #81: scrub every string leaf of each tool call's `arguments`
                // (id + name are non-secret structural identifiers — kept verbatim).
                tool_calls: tool_calls
                    .iter()
                    .map(|tc| ToolCall {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: self.redact_value(&tc.arguments),
                    })
                    .collect(),
            },
            Message::ToolResult {
                tool_call_id,
                content,
            } => Message::ToolResult {
                tool_call_id: tool_call_id.clone(),
                content: self.redact(content),
            },
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{DETAIL_TRUNCATED, Redactor, TRANSPORT_DETAIL_MAX_BYTES, scrub_then_bound};
    use crate::domain::llm::{Message, ToolCall};

    /// The r1.s4.w2 (#150) MOVE FIXTURE — the risk gate's `no-secret-in-log`
    /// control, pinned as a byte-for-byte expectation rather than a property.
    ///
    /// It exercises both halves of the ruleset on one string: a caller-tagged secret
    /// VALUE, an API-key-shaped token nobody tagged, and — the half that is about
    /// what must NOT change — prices, R-multiples and percentages, which share no
    /// lexical shape with a credential and would take the coach's whole context with
    /// them if a numeric rule were ever added here.
    ///
    /// A pinned string, because the control is "the moved redactor scrubs EXACTLY the
    /// same patterns". A property assertion ("the secret is gone") would still pass
    /// if the scrub widened and started eating the numbers.
    const FIXTURE: &str = "entry 30000.50 exited 33000.75 (+2.5R, 1.25%) key sk-live-AAAABBBBCCCCDDDD tagged tok_9f3c2b1a authed";
    const FIXTURE_REDACTED: &str =
        "entry 30000.50 exited 33000.75 (+2.5R, 1.25%) key «REDACTED» tagged «REDACTED» authed";

    fn fixture_redactor() -> Redactor {
        Redactor::from_config(vec!["tok_9f3c2b1a".to_owned()])
    }

    #[test]
    fn the_moved_redactor_scrubs_exactly_the_same_credential_and_price_patterns() {
        assert_eq!(
            fixture_redactor().redact(FIXTURE),
            FIXTURE_REDACTED,
            "the pinned fixture's redacted output changed — the move was supposed to \
             be an address change, not a behaviour change (#150 risk gate, \
             no-secret-in-log)"
        );
    }

    /// Least privilege, stated where the module lives: the kernel's whole input is
    /// the strings it is given, so a `Redactor` built from NO tagged secrets still
    /// strips structural keys and still leaves every number alone.
    #[test]
    fn the_default_redactor_holds_no_credential_and_still_strips_key_shapes() {
        let scrubbed = Redactor::default().redact(FIXTURE);
        assert!(
            !scrubbed.contains("sk-live-AAAABBBBCCCCDDDD"),
            "an API-key shape is structural, not data: {scrubbed}"
        );
        assert!(
            scrubbed.contains("30000.50")
                && scrubbed.contains("2.5R")
                && scrubbed.contains("1.25%"),
            "prices, R-multiples and percentages survive verbatim: {scrubbed}"
        );
        assert!(
            scrubbed.contains("tok_9f3c2b1a"),
            "an UNTAGGED value is not a secret this redactor knows about: {scrubbed}"
        );
    }

    /// The r1.s4.w2 (#150) risk gate's `least-privilege` control, asserted on the
    /// SOURCE rather than described in prose.
    ///
    /// This module is a pure text kernel. If it ever grows a credential literal, an
    /// environment read, a filesystem or Keychain reach, or a dependency on
    /// `crate::adapters`, then the thing that crossed into the domain ring is no
    /// longer the pure half — and the ADR-0012 / ADR-0016 boundary this move was
    /// made to respect is the one that broke. Source properties decay silently, so
    /// this is a test rather than a comment.
    #[test]
    fn the_domain_redaction_kernel_reaches_nothing() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/domain/redaction.rs");
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        // The control is about the PRODUCTION kernel, so the scan stops at the test
        // module — which necessarily names every banned token, in this very list.
        // Comments are blanked too: the prose above carries `Keychain`, `adapters`
        // and `environment` on purpose.
        let production = source
            .split_once("#[cfg(test)]")
            .map_or(source.as_str(), |(before, _)| before);
        let code: String = production
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        for banned in [
            "crate::adapters",
            "std::env",
            "std::fs",
            "env!",
            "option_env!",
            "keychain",
            "Keychain",
            "security_framework",
            "ApiKey",
            "CredentialSource",
            "reqwest",
        ] {
            assert!(
                !code.contains(banned),
                "src/domain/redaction.rs names `{banned}` in code — the moved kernel \
                 holds no credential, reads no environment, file or Keychain, and \
                 depends on nothing in the adapters ring (#150 risk gate, \
                 least-privilege)"
            );
        }
        // Positive control: the scan is reading the real file, not an empty string.
        assert!(
            code.contains("pub fn redact"),
            "the least-privilege scan is reading the wrong file"
        );
    }

    /// The message-level helper the ledger decorator calls: structure preserved,
    /// every text leaf scrubbed, tool-call ids and names untouched.
    #[test]
    fn message_redaction_preserves_structure_and_scrubs_every_text_leaf() {
        let redacted = fixture_redactor().redact_messages(&[
            Message::System {
                content: FIXTURE.to_owned(),
            },
            Message::Assistant {
                content: Some(FIXTURE.to_owned()),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_owned(),
                    name: "propose_mutation".to_owned(),
                    arguments: serde_json::json!({ "note": FIXTURE, "period": 21 }),
                }],
            },
        ]);

        match &redacted[0] {
            Message::System { content } => assert_eq!(content, FIXTURE_REDACTED),
            other => panic!("the role is preserved, got {other:?}"),
        }
        match &redacted[1] {
            Message::Assistant {
                content,
                tool_calls,
            } => {
                assert_eq!(content.as_deref(), Some(FIXTURE_REDACTED));
                assert_eq!(tool_calls[0].id, "call_1", "the id is structural");
                assert_eq!(tool_calls[0].name, "propose_mutation");
                assert_eq!(tool_calls[0].arguments["note"], FIXTURE_REDACTED);
                assert_eq!(
                    tool_calls[0].arguments["period"], 21,
                    "a non-string leaf is preserved verbatim"
                );
            }
            other => panic!("the role is preserved, got {other:?}"),
        }
    }

    // ---- r3.s1.w5: the scrub-then-bound seam --------------------------------

    /// The seam's order is its point: redact the WHOLE string first — the
    /// exact-match replacement needs the credential intact to find it — and
    /// bound only the scrubbed result. Bounding first (the pre-change adapter)
    /// cuts a credential straddling the boundary into a fragment neither the
    /// exact-value nor the structural redaction recognizes, and the fragment
    /// is persisted.
    #[test]
    fn scrub_then_bound_redacts_the_whole_string_before_bounding() {
        let key = "the-quick-brown-fox-leaps-over-the-lazy-transport-canary-key";
        let redactor = Redactor::from_config(vec![key.to_owned()]);
        let mut detail = "x".repeat(4_040);
        detail.push_str(key);
        detail.push_str(&"x".repeat(512));
        assert!(detail.len() > TRANSPORT_DETAIL_MAX_BYTES);

        let stored = scrub_then_bound(&redactor, &detail, TRANSPORT_DETAIL_MAX_BYTES);
        assert!(
            (0..=key.len() - 8).all(|start| !stored.contains(&key[start..start + 8])),
            "no fragment of the credential of length >= 8 survives: {stored}"
        );
        assert!(
            stored.len() <= TRANSPORT_DETAIL_MAX_BYTES,
            "the stored detail is bounded: {} bytes",
            stored.len()
        );
        assert!(
            stored.ends_with(DETAIL_TRUNCATED),
            "a cut detail ends in the marker: …{}",
            &stored[stored.len() - 48..]
        );

        // The order is the defect: bounding FIRST — the pre-change adapter —
        // leaks a fragment of the same detail. If this ever stops holding,
        // the seam's order has stopped mattering; re-earn this test with a
        // different offset, never by weakening it.
        let cut = TRANSPORT_DETAIL_MAX_BYTES - DETAIL_TRUNCATED.len();
        let mut bounded = detail[..cut].to_owned();
        bounded.push_str(DETAIL_TRUNCATED);
        let leaked = redactor.redact(&bounded);
        assert!(
            (0..=key.len() - 8).any(|start| leaked.contains(&key[start..start + 8])),
            "bound-then-redact must demonstrably leak a fragment of the same detail"
        );
    }

    /// The bound cuts at a UTF-8 char boundary: a multi-byte run is never
    /// split mid-character.
    #[test]
    fn scrub_then_bound_cuts_at_a_char_boundary() {
        let redactor = Redactor::from_config(Vec::new());
        let detail = "é".repeat(TRANSPORT_DETAIL_MAX_BYTES); // 2 bytes per char
        let stored = scrub_then_bound(&redactor, &detail, TRANSPORT_DETAIL_MAX_BYTES);
        assert!(
            stored.len() <= TRANSPORT_DETAIL_MAX_BYTES,
            "bounded: {} bytes",
            stored.len()
        );
        assert!(stored.ends_with(DETAIL_TRUNCATED));
        // The cut left whole characters: everything before the marker is 'é'.
        let head = stored.trim_end_matches(DETAIL_TRUNCATED);
        assert!(head.chars().all(|c| c == 'é'), "no split character");
    }

    /// A detail that already fits comes back verbatim — no marker.
    #[test]
    fn scrub_then_bound_leaves_a_fitting_detail_verbatim() {
        let redactor = Redactor::from_config(Vec::new());
        let detail = "server error after 1 attempt(s): upstream refused (HTTP 503)";
        let stored = scrub_then_bound(&redactor, detail, TRANSPORT_DETAIL_MAX_BYTES);
        assert_eq!(stored, detail);
    }
}
