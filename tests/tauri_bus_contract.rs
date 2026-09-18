//! AC-3 / AC-4 / AC-5 — the command-bus contract (r1.s1.w1, ADR-0020 step 5).
//!
//! This file is the **pinned interface** `r1.s1.w3`, `r1.s1.w4` and `r1.s1.w5` code
//! against. It is fixed here so those three items do not each discover their own
//! answer to the same five questions, and it is a test rather than prose so a later
//! item cannot quietly weaken it.
//!
//! The five clauses of the contract, and what asserts each:
//!
//! | Clause | Test |
//! |---|---|
//! | One serializable error shape | `domain_error_maps_to_one_serializable_shape` (AC-4) |
//! | Event correlation via a per-invocation channel | `event_carries_its_command_run_id` (AC-5) |
//! | | `two_runs_get_two_channels_and_never_cross` |
//! | Async + cancellation | `an_unmounted_screen_cancels_its_in_flight_stream` |
//! | | `every_registered_bus_command_is_async` |
//! | Managed state ownership | `managed_state_owns_the_pool_and_serves_a_real_round_trip` |
//! | One append-only registration point | `command_registration_is_one_append_only_list` |
//! | | `the_route_table_is_one_append_only_entry_per_screen` |
//!
//! **The channel tests drive the REAL `tauri::ipc::Channel`, not a stand-in.**
//! `Channel::new` takes a message handler and needs no running app, so the test can
//! collect exactly the bytes the webview would have received and deserialize them
//! back. A fake sink would have proved the core loop; this proves the boundary.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, Mutex};

use pulse::{
    BUS_COMMANDS, BacktestAppError, BacktestError, BacktestRunId, BusError, BusErrorCode, BusEvent,
    ComposerError, DataError, DesktopState, EventSink, ExchangeError, LlmError, Pair,
    ReadBackFailure, ReadBackStage, RunId, StrategyRepository, Timeframe, demo_stream_core,
    shell_info_core,
};
use tauri::ipc::{Channel, InvokeResponseBody};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A real `tauri::ipc::Channel<BusEvent>` plus the decoded events it received.
///
/// `Channel::send` hands the serialized payload to the handler exactly as it would
/// hand it to the webview, so what lands in `received` is the wire representation.
fn recording_channel() -> (Channel<BusEvent>, Arc<Mutex<Vec<serde_json::Value>>>) {
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    let channel = Channel::new(move |body: InvokeResponseBody| {
        let json = match body {
            InvokeResponseBody::Json(s) => s,
            InvokeResponseBody::Raw(bytes) => String::from_utf8(bytes).unwrap(),
        };
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        sink.lock().unwrap().push(value);
        Ok(())
    });
    (channel, received)
}

/// An `EventSink` that refuses every send — the shape of a screen that unmounted
/// while its stream was still running.
struct DeadSink;

impl EventSink for DeadSink {
    fn send_event(&self, _event: BusEvent) -> Result<(), BusError> {
        Err(BusError::new(
            BusErrorCode::Internal,
            "channel closed".to_owned(),
        ))
    }
}

fn manifest_path(rel: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(rel)
}

// ---------------------------------------------------------------------------
// AC-4 — one serializable error shape
// ---------------------------------------------------------------------------

/// The ONE key set every `BusError` serializes to, sorted.
///
/// Pinned as a constant so the two places that assert it cannot drift apart, and so
/// adding a field is a deliberate edit here rather than a quiet widening somewhere.
/// r2.s1.w4 added `child_run_id` for the same reason `run_id` exists — a
/// `compare_child_run` refusal names the run id it was asked about, and a screen
/// must not parse prose to learn it.
const BUS_ERROR_KEYS: [&str; 5] = ["child_run_id", "code", "message", "run_id", "session_id"];

