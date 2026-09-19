//! `pulse strategy` — the FR-11 strategy-tree CLI surface (VS-1.1.4 work-1.05).
//!
//! The demo surface for the persistence slice: a nested-subcommand `clap` derive
//! (`create` / `version create` / `list` / `show` / `clone` / `tag` / `pin` /
//! `archive` / `compare`) routed through the [`StrategyRepository`] port over the
//! [`Db`] pool — **with NO LLM in the loop** (FR-11; `creating_llm_call_ids` is
//! always `[]`). This is the first nested-subcommand surface (the top-level
//! `Command` enum is flat); `strategy` carries a `#[command(subcommand)]` of a
//! `StrategyCommand` enum, with `version` nesting a further `VersionCommand`.
//!
//! **Consumer-only (spec §9):** no new repo methods, no schema, no migration
//! logic, no domain types. `clone` = `get_version` + `create_version`; `compare`
//! = two reads + the pure domain [`diff_versions`]. The repo (1.03) owns the
//! `Migrator::load` + `version_hash` derivation; this CLI never parses, migrates,
//! or hashes a DSL itself.
//!
//! **Async (spec §3 / handoff §4-3):** the `StrategyRepository` port is async, so
//! [`run_strategy`] is `async fn` and is awaited inside the existing `dispatch`
//! future — the `mod.rs` sync→async bridge (audit C3) is reused verbatim (no new
//! `#[tokio::main]`, no new runtime).

use std::path::PathBuf;

use clap::{Args, Subcommand};

use crate::adapters::db::Db;
use crate::adapters::db::SqliteStrategyRepo;
use crate::domain::strategy::{CreatedBy, NewVersion, StrategyId, VersionId, diff_versions};
use crate::domain::{DataError, StrategyRepository};

/// `pulse strategy <SUBCOMMAND> [--db <path>]`.
///
/// `--db` is the escape hatch the integration tests + the demo use to point at a
/// `TempDir` `pulse.db` instead of the real Application Support path; omitted, the
/// dispatch resolves `default_db_path()` (gate-7 C3 startup wiring lives in
/// `mod.rs`'s `dispatch`, which calls `open_migrated` BEFORE `run_strategy`).
#[derive(Debug, Args)]
pub struct StrategyArgs {
    /// The strategy subcommand to run.
    #[command(subcommand)]
    pub command: StrategyCommand,
    /// `pulse.db` path override (defaults to the platform Application Support db).
    ///
    /// `global = true` so it parses in any position — `strategy --db <p> create
    /// …` AND `strategy create … --db <p>` both work (the demo + the binary smoke
    /// test pass `--db` AFTER the verb; without `global` clap binds it to the
    /// flat top-level args and rejects it once a nested subcommand is active).
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,
}

/// The FR-11 affordance set — one variant per browse/clone/tag/pin/archive/compare
/// op (plus `create` / `version` / `list` / `show`). **No LLM** (FR-11).
#[derive(Debug, Subcommand)]
pub enum StrategyCommand {
    /// Create a new strategy.
    Create {
        /// Human-readable strategy name (positional).
        name: String,
        /// Optional owner label.
        #[arg(long)]
        owner: Option<String>,
        /// Comma-separated tags (FR-11 tag).
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
    },
    /// Manage a strategy's immutable versions.
    Version(VersionArgs),
    /// List strategies (FR-11 browse).
    List {
        /// Include archived strategies in the listing.
        #[arg(long)]
        include_archived: bool,
    },
    /// Show a strategy + its parent-ordered version subtree (FR-11 browse).
    Show {
        /// The strategy id (positional).
        strategy: String,
    },
    /// Clone a version into a new immutable version (FR-11 clone = parent set).
    Clone {
        /// The source VERSION id to clone from.
        #[arg(long)]
        from: String,
        /// Override DSL file; omitted re-uses the source version's verbatim DSL.
        #[arg(long)]
        dsl: Option<PathBuf>,
    },
    /// Set a strategy's tags (FR-11 tag).
    Tag {
        /// The strategy id (positional).
        strategy: String,
        /// Comma-separated tags (replaces the existing set).
        #[arg(long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
    },
    /// Pin (or clear) a strategy's canonical version (FR-11 pin).
    Pin {
        /// The strategy id (positional).
        strategy: String,
        /// The version id to pin; omitted clears the pin.
        #[arg(long = "version")]
        version: Option<String>,
    },
    /// Archive or un-archive a strategy (FR-11 archive).
    Archive {
        /// The strategy id (positional).
        strategy: String,
        /// Un-archive instead of archive.
        #[arg(long = "unarchive")]
        unarchive: bool,
    },
    /// Compare two versions field-by-field (FR-11 compare; pure domain diff).
    Compare {
        /// The first VERSION id (positional).
        a: String,
        /// The second VERSION id (positional).
        b: String,
    },
}

