//! r6.s5.w1 — the eight Cipher runs of criterion 5 are registered before any of them runs.
//!
//! `docs/certification/r6-registration.md` names each run's strategy file by its SHA-256, its pair, the
//! walk-forward rule and its constants, the power bar and the certify cap (PDR-0044 decision 3; PDR-0047
//! decision 2; STR-0003). This suite fails when the document and the code or the files disagree, so nobody
//! can edit a file or a constant without a failing test:
//!
//! 1. the Runs table is exactly R1..R8, v1 then v2, each over the four pairs in order
//!    (`eight_runs_are_registered`);
//! 2. each row's SHA-256 is the hash of its file's bytes (`each_row_pins_its_file_bytes`);
//! 3. each row's pair is in the broker pin table (`each_pair_is_pinned`);
//! 4. the Constants table is the code (`the_constants_are_the_code`);
//! 5. each registered file loads, validates and compiles (`each_registered_file_loads_and_compiles`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    ASSUMED_EDGE_R, BinanceAdapter, CompiledStrategy, ExchangeAdapter, K_DEFAULT,
    MIN_HOLDOUT_TRADES, Migrator, N_MIN, POWER_BAR, Pair, StrategyDsl, VerdictRule, Z, compile,
    folds_required, validate,
};
use sha2::{Digest, Sha256};

/// The registration document, read at compile time.
const REGISTRATION: &str = include_str!("../docs/certification/r6-registration.md");

const V1_FILE: &str = "examples/strategies/cipher-v1.json";
const V2_FILE: &str = "examples/strategies/cipher-v2.json";
const PAIRS: [&str; 4] = ["BTCUSDT", "ETHUSDT", "SOLUSDT", "XRPUSDT"];

/// One row of the Runs table.
struct RunRow {
    run: String,
    file: String,
    sha256: String,
    pair: String,
}

/// A markdown table line: starts with `| `, is not the `|---` rule; cells trimmed, the empty first and last
/// cells dropped.
fn table_cells(line: &str) -> Option<Vec<String>> {
    if !line.starts_with("| ") || line.starts_with("|---") {
        return None;
    }
    let mut cells: Vec<String> = line.split('|').map(|cell| cell.trim().to_owned()).collect();
    cells.remove(0);
    cells.pop();
    Some(cells)
}

/// The table lines under the `## <heading>` section.
fn section_rows(heading: &str) -> Vec<Vec<String>> {
    let marker = format!("## {heading}");
    let mut in_section = false;
    let mut rows = Vec::new();
    for line in REGISTRATION.lines() {
        if line.starts_with("## ") {
            in_section = line.trim_end() == marker;
            continue;
        }
        if in_section && let Some(cells) = table_cells(line) {
            rows.push(cells);
        }
    }
    rows
}

/// `^R[0-9]+$` without a regex dependency.
fn is_run_label(cell: &str) -> bool {
    cell.strip_prefix('R')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
}

/// The Runs rows: table lines under `## Runs` whose first cell is `R<n>`.
fn run_rows() -> Vec<RunRow> {
    section_rows("Runs")
        .into_iter()
        .filter(|cells| is_run_label(&cells[0]))
        .map(|cells| {
            assert_eq!(cells.len(), 4, "a Runs row has four cells: {cells:?}");
            RunRow {
                run: cells[0].clone(),
                file: cells[1].clone(),
                sha256: cells[2].clone(),
                pair: cells[3].clone(),
            }
        })
        .collect()
}

/// The Constants rows: table lines under `## Constants` whose first cell is not `Name`, as (name, value).
fn constant_rows() -> Vec<(String, String)> {
    section_rows("Constants")
        .into_iter()
        .filter(|cells| cells[0] != "Name")
        .map(|cells| {
            assert_eq!(cells.len(), 2, "a Constants row has two cells: {cells:?}");
            (cells[0].clone(), cells[1].clone())
        })
        .collect()
}