/// Assert one mapped error serializes to the shared `BusError` shape and
/// return its sorted key set for the cross-error uniformity check below.
fn assert_bus_error_shape(label: &str, err: &BusError, expected_code: BusErrorCode) -> Vec<String> {
    assert_eq!(
        err.code, expected_code,
        "{label} mapped to the wrong BusErrorCode"
    );

    let value =
        serde_json::to_value(err).unwrap_or_else(|e| panic!("{label} must serialize to JSON: {e}"));
    let object = value
        .as_object()
        .unwrap_or_else(|| panic!("{label} must serialize to a JSON OBJECT, got {value}"));

    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    // r1.s3.w3 widened the shape to {code, message, run_id}; r1.s4.w3's review
    // added `session_id` for the same reason `run_id` exists — a Busy refusal
    // defers to a coach turn the caller must then RELOAD, and a screen that
    // parses the id out of the prose is one rewording away from starting a
    // second billable turn. The invariant this asserts is unchanged and, if
    // anything, stronger: ONE key set for every error, so the frontend still
    // renders with one code path. Both optional fields are ALWAYS present
    // rather than skipped-when-absent — a field that sometimes vanishes reaches
    // TypeScript as `undefined` while its generated type says `string | null`,
    // which is exactly the mismatch this clause exists to stop.
    assert_eq!(
        keys, BUS_ERROR_KEYS,
        "{label} must serialize to exactly {BUS_ERROR_KEYS:?}, got {keys:?}"
    );
    // No DOMAIN-family error can name a persisted run: only the application
    // ring's saved-but-unreadable case knows a run id, and it is not in this set.
    assert!(
        object["run_id"].is_null(),
        "{label} must carry a null run_id — only a saved-but-unreadable backtest \
         has a row to name"
    );
    assert!(
        object["child_run_id"].is_null(),
        "{label} must carry a null child_run_id — only a compare_child_run refusal \
         names the run it was asked about"
    );

    // `message` is the error's DISPLAY rendering, never a stringified `Debug`.
    // The cheapest reliable tell for a leaked `Debug` is Rust variant syntax:
    // `Parse("bad decimal")` / `UnknownSymbol(` etc.
    let message = object["message"].as_str().unwrap_or_else(|| {
        panic!(
            "{label}: `message` must be a JSON string, got {}",
            object["message"]
        )
    });
    assert!(
        !message.is_empty(),
        "{label}: `message` must not be empty -- an empty error renders as nothing"
    );
    for debug_marker in ["\", ", "\")", "Parse(", "Config(", "UnknownSymbol(", "Db("] {
        assert!(
            !message.contains(debug_marker),
            "{label}: `message` looks like a stringified Debug (contains {debug_marker:?}): \
             {message}\nUse the error's Display rendering."
        );
    }

    // `code` is a plain string discriminant the frontend can switch on -- not a
    // nested object, not a number that would renumber if a variant is inserted.
    assert!(
        object["code"].is_string(),
        "{label}: `code` must serialize as a string discriminant, got {}",
        object["code"]
    );

    keys.iter().map(|k| (*k).to_owned()).collect()
}

#[test]
fn domain_error_maps_to_one_serializable_shape() {
    // Every domain error that can cross the boundary, mapped through `From`.
    // A new crossable error type added later without a `From` impl will not compile
    // into this list, which is the point of listing them exhaustively here.
    let mapped: Vec<(&str, BusError, BusErrorCode)> = vec![
        (
            "DataError",
            DataError::Parse("bad decimal".to_owned()).into(),
            BusErrorCode::Data,
        ),
        (
            "DataError::Db",
            DataError::Db("no such table: strategy".to_owned()).into(),
            BusErrorCode::Data,
        ),
        (
            "BacktestError",
            BacktestError::NoStopLoss.into(),
            BusErrorCode::Backtest,
        ),
        (
            "ExchangeError",
            ExchangeError::UnknownSymbol("DOGEUSDT".to_owned()).into(),
            BusErrorCode::Exchange,
        ),
        (
            "LlmError",
            LlmError::Config("missing api key".to_owned()).into(),
            BusErrorCode::Llm,
        ),
        (
            "ComposerError",
            ComposerError::MaxTurns.into(),
            BusErrorCode::Composer,
        ),
        // r2.s1 F10: a windowed run whose sliced primary is empty is a
        // caller-correctable bad-argument refusal — the MCP surface maps the
        // same variant to `field_error("window", …)` — not a storage failure.
        (
            "BacktestAppError::WindowEmpty",
            BacktestAppError::WindowEmpty {
                pair: Pair::parse("BTCUSDT").unwrap(),
                timeframe: Timeframe::M15,
                from_ms: 1_700_000_000_000,
                to_ms: 1_700_086_400_000,
            }
            .into(),
            BusErrorCode::Validation,
        ),
    ];

    assert!(
        mapped.len() >= 6,
        "the crossable-error list must cover every domain error family"
    );

    // The invariant: ONE shape. Every mapping serializes to an object with exactly
    // the same key set -- so the frontend renders errors with one code path and never
    // has to sniff which family an error came from.
    let shapes: Vec<Vec<String>> = mapped
        .iter()
        .map(|(label, err, expected_code)| assert_bus_error_shape(label, err, *expected_code))
        .collect();

    let first = &shapes[0];
    for (i, shape) in shapes.iter().enumerate() {
        assert_eq!(
            shape, first,
            "shape {i} differs from the first -- that is more than one error shape crossing"
        );
    }

    // The shape survives a full round trip, so the generated TypeScript type is a
    // faithful description of the wire format and not merely of the Rust type.
    let err = BusError::from(DataError::Io("disk full".to_owned()));
    let json = serde_json::to_string(&err).unwrap();
    let back: BusError = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back, err,
        "BusError must round-trip through serde unchanged"
    );
}