/// `pulse strategy version <SUBCOMMAND>` — the nested version surface.
#[derive(Debug, Args)]
pub struct VersionArgs {
    /// The version subcommand to run.
    #[command(subcommand)]
    pub command: VersionCommand,
}

/// The version sub-verbs (only `create` this slice — immutable create-only).
#[derive(Debug, Subcommand)]
pub enum VersionCommand {
    /// Create a new immutable version from a `--dsl <file>` document.
    Create {
        /// The owning strategy id.
        #[arg(long)]
        strategy: String,
        /// The DSL JSON document to persist (read from this file).
        #[arg(long = "dsl")]
        dsl_file: PathBuf,
        /// The parent version id, if any (FR-11 clone = parent set).
        #[arg(long)]
        parent: Option<String>,
        /// Provenance label (`human` by default; FR-11 is LLM-free this slice).
        #[arg(long = "created-by", default_value = "human")]
        created_by: String,
    },
}

/// Map a `DataError` (repo boundary) to an `anyhow::Error` with a context label
/// (mirrors `run_fetch_data`/`run_indicators`' store-error mapping). The binary
/// shim (`main.rs`) turns the `Err` into a non-zero `ExitCode::FAILURE`.
fn db_err(label: &str, e: &DataError) -> anyhow::Error {
    anyhow::anyhow!("{label}: {e}")
}

/// Parse a `--created-by` token into the [`CreatedBy`] provenance enum (the
/// `snake_case` strings ARE the column text; FR-11 is human-only here, but the
/// full set is accepted for forward-compat). Routed through serde so the accepted
/// tokens stay 1:1 with the domain enum.
fn parse_created_by(token: &str) -> anyhow::Result<CreatedBy> {
    serde_json::from_value(serde_json::Value::String(token.to_owned()))
        .map_err(|e| anyhow::anyhow!("invalid --created-by {token:?}: {e}"))
}

/// Orchestrate `pulse strategy` over the [`StrategyRepository`] port. The `Db`
/// pool is opened (migrate-then-open) by the caller (`mod.rs::dispatch`); here we
/// build the production repo over its pool and dispatch the verb.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] on a repo failure, a not-found id, a missing/
/// unreadable `--dsl` file, or an invalid `--created-by` token. The immutability
/// ABORT (a trigger `RAISE`) surfaces as a `DataError::Db` carrying the
/// `strategy_version is immutable` text.
pub async fn run_strategy(db: &Db, args: &StrategyArgs) -> anyhow::Result<()> {
    let repo = SqliteStrategyRepo::new(db.pool().clone());
    // Each arm delegates to a small per-verb helper so this dispatcher stays a
    // flat router (clippy `too_many_lines` — one verb, one fn).
    match &args.command {
        StrategyCommand::Create { name, owner, tags } => {
            verb_create(&repo, name, owner.as_deref(), tags).await
        }
        StrategyCommand::Version(VersionArgs {
            command:
                VersionCommand::Create {
                    strategy,
                    dsl_file,
                    parent,
                    created_by,
                },
        }) => verb_version_create(&repo, strategy, dsl_file, parent.as_deref(), created_by).await,
        StrategyCommand::List { include_archived } => verb_list(&repo, *include_archived).await,
        StrategyCommand::Show { strategy } => verb_show(&repo, strategy).await,
        StrategyCommand::Clone { from, dsl } => verb_clone(&repo, from, dsl.as_deref()).await,
        StrategyCommand::Tag { strategy, tags } => verb_tag(&repo, strategy, tags).await,
        StrategyCommand::Pin { strategy, version } => {
            verb_pin(&repo, strategy, version.as_deref()).await
        }
        StrategyCommand::Archive {
            strategy,
            unarchive,
        } => verb_archive(&repo, strategy, *unarchive).await,
        StrategyCommand::Compare { a, b } => verb_compare(&repo, a, b).await,
    }
}