fn file_path(relative: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Load a document through the read path the engine consumes: `Migrator::v1().load` (version-safe).
fn dsl_of(relative: &str) -> StrategyDsl {
    let text = std::fs::read_to_string(file_path(relative)).expect("read the registered document");
    Migrator::v1()
        .load(&text)
        .expect("the document loads through Migrator::v1")
        .dsl
}

/// `Migrator::v1().load` -> `validate` -> `compile`.
fn compiled_of(relative: &str) -> CompiledStrategy {
    compile(&validate(&dsl_of(relative)).expect("the document validates"))
        .expect("the document compiles")
}

/// The eight runs are R1..R8; R1-R4 are v1 and R5-R8 are v2; each group runs the four pairs in order.
#[test]
fn eight_runs_are_registered() {
    let rows = run_rows();
    let labels: Vec<&str> = rows.iter().map(|row| row.run.as_str()).collect();
    assert_eq!(
        labels,
        ["R1", "R2", "R3", "R4", "R5", "R6", "R7", "R8"],
        "the Runs table holds exactly R1..R8, in order"
    );
    for (index, row) in rows.iter().enumerate() {
        let expected_file = if index < 4 { V1_FILE } else { V2_FILE };
        assert_eq!(row.file, expected_file, "{}: the strategy file", row.run);
        assert_eq!(row.pair, PAIRS[index % 4], "{}: the pair", row.run);
    }
}

/// Each row's SHA-256 cell is the lowercase hex digest of its file's bytes.
#[test]
fn each_row_pins_its_file_bytes() {
    for row in run_rows() {
        assert!(
            row.sha256.len() == 64
                && row
                    .sha256
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "{}: the SHA-256 cell is 64 lowercase hex characters, got {:?}",
            row.run,
            row.sha256
        );
        let bytes = std::fs::read(file_path(&row.file)).expect("read the registered file");
        let digest = hex::encode(Sha256::digest(&bytes));
        assert_eq!(
            digest, row.sha256,
            "{}: {} changed since it was registered",
            row.run, row.file
        );
    }
}

/// Each row's pair is in the broker pin table.
#[test]
fn each_pair_is_pinned() {
    let adapter = BinanceAdapter::new();
    for row in run_rows() {
        assert!(
            adapter.symbol_filters(&Pair::new(&row.pair)).is_ok(),
            "{}: {} is not in the broker pin table",
            row.run,
            row.pair
        );
    }
}

/// The Constants table holds the thirteen names in order, and each value is the code's.
#[test]
fn the_constants_are_the_code() {
    let rows = constant_rows();
    let names: Vec<&str> = rows.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "freeze_id",
            "holdout_start",
            "h",
            "rule",
            "k",
            "folds_required",
            "n_min",
            "wf_v2_z",
            "power_bar",
            "assumed_edge_r",
            "min_holdout_trades",
            "primary_timeframe",
            "htf_timeframe",
        ],
        "the Constants table holds exactly the thirteen names, in order"
    );
    let value = |name: &str| -> &str {
        rows.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .expect("the name is present")
    };
    let float = |name: &str| -> f64 {
        value(name)
            .parse::<f64>()
            .unwrap_or_else(|e| panic!("{name} is not an f64: {e}"))
    };

    assert_eq!(value("rule"), VerdictRule::WfV2.name(), "rule");
    assert_eq!(value("k"), K_DEFAULT.to_string(), "k");
    assert_eq!(
        value("folds_required"),
        folds_required(K_DEFAULT).to_string(),
        "folds_required"
    );
    assert_eq!(value("n_min"), N_MIN.to_string(), "n_min");
    // The wf-v2 pooled lower-bound z, not the C1 gate's `c1_z`.
    assert!((float("wf_v2_z") - Z).abs() == 0.0, "wf_v2_z");
    assert!((float("power_bar") - POWER_BAR).abs() == 0.0, "power_bar");
    assert!(
        (float("assumed_edge_r") - ASSUMED_EDGE_R).abs() == 0.0,
        "assumed_edge_r"
    );
    assert_eq!(
        value("min_holdout_trades"),
        MIN_HOLDOUT_TRADES.to_string(),
        "min_holdout_trades"
    );
    // Prod facts and STR-0003's timeframes, pinned so an edit to the document fails here.
    assert_eq!(
        value("freeze_id"),
        "30f98b21-cb1d-4103-9393-a71be01d9dbf",
        "freeze_id"
    );
    assert_eq!(
        value("holdout_start"),
        "2026-10-10T00:00:00Z",
        "holdout_start"
    );
    assert_eq!(value("h"), "2", "h");
    assert_eq!(value("primary_timeframe"), "15m", "primary_timeframe");
    assert_eq!(value("htf_timeframe"), "4h", "htf_timeframe");
}

/// Each registered file loads, validates and compiles as the two example suites load it.
#[test]
fn each_registered_file_loads_and_compiles() {
    for relative in [V1_FILE, V2_FILE] {
        let compiled = compiled_of(relative);
        let setup = compiled
            .setup()
            .unwrap_or_else(|| panic!("{relative} declares a setup"));
        assert!(
            setup.long.is_some(),
            "{relative}: the long side is declared"
        );
        assert!(
            setup.short.is_some(),
            "{relative}: the short side is declared"
        );
    }
}
