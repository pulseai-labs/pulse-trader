//! VS-1.2.3 work-3.04 — the slice's empirical determinism proof (NFR-2, NFR-8,
//! BACKLOG-12).
//!
//! Runs ONE fixed strategy over ONE fixed candle fixture — the canonical golden
//! e2e fixture (`tests/fixtures/strategies/rsi-oversold-long.json` over the M15
//! `tests/fixtures/btcusdt-1m-store/`) — **100x sequentially** AND **100x via
//! Rayon `into_par_iter`**, and asserts ALL 200 `result_content_hash()` values
//! are byte-identical. That is in-process determinism: `run_backtest` is a pure
//! function of its inputs (no shared mutable state — `static mut`, `OnceCell`,
//! `thread_rng`, `Mutex`, interior-mutability indicator caches — so the parallel
//! arm cannot flake; audit C4 reentrancy precondition, verified clean at
//! baseline). The fixture is reused (no new golden); the load path is
//! reconstructed against the PUBLIC library API exactly as `tests/backtest_fixture.rs`
//! does, because that file's builders are test-private.
//!
//! ## Local vs. CI (the deliberate debug/`--release` split, gate-2 grill)
//!
//! The local `cargo test --test determinism` AC runs in **debug** (fast,
//! in-process 100x-identity on the dev arch — the only arch a single runner can
//! exercise). The **cross-arch** proof is the CI `determinism` matrix
//! (`ubuntu-latest` `x86_64` + `ubuntu-24.04-arm` `arm64`), which runs this test
//! **`--release`** (the profile the `.app` ships, so a release-only
//! auto-vectorization divergence cannot ship unproven). "Both arches produce the
//! same content hash" is realized by the CI matrix + the `determinism-crossarch`
//! compare job, NOT by a local AC.
//!
//! ## Hash transport is a FILE, not stdout (gate-2 grill)
//!
//! [`emit_content_hash`] is `#[ignore]`d and **writes** the single canonical
//! `result_content_hash()` to `target/determinism-hash.txt` via `std::fs::write`.
//! CI uploads that file as the per-arch artifact and `cmp`s the two arches'
//! files. Test-harness stdout formatting (`--nocapture`) is NEVER load-bearing
//! for a determinism verdict.
//!
//! ## D3 reversibility (full hash now; documented one-line fallback)
//!
//! These tests + the CI emitter assert on `result_content_hash()` — the **FULL**
//! result hash, which folds in the f64-derived `regime_breakdown`. This is the
//! empirical gate that validates that choice (D3): 3.02 turned FMA/fast-math off
//! and the indicator/regime path has no transcendentals, so the f64 path is
//! correctly-rounded and bit-identical across IEEE-754-compliant arches — the
//! expected cross-arch outcome is GREEN. **If the `determinism-crossarch` CI job
//! ever goes RED on the regime (f64) component, the one-line fallback is to swap
//! `result_content_hash()` for 3.03's regime-free `money_math_hash()`** here and
//! in the CI emitter command, and file a narrowed #29. The choice stays
//! reversible up to that job's first cross-arch result; until then, author for
//! the full hash.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use pulse::{
    BacktestConfig, BacktestResult, BinanceAdapter, CandleSeries, CandleStore, CompiledStrategy,
    ExchangeAdapter, Migrator, Pair, SymbolFilters, Timeframe, compile, run_backtest, validate,
};
use rayon::prelude::*;

/// How many times the backtest is run on each arm (sequential and parallel).
/// 100x each ⇒ 200 hashes total, all required byte-identical.
const RUNS: usize = 100;

/// Load the primary M15 candle series from the committed offline fixture store,
/// mirroring `tests/backtest_fixture.rs::load_primary` against the public API:
/// `with_base_dir` -> `read_head` -> `read_snapshot`. No network, no LLM, no DB,
/// no glob ordering, no wall-clock — the committed fixture is byte-identical on
/// every runner (arch-invariance is a hard requirement, not incidental).
fn load_primary() -> CandleSeries {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/btcusdt-1m-store");
    let store = CandleStore::with_base_dir(base);
    let pair = Pair::new("BTCUSDT");
    let head = store
        .read_head(&pair, Timeframe::M15)
        .expect("read M15 HEAD")
        .expect("M15 HEAD present in fixture store");
    store
        .read_snapshot(&pair, Timeframe::M15, &head)
        .expect("read M15 snapshot")
}