/// `create` — mint a new strategy; echo its id (so the demo/tests can capture it).
async fn verb_create<R: StrategyRepository>(
    repo: &R,
    name: &str,
    owner: Option<&str>,
    tags: &[String],
) -> anyhow::Result<()> {
    let strat = repo
        .create_strategy(name, owner, tags)
        .await
        .map_err(|e| db_err("create strategy", &e))?;
    println!("{}", strat.id.as_str());
    Ok(())
}

/// `version create` — read the `--dsl <file>`, build a `NewVersion` (empty LLM
/// ids — FR-11), persist via the repo (which migrates/validates/hashes), echo id.
async fn verb_version_create<R: StrategyRepository>(
    repo: &R,
    strategy: &str,
    dsl_file: &std::path::Path,
    parent: Option<&str>,
    created_by: &str,
) -> anyhow::Result<()> {
    let dsl_json = std::fs::read_to_string(dsl_file)
        .map_err(|e| anyhow::anyhow!("read --dsl file {}: {e}", dsl_file.display()))?;
    let created = repo
        .create_version(NewVersion {
            strategy_id: StrategyId::new(strategy.to_owned()),
            parent_version_id: parent.map(VersionId::new),
            dsl_json,
            created_by: parse_created_by(created_by)?,
            // FR-11: NO LLM this slice — always empty (no LLMCall table).
            creating_llm_call_ids: vec![],
        })
        .await
        .map_err(|e| db_err("create version", &e))?;
    println!("{}", created.id.as_str());
    Ok(())
}

/// `list` — the FR-11 browse listing (one tab line per strategy).
async fn verb_list<R: StrategyRepository>(repo: &R, include_archived: bool) -> anyhow::Result<()> {
    let strategies = repo
        .list_strategies(include_archived)
        .await
        .map_err(|e| db_err("list strategies", &e))?;
    for s in &strategies {
        println!(
            "{}\t{}\t{}\tarchived={}",
            s.id.as_str(),
            s.name,
            s.tags.join(","),
            s.archived,
        );
    }
    Ok(())
}

/// `show` — the strategy header + its parent-ordered version subtree (FR-11
/// browse). A not-found id is a non-zero exit (mirrors `run_indicators`).
async fn verb_show<R: StrategyRepository>(repo: &R, strategy: &str) -> anyhow::Result<()> {
    let id = StrategyId::new(strategy.to_owned());
    let s = repo
        .get_strategy(&id)
        .await
        .map_err(|e| db_err("get strategy", &e))?
        .ok_or_else(|| anyhow::anyhow!("no such strategy `{strategy}`"))?;
    println!(
        "strategy\t{}\t{}\towner={}\tpinned={}\tarchived={}",
        s.id.as_str(),
        s.name,
        s.owner.as_deref().unwrap_or("—"),
        s.pinned_version_id.as_ref().map_or("—", |v| v.as_str()),
        s.archived,
    );
    let tree = repo
        .version_tree(&id)
        .await
        .map_err(|e| db_err("version tree", &e))?;
    for v in &tree {
        println!(
            "version\t{}\tparent={}\thash={}\tcreated_by={}\tcreated_at={}",
            v.id.as_str(),
            v.parent_version_id.as_ref().map_or("—", |p| p.as_str()),
            v.version_hash,
            created_by_label(v.created_by),
            v.created_at.to_rfc3339(),
        );
    }
    Ok(())
}

/// `clone` — FR-11 clone = parent set: fetch the source version, re-use its
/// verbatim DSL (or a `--dsl` override), and `create_version` with the source as
/// parent. NO new repo method (composition of `get_version` + `create_version`).
async fn verb_clone<R: StrategyRepository>(
    repo: &R,
    from: &str,
    dsl: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let source_id = VersionId::new(from.to_owned());
    let source = repo
        .get_version(&source_id)
        .await
        .map_err(|e| db_err("get source version", &e))?
        .ok_or_else(|| anyhow::anyhow!("no such version `{from}`"))?;
    let dsl_json = match dsl {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read --dsl file {}: {e}", path.display()))?,
        None => source.dsl_original.clone(),
    };
    let created = repo
        .create_version(NewVersion {
            strategy_id: source.strategy_id.clone(),
            parent_version_id: Some(source_id),
            dsl_json,
            created_by: CreatedBy::Human,
            creating_llm_call_ids: vec![],
        })
        .await
        .map_err(|e| db_err("clone version", &e))?;
    println!("{}", created.id.as_str());
    Ok(())
}

