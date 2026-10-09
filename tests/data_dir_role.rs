//! r4.s2.w3 (demo line d70) — the data-dir role marker, the startup refusal
//! order, QA seeding and the handshake's `role` field.
//!
//! The marker (`<data dir>/server-role`) is what keeps QA and prod from ever
//! serving each other's data: `pulse serve --role <prod|qa>` writes it on an
//! unmarked dir, continues on the same role, and REFUSES — non-zero, naming both
//! roles and the dir — on a different one, before anything is opened: no
//! database file created or migrated, no instance lock, no start-log entry, no
//! marker change. `pulse qa-seed` revokes every token a copied database carries
//! and issues the two fresh QA tokens, and the handshake reports the role.
//!
//! Every case drives the REAL binary in a temporary dir (the `launchd_units.rs`
//! and `import_move_safety.rs` precedent), with `HOME` inside the temp dir and
//! the XDG overrides stripped, so nothing here can reach a real data dir. The
//! refusal runs pass `--start-limit 3/900`, so "no start-log entry" is a real
//! assertion rather than a flag the server never had.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// The one binary this crate builds (ADR-0015) — the same artifact `pulse
/// serve` runs from in production.
const PULSE: &str = env!("CARGO_BIN_EXE_pulse");

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// A `pulse` invocation with the ambient config environment stripped: no
/// `PULSE_CONFIG_DIR` (so no `.env` is ever read by a test server) and no XDG
/// overrides (the caller sets `HOME` inside its temp dir). Each test also
/// passes explicit `--db`/`--data-dir`, so no default path can be reached.
fn pulse_cmd() -> Command {
    let mut cmd = Command::new(PULSE);
    cmd.env_remove("PULSE_CONFIG_DIR")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_CACHE_HOME");
    cmd
}

/// Run `pulse` to completion inside `home`, capturing both streams.
fn run(home: &Path, args: &[&str]) -> Output {
    pulse_cmd()
        .env("HOME", home)
        .args(args)
        .output()
        .expect("spawn the pulse binary")
}

