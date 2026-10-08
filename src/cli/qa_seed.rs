//! `pulse qa-seed` — QA's database, seeded from a copy of prod's (r4.s2.w3, C5).
//!
//! QA's database is a COPY of prod's (G1), so it arrives carrying prod's
//! `client_token` rows: a stale URL or an old MCP login would then land on QA
//! with no error at all. The seed closes that hole — it revokes EVERY active
//! token (each revoke audited exactly like any other) and issues the two fresh
//! QA tokens in ONE transaction — then marks the data dir `qa` and prints the
//! two tokens once on stdout. Everything else goes to stderr, and no token ever
//! reaches a log line or the report.
//!
//! **Refusals.** A data dir marked `prod` is never seeded, and a database a
//! live server (or an in-flight import/restore) holds is refused by name
//! through w2's instance lock (#250) — the lock is taken before any write and
//! held for the run.
//!
//! **Stdout discipline** (mirror `pulse token issue`): the two tokens are the
//! ONLY stdout lines, written CHECKED, in label order (`qa-app`, then
//! `qa-agent`). A failure after the transaction — writing the marker, printing
//! the tokens — best-effort revokes the two fresh tokens and exits non-zero: a
//! credential that cannot be revealed is revoked, not left behind.
//!
//! `--db` and `--data-dir` are REQUIRED. The defaults are prod's paths, and a
//! seed that silently defaulted onto prod's data would be exactly the accident
//! the role marker exists to prevent.

use std::path::PathBuf;

use clap::Args;

use crate::adapters::db::NewToken;
use crate::adapters::db::SqliteClientTokenRepo;
use crate::adapters::db::instance_lock::InstanceLock;
use crate::server::auth::{Scope, hash_token, mint_token};
use crate::server::role::{self, ServerRole};

/// `pulse qa-seed --db <path> --data-dir <path>`.
#[derive(Debug, Args)]
pub struct QaSeedArgs {
    /// Path to QA's `pulse.db` (a restored prod backup). Required — there is no
    /// default: the default database is prod's.
    #[arg(long)]
    db: PathBuf,
    /// QA's data dir (the `server-role` marker's home). Required — same reason.
    #[arg(long)]
    data_dir: PathBuf,
}

/// The two fresh tokens the seed issues, in the order their lines print.
const QA_TOKENS: [(&str, Scope); 2] = [("qa-app", Scope::App), ("qa-agent", Scope::Agent)];

/// One fresh QA token: its label, its scope, the plaintext (printed once, then
/// dropped) and the SHA-256 hex the store keeps.
struct FreshToken {
    label: &'static str,
    scope: Scope,
    token: String,
    token_sha256: String,
}

/// Run the seed: the marker refusal, the instance lock, one transaction, the
/// marker, the print.
///
/// # Errors
///
/// Returns an [`anyhow::Error`] — printed to stderr by `main` with a non-zero
/// exit — on any refusal (prod-marked dir, held database), a database failure,
/// or a failure to write the marker or print the tokens (in which case the two
/// fresh tokens are best-effort revoked first).
pub(crate) async fn run_qa_seed(args: &QaSeedArgs) -> anyhow::Result<()> {
    // ---- The marker: a prod-marked dir is never seeded (C5). Read first, so
    // the refusal lands before the lock file or any write.
    match role::read(&args.data_dir)
        .map_err(|error| anyhow::anyhow!("qa-seed: refusing: {error}"))?
    {
        Some(ServerRole::Prod) => anyhow::bail!(
            "qa-seed: refusing: this data dir is marked prod ({}); QA seeds its own data dir",
            args.data_dir.display()
        ),
        Some(ServerRole::Qa) | None => {}
    }

    // ---- The instance lock (#250, w2): a live server — or an in-flight
    // import/restore — holds it, and the seed refuses by name instead of
    // writing under one. Held until the run ends.
    let _instance_lock = InstanceLock::acquire(&args.db)
        .map_err(|error| anyhow::anyhow!("qa-seed: refusing: {error}"))?;

    // ---- The database (the one migrate-then-open every arm uses).
    let db = super::open_db(Some(&args.db)).await?;

    // ---- ONE transaction: every copied token revoked, the two fresh ones
    // issued. A failure here leaves the token set exactly as it was.
    let fresh: Vec<FreshToken> = QA_TOKENS
        .iter()
        .map(|(label, scope)| {
            let token = mint_token();
            FreshToken {
                label,
                scope: *scope,
                token_sha256: hash_token(&token),
                token,
            }
        })
        .collect();
    let rows: Vec<NewToken<'_>> = fresh
        .iter()
        .map(|fresh| NewToken {
            label: fresh.label,
            scope: fresh.scope.as_str(),
            token_sha256: &fresh.token_sha256,
        })
        .collect();
    let repo = SqliteClientTokenRepo::new(db.pool().clone());
    let revoked = repo
        .replace_all_with(&rows, "cli:qa-seed")
        .await
        .map_err(|error| anyhow::anyhow!("qa-seed: {error}"))?;
    eprintln!(
        "pulse qa-seed: revoked {} copied token(s); issued qa-app and qa-agent",
        revoked.len()
    );

    // ---- The marker and the print. Any failure AFTER the transaction revokes
    // the two fresh tokens (best effort): they exist in the database but no
    // operator has seen them, and an unrevealable credential is revoked, not
    // left behind (the `pulse token issue` rule).
    let published = publish(&args.data_dir, &fresh);
    if let Err(error) = published {
        for fresh in &fresh {
            if let Err(refusal) = repo.revoke(fresh.label).await {
                eprintln!(
                    "pulse qa-seed: WARNING — could not revoke {}: {refusal}",
                    fresh.label
                );
            }
        }
        return Err(error);
    }
    Ok(())
}

/// Write the `qa` marker, then print the two tokens as the ONLY stdout lines.
fn publish(data_dir: &std::path::Path, fresh: &[FreshToken]) -> anyhow::Result<()> {
    role::write(data_dir, ServerRole::Qa).map_err(|error| {
        anyhow::anyhow!(
            "qa-seed: the tokens are issued but the marker could not be written: {error}"
        )
    })?;
    write_token_lines(&fresh[0].token, &fresh[1].token).map_err(|error| {
        anyhow::anyhow!(
            "qa-seed: the tokens are issued but could not be printed ({error}); they have been revoked — re-run the command"
        )
    })
}

/// Write the two freshly minted tokens as the ONLY stdout lines, and report a
/// write failure instead of panicking on it — the same rule `pulse token
/// issue` follows, for the same reason: this is the one moment the plaintext
/// exists nowhere else, so a panic would leave two tokens nobody can use and
/// nobody can reveal.
fn write_token_lines(app: &str, agent: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut lines = stdout.lock();
    writeln!(lines, "{app}")?;
    writeln!(lines, "{agent}")?;
    lines.flush()
}
