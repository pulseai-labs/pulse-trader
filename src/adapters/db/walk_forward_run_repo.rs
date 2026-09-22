//! The `SQLite` adapter implementing the [`WalkForwardRunRepository`] port
//! (r2.s3.w3 — `rolling-oos/v1` + `wf-v1`, ADR-0025).
//!
//! Implemented on [`SqliteBacktestRunRepo`]: a walk-forward run is a parent row
//! whose K folds are **ordinary persisted windowed `backtest_run` rows** — so
//! the write path is `insert_run_row` + `insert_trade_rows` (with the `0013`
//! membership pair set), and `list_runs_for_version` / `get_run` keep seeing the
//! folds like any other run (L8). This module adds only the parent + fold-row
//! surface: `save_walk_forward_run` is **one transaction** (a9 — the ownership
//! check, the `walk_forward_run` row, then per fold the run + trades + fold
//! row; any failure rolls everything back), and `get_walk_forward_run` is
//! fail-closed like `get_run` — a corrupt column or a fold whose `backtest_run`
//! is missing is an `Err`, never a partial read.
//!
//! The `sqlx` confinement rule is unchanged: this file is still `adapters::db`.

use uuid::Uuid;

use crate::adapters::db::backtest_run_repo::{
    SqliteBacktestRunRepo, check_inputs_path_safe, decimal_text, insert_run_row, insert_trade_rows,
    parse_decimal,
};
use crate::domain::backtest::{
    BacktestRunId, CandleWindow, FoldScheme, FoldVerdict, N_MIN, RunVerdict, VerdictRule,
    WalkForwardFold, WalkForwardMembership, WalkForwardRun, WalkForwardRunDraft, WalkForwardRunId,
};
use crate::domain::strategy::VersionId;
use crate::domain::{BacktestRunRepository, Clock, DataError, WalkForwardRunRepository};