/// Compile the canonical known-strategy DSL through the full read-path the engine
/// consumes (`Migrator::v1().load` -> `validate` -> `compile`), mirroring
/// `tests/backtest_fixture.rs::run_golden`. Returns the compiled strategy so it
/// can be shared (by `&`) across every sequential and parallel run.
fn compile_golden() -> CompiledStrategy {
    let json = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/strategies/rsi-oversold-long.json"),
    )
    .expect("read strategy fixture json");
    let loaded = Migrator::v1().load(&json).expect("load (migrate) strategy");
    let validated = validate(&loaded.dsl).expect("strategy validates");
    compile(&validated).expect("strategy compiles")
}

/// Resolve the REAL BTCUSDT USD-M filters through the `BinanceAdapter` port (the
/// same `lot_step = 0.001` constraint the golden quantizes to). Shared by `&`
/// across all runs.
fn golden_filters() -> SymbolFilters {
    BinanceAdapter::new()
        .symbol_filters(&Pair::new("BTCUSDT"))
        .expect("BTCUSDT filters resolve through the port")
}

/// One backtest over the shared, REUSED golden fixture. All inputs are by shared
/// reference, so the same compiled strategy / candle series / filters are reused
/// across every run (sequential and parallel) — proving `run_backtest` is a pure
/// function of its inputs with no shared mutable state.
fn run_once(
    compiled: &CompiledStrategy,
    primary: &CandleSeries,
    filters: &SymbolFilters,
) -> BacktestResult {
    run_backtest(compiled, primary, None, &BacktestConfig::default(), filters)
        .expect("backtest runs over the fixture")
}

/// NFR-2 / BACKLOG-12 in-process determinism: the SAME backtest run 100x
/// sequentially AND 100x via Rayon `into_par_iter` over the REUSED golden fixture
/// produces 200 byte-identical `result_content_hash()` values. The parallel arm
/// only proves something because `run_backtest` is reentrant (audit C4: no shared
/// mutable state) — a hidden cache would make this arm flake, not fail cleanly.
#[test]
fn backtest_is_byte_deterministic_across_100x_sequential_and_parallel() {
    let primary = load_primary();
    let compiled = compile_golden();
    let filters = golden_filters();

    // 100x SEQUENTIAL.
    let sequential: Vec<String> = (0..RUNS)
        .map(|_| run_once(&compiled, &primary, &filters).result_content_hash())
        .collect();

    // 100x PARALLEL via Rayon — exercises `run_backtest` reentrancy over the same
    // shared (`&`) inputs. If any shared mutable state existed, this arm would
    // produce a divergent hash (or flake) rather than match.
    let parallel: Vec<String> = (0..RUNS)
        .into_par_iter()
        .map(|_| run_once(&compiled, &primary, &filters).result_content_hash())
        .collect();

    assert_eq!(
        sequential.len(),
        RUNS,
        "sequential arm produced {RUNS} hashes"
    );
    assert_eq!(parallel.len(), RUNS, "parallel arm produced {RUNS} hashes");

    // The canonical hash is the first sequential run's; all 200 must equal it.
    let canonical = &sequential[0];
    for (i, h) in sequential.iter().enumerate() {
        assert_eq!(
            h, canonical,
            "sequential run {i} content hash diverged from the canonical hash \
             (in-process non-determinism — NFR-2 violation)"
        );
    }
    for (i, h) in parallel.iter().enumerate() {
        assert_eq!(
            h, canonical,
            "parallel run {i} content hash diverged from the canonical hash \
             (run_backtest reentrancy broken — a shared-mutable-state cache was \
             introduced; audit C4 precondition violated)"
        );
    }
}

/// Tamper-proof hash transport (gate-2 grill): runs the backtest ONCE and WRITES
/// the single canonical `result_content_hash()` to `target/determinism-hash.txt`
/// via `std::fs::write`. `#[ignore]`d so it does not run in the default suite; CI
/// invokes it `--release -- --ignored`, uploads the file as the per-arch
/// artifact, and the dependent `determinism-crossarch` job `cmp`s the two arches'
/// files. NO `--nocapture` stdout scraping — the file is the only transport.
///
/// D3 fallback: if the cross-arch `cmp` ever fails on the f64 regime component,
/// swap `result_content_hash()` below (and in the CI emitter command) for
/// `money_math_hash()` and file a narrowed #29 — see the module header.
#[test]
#[ignore = "CI emitter: writes the canonical content hash to target/determinism-hash.txt for the cross-arch artifact compare"]
fn emit_content_hash() {
    let primary = load_primary();
    let compiled = compile_golden();
    let filters = golden_filters();

    let hash = run_once(&compiled, &primary, &filters).result_content_hash();

    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/determinism-hash.txt");
    // `target/` always exists during a cargo test invocation (cargo created it).
    std::fs::write(&out, &hash).expect("write target/determinism-hash.txt");
}