/// Every `BusErrorCode`'s wire token, pinned by exact string.
///
/// The frontend branches on these words. A rename is therefore a breaking change
/// to a contract no compiler checks — TypeScript sees `string`, and a screen
/// comparing against the old spelling silently stops matching. r1.s4.w3 added
/// `busy` (`#141`'s single-flight refusal) and pins it here beside the others for
/// exactly that reason: it is the one code the coach rail renders as a STATE — a
/// transient "not settled yet, check again" — rather than as an error, so its
/// spelling is load-bearing in a way the others' are not. r2.s1.w4 adds
/// `not_found`, the refusal family for a named thing that is not there — first
/// used by `compare_child_run` for an unknown run id, an absent parent, or a
/// parent with no run.
#[test]
fn every_bus_error_code_serializes_as_its_pinned_token() {
    let pinned: Vec<(BusErrorCode, &str)> = vec![
        (BusErrorCode::Data, "data"),
        (BusErrorCode::Validation, "validation"),
        (BusErrorCode::Backtest, "backtest"),
        (BusErrorCode::Exchange, "exchange"),
        (BusErrorCode::Llm, "llm"),
        (BusErrorCode::Composer, "composer"),
        (BusErrorCode::Busy, "busy"),
        (BusErrorCode::NotFound, "not_found"),
        (BusErrorCode::Internal, "internal"),
    ];

    for (code, token) in &pinned {
        assert_eq!(
            serde_json::to_value(code).unwrap(),
            serde_json::json!(token),
            "{code:?} must serialize as {token:?}"
        );
        // And back, so the generated TypeScript union is a faithful description of
        // the wire in both directions.
        let round_tripped: BusErrorCode = serde_json::from_value(serde_json::json!(token)).unwrap();
        assert_eq!(round_tripped, *code);
    }

    // The list is exhaustive by construction: a new variant added without a row
    // here fails this match, not merely this assertion.
    for (code, _) in &pinned {
        match code {
            BusErrorCode::Data
            | BusErrorCode::Validation
            | BusErrorCode::Backtest
            | BusErrorCode::Exchange
            | BusErrorCode::Llm
            | BusErrorCode::Composer
            | BusErrorCode::Busy
            | BusErrorCode::NotFound
            | BusErrorCode::Internal => {}
        }
    }
    assert_eq!(pinned.len(), 9, "every code is pinned exactly once");
}

/// The one crossable error that CAN name a persisted run. The shape test above
/// loops only the domain-family errors, every one of which serializes `run_id`
/// as null — so without this case the `with_run_id` serialization path, the exact
/// payload the Backtest Lab renders as "saved, but could not be read back", is
/// unpinned by this suite: same key set, non-null id.
#[test]
fn the_saved_but_unreadable_case_keeps_the_one_shape_with_a_named_run() {
    let saved: BusError = BacktestAppError::SavedButReadBackFailed {
        run_id: BacktestRunId::new("run-1"),
        stage: ReadBackStage::Trades,
        failure: ReadBackFailure::Missing,
    }
    .into();
    assert_eq!(
        saved.code,
        BusErrorCode::Data,
        "the saved case maps to the Data family"
    );
    let value = serde_json::to_value(&saved).unwrap();
    let object = value.as_object().expect("the saved case is an object too");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys, BUS_ERROR_KEYS,
        "the saved-but-unreadable case keeps the ONE shape"
    );
    assert_eq!(
        object["run_id"],
        serde_json::json!("run-1"),
        "run_id serializes as the bare id, not a wrapper object"
    );
}