/// The `walk_forward_run` parent-row insert — one statement inside `tx`.
async fn insert_walk_forward_run_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    wf_run_id: &str,
    version_id_str: &str,
    created_at: &str,
    draft: &WalkForwardRunDraft,
) -> Result<(), DataError> {
    let scheme_name = draft.scheme.name();
    let rule_name = draft.rule.name();
    let k_i64 = i64::from(draft.scheme.k());
    let folds_holding = i64::from(draft.verdict.folds_holding);
    let folds_required = i64::from(draft.verdict.folds_required);
    let pooled_n = i64::try_from(draft.verdict.pooled.n)
        .map_err(|e| DataError::Db(format!("pooled_n overflows i64: {e}")))?;
    let pooled_mean_r = decimal_text(draft.verdict.pooled.mean_r);
    // `seq` mints inside the statement: `MAX(seq)+1` under the write lock makes
    // it the monotonic insertion sequence `0014`'s pointer rule orders by —
    // two saves in one `created_at` millisecond order by which committed the
    // row first, never by the random id's lexical luck.
    sqlx::query!(
        "INSERT INTO walk_forward_run \
         (id, seq, strategy_version_id, created_at, scheme, rule, k, \
          span_from_ms, span_to_ms, from_defaulted, engine_fingerprint, \
          folds_holding, folds_required, pooled_n, pooled_mean_r, \
          pooled_lower_bound, pass) \
         VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM walk_forward_run), \
                 ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        wf_run_id,
        version_id_str,
        created_at,
        scheme_name,
        rule_name,
        k_i64,
        draft.span.from_ms,
        draft.span.to_ms,
        draft.from_defaulted,
        draft.engine_fingerprint,
        folds_holding,
        folds_required,
        pooled_n,
        pooled_mean_r,
        draft.verdict.pooled.lower_bound,
        draft.verdict.pass,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;
    Ok(())
}

/// One fold's three writes: the ordinary `backtest_run` (with the 0013
/// membership pair), its `trade` rows, and the `walk_forward_fold` row.
///
/// `run_id` is minted by the caller and passed in (F9), for the same reason
/// `wf_run_id` is: the coach-accept path mints every id in its transaction from
/// the injected `IdSource`, and a fold run minted here from `Uuid::new_v4()`
/// would be the one id in that transaction the injected source did not choose.
async fn insert_fold_rows(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    wf_run_id: &str,
    version_id_str: &str,
    created_at: &str,
    fold: &crate::domain::backtest::WalkForwardFoldDraft,
    run_id: &str,
) -> Result<(), DataError> {
    let membership = WalkForwardMembership {
        run_id: WalkForwardRunId::new(wf_run_id),
        fold_index: fold.index,
    };
    insert_run_row(
        tx,
        run_id,
        version_id_str,
        created_at,
        &fold.inputs,
        &fold.result,
        &fold.summary,
        fold.starting_equity,
        Some(&membership),
    )
    .await?;
    insert_trade_rows(tx, run_id, &fold.result.trades).await?;
    let fold_index_i64 = i64::from(fold.index);
    let fold_n = i64::try_from(fold.verdict.n)
        .map_err(|e| DataError::Db(format!("fold n overflows i64: {e}")))?;
    let fold_mean_r = decimal_text(fold.verdict.mean_r);
    sqlx::query!(
        "INSERT INTO walk_forward_fold \
         (walk_forward_run_id, fold_index, window_from_ms, window_to_ms, \
          backtest_run_id, n, mean_r, lower_bound, holds) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        wf_run_id,
        fold_index_i64,
        fold.window.from_ms,
        fold.window.to_ms,
        run_id,
        fold_n,
        fold_mean_r,
        fold.verdict.lower_bound,
        fold.verdict.holds,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;
    Ok(())
}

/// The persisted scheme/rule names decode through the same domain validation
/// the writer used — an unknown name or an out-of-range `k` is corrupt, not
/// defaulted.
fn decode_scheme_and_rule(
    id_str: &str,
    scheme_text: &str,
    rule_text: &str,
    k_i64: i64,
) -> Result<(FoldScheme, VerdictRule), DataError> {
    let scheme = match scheme_text {
        "rolling-oos/v1" => FoldScheme::rolling_oos(k_i64)
            .map_err(|e| DataError::Db(format!("walk_forward_run `{id_str}`: {e}")))?,
        other => {
            return Err(DataError::Db(format!(
                "walk_forward_run `{id_str}` has unknown scheme `{other}`"
            )));
        }
    };
    let rule = match rule_text {
        "wf-v1" => VerdictRule::WfV1,
        other => {
            return Err(DataError::Db(format!(
                "walk_forward_run `{id_str}` has unknown rule `{other}`"
            )));
        }
    };
    Ok((scheme, rule))
}

/// The `walk_forward_run` row's `RunVerdict` decode — `pooled.holds` is derived
/// (`n >= N_MIN && lower_bound > 0`), not a column, so it is recomputed and
/// round-trips exactly.
fn decode_run_verdict(
    folds_holding: i64,
    folds_required: i64,
    pooled_n: i64,
    pooled_mean_r: &str,
    pooled_lower_bound: f64,
    pass: i64,
) -> Result<RunVerdict, DataError> {
    let n = usize::try_from(pooled_n)
        .map_err(|e| DataError::Db(format!("pooled_n {pooled_n}: {e}")))?;
    Ok(RunVerdict {
        folds_holding: u8::try_from(folds_holding)
            .map_err(|e| DataError::Db(format!("folds_holding {folds_holding}: {e}")))?,
        folds_required: u8::try_from(folds_required)
            .map_err(|e| DataError::Db(format!("folds_required {folds_required}: {e}")))?,
        pooled: FoldVerdict {
            n,
            mean_r: parse_decimal("walk_forward_run.pooled_mean_r", pooled_mean_r)?,
            lower_bound: pooled_lower_bound,
            holds: n >= N_MIN && pooled_lower_bound > 0.0,
        },
        pass: pass != 0,
    })
}

/// The raw `walk_forward_fold` row shape the read path decodes.
struct FoldRow {
    fold_index: i64,
    window_from_ms: i64,
    window_to_ms: i64,
    backtest_run_id: String,
    n: i64,
    mean_r: String,
    lower_bound: f64,
    holds: i64,
}

/// One `walk_forward_fold` row's decode — fail-closed, and fail-closed about the
/// run it points at too (F11).
///
/// The port's contract for this read is "missing or corrupt is an `Err`, never a
/// partial read", and a fold's `backtest_run` can fail it two ways: the row can
/// be ABSENT, or it can be present and not DECODE — a corrupt money column, an
/// unsupported `schema_version`, a broken tamper hash. An existence probe sees
/// only the first, so it hands back a fold whose run id the caller cannot follow
/// anywhere: a partial read wearing a valid id.
///
/// So the check is the run's own read, `get_run` — the same decodability
/// definition `get_backtest_run` and the fold surfaces use, so a fold is refused
/// in exactly the cases its run would be. It is a real decode per fold (trades
/// and the hash included), which is what the callers already do one layer up
/// when they load each fold's run.
async fn fetch_fold<C: Clock + Send + Sync>(
    repo: &SqliteBacktestRunRepo<C>,
    parent_id: &str,
    f: &FoldRow,
) -> Result<WalkForwardFold, DataError> {
    match repo
        .get_run(&BacktestRunId::new(f.backtest_run_id.clone()))
        .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            return Err(DataError::Db(format!(
                "walk_forward_run `{parent_id}` fold {} references missing backtest_run `{}`",
                f.fold_index, f.backtest_run_id
            )));
        }
        Err(e) => {
            return Err(DataError::Db(format!(
                "walk_forward_run `{parent_id}` fold {} references corrupt backtest_run `{}`: {e}",
                f.fold_index, f.backtest_run_id
            )));
        }
    }
    Ok(WalkForwardFold {
        index: u8::try_from(f.fold_index)
            .map_err(|e| DataError::Db(format!("walk_forward_fold index {}: {e}", f.fold_index)))?,
        window: CandleWindow::new(f.window_from_ms, f.window_to_ms)
            .map_err(|e| DataError::Db(format!("walk_forward_fold `{parent_id}` window: {e}")))?,
        backtest_run_id: BacktestRunId::new(f.backtest_run_id.clone()),
        verdict: FoldVerdict {
            n: usize::try_from(f.n)
                .map_err(|e| DataError::Db(format!("walk_forward_fold n {}: {e}", f.n)))?,
            mean_r: parse_decimal("walk_forward_fold.mean_r", &f.mean_r)?,
            lower_bound: f.lower_bound,
            holds: f.holds != 0,
        },
    })
}

