//! Agent identity resolution for `pulse mcp` (r2.s1.w2).
//!
//! The `w1` domain `AgentName` newtype is deliberately NOT imported here — `w1`
//! lands in parallel and this file owns the validation until `w3` re-points it.
//! The rules are the spec's: 1–64 characters of `[A-Za-z0-9._-]`, normalized to
//! lowercase.
//!
//! Precedence: the `--agent-name` flag beats the MCP `initialize` handshake's
//! `clientInfo.name`, which beats the `unknown` fallback. A bad flag is a
//! startup error (the CLI validates before serving); a bad handshake name
//! degrades to `unknown` with one stderr line — it never fails the session.

/// The fallback agent name when neither the flag nor the handshake yields a
/// valid name.
const UNKNOWN_NAME: &str = "unknown";

/// Where the resolved agent name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentNameSource {
    /// The `--agent-name` CLI flag (highest precedence).
    Flag,
    /// The `initialize` request's `clientInfo.name`.
    Handshake,
    /// Neither source yielded a valid name.
    Unknown,
}

/// The resolved agent identity: the normalized name plus its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    /// The normalized (lowercase) agent name.
    pub name: String,
    /// Which source produced `name`.
    pub source: AgentNameSource,
}

impl AgentIdentity {
    /// Resolve the session's agent identity from the CLI flag and the
    /// `initialize` handshake name, in precedence order.
    ///
    /// A `Some` flag always wins — the CLI validates it before serving, so an
    /// invalid flag here is defensive-only and degrades like a bad handshake.
    /// An invalid handshake name degrades to `unknown` with one stderr line;
    /// it never fails the session.
    #[must_use]
    pub fn resolve(flag: Option<&str>, handshake: Option<&str>) -> AgentIdentity {
        if let Some(raw) = flag {
            return match validate_agent_name(raw) {
                Ok(name) => AgentIdentity {
                    name,
                    source: AgentNameSource::Flag,
                },
                Err(reason) => {
                    eprintln!("pulse mcp: ignoring invalid --agent-name {raw:?}: {reason}");
                    AgentIdentity::unknown()
                }
            };
        }
        if let Some(raw) = handshake {
            return match validate_agent_name(raw) {
                Ok(name) => AgentIdentity {
                    name,
                    source: AgentNameSource::Handshake,
                },
                Err(reason) => {
                    eprintln!(
                        "pulse mcp: clientInfo.name {raw:?} is not a valid agent name ({reason}); reporting '{UNKNOWN_NAME}'"
                    );
                    AgentIdentity::unknown()
                }
            };
        }
        AgentIdentity::unknown()
    }

    fn unknown() -> AgentIdentity {
        AgentIdentity {
            name: UNKNOWN_NAME.to_owned(),
            source: AgentNameSource::Unknown,
        }
    }
}

/// Validate a candidate agent name against the `AgentName` rules: 1–64
/// characters of `[A-Za-z0-9._-]`.
///
/// Returns the **lowercased** name on success, or a human-readable reason on
/// failure. The CLI uses this on `--agent-name` (invalid → non-zero exit
/// before serving); [`AgentIdentity::resolve`] uses it on the handshake name.
///
/// # Errors
///
/// Returns `Err(reason)` when the name is empty, over 64 bytes, or contains a
/// character outside the allowed set.
pub fn validate_agent_name(raw: &str) -> Result<String, String> {
    let name = raw.to_ascii_lowercase();
    if name.is_empty() {
        return Err("agent name must be 1-64 characters".to_owned());
    }
    if name.len() > 64 {
        return Err(format!(
            "agent name must be 1-64 characters, got {}",
            name.len()
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
    {
        return Err(format!(
            "agent name {raw:?} contains {bad:?}: allowed characters are A-Z, a-z, 0-9, '.', '_', '-'"
        ));
    }
    Ok(name)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{AgentIdentity, AgentNameSource, validate_agent_name};

    #[test]
    fn agent_identity_flag_beats_handshake() {
        let id = AgentIdentity::resolve(Some("Claude-Code"), Some("other-agent"));
        assert_eq!(id.name, "claude-code");
        assert_eq!(id.source, AgentNameSource::Flag);
    }

    #[test]
    fn agent_identity_handshake_used_when_no_flag() {
        let id = AgentIdentity::resolve(None, Some("Cursor-Agent"));
        assert_eq!(id.name, "cursor-agent");
        assert_eq!(id.source, AgentNameSource::Handshake);
    }

    #[test]
    fn agent_identity_invalid_handshake_degrades_to_unknown() {
        let id = AgentIdentity::resolve(None, Some("Bad Name!"));
        assert_eq!(id.name, "unknown");
        assert_eq!(id.source, AgentNameSource::Unknown);
    }

    #[test]
    fn agent_identity_invalid_flag_degrades_defensively() {
        // The CLI exits on an invalid flag before serve() — resolve() never
        // trusts it anyway if one slips through.
        let id = AgentIdentity::resolve(Some("bad name"), Some("good-name"));
        assert_eq!(id.name, "unknown");
        assert_eq!(id.source, AgentNameSource::Unknown);
    }

    #[test]
    fn agent_identity_unknown_when_neither_source() {
        let id = AgentIdentity::resolve(None, None);
        assert_eq!(id.name, "unknown");
        assert_eq!(id.source, AgentNameSource::Unknown);
    }

    #[test]
    fn agent_identity_name_is_lowercased() {
        let id = AgentIdentity::resolve(Some("UPPER.Name_1"), None);
        assert_eq!(id.name, "upper.name_1");
    }

    #[test]
    fn agent_identity_validation_boundaries() {
        assert!(validate_agent_name("a").is_ok());
        assert_eq!(
            validate_agent_name(&"x".repeat(64)).map(|n| n.len()),
            Ok(64)
        );
        assert!(validate_agent_name(&"x".repeat(65)).is_err());
        assert!(validate_agent_name("").is_err());
        assert!(validate_agent_name("has space").is_err());
        assert!(validate_agent_name("bad!name").is_err());
        assert!(validate_agent_name("unicode-é").is_err());
        assert!(validate_agent_name("dots_.-ok9").is_ok());
    }
}