/// Both streams, for a failure message.
fn text(out: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Run `pulse` to completion, BOUNDED (30 s). A refused start must EXIT — that
/// is the whole point of the refusal — so a command still running at the
/// deadline is killed and its output returned: the assertion that follows then
/// fails on the missing refusal instead of hanging the suite.
fn run_refused(home: &Path, args: &[&str]) -> Output {
    let mut child = pulse_cmd()
        .env("HOME", home)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the pulse binary");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if child.try_wait().expect("wait on the child").is_some() {
            return child.wait_with_output().expect("collect the output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            return child
                .wait_with_output()
                .expect("collect the output after the deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// A scratch root plus a hermetic `HOME` inside it.
fn scratch() -> (TempDir, std::path::PathBuf) {
    let root = TempDir::new().expect("a temp dir");
    let home = root.path().join("home");
    fs::create_dir_all(&home).expect("create HOME");
    (root, home)
}

/// Write a role marker exactly as `pulse serve` does (one line).
fn write_marker(dir: &Path, role: &str) {
    fs::write(dir.join("server-role"), format!("{role}\n")).expect("write the marker");
}

fn marker_of(dir: &Path) -> String {
    fs::read_to_string(dir.join("server-role")).expect("the marker exists")
}

/// SHA-256 of a file, hex — the byte-identical assertions on refused starts.
fn sha256_file(path: &Path) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(fs::read(path).expect("read the file")))
}

/// A running `pulse serve`, killed when the test drops it (the flock dies with
/// the process, so the next start is free).
struct Running {
    child: Child,
    /// `http://127.0.0.1:<port>` — the address the startup line reported.
    base: String,
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Start a server and wait for its `listening on` startup line (stderr — stdout
/// stays empty), returning the child and its base URL. The wait is bounded: a
/// server that exits or never binds fails the test with what it printed.
///
/// `zombie_processes`: the child is moved into [`Running`], whose `Drop` kills
/// and REAPS it — the panic paths included — and clippy cannot see through the
/// guard type.
#[allow(clippy::zombie_processes)]
fn start_server(home: &Path, args: &[&str]) -> Running {
    let mut child = pulse_cmd()
        .env("HOME", home)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn pulse serve");
    let stderr = child.stderr.take().expect("stderr is piped");
    let (sender, receiver) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            // Keep READING when nobody listens any more: this pipe is the
            // server's stderr, and CLOSING it (the old `break` on a failed
            // send) makes the server's next log write fail, which kills the
            // connection the request rides — the `qa_seed_can_be_re_run` test
            // makes more than one request against one server.
            let _ = sender.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut seen: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "no `listening on` line within 90 s; saw: {seen:?}"
        );
        let Ok(line) = receiver.recv_timeout(left) else {
            panic!("the server stopped before it listened; saw: {seen:?}");
        };
        if let Some(address) = line.split("listening on ").nth(1) {
            return Running {
                child,
                base: format!("http://{}", address.trim()),
            };
        }
        seen.push(line);
    }
}

/// A raw handshake GET: the response TEXT, so an assertion can see a field the
/// server OMITS (a typed client cannot distinguish absent from null).
fn handshake(base: &str, token: &str) -> String {
    let address = base.strip_prefix("http://").expect("an http base");
    let mut stream = std::net::TcpStream::connect(address).expect("connect the server");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set a read timeout");
    let request = format!(
        "GET /api/v1/handshake HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .expect("write the request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read the response");
    response
}

/// Issue one token through the real CLI and return its one stdout line.
fn token_issue(home: &Path, db: &Path, scope: &str, label: &str) -> String {
    let out = run(
        home,
        &[
            "token",
            "issue",
            "--scope",
            scope,
            "--label",
            label,
            "--db",
            db.to_str().expect("utf-8 db path"),
        ],
    );
    assert!(out.status.success(), "token issue failed: {}", text(&out));
    String::from_utf8(out.stdout)
        .expect("utf-8 token")
        .trim()
        .to_owned()
}

/// `pulse token list` — labels and states, never a token (the CLI's own rule).
fn token_list(home: &Path, db: &Path) -> String {
    let out = run(
        home,
        &["token", "list", "--db", db.to_str().expect("utf-8 db path")],
    );
    assert!(out.status.success(), "token list failed: {}", text(&out));
    String::from_utf8(out.stdout).expect("utf-8 list")
}

// ---------------------------------------------------------------------------
// The refusal order (spec §1): a cross-role start touches NOTHING.
// ---------------------------------------------------------------------------

#[test]
fn a_qa_start_is_refused_on_a_dir_marked_prod() {
    let (root, home) = scratch();
    let data = root.path().join("prod-data");
    fs::create_dir_all(&data).unwrap();
    write_marker(&data, "prod");
    let db = data.join("pulse.db");

    let out = run_refused(
        &home,
        &[
            "serve",
            "--role",
            "qa",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--start-limit",
            "3/900",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let text = text(&out);
    assert!(
        !out.status.success(),
        "a QA start on a prod-marked dir must exit non-zero: {text}"
    );
    assert!(
        text.contains("marked prod") && text.contains("--role qa"),
        "the refusal names both roles: {text}"
    );
    assert!(
        text.contains(data.to_str().unwrap()),
        "the refusal names the data dir: {text}"
    );
    assert!(
        !text.contains("listening on"),
        "refused before binding: {text}"
    );
    assert_eq!(marker_of(&data).trim(), "prod", "the marker is unchanged");
    assert!(
        !db.exists(),
        "refused before the database was created: {}",
        db.display()
    );
    assert!(
        !data.join("pulse.db.serve.lock").exists(),
        "no instance lock is taken by a refused start"
    );
    assert!(
        !data.join("serve-starts").exists() && !data.join("serve-start-limit").exists(),
        "no start-log entry and no start-limit marker"
    );
}

#[test]
fn a_prod_start_is_refused_on_a_dir_marked_qa() {
    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    fs::create_dir_all(&data).unwrap();
    write_marker(&data, "qa");
    let db = data.join("pulse.db");
    // A file where the database would be: a refusal that reached `open_db`
    // would have to touch it (or fail on it) — the marker refusal comes first.
    fs::write(
        &db,
        b"NOT A DATABASE - a refused start must never open this",
    )
    .unwrap();
    let before = sha256_file(&db);

    let out = run_refused(
        &home,
        &[
            "serve",
            "--role",
            "prod",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--start-limit",
            "3/900",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let text = text(&out);
    assert!(
        !out.status.success(),
        "a prod start on a qa-marked dir must exit non-zero: {text}"
    );
    assert!(
        text.contains("marked qa") && text.contains("--role prod"),
        "the refusal names both roles: {text}"
    );
    assert_eq!(marker_of(&data).trim(), "qa", "the marker is unchanged");
    assert_eq!(
        sha256_file(&db),
        before,
        "the database file is byte-identical after a refused start"
    );
    assert!(
        !data.join("pulse.db.serve.lock").exists(),
        "no instance lock"
    );
    assert!(
        !data.join("serve-starts").exists() && !data.join("serve-start-limit").exists(),
        "no start-log entry and no start-limit marker"
    );
}

#[test]
fn a_role_start_is_refused_when_the_db_is_outside_the_data_dir() {
    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    let elsewhere = root.path().join("elsewhere");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let db = elsewhere.join("pulse.db");

    let out = run_refused(
        &home,
        &[
            "serve",
            "--role",
            "qa",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let text = text(&out);
    assert!(
        !out.status.success(),
        "a database outside the data dir must be refused: {text}"
    );
    assert!(
        text.contains(db.to_str().unwrap()) && text.contains(data.to_str().unwrap()),
        "the refusal names the database and the data dir: {text}"
    );
    assert!(
        !data.join("server-role").exists(),
        "a refused start writes no marker"
    );
    assert!(
        !db.exists(),
        "and the database is not created: {}",
        db.display()
    );
    assert!(
        !elsewhere.join("pulse.db.serve.lock").exists()
            && !data.join("pulse.db.serve.lock").exists(),
        "no instance lock is taken by a refused start"
    );
}

// ---------------------------------------------------------------------------
// The marker's happy path: an unmarked dir is marked, then accepted.
// ---------------------------------------------------------------------------

#[test]
fn an_unmarked_dir_is_marked_on_first_start_and_accepted_on_the_next() {
    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    fs::create_dir_all(&data).unwrap();
    let db = data.join("pulse.db");
    let args = [
        "serve",
        "--role",
        "qa",
        "--dev-loopback",
        "--bind",
        "127.0.0.1:0",
        "--db",
        db.to_str().unwrap(),
        "--data-dir",
        data.to_str().unwrap(),
    ];

    let first = start_server(&home, &args);
    assert!(first.base.starts_with("http://127.0.0.1:"));
    assert_eq!(
        marker_of(&data).trim(),
        "qa",
        "the first start marks the unmarked dir"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = fs::metadata(data.join("server-role"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the marker is private");
    }
    drop(first);

    // The same role is accepted on the next start (the marker is read, not
    // rewritten) and the server listens again.
    let second = start_server(&home, &args);
    assert!(second.base.starts_with("http://127.0.0.1:"));
    assert_eq!(marker_of(&data).trim(), "qa");
}

/// PR-354 fix C2a: the FIRST start may be the one that creates the data dir.
/// `role::write` never created it, so `pulse serve --role prod` on a fresh
/// data dir failed with ENOENT before `open_db` could — and launchd relaunched
/// it forever (deploy-mac's "the server creates it on first start" case).
#[test]
fn a_first_start_creates_the_data_dir_it_marks() {
    let (root, home) = scratch();
    let data = root.path().join("fresh-data");
    assert!(!data.exists(), "the dir is this start's to make");
    let db = data.join("pulse.db");

    let server = start_server(
        &home,
        &[
            "serve",
            "--role",
            "prod",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );

    assert!(server.base.starts_with("http://127.0.0.1:"));
    assert_eq!(
        marker_of(&data).trim(),
        "prod",
        "the fresh dir is created and marked"
    );
    assert!(db.exists(), "and the database is there beside the marker");
}

// ---------------------------------------------------------------------------
// The handshake's additive `role` field (C5).
// ---------------------------------------------------------------------------

#[test]
fn the_handshake_carries_the_role_and_omits_it_without_the_flag() {
    let (root, home) = scratch();

    // A `--role qa` server: the marker is written by this start, the token is
    // issued first (the handshake needs a live token).
    let qa_data = root.path().join("qa-data");
    fs::create_dir_all(&qa_data).unwrap();
    let qa_db = qa_data.join("pulse.db");
    let qa_token = token_issue(&home, &qa_db, "app", "hs-qa");
    let qa = start_server(
        &home,
        &[
            "serve",
            "--role",
            "qa",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            qa_db.to_str().unwrap(),
            "--data-dir",
            qa_data.to_str().unwrap(),
        ],
    );
    let body = handshake(&qa.base, &qa_token);
    assert!(
        body.starts_with("HTTP/1.1 200") || body.contains("HTTP/1.1 200"),
        "the handshake answers 200: {body}"
    );
    assert!(body.contains("\"role\":\"qa\""), "{body}");
    drop(qa);

    // A `--role prod` server reports prod.
    let prod_data = root.path().join("prod-data");
    fs::create_dir_all(&prod_data).unwrap();
    let prod_db = prod_data.join("pulse.db");
    let prod_token = token_issue(&home, &prod_db, "app", "hs-prod");
    let prod = start_server(
        &home,
        &[
            "serve",
            "--role",
            "prod",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            prod_db.to_str().unwrap(),
            "--data-dir",
            prod_data.to_str().unwrap(),
        ],
    );
    let body = handshake(&prod.base, &prod_token);
    assert!(body.contains("\"role\":\"prod\""), "{body}");
    drop(prod);

    // No `--role`: the field is OMITTED entirely (an older server's shape), and
    // no marker is written.
    let plain_data = root.path().join("plain-data");
    fs::create_dir_all(&plain_data).unwrap();
    let plain_db = plain_data.join("pulse.db");
    let plain_token = token_issue(&home, &plain_db, "app", "hs-plain");
    let plain = start_server(
        &home,
        &[
            "serve",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            plain_db.to_str().unwrap(),
            "--data-dir",
            plain_data.to_str().unwrap(),
        ],
    );
    let body = handshake(&plain.base, &plain_token);
    assert!(
        !body.contains("\"role\""),
        "a server without --role omits the field: {body}"
    );
    assert!(
        !plain_data.join("server-role").exists(),
        "and writes no marker"
    );
}

// ---------------------------------------------------------------------------
// QA seeding (C5): every copied token revoked, two fresh ones, the marker.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qa_seed_revokes_every_copied_token_and_issues_two_fresh_ones() {
    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    fs::create_dir_all(&data).unwrap();
    let db = data.join("pulse.db");
    // Three copied tokens — the shape a restored prod backup carries.
    let copied = ["copied-app", "copied-agent", "copied-campaign"];
    for (index, label) in copied.iter().enumerate() {
        let scope = if index == 1 { "agent" } else { "app" };
        let _ = token_issue(&home, &db, scope, label);
    }

    let out = run(
        &home,
        &[
            "qa-seed",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "qa-seed failed: {}", text(&out));

    // The two fresh tokens are the ONLY stdout lines, one per label.
    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        2,
        "exactly two stdout lines (the tokens): {stdout:?}"
    );
    for line in &lines {
        assert!(
            line.starts_with("pt_") && line.len() == 46,
            "a minted token: {line:?}"
        );
    }

    // Every copied token is revoked; exactly the fresh two are active.
    let list = token_list(&home, &db);
    for label in copied {
        assert!(
            list.lines()
                .any(|line| line.starts_with(&format!("{label} ")) && line.contains("revoked:")),
            "{label} must be revoked: {list}"
        );
    }
    for label in ["qa-app", "qa-agent"] {
        assert!(
            list.lines()
                .any(|line| line.starts_with(&format!("{label} ")) && line.ends_with("active")),
            "{label} must be active: {list}"
        );
    }

    // Every revoke is audited (the risk gate's audit trail), and the marker
    // says `qa`.
    let handle = pulse::Db::with_path(&db).await.expect("open the seeded db");
    let revoked: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM token_audit WHERE event = 'revoked'")
            .fetch_one(handle.pool())
            .await
            .expect("count revoked audits");
    let issued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM token_audit WHERE event = 'issued'")
        .fetch_one(handle.pool())
        .await
        .expect("count issued audits");
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM client_token WHERE revoked_at IS NULL")
            .fetch_one(handle.pool())
            .await
            .expect("count active tokens");
    handle.pool().close().await;
    assert_eq!(revoked, 3, "one audited revoke per copied token");
    assert_eq!(issued, 5, "three copied issues plus the two fresh ones");
    assert_eq!(active, 2, "exactly the two fresh tokens are active");
    assert_eq!(marker_of(&data).trim(), "qa", "the seed marks the dir qa");
}

#[test]
fn qa_seed_refuses_a_database_a_live_server_holds() {
    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    fs::create_dir_all(&data).unwrap();
    let db = data.join("pulse.db");
    let server = start_server(
        &home,
        &[
            "serve",
            "--role",
            "qa",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );

    let out = run_refused(
        &home,
        &[
            "qa-seed",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let text = text(&out);
    assert!(
        !out.status.success(),
        "a held database refuses the seed: {text}"
    );
    assert!(
        text.contains("holds it") && text.contains(db.to_str().unwrap()),
        "the refusal names the holder and the database: {text}"
    );
    drop(server);
}

#[test]
fn qa_seed_refuses_a_dir_marked_prod() {
    let (root, home) = scratch();
    let data = root.path().join("prod-data");
    fs::create_dir_all(&data).unwrap();
    write_marker(&data, "prod");
    let db = data.join("pulse.db");

    let out = run_refused(
        &home,
        &[
            "qa-seed",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let text = text(&out);
    assert!(!out.status.success(), "a prod-marked dir refuses: {text}");
    assert!(
        text.contains("marked prod") && text.contains(data.to_str().unwrap()),
        "the refusal names the marker and the dir: {text}"
    );
    assert_eq!(marker_of(&data).trim(), "prod", "the marker is unchanged");
    assert!(
        !db.exists(),
        "the refusal lands before the database is touched: {}",
        db.display()
    );
}

#[test]
fn qa_seed_refuses_a_database_outside_the_data_dir() {
    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    let elsewhere = root.path().join("old-prod-data");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let db = elsewhere.join("pulse.db");
    // The database the cutover leaves where it is: draco-desk's old prod db is
    // the rollback, it sits OUTSIDE QA's data dir, and it carries prod's copied
    // tokens. A seed pointed at it must revoke none of them.
    for (index, label) in ["copied-app", "copied-agent"].iter().enumerate() {
        let scope = if index == 1 { "agent" } else { "app" };
        let _ = token_issue(&home, &db, scope, label);
    }
    let before = token_list(&home, &db);

    let out = run_refused(
        &home,
        &[
            "qa-seed",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let text = text(&out);
    assert!(
        !out.status.success(),
        "a database outside the data dir must be refused: {text}"
    );
    assert!(
        text.contains(db.to_str().unwrap()) && text.contains(data.to_str().unwrap()),
        "the refusal names the database and the data dir: {text}"
    );

    // The token set is exactly as it was: nothing revoked, nothing issued — the
    // one transaction never ran.
    assert_eq!(
        token_list(&home, &db),
        before,
        "a refused seed leaves every token in the database alone"
    );
    assert!(
        !data.join("server-role").exists(),
        "a refused seed writes no marker"
    );
    assert!(
        !elsewhere.join("pulse.db.serve.lock").exists(),
        "the refusal lands before the instance lock"
    );
}

/// PR-354 fix C7: the recovery the publish-failure message promises is TRUE.
/// The marker write and the token print run INSIDE the seed's transaction, so a
/// failed print rolls the replace back — nothing was issued, and a re-run issues
/// the two QA tokens instead of dying on `LabelExists`. Before the fix the
/// fresh pair was committed and then revoked, and revoked labels are never
/// reused, so every re-run was blocked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qa_seed_can_be_re_run_after_a_publish_failure() {
    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    fs::create_dir_all(&data).unwrap();
    let db = data.join("pulse.db");
    let copied = token_issue(&home, &db, "app", "copied-app");

    // The publish failure: the read end of the child's stdout is CLOSED before
    // it can print, so the token print fails with a broken pipe — the operator's
    // broken pipe / full disk, injected without any seam in the binary.
    let mut child = pulse_cmd()
        .env("HOME", &home)
        .args([
            "qa-seed",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn qa-seed with a closed stdout");
    drop(child.stdout.take().expect("stdout is piped"));
    let failed = child.wait_with_output().expect("wait for qa-seed");
    let failure_text = String::from_utf8_lossy(&failed.stderr).into_owned();
    assert!(
        !failed.status.success(),
        "the print failure fails the run: {failure_text}"
    );
    assert!(
        failure_text.contains("re-run the command"),
        "the failure tells the operator to re-run: {failure_text}"
    );

    assert!(
        !data.join("server-role").exists(),
        "a failed print leaves no marker: the marker is written only after the commit"
    );

    // The re-run: it must SUCCEED — the failed run issued nothing, so the two
    // labels are free (and the copied token is still the active one).
    let again = run(
        &home,
        &[
            "qa-seed",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    assert!(
        again.status.success(),
        "the re-run the message promised succeeds: {}",
        text(&again)
    );
    let stdout = String::from_utf8(again.stdout).expect("utf-8 stdout");
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 2, "exactly two stdout lines: {stdout:?}");

    // And both tokens WORK: a QA server accepts each, reporting the scope its
    // line order promises (qa-app then qa-agent).
    let server = start_server(
        &home,
        &[
            "serve",
            "--role",
            "qa",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    for (token, scope) in lines.iter().zip(["app", "agent"]) {
        assert!(token.starts_with("pt_"), "a minted token: {token:?}");
        let body = handshake(&server.base, token);
        assert!(
            body.contains("HTTP/1.1 200"),
            "the re-run's {scope} token works: {body}"
        );
        assert!(
            body.contains(&format!("\"scope\":\"{scope}\"")),
            "and it carries the {scope} scope: {body}"
        );
        assert!(body.contains("\"role\":\"qa\""), "on the qa role: {body}");
    }
    // The copied prod token was revoked by the re-run: the server refuses it.
    let refused = handshake(&server.base, &copied);
    assert!(
        refused.contains("HTTP/1.1 401"),
        "the copied token is revoked by the seed: {refused}"
    );
    drop(server);

    // And the database says the same.
    let list = token_list(&home, &db);
    assert!(
        list.lines()
            .any(|line| line.starts_with("copied-app ") && line.contains("revoked:")),
        "the copied token is revoked: {list}"
    );
}

/// PR-354 fix D2: a marker failure AFTER the commit leaves two VALID tokens and
/// says so — the tokens are committed and printed, `pulse serve --role qa`
/// writes the marker on its first start, and no re-run is needed. The old order
/// wrote the marker inside the transaction, so a failed commit could leave a
/// `qa` marker over a database whose prod tokens were still active.
///
/// The injection: the DATA DIR is read-only (the marker's temp file cannot be
/// created) while the database lives in a writable subdirectory — so the marker
/// read sees no marker, the lock, the database and the commit all work, and
/// only the marker write fails.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn qa_seed_reports_valid_tokens_when_the_marker_fails_after_the_commit() {
    use std::os::unix::fs::PermissionsExt as _;

    let (root, home) = scratch();
    let data = root.path().join("qa-data");
    let db_dir = data.join("db");
    fs::create_dir_all(&db_dir).unwrap();
    let db = db_dir.join("pulse.db");
    let copied = token_issue(&home, &db, "app", "copied-app");
    fs::set_permissions(&data, fs::Permissions::from_mode(0o500)).unwrap();

    let out = run(
        &home,
        &[
            "qa-seed",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    let text = text(&out);
    assert!(
        !out.status.success(),
        "the marker failure fails the run: {text}"
    );
    assert!(
        text.contains("ARE valid") && text.contains("no re-run is needed"),
        "the message says the tokens are valid and no re-run is needed: {text}"
    );
    assert!(
        !data.join("server-role").exists(),
        "and no marker was written"
    );

    let stdout = String::from_utf8(out.stdout).expect("utf-8 stdout");
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 2, "the two tokens were printed: {stdout:?}");

    // Both printed tokens WORK — a server WITHOUT --role (no marker check)
    // accepts each with its scope — and the copied prod token is refused.
    fs::set_permissions(&data, fs::Permissions::from_mode(0o700)).unwrap();
    let server = start_server(
        &home,
        &[
            "serve",
            "--dev-loopback",
            "--bind",
            "127.0.0.1:0",
            "--db",
            db.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
        ],
    );
    for (token, scope) in lines.iter().zip(["app", "agent"]) {
        let body = handshake(&server.base, token);
        assert!(
            body.contains("HTTP/1.1 200"),
            "the committed {scope} token works: {body}"
        );
        assert!(
            body.contains(&format!("\"scope\":\"{scope}\"")),
            "and it carries the {scope} scope: {body}"
        );
    }
    let refused = handshake(&server.base, &copied);
    assert!(
        refused.contains("HTTP/1.1 401"),
        "the copied token is revoked by the seed: {refused}"
    );
    drop(server);
}