/// The `walk_forward_run` row, raw — `None` when no such parent exists.
async fn fetch_run_row(pool: &sqlx::SqlitePool, id_str: &str) -> Result<Option<RunRow>, DataError> {
    sqlx::query_as!(
        RunRow,
        r#"SELECT
             id                  AS "id!: String",
             strategy_version_id AS "strategy_version_id!: String",
             created_at          AS "created_at!: String",
             scheme              AS "scheme!: String",
             rule                AS "rule!: String",
             k                   AS "k!: i64",
             span_from_ms        AS "span_from_ms!: i64",
             span_to_ms          AS "span_to_ms!: i64",
             from_defaulted      AS "from_defaulted!: i64",
             engine_fingerprint  AS "engine_fingerprint!: String",
             folds_holding       AS "folds_holding!: i64",
             folds_required      AS "folds_required!: i64",
             pooled_n            AS "pooled_n!: i64",
             pooled_mean_r       AS "pooled_mean_r!: String",
             pooled_lower_bound  AS "pooled_lower_bound!: f64",
             pass                AS "pass!: i64"
           FROM walk_forward_run WHERE id = ?1"#,
        id_str,
    )
    .fetch_optional(pool)
    .await
    .map_err(|e| DataError::Db(e.to_string()))
}

/// The raw `walk_forward_run` row shape the read path decodes.
struct RunRow {
    id: String,
    strategy_version_id: String,
    created_at: String,
    scheme: String,
    rule: String,
    k: i64,
    span_from_ms: i64,
    span_to_ms: i64,
    from_defaulted: i64,
    engine_fingerprint: String,
    folds_holding: i64,
    folds_required: i64,
    pooled_n: i64,
    pooled_mean_r: String,
    pooled_lower_bound: f64,
    pass: i64,
}

