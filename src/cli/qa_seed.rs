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
//! **Refusals.** A data dir marked `prod` is never seeded; a database outside
//! the data dir is refused by name (the rule `pulse serve --role` applies — one
//! copy of it, in `server::role`); and a database a live server (or an in-flight
//! import/restore) holds is refused by name through w2's instance lock (#250) —
//! the lock is taken before any write and held for the run. The first two are
//! pure, and both land before the lock.
//!
//! **Stdout discipline** (mirror `pulse token issue`): the two tokens are the
//! ONLY stdout lines, written CHECKED, in label order (`qa-app`, then
//! `qa-agent`). The print runs INSIDE the seed's one transaction (PR-354 fix
//! C7): a print failure rolls the replace back — nothing was issued, so no
//! credential exists that no operator has seen, and the re-run the message
//! promises succeeds. A committed-then-revoked pair could not be re-issued:
//! labels are never reused (the `UNIQUE` holds after a revoke), so the failure
//! would leave `qa-app`/`qa-agent` taken forever.
//!
//! **The marker goes LAST** (PR-354 fix D2): print, COMMIT, then write
//! `<data dir>/server-role`. A marker failure after the commit leaves two VALID
//! tokens and says so (`pulse serve --role qa` writes the marker on its first
//! start — no re-run needed); a commit failure after the print says the printed
//! tokens are NOT valid and that a re-run works. The old order wrote the marker
//! first, so a failed commit left a `qa` marker over a database whose prod
//! tokens were still active.
//!
//! `--db` and `--data-dir` are REQUIRED. The defaults are prod's paths, and a
//! seed that silently defaulted onto prod's data would be exactly the accident
//! the role marker exists to prevent.

use std::path::PathBuf;

use clap::Args;

use crate::adapters::db::NewToken;
use crate::adapters::db::RESERVED_LABELS;
use crate::adapters::db::ReplacePublishedError;
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

/// The two fresh tokens the seed issues, in the order their lines print — the
/// reserved labels themselves ([`RESERVED_LABELS`], PR-354 fix D7), so the
/// reservation and the seed can never drift apart.
const QA_TOKENS: [(&str, Scope); 2] = [
    (RESERVED_LABELS[0], Scope::App),
    (RESERVED_LABELS[1], Scope::Agent),
];

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
/// a failure to print the tokens (the transaction rolls back: nothing was
/// issued and the re-run succeeds), or a failure to write the marker AFTER the
/// commit (the tokens are valid then, and the message says so).
pub(crate) async fn run_qa_seed(args: &QaSeedArgs) -> anyhow::Result<()> {
    // ---- The containment rule first (`serve --role`'s own, one copy of it):
    // the marker describes the data dir's data, so a `--db` outside `--data-dir`
    // is refused by name. This is the cutover's own accident: draco-desk's old
    // prod db is a rollback that sits in the old data dir and is unlocked once
    // prod stops there, and the seed would revoke every token in it (F3). Pure,
    // so it lands before the lock file and before any write.
    role::ensure_db_inside_data_dir(&args.data_dir, &args.db)
        .map_err(|error| anyhow::anyhow!("qa-seed: refusing: {error}"))?;

    // ---- The marker: a prod-marked dir is never seeded (C5). Read next, still
    // before the lock file or any write.
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
    // ---- ONE transaction: every copied token revoked, the two fresh ones
    // issued — and the marker write and the token print run INSIDE it (PR-354
    // fix C7). A failure there rolls the whole replace back, so NOTHING was
    // issued: no credential exists that no operator has seen, and the fresh
    // labels stay free — the re-run the failure message promises succeeds
    // instead of dying on `LabelExists` (labels are never reused, so the
    // committed-then-revoked pair this used to leave behind blocked every
    // re-run).
    let revoked = match repo
        .replace_all_with_published(&rows, "cli:qa-seed", || print_tokens(&fresh))
        .await
    {
        Ok(revoked) => revoked,
        Err(ReplacePublishedError::Store(error)) => {
            return Err(anyhow::anyhow!("qa-seed: {error}"));
        }
        Err(ReplacePublishedError::Publish(error)) => return Err(error),
        Err(ReplacePublishedError::Commit(error)) => {
            return Err(anyhow::anyhow!(
                "qa-seed: the tokens printed above are NOT valid: the transaction could not be \
                 committed ({error}); nothing was issued — re-run the command"
            ));
        }
    };
    eprintln!(
        "pulse qa-seed: revoked {} copied token(s); issued qa-app and qa-agent",
        revoked.len()
    );

    // ---- The `qa` marker LAST, after the commit (PR-354 fix D2): the tokens
    // are committed and printed by now, so a marker failure leaves two VALID
    // tokens and an unmarked dir — never a `qa` marker over a database whose
    // prod tokens are still active. `pulse serve --role qa` writes the marker
    // itself on its first start (the absent-marker path), so no re-run is
    // needed.
    role::write(&args.data_dir, ServerRole::Qa).map_err(|error| {
        anyhow::anyhow!(
            "qa-seed: the tokens above ARE valid, but the marker could not be written: {error}; \
             `pulse serve --role qa` creates it on its first start — no re-run is needed"
        )
    })
}

