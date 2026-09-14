//! Shared fixtures for the three coach test binaries (`coach_turn` = demo `d6`,
//! `coach_failures` = demo `d7`, `coach_redaction`).
//!
//! A `tests/<dir>/mod.rs` module rather than a fourth test binary: cargo compiles
//! only `tests/*.rs` as test targets, so this file is linked into each binary that
//! declares `mod coach_support;` and never runs as a suite of its own.
//!
//! Everything here is offline by construction — a scripted provider, a temp
//! `SQLite`, a fixture price table. **No live LLM call exists in any of it**: two
//! of the three binaries are demo-ledger lines re-run at every future spine close.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use pulse::{
    DataError, LlmBackend, LlmCall, LlmCallCapture, LlmCallId, LlmCallRepository, LlmConfig,
    ModelPrice, PriceTable,
};
use rust_decimal::Decimal;

/// An `LlmCallRepository` decorator that records every minted id into a shared
/// buffer — the test-side equivalent of the CLI's `CapturingRepo`, which is
/// `pub(crate)` and therefore not reachable from an integration test.
///
/// The coach reads this buffer to learn which ledger row its turn produced, the
/// same way the composer does.
pub struct CapturingLlmRepo<R> {
    inner: R,
    ids: LlmCallCapture,
}

impl<R> CapturingLlmRepo<R> {
    pub fn new(inner: R, ids: LlmCallCapture) -> Self {
        Self { inner, ids }
    }
}

impl<R: LlmCallRepository + Send + Sync> LlmCallRepository for CapturingLlmRepo<R> {
    async fn save_call(&self, call: &LlmCall) -> Result<LlmCallId, DataError> {
        // Persist through the real repo FIRST (its clock owns `created_at`), and
        // only share the id once the write actually succeeded.
        let id = self.inner.save_call(call).await?;
        if let Ok(mut ids) = self.ids.lock() {
            ids.push(id.clone());
        }
        Ok(id)
    }

    async fn get_call(&self, id: &LlmCallId) -> Result<Option<LlmCall>, DataError> {
        self.inner.get_call(id).await
    }
}

/// The fixture chat config — a priced model, deterministic settings.
pub fn config() -> LlmConfig {
    LlmConfig {
        backend: LlmBackend::Ollama,
        model: "glm-5.3-flash".to_owned(),
        temperature: 0.0,
        max_tokens: 2_048,
        reasoning_effort: None,
    }
}

/// A fixture price table covering [`config`]'s model. Nominal values — the point
/// is that a cost is computed and recorded, not what it is.
pub fn test_prices() -> PriceTable {
    let mut models = HashMap::new();
    models.insert(
        "glm-5.3-flash".to_owned(),
        ModelPrice {
            input_per_mtok: Decimal::new(1, 0),
            output_per_mtok: Decimal::new(2, 0),
        },
    );
    PriceTable::from_config("CNY", models)
}

/// The canonical RSI-oversold strategy as DSL JSON — the document every coach
/// fixture mutates. Written as JSON (not a built `StrategyDsl`) because
/// `create_version` takes the raw document and routes it through the `Migrator`.
pub fn canonical_dsl_json() -> String {
    serde_json::json!({
        "schema_version": "1.0.0",
        "name": "RSI Oversold",
        "direction": "long",
        // `ValueSource` is tagged `type`; the nested `IndicatorSpec` is tagged
        // `indicator` under the `spec` field — both struct variants, per the
        // DSL-wide serde invariant.
        "entry": {
            "type": "Compare",
            "lhs": { "type": "Indicator", "spec": { "indicator": "Rsi", "period": 14 } },
            "op": "Lt",
            "rhs": { "type": "Constant", "value": "30" }
        },
        "filters": [],
        "exits": [
            { "type": "StopLoss", "distance_pct": "0.05" },
            { "type": "TakeProfit", "target_r": "2" }
        ],
        "risk": { "risk_per_trade_pct": "0.01", "max_leverage": "3" }
    })
    .to_string()
}

/// A shared, empty capture buffer.
pub fn capture() -> LlmCallCapture {
    Arc::new(Mutex::new(Vec::new()))
}

// ---------------------------------------------------------------------------
// Run-immutability surgery — ONE implementation, shared
// ---------------------------------------------------------------------------

/// Run `statements` against `backtest_run` with its immutability trigger lifted,
/// then put the trigger back.
///
/// `0003` makes `backtest_run` immutable and `0006` refuses an incomplete insert, so
/// a row the CURRENT schema would never accept — a pre-`0006` legacy run, a row the
/// repository will refuse to read back — is otherwise unconstructible. Every
/// assertion after the surgery runs against the restored trigger.
///
/// Three things this gets right, each of which one of its two former copies got
/// wrong (`pulseai-labs/pulse-trader#153`):
///
/// 1. **ONE connection for the whole sequence.** Handing DROP → write → CREATE back
///    to the pool between statements lets another connection start its read snapshot
///    before the DROP is visible, which showed up as an intermittent "trigger
///    already exists" on the restore.
/// 2. **The trigger is restored from its OWN stored definition**, read out of
///    `sqlite_master` first — never from a DDL string copied into the test. A copy
///    silently restores a DIFFERENT trigger under the same name the day `0003`
///    changes, and every later assertion runs against the wrong rule while the name
///    match hides it.
/// 3. **The restore happens even when the surgery fails.** Panicking on the write
///    would return the connection to the pool with `backtest_run` mutable, so the
///    failure would take the immutability rule down with it for the rest of the
///    test. The write's error is held and re-raised after the trigger is back.
pub async fn with_run_immutability_lifted(pool: &sqlx::SqlitePool, statements: &[&str]) {
    with_trigger_lifted(pool, "backtest_run_no_update", statements).await;
}

/// The same surgery against any named trigger.
///
/// `backtest_run`'s immutability is not the only rule a fixture has to step around:
/// `0008` pins a coaching session's identity the same way, and a test about an
/// ABANDONED claim has to back-date one. Everything the doc above says applies
/// unchanged — one connection, restore from the stored definition, restore even
/// when the surgery fails.
pub async fn with_trigger_lifted(pool: &sqlx::SqlitePool, trigger: &str, statements: &[&str]) {
    let mut conn = pool.acquire().await.expect("a dedicated connection");

    let definition: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1")
            .bind(trigger)
            .fetch_one(&mut *conn)
            .await
            .expect("the immutability trigger exists");

    sqlx::query(&format!("DROP TRIGGER {trigger}"))
        .execute(&mut *conn)
        .await
        .expect("lift the trigger");

    let mut outcome = Ok(());
    for statement in statements {
        if let Err(e) = sqlx::query(statement).execute(&mut *conn).await {
            outcome = Err((*statement, e));
            break;
        }
    }

    sqlx::query(&definition)
        .execute(&mut *conn)
        .await
        .expect("restore the trigger");

    if let Err((statement, e)) = outcome {
        panic!("surgery `{statement}` failed: {e}");
    }

    let back: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name=?1")
            .bind(trigger)
            .fetch_one(&mut *conn)
            .await
            .expect("count the trigger");
    assert_eq!(back, 1, "the `{trigger}` guard is back in place");
}