/// The whole write shape of one walk-forward run, on the CALLER's transaction
/// (r2.s3.w4): the `walk_forward_run` parent row, each fold's ordinary
/// `backtest_run` + `trade` + `walk_forward_fold` rows, and — last — the owning
/// version's certification pointer moved to this run (ADR-0025 L6: EVERY
/// persisted walk-forward advances `latest_walk_forward_run_id`, pass or fail —
/// a newer failing run is exactly how a version de-certifies).
///
/// Fail closed on a draft that is not internally the scheme's own shape (F5).
/// `WalkForwardRunDraft` is a public struct with unconstrained fields — a
/// caller can build `k=6` with zero folds and `pass=true` and, absent this
/// gate, the pointer update below would still read the version "certified".
/// So the SHARED insert path — the one funnel `save_walk_forward_run` and the
/// coach accept both pass through — refuses an incoherent draft before a
/// single row is written:
///
/// - the fold count IS the scheme's `k`, and `folds[i].index == i`;
/// - the fold windows tile `draft.span` contiguously and in order (what
///   `fold_windows` produces — `from <= to` per window, first `from` at
///   `span.from_ms`, each `to` the next `from`, last `to` at `span.to_ms`);
/// - every fold's engine fingerprint IS the draft's recorded fingerprint;
/// - the recorded verdict is what the recorded fold verdicts say:
///   `folds_holding` counts the folds' `holds`, `folds_required` is the
///   scheme's `⌈2K/3⌉`, and `pass` is `holding >= required && pooled.holds`.
///
/// Consistency, not re-derivation: re-running the engine over fold inputs is
/// the application's job upstream; this boundary proves the draft's own claims
/// agree with each other, so an incoherent draft cannot smuggle a
/// certification past the pointer update.
fn validate_draft(draft: &WalkForwardRunDraft) -> Result<(), DataError> {
    let incoherent = |what: String| DataError::Db(format!("walk-forward draft refused: {what}"));
    let k = draft.scheme.k();
    if draft.folds.len() != usize::from(k) {
        return Err(incoherent(format!(
            "scheme k={k} but {} fold(s) recorded",
            draft.folds.len()
        )));
    }
    let mut cursor = draft.span.from_ms;
    for (fold, want_index) in draft.folds.iter().zip(0..k) {
        if fold.index != want_index {
            return Err(incoherent(format!(
                "fold {want_index} expected but index {} recorded",
                fold.index
            )));
        }
        if fold.window.from_ms != cursor || fold.window.from_ms > fold.window.to_ms {
            return Err(incoherent(format!(
                "fold {} window {cursor}.. does not continue the counted span",
                fold.index
            )));
        }
        if fold.result.engine_fingerprint.as_str() != draft.engine_fingerprint.as_str() {
            return Err(incoherent(format!(
                "fold {} ran under a different engine fingerprint than the run records",
                fold.index
            )));
        }
        cursor = fold.window.to_ms;
    }
    if cursor != draft.span.to_ms {
        return Err(incoherent(format!(
            "the folds end at {cursor}, not the recorded span's {}",
            draft.span.to_ms
        )));
    }
    let holding = draft.folds.iter().filter(|f| f.verdict.holds).count();
    if usize::from(draft.verdict.folds_holding) != holding {
        return Err(incoherent(format!(
            "verdict records {} holding folds but the fold verdicts hold {holding}",
            draft.verdict.folds_holding
        )));
    }
    let required = crate::domain::backtest::folds_required(k);
    if draft.verdict.folds_required != required {
        return Err(incoherent(format!(
            "verdict claims folds_required {} but the scheme's is {required}",
            draft.verdict.folds_required
        )));
    }
    let pass = holding >= usize::from(required) && draft.verdict.pooled.holds;
    if draft.verdict.pass != pass {
        return Err(incoherent(format!(
            "verdict records pass={} but the fold verdicts make it {pass}",
            draft.verdict.pass
        )));
    }
    Ok(())
}

/// `wf_run_id`, every fold's `backtest_run` id, and `created_at` are minted by
/// the caller (`Uuid`/`Clock` on the standalone path; the coach accept's
/// injected `IdSource`/`Clock` inside `commit_acceptance`), so the same insert
/// lands identically under both writers — including the fold-run ids, which the
/// coach path mints from the one source that mints every other id in its
/// transaction (F9). The ownership guard stays with the callers: the standalone
/// path checks before minting; the coach path just inserted the child row
/// itself.
///
/// # Errors
///
/// Returns [`DataError::Db`] when the draft is incoherent (see
/// [`validate_draft`]) or when `fold_run_ids` does not name exactly one id per
/// draft fold — a caller-side mismatch is refused here rather than silently
/// pairing ids with the wrong folds.
pub(crate) async fn insert_walk_forward_in_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    version_id_str: &str,
    draft: &WalkForwardRunDraft,
    wf_run_id: &str,
    fold_run_ids: &[String],
    created_at: &str,
) -> Result<WalkForwardRunId, DataError> {
    validate_draft(draft)?;
    if fold_run_ids.len() != draft.folds.len() {
        return Err(DataError::Db(format!(
            "walk-forward draft refused: {} fold run id(s) minted for {} fold(s)",
            fold_run_ids.len(),
            draft.folds.len()
        )));
    }
    insert_walk_forward_run_row(tx, wf_run_id, version_id_str, created_at, draft).await?;
    for (fold, run_id) in draft.folds.iter().zip(fold_run_ids) {
        insert_fold_rows(tx, wf_run_id, version_id_str, created_at, fold, run_id).await?;
    }

    // The pointer moves INSIDE the same transaction — a walk-forward run and
    // the certification it implies are one atomic fact, never two writes that
    // could disagree.
    sqlx::query!(
        "UPDATE strategy_version SET latest_walk_forward_run_id = ?1 WHERE id = ?2",
        wf_run_id,
        version_id_str,
    )
    .execute(&mut **tx)
    .await
    .map_err(|e| DataError::Db(e.to_string()))?;

    Ok(WalkForwardRunId::new(wf_run_id))
}