#[test]
fn a_domain_error_message_is_its_display_rendering() {
    // Pinned explicitly rather than only implied by the marker scan above.
    let source = DataError::Parse("bad decimal".to_owned());
    let expected = source.to_string();
    let mapped = BusError::from(source);
    assert_eq!(
        mapped.message, expected,
        "the bus must carry Display, which is the text a human reads"
    );
}

// ---------------------------------------------------------------------------
// AC-5 — event correlation via a per-invocation channel
// ---------------------------------------------------------------------------

#[tokio::test]
async fn event_carries_its_command_run_id() {
    let (channel, received) = recording_channel();
    let run_id = RunId::new();

    let outcome = demo_stream_core(&run_id, 4, &channel)
        .await
        .expect("the demo stream must complete over a live channel");

    let events = received.lock().unwrap().clone();
    assert!(
        !events.is_empty(),
        "a stream that emits nothing proves nothing -- the core must send events"
    );
    assert_eq!(
        u32::try_from(events.len()).expect("event count fits in u32"),
        outcome.emitted,
        "the outcome's emitted count must match what actually crossed the channel"
    );
    assert!(
        !outcome.cancelled,
        "an undisturbed run must not report cancellation"
    );
    assert_eq!(
        outcome.run_id, run_id,
        "the outcome must report the run id it was given"
    );

    // The load-bearing assertion: EVERY event carries the run id of the command
    // invocation that produced it. This is what lets a screen attribute a token to
    // the compose run it started rather than to whichever run finished last.
    for (i, event) in events.iter().enumerate() {
        let object = event
            .as_object()
            .unwrap_or_else(|| panic!("event {i} must be a JSON object, got {event}"));
        let carried = object
            .get("runId")
            .unwrap_or_else(|| panic!("event {i} has no `runId` field: {event}"))
            .as_str()
            .unwrap_or_else(|| panic!("event {i}'s `runId` must be a string: {event}"));
        assert_eq!(
            carried,
            run_id.as_str(),
            "event {i} carries run id {carried:?} but the command was invoked with {:?}",
            run_id.as_str()
        );
    }

    // Sequence numbers are monotonic from 0, so a dropped event is detectable by the
    // frontend rather than silently producing a shorter stream.
    for (i, event) in events.iter().enumerate() {
        let seq = event["seq"]
            .as_u64()
            .unwrap_or_else(|| panic!("event {i} has no numeric `seq`"));
        assert_eq!(
            seq, i as u64,
            "event sequence numbers must be monotonic from 0"
        );
    }
}

#[tokio::test]
async fn two_runs_get_two_channels_and_never_cross() {
    // Grill A2: the channel IS the correlation. A second compose run gets a second
    // channel and cannot be mistaken for the first. If this ever regressed to a
    // global event bus, one of these buffers would contain the other's events.
    let (channel_a, received_a) = recording_channel();
    let (channel_b, received_b) = recording_channel();
    let run_a = RunId::new();
    let run_b = RunId::new();

    assert_ne!(run_a, run_b, "two runs must mint two distinct run ids");
    assert_ne!(
        channel_a.id(),
        channel_b.id(),
        "two invocations must hold two distinct channels"
    );

    // Interleave them, which is the case a global bus gets wrong.
    let (a, b) = tokio::join!(
        demo_stream_core(&run_a, 3, &channel_a),
        demo_stream_core(&run_b, 5, &channel_b),
    );
    a.expect("run a completes");
    b.expect("run b completes");

    let events_a = received_a.lock().unwrap().clone();
    let events_b = received_b.lock().unwrap().clone();

    assert!(!events_a.is_empty() && !events_b.is_empty());
    for event in &events_a {
        assert_eq!(
            event["runId"].as_str().unwrap(),
            run_a.as_str(),
            "channel A received an event belonging to another run: {event}"
        );
    }
    for event in &events_b {
        assert_eq!(
            event["runId"].as_str().unwrap(),
            run_b.as_str(),
            "channel B received an event belonging to another run: {event}"
        );
    }
    assert_ne!(
        events_a.len(),
        events_b.len(),
        "the two runs were given different step counts; equal lengths would suggest \
         the streams were merged"
    );
}

