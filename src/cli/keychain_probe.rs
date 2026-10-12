//! `pulse keychain-probe` — the r4.s3.w1 identity probe (HIDDEN from user help).
//!
//! `write` / `read` / `delete` operate on ONE fixed, non-secret Keychain item
//! (`PulseTrader/r4s3_identity_probe`, `src/adapters/secrets.rs`). The verb exists so
//! the `MacBook` walk can prove the premise w2's token store rests on: a REBUILT,
//! signed app reads back what an earlier build wrote. The item's value is a fixed,
//! non-secret marker constant, which is why `read` prints it; the probe's only
//! account literal is its own, so no credential account
//! (`server_app_token`, `glm_api_key`) can ever reach these lines.
//!
//! Deliberately hidden: `#[command(hide = true)]` on the `Command` variant keeps it
//! out of `pulse --help`, and `the_probe_is_not_listed_in_user_help` pins that.

use clap::Subcommand;

use crate::adapters::secrets::{
    keychain_probe_delete, keychain_probe_item, keychain_probe_read, keychain_probe_write,
};

/// `pulse keychain-probe {write,read,delete}` — one fixed item, one line per outcome.
#[derive(Debug, Clone, Copy, clap::Args)]
pub struct KeychainProbeArgs {
    /// The action to run against the fixed probe item.
    #[command(subcommand)]
    pub action: KeychainProbeAction,
}

/// The three actions, each with exactly one stdout line.
#[derive(Debug, Clone, Copy, Subcommand)]
pub enum KeychainProbeAction {
    /// Write the fixed marker into the probe item.
    Write,
    /// Read the probe item back and check it holds this build's marker.
    Read,
    /// Delete the probe item.
    Delete,
}

impl KeychainProbeAction {
    /// The outcome line's verb — ONE mapping shared by the CLI surface and the
    /// printed evidence, so the two cannot disagree.
    fn label(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Read => "read",
            Self::Delete => "delete",
        }
    }
}

/// The one stdout line per outcome: `<verb> ok <item>`, plus — for `read` only — the
/// marker it read back (the probe's own fixed, non-secret constant; AC-U1 asks for
/// it). No other line can carry a value.
fn outcome_line(action: KeychainProbeAction, item: &str, marker: Option<&str>) -> String {
    match marker {
        Some(value) => format!("keychain-probe: {} ok {item} {value}", action.label()),
        None => format!("keychain-probe: {} ok {item}", action.label()),
    }
}

/// Run one probe action against the fixed item, printing its one line on success.
///
/// # Errors
///
/// Returns the probe's error when the platform store refuses the action (absent
/// item, unreadable item, value mismatch, nothing to delete). `main` prints it to
/// stderr and exits non-zero.
pub(crate) fn run_keychain_probe(args: KeychainProbeArgs) -> anyhow::Result<()> {
    let item = keychain_probe_item();
    let marker = match args.action {
        KeychainProbeAction::Write => {
            keychain_probe_write().map_err(|error| anyhow::anyhow!("{error}"))?;
            None
        }
        KeychainProbeAction::Read => {
            Some(keychain_probe_read().map_err(|error| anyhow::anyhow!("{error}"))?)
        }
        KeychainProbeAction::Delete => {
            keychain_probe_delete().map_err(|error| anyhow::anyhow!("{error}"))?;
            None
        }
    };
    println!("{}", outcome_line(args.action, &item, marker.as_deref()));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{KeychainProbeAction, outcome_line};
    use crate::adapters::secrets::{
        KEYCHAIN_PROBE_ACCOUNT, KEYCHAIN_PROBE_MARKER, keychain_probe_item,
    };
    use crate::cli::{Cli, Command};

    #[test]
    fn the_three_outcome_lines_are_exact_and_only_read_carries_the_marker() {
        let item = keychain_probe_item();
        assert_eq!(
            outcome_line(KeychainProbeAction::Write, &item, None),
            format!("keychain-probe: write ok {item}")
        );
        assert_eq!(
            outcome_line(
                KeychainProbeAction::Read,
                &item,
                Some(KEYCHAIN_PROBE_MARKER)
            ),
            format!("keychain-probe: read ok {item} {KEYCHAIN_PROBE_MARKER}")
        );
        assert_eq!(
            outcome_line(KeychainProbeAction::Delete, &item, None),
            format!("keychain-probe: delete ok {item}")
        );
    }

    #[test]
    fn no_outcome_line_can_name_another_account() {
        let item = keychain_probe_item();
        for line in [
            outcome_line(KeychainProbeAction::Write, &item, None),
            outcome_line(
                KeychainProbeAction::Read,
                &item,
                Some(KEYCHAIN_PROBE_MARKER),
            ),
            outcome_line(KeychainProbeAction::Delete, &item, None),
        ] {
            assert!(
                line.contains(KEYCHAIN_PROBE_ACCOUNT),
                "every line names the probe item: {line}"
            );
            for account in ["server_app_token", "glm_api_key"] {
                assert!(
                    !line.contains(account),
                    "no credential account may reach a probe line: {line}"
                );
            }
        }
    }

    #[test]
    fn parses_the_three_actions_and_requires_exactly_one() {
        let action_of = |action: &str| {
            let cli = Cli::try_parse_from(["pulse", "keychain-probe", action])
                .unwrap_or_else(|error| panic!("parse {action}: {error}"));
            match cli.command {
                Command::KeychainProbe(args) => args.action,
                other => panic!("expected keychain-probe, got {other:?}"),
            }
        };
        assert!(matches!(action_of("write"), KeychainProbeAction::Write));
        assert!(matches!(action_of("read"), KeychainProbeAction::Read));
        assert!(matches!(action_of("delete"), KeychainProbeAction::Delete));
        assert!(
            Cli::try_parse_from(["pulse", "keychain-probe"]).is_err(),
            "an action is required"
        );
        assert!(
            Cli::try_parse_from(["pulse", "keychain-probe", "write", "--marker", "x"]).is_err(),
            "the item and its marker are fixed; the verb takes no flags"
        );
    }

    #[test]
    fn the_probe_is_not_listed_in_user_help() {
        let help = Cli::command().render_help().to_string();
        assert!(
            help.contains("token"),
            "sanity: the rendered help lists visible subcommands:\n{help}"
        );
        assert!(
            !help.contains("keychain-probe"),
            "the probe must stay out of user help:\n{help}"
        );
    }
}