/// Print the two tokens as the ONLY stdout lines, INSIDE the seed's transaction
/// (PR-354 fix C7): a print failure returns before the commit, so the whole
/// replace rolls back and nothing was issued. The `qa` marker is written after
/// the commit (PR-354 fix D2), never before it.
fn print_tokens(fresh: &[FreshToken]) -> anyhow::Result<()> {
    write_token_lines(&fresh[0].token, &fresh[1].token).map_err(|error| {
        anyhow::anyhow!(
            "qa-seed: the tokens could not be printed ({error}); nothing was issued — re-run the command"
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{QaSeedArgs, run_qa_seed};
    use crate::adapters::db::client_token_repo::probe;
    use crate::adapters::db::{NewToken, SqliteClientTokenRepo, open_migrated};
    use crate::server::role::ROLE_MARKER_NAME;
    use std::fs;
    use tempfile::TempDir;

    /// PR-354 fix D2: a COMMIT failure lands AFTER the print, so the run must
    /// say the printed tokens are NOT valid, leave NO `qa` marker (the marker is
    /// written only after a successful commit) and leave the database exactly as
    /// it was — and the re-run must then succeed.
    #[tokio::test]
    async fn a_commit_failure_leaves_no_marker_and_the_re_run_works() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("qa-data");
        fs::create_dir_all(&data_dir).unwrap();
        let db = data_dir.join("pulse.db");
        let handle = open_migrated(&db).await.unwrap();
        // A copied prod token, so the replace has something to revoke.
        let repo = SqliteClientTokenRepo::new(handle.pool().clone());
        repo.issue("copied-app", "app", &"a".repeat(64), "cli:token-issue")
            .await
            .unwrap();
        handle.pool().close().await;

        let args = QaSeedArgs {
            db: db.clone(),
            data_dir: data_dir.clone(),
        };
        probe::fail_next_commit();
        let error = run_qa_seed(&args)
            .await
            .expect_err("the injected commit failure fails the run");
        let message = error.to_string();
        assert!(
            message.contains("NOT valid") && message.contains("re-run"),
            "the message says the printed tokens are not valid: {message}"
        );
        assert!(
            !data_dir.join(ROLE_MARKER_NAME).exists(),
            "no marker after a commit failure"
        );

        // Nothing was written: the copied token is still active, and the fresh
        // labels are free — the re-run succeeds and marks the dir.
        run_qa_seed(&args).await.expect("the re-run succeeds");
        assert_eq!(
            fs::read_to_string(data_dir.join(ROLE_MARKER_NAME)).unwrap(),
            "qa\n"
        );
        let handle = open_migrated(&db).await.unwrap();
        let active: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_token WHERE revoked_at IS NULL")
                .fetch_one(handle.pool())
                .await
                .unwrap();
        assert_eq!(active, 2, "exactly the two fresh tokens are active");
        handle.pool().close().await;
    }

    /// The reserved labels (PR-354 fix D7) are exactly the seed's two: the
    /// ordinary issue path refuses them, and the seed itself still issues both.
    #[tokio::test]
    async fn the_seed_issues_the_labels_the_ordinary_path_reserves() {
        let dir = TempDir::new().unwrap();
        let data_dir = dir.path().join("qa-data");
        fs::create_dir_all(&data_dir).unwrap();
        let db = data_dir.join("pulse.db");
        let handle = open_migrated(&db).await.unwrap();
        let repo = SqliteClientTokenRepo::new(handle.pool().clone());
        let fresh = [
            NewToken {
                label: "qa-app",
                scope: "app",
                token_sha256: &"b".repeat(64),
            },
            NewToken {
                label: "qa-agent",
                scope: "agent",
                token_sha256: &"c".repeat(64),
            },
        ];
        repo.replace_all_with_published(&fresh, "cli:qa-seed", || Ok::<(), String>(()))
            .await
            .expect("the seed issues the reserved labels");
        let active: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM client_token WHERE revoked_at IS NULL")
                .fetch_one(handle.pool())
                .await
                .unwrap();
        assert_eq!(active, 2);
        handle.pool().close().await;
    }
}