// ---------------------------------------------------------------------------
// Async and cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unmounted_screen_cancels_its_in_flight_stream() {
    // The contract clause: when a screen unmounts, its channel dies. The in-flight
    // command must STOP at the next send rather than run to completion emitting into
    // nothing, and it must report the stop as cancellation rather than as an error
    // the UI would render in a screen that no longer exists.
    let run_id = RunId::new();
    let outcome = demo_stream_core(&run_id, 100, &DeadSink)
        .await
        .expect("a dead channel is cancellation, not a bus error");

    assert!(
        outcome.cancelled,
        "a stream whose channel is gone must report cancelled"
    );
    assert!(
        outcome.emitted < 100,
        "a cancelled stream must stop early, emitted {} of 100",
        outcome.emitted
    );
}

#[test]
fn every_registered_bus_command_is_async() {
    // "Commands are async and a slow one does not block the window" is only true if
    // every command actually IS async -- a single sync command blocks the IPC thread
    // and the window with it. Asserted structurally so it cannot rot.
    let source = std::fs::read_to_string(manifest_path("src/tauri/commands.rs"))
        .expect("read src/tauri/commands.rs");

    let mut checked = 0_usize;
    let lines: Vec<&str> = source.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "#[tauri::command]" {
            continue;
        }
        // Find the signature line that follows the attribute stack.
        let signature = lines[i + 1..]
            .iter()
            .find(|l| l.contains("fn "))
            .unwrap_or_else(|| panic!("#[tauri::command] at line {} has no fn", i + 1));
        assert!(
            signature.contains("async fn "),
            "every #[tauri::command] must be `async fn` so a slow command cannot block \
             the window; this one is not:\n  {signature}"
        );
        checked += 1;
    }

    assert_eq!(
        checked,
        BUS_COMMANDS.len(),
        "found {checked} #[tauri::command] functions but BUS_COMMANDS lists {}; the list \
         and the code must not drift",
        BUS_COMMANDS.len()
    );
}

// ---------------------------------------------------------------------------
// Managed state ownership
// ---------------------------------------------------------------------------

#[tokio::test]
async fn managed_state_owns_the_pool_and_serves_a_real_round_trip() {
    // What lives in managed state: the DB pool and the repositories built over it --
    // opened ONCE at startup, shared by every command. What a command builds per
    // call: only cheap request-scoped values. This test proves the first half by
    // driving a real round trip through state that was opened once.
    let temp = TempDir::new().expect("tempdir");
    let db_path = temp.path().join("pulse.db");

    let state = DesktopState::open(&db_path)
        .await
        .expect("managed state opens (and migrates) the pool");

    // The round-trip command reads through the state's repo.
    let before = shell_info_core(&state)
        .await
        .expect("shell_info round trip");
    assert_eq!(
        before.strategy_count, 0,
        "a fresh database must report zero strategies"
    );
    assert!(
        !before.engine_fingerprint.is_empty(),
        "shell info must carry the engine fingerprint"
    );
    assert!(
        !before.app_version.is_empty(),
        "shell info must carry the app version"
    );

    // Write through the SAME state -- proving the pool is shared, not re-opened.
    state
        .strategy_repo()
        .create_strategy("Bus contract probe", Some("r1.s1.w1"), &[])
        .await
        .expect("create a strategy through managed state's repo");

    let after = shell_info_core(&state)
        .await
        .expect("shell_info round trip");
    assert_eq!(
        after.strategy_count, 1,
        "the round-trip command must observe a write made through the same managed state"
    );
}

// ---------------------------------------------------------------------------
// One append-only registration point
// ---------------------------------------------------------------------------