/// `tag` — replace a strategy's tag set (FR-11 tag).
async fn verb_tag<R: StrategyRepository>(
    repo: &R,
    strategy: &str,
    tags: &[String],
) -> anyhow::Result<()> {
    let s = repo
        .set_tags(&StrategyId::new(strategy.to_owned()), tags)
        .await
        .map_err(|e| db_err("set tags", &e))?;
    println!("{}\t{}", s.id.as_str(), s.tags.join(","));
    Ok(())
}

/// `pin` — set (or clear, when `version` is `None`) the canonical version (FR-11).
async fn verb_pin<R: StrategyRepository>(
    repo: &R,
    strategy: &str,
    version: Option<&str>,
) -> anyhow::Result<()> {
    let version_id = version.map(VersionId::new);
    let s = repo
        .set_pinned_version(&StrategyId::new(strategy.to_owned()), version_id.as_ref())
        .await
        .map_err(|e| db_err("set pinned version", &e))?;
    println!(
        "{}\tpinned={}",
        s.id.as_str(),
        s.pinned_version_id.as_ref().map_or("—", |v| v.as_str()),
    );
    Ok(())
}

/// `archive` — archive (or `--unarchive`) a strategy (FR-11 archive).
async fn verb_archive<R: StrategyRepository>(
    repo: &R,
    strategy: &str,
    unarchive: bool,
) -> anyhow::Result<()> {
    let s = repo
        .archive_strategy(&StrategyId::new(strategy.to_owned()), !unarchive)
        .await
        .map_err(|e| db_err("archive strategy", &e))?;
    println!("{}\tarchived={}", s.id.as_str(), s.archived);
    Ok(())
}

/// `compare` — two reads + the pure domain `diff_versions` (NOT a repo method).
async fn verb_compare<R: StrategyRepository>(repo: &R, a: &str, b: &str) -> anyhow::Result<()> {
    let va = repo
        .get_version(&VersionId::new(a.to_owned()))
        .await
        .map_err(|e| db_err("get version a", &e))?
        .ok_or_else(|| anyhow::anyhow!("no such version `{a}`"))?;
    let vb = repo
        .get_version(&VersionId::new(b.to_owned()))
        .await
        .map_err(|e| db_err("get version b", &e))?
        .ok_or_else(|| anyhow::anyhow!("no such version `{b}`"))?;
    let diff = diff_versions(&va, &vb);
    println!("same_version\t{}", diff.same_version);
    println!(
        "dsl_schema_version_changed\t{}",
        diff.dsl_schema_version_changed
    );
    println!("dsl_changed\t{}", diff.dsl_changed);
    println!("dsl_original_changed\t{}", diff.dsl_original_changed);
    println!("version_hash_changed\t{}", diff.version_hash_changed);
    println!("created_by_changed\t{}", diff.created_by_changed);
    println!("parent_changed\t{}", diff.parent_changed);
    Ok(())
}