impl<C: Clock + Send + Sync> WalkForwardRunRepository for SqliteBacktestRunRepo<C> {
    // One transaction: the ownership guard `save_run` makes, then the shared
    // insert (parent row, per-fold run + trades + fold row, the version's
    // certification pointer — a9 + r2.s3.w4's L6). Any failure rolls everything
    // back — nothing partial persists.
    async fn save_walk_forward_run(
        &self,
        strategy_version_id: &VersionId,
        draft: &WalkForwardRunDraft,
    ) -> Result<WalkForwardRunId, DataError> {
        let version_id_str = strategy_version_id.as_str().to_owned();
        let wf_run_id = Uuid::new_v4().to_string();
        // The fold-run ids, minted here beside the parent's so the shared insert
        // pairs each with its fold (F9). This path has no injected `IdSource` —
        // `Uuid` is what it mints the walk-forward id with — and it stays that
        // way: the seam's behaviour is unchanged, only the coach path's ids now
        // come from its source.
        let fold_run_ids: Vec<String> = draft
            .folds
            .iter()
            .map(|_| Uuid::new_v4().to_string())
            .collect();
        let created_at = self.now_rfc3339()?;

        // The version tags are checked BEFORE the transaction opens (the
        // `save_run` precedent — an unsafe one persists nothing at all).
        for fold in &draft.folds {
            check_inputs_path_safe(&fold.inputs)?;
        }

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;

        // #39 ownership-on-write, same shape as `save_run`'s.
        let owns = sqlx::query!(
            r#"SELECT 1 AS "one!: i64" FROM strategy_version WHERE id = ?1"#,
            version_id_str,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;
        if owns.is_none() {
            return Err(DataError::Db(format!(
                "cannot save walk-forward run: strategy_version `{version_id_str}` does not exist (#39 ownership-on-write)"
            )));
        }

        let wf_id = insert_walk_forward_in_tx(
            &mut tx,
            &version_id_str,
            draft,
            &wf_run_id,
            &fold_run_ids,
            &created_at,
        )
        .await?;

        tx.commit()
            .await
            .map_err(|e| DataError::Db(e.to_string()))?;
        Ok(wf_id)
    }

    // Fail-closed read (the `get_run` discipline): any corrupt column or a fold
    // whose backtest_run row is gone is an `Err`, never a partial read.
    async fn get_walk_forward_run(
        &self,
        id: &WalkForwardRunId,
    ) -> Result<Option<WalkForwardRun>, DataError> {
        let id_str = id.as_str();
        let Some(r) = fetch_run_row(&self.pool, id_str).await? else {
            return Ok(None);
        };

        let (scheme, rule) = decode_scheme_and_rule(id_str, &r.scheme, &r.rule, r.k)?;
        let fold_rows = sqlx::query_as!(
            FoldRow,
            r#"SELECT
                 fold_index       AS "fold_index!: i64",
                 window_from_ms   AS "window_from_ms!: i64",
                 window_to_ms     AS "window_to_ms!: i64",
                 backtest_run_id  AS "backtest_run_id!: String",
                 n                AS "n!: i64",
                 mean_r           AS "mean_r!: String",
                 lower_bound      AS "lower_bound!: f64",
                 holds            AS "holds!: i64"
               FROM walk_forward_fold WHERE walk_forward_run_id = ?1
               ORDER BY fold_index"#,
            id_str,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(e.to_string()))?;

        let mut folds = Vec::with_capacity(fold_rows.len());
        for f in &fold_rows {
            folds.push(fetch_fold(self, id_str, f).await?);
        }

        Ok(Some(WalkForwardRun {
            id: WalkForwardRunId::new(r.id),
            strategy_version_id: VersionId::new(r.strategy_version_id),
            created_at: r.created_at,
            scheme,
            rule,
            span: CandleWindow::new(r.span_from_ms, r.span_to_ms)
                .map_err(|e| DataError::Db(format!("walk_forward_run `{id_str}` span: {e}")))?,
            from_defaulted: r.from_defaulted != 0,
            engine_fingerprint: r.engine_fingerprint,
            verdict: decode_run_verdict(
                r.folds_holding,
                r.folds_required,
                r.pooled_n,
                &r.pooled_mean_r,
                r.pooled_lower_bound,
                r.pass,
            )?,
            folds,
        }))
    }
}