#[test]
fn command_registration_is_one_append_only_list() {
    // The round-3 DAG dropped the w3 -> w4 edge on this property: two items each
    // adding one screen must conflict TEXTUALLY (same file, adjacent lines) and never
    // SEMANTICALLY. That holds only while there is exactly one list, one entry per
    // line, with no duplicates.
    assert!(
        !BUS_COMMANDS.is_empty(),
        "the registration list must not be empty"
    );

    let mut sorted = BUS_COMMANDS.to_vec();
    sorted.sort_unstable();
    let mut deduped = sorted.clone();
    deduped.dedup();
    assert_eq!(
        sorted, deduped,
        "BUS_COMMANDS contains a duplicate -- a duplicate registration is a silent \
         semantic conflict, which is exactly what append-only exists to prevent"
    );

    let source = std::fs::read_to_string(manifest_path("src/tauri/commands.rs"))
        .expect("read src/tauri/commands.rs");

    // Exactly ONE list declaration.
    assert_eq!(
        source.matches("pub const BUS_COMMANDS").count(),
        1,
        "there must be exactly one BUS_COMMANDS declaration -- a second list is a \
         second registration point"
    );

    // One entry per line, so two items appending one command each produce a clean
    // textual conflict a human resolves by keeping both lines.
    for name in BUS_COMMANDS {
        let quoted = format!("\"{name}\"");
        let occurrences = source
            .lines()
            .filter(|line| line.trim_start().starts_with(&quoted))
            .count();
        assert_eq!(
            occurrences, 1,
            "command `{name}` must appear on exactly one line of its own in the \
             registration list (found {occurrences})"
        );
    }

    // And the registered names are what the handler actually wires up.
    for name in BUS_COMMANDS {
        assert!(
            source.contains(&format!("async fn {name}")),
            "BUS_COMMANDS lists `{name}` but no `async fn {name}` exists in commands.rs"
        );
    }

    // The OTHER half of clause 4, and the one that actually decides whether a command
    // is invokable: `collect_commands!` in `src/tauri/mod.rs`. A name can be in
    // BUS_COMMANDS with a matching `async fn` and STILL be unreachable from the
    // frontend if it was never collected -- the failure mode is a runtime "command not
    // found" on a screen nobody opened yet. Assert the two halves cannot drift.
    let builder_source =
        std::fs::read_to_string(manifest_path("src/tauri/mod.rs")).expect("read src/tauri/mod.rs");

    // Strip `//` comments before counting, so the contract can be DOCUMENTED in the very
    // file it governs (the repo's convention -- see tests/determinism_guard.rs).
    let builder: String = builder_source
        .lines()
        .map(|line| line.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        builder.matches("collect_commands!").count(),
        1,
        "there must be exactly one collect_commands! in code -- a second is a second \
         registration point"
    );

    let collected: Vec<String> = builder
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix("commands::")
                .map(|rest| rest.trim_end_matches(',').to_owned())
        })
        .collect();

    let mut expected: Vec<String> = BUS_COMMANDS.iter().map(|s| (*s).to_owned()).collect();
    let mut actual = collected.clone();
    expected.sort();
    actual.sort();
    assert_eq!(
        actual, expected,
        "collect_commands! and BUS_COMMANDS disagree.\n  collected: {collected:?}\n  \
         listed:    {BUS_COMMANDS:?}\nEvery command must appear in BOTH, or it is \
         registered-but-unlisted (invisible to this gate) or listed-but-unregistered \
         (invokes to 'command not found')."
    );

    // One per line here too, for the same merge-conflict reason.
    for name in BUS_COMMANDS {
        let occurrences = builder
            .lines()
            .filter(|line| line.trim() == format!("commands::{name},"))
            .count();
        assert_eq!(
            occurrences, 1,
            "`commands::{name}` must appear on exactly one line of its own inside \
             collect_commands! (found {occurrences})"
        );
    }
}

#[test]
fn the_route_table_is_one_append_only_entry_per_screen() {
    // The frontend half of the same property: routes in ONE table, one line per
    // screen. `w3` and `w4` each add one screen in round 3.
    let routes = std::fs::read_to_string(manifest_path("ui/src/routes.ts"))
        .expect("read ui/src/routes.ts -- the single route table");

    assert_eq!(
        routes.matches("export const ROUTES").count(),
        1,
        "there must be exactly one ROUTES table"
    );

    // Each route entry declares `path:` once, on its own line.
    let paths: Vec<String> = routes
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            trimmed.strip_prefix("path: ").map(|rest| {
                rest.trim_end_matches(',')
                    .trim_matches(['"', '\''].as_ref())
                    .to_owned()
            })
        })
        .collect();

    assert!(
        !paths.is_empty(),
        "the route table must declare at least the placeholder screen, one `path:` per line"
    );

    let mut sorted = paths.clone();
    sorted.sort();
    let mut deduped = sorted.clone();
    deduped.dedup();
    assert_eq!(
        sorted, deduped,
        "duplicate route path in ROUTES -- two screens claiming one path is a \
         semantic conflict, which append-only exists to prevent: {paths:?}"
    );
}