/// The `snake_case` provenance label for human-readable output (the same token
/// `--created-by` accepts, via serde).
fn created_by_label(created_by: CreatedBy) -> &'static str {
    match created_by {
        CreatedBy::Human => "human",
        CreatedBy::ComposerLlm => "composer_llm",
        CreatedBy::CoachLlm => "coach_llm",
        CreatedBy::AutoOptimizer => "auto_optimizer",
        CreatedBy::Migration => "migration",
        CreatedBy::ExternalAgent => "external_agent",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{StrategyCommand, VersionCommand, parse_created_by};
    use crate::cli::{Cli, Command};
    use crate::domain::strategy::CreatedBy;
    use clap::Parser;

    /// Helper: parse a `pulse strategy …` command line and return the
    /// `StrategyArgs`.
    fn parse_strategy(command_line: &[&str]) -> super::StrategyArgs {
        let cli = Cli::try_parse_from(command_line).expect("parse strategy command line");
        match cli.command {
            Command::Strategy(strategy_args) => strategy_args,
            other => panic!("expected a strategy command, got {other:?}"),
        }
    }

    #[test]
    fn parses_create_with_owner_and_comma_tags() {
        let args = parse_strategy(&[
            "pulse", "strategy", "create", "demo", "--owner", "me", "--tag", "a,b",
        ]);
        let StrategyCommand::Create { name, owner, tags } = args.command else {
            panic!("expected create");
        };
        assert_eq!(name, "demo");
        assert_eq!(owner.as_deref(), Some("me"));
        assert_eq!(tags, vec!["a".to_owned(), "b".to_owned()]);
        assert!(args.db.is_none(), "no --db ⇒ default path resolved later");
    }

    #[test]
    fn parses_db_override() {
        let args = parse_strategy(&["pulse", "strategy", "--db", "/tmp/x.db", "list"]);
        assert_eq!(
            args.db.as_deref().map(std::path::Path::to_str),
            Some(Some("/tmp/x.db"))
        );
        assert!(matches!(args.command, StrategyCommand::List { .. }));
    }

    #[test]
    fn parses_version_create_through_nested_subcommand() {
        let args = parse_strategy(&[
            "pulse",
            "strategy",
            "version",
            "create",
            "--strategy",
            "sid-1",
            "--dsl",
            "path.json",
        ]);
        let StrategyCommand::Version(v) = args.command else {
            panic!("expected version");
        };
        let VersionCommand::Create {
            strategy,
            dsl_file,
            parent,
            created_by,
        } = v.command;
        assert_eq!(strategy, "sid-1");
        assert_eq!(dsl_file.to_str(), Some("path.json"));
        assert!(parent.is_none());
        assert_eq!(created_by, "human", "default --created-by is human");
    }

    #[test]
    fn parses_version_create_with_parent_and_created_by() {
        let args = parse_strategy(&[
            "pulse",
            "strategy",
            "version",
            "create",
            "--strategy",
            "sid",
            "--dsl",
            "d.json",
            "--parent",
            "vid-0",
            "--created-by",
            "composer_llm",
        ]);
        let StrategyCommand::Version(v) = args.command else {
            panic!("expected version");
        };
        let VersionCommand::Create {
            parent, created_by, ..
        } = v.command;
        assert_eq!(parent.as_deref(), Some("vid-0"));
        assert_eq!(created_by, "composer_llm");
    }

    #[test]
    fn parses_compare_two_positionals() {
        let args = parse_strategy(&["pulse", "strategy", "compare", "va", "vb"]);
        let StrategyCommand::Compare { a, b } = args.command else {
            panic!("expected compare");
        };
        assert_eq!(a, "va");
        assert_eq!(b, "vb");
    }

    #[test]
    fn parses_pin_with_and_without_version() {
        let with = parse_strategy(&["pulse", "strategy", "pin", "sid", "--version", "vid"]);
        let StrategyCommand::Pin { strategy, version } = with.command else {
            panic!("expected pin");
        };
        assert_eq!(strategy, "sid");
        assert_eq!(version.as_deref(), Some("vid"));

        let clear = parse_strategy(&["pulse", "strategy", "pin", "sid"]);
        let StrategyCommand::Pin { version, .. } = clear.command else {
            panic!("expected pin");
        };
        assert!(version.is_none(), "no --version ⇒ clear the pin");
    }

    #[test]
    fn parses_clone_tag_archive_show() {
        let cloned = parse_strategy(&["pulse", "strategy", "clone", "--from", "vid"]);
        assert!(matches!(cloned.command, StrategyCommand::Clone { .. }));

        let tagged = parse_strategy(&["pulse", "strategy", "tag", "sid", "--tag", "x,y"]);
        let StrategyCommand::Tag { tags, .. } = tagged.command else {
            panic!("expected tag");
        };
        assert_eq!(tags, vec!["x".to_owned(), "y".to_owned()]);

        let archived = parse_strategy(&["pulse", "strategy", "archive", "sid", "--unarchive"]);
        let StrategyCommand::Archive { unarchive, .. } = archived.command else {
            panic!("expected archive");
        };
        assert!(unarchive);

        let shown = parse_strategy(&["pulse", "strategy", "show", "sid"]);
        assert!(matches!(shown.command, StrategyCommand::Show { .. }));
    }

    #[test]
    fn rejects_unknown_verb() {
        let err = Cli::try_parse_from(["pulse", "strategy", "frobnicate"]);
        assert!(err.is_err(), "an unknown strategy verb must error");
    }

    #[test]
    fn created_by_token_maps_to_enum() {
        assert_eq!(parse_created_by("human").unwrap(), CreatedBy::Human);
        assert_eq!(
            parse_created_by("composer_llm").unwrap(),
            CreatedBy::ComposerLlm
        );
        assert_eq!(
            parse_created_by("external_agent").unwrap(),
            CreatedBy::ExternalAgent
        );
        assert!(parse_created_by("bogus").is_err(), "unknown token rejects");
    }
}
