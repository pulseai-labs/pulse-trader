//! The `SQLite` adapter implementing the [`PaperSessionRepository`] port
//! (r3.s4.w2 — the paper-session aggregate, ADR-0027).
//!
//! `0018` owns the invariants this adapter rides: `paper_session` is written
//! once (the row's `BEFORE UPDATE`/`DELETE` triggers abort — a promotion is
//! immutable), `paper_event`/`paper_bar` append only, and the graduation
//! CHECK refuses an override without a reason or a certified promotion
//! without its run. `seq` mints `MAX(seq)+1` **inside the write transaction**
//! on every append-only table, the walk-forward runs' mint — two writes in
//! one `created_at` millisecond order by which committed first, never by the
//! random id's lexical luck.
//!
//! The sqlx confinement rule is unchanged: this file is `adapters::db`.

use sqlx::SqlitePool;
use uuid::Uuid;

use crate::adapters::clock::SystemClock;
use crate::adapters::db::backtest_run_repo::{decimal_text, parse_decimal, parse_timeframe};
use crate::domain::PaperEvent;
use crate::domain::PaperSessionRepository;
use crate::domain::backtest::WalkForwardRunId;
use crate::domain::paper::session::{
    CertifiedDataVersion, Graduation, NonEmptyText, PaperSession, PaperSessionDraft, PaperSessionId,
};
use crate::domain::strategy::VersionId;
use crate::domain::{Clock, DataError, DataVersion, EngineFingerprint, Pair, Timeframe};
use chrono::{DateTime, SecondsFormat, Utc};

/// The `SQLite` paper-session repository.
#[derive(Clone)]
pub struct SqlitePaperSessionRepo<C: Clock> {
    pool: SqlitePool,
    clock: C,
}

impl<C: Clock> SqlitePaperSessionRepo<C> {
    /// A repository over `pool` driven by the injected `clock`.
    pub fn with_clock(pool: SqlitePool, clock: C) -> Self {
        Self { pool, clock }
    }

    /// The injected instant, as the `(DateTime, RFC3339 text)` pair the other
    /// repositories stamp provenance with (NFR-2: reproducible timestamps).
    fn now_rfc3339(&self) -> Result<(DateTime<Utc>, String), DataError> {
        let now_ms = self.clock.now_ms();
        let dt = DateTime::from_timestamp_millis(now_ms).ok_or_else(|| {
            DataError::Db(format!("clock.now_ms() {now_ms} is out of DateTime range"))
        })?;
        Ok((dt, dt.to_rfc3339_opts(SecondsFormat::Millis, true)))
    }
}

impl SqlitePaperSessionRepo<SystemClock> {
    /// A repository over `pool` on the wall clock (production shape).
    #[must_use]
    pub fn new(pool: SqlitePool) -> Self {
        Self::with_clock(pool, SystemClock)
    }
}

impl<C: Clock> SqlitePaperSessionRepo<C> {
    /// The `certified_data_versions` JSON text for a graduation.
    fn data_versions_json(graduation: &Graduation) -> String {
        let versions = match graduation {
            Graduation::Certified { data_versions, .. } => data_versions.as_slice(),
            Graduation::Override { .. } => &[],
        };
        serde_json::to_string(versions).unwrap_or_else(|_| "[]".to_owned())
    }
}

/// Reconstruct the graduation sum from the row's columns — the inverse of the
/// `0018` CHECK encoding. Free-standing (it reads no `C` state) so the shared
/// row decoder can call it.
fn graduation_from_row(
    graduation: &str,
    walk_forward_run_id: Option<String>,
    override_reason: Option<String>,
    override_at: Option<String>,
    certified_data_versions: &str,
) -> Result<Graduation, DataError> {
    match graduation {
        "certified" => {
            let run_id = walk_forward_run_id.ok_or_else(|| {
                DataError::Db("paper_session: certified row without walk_forward_run_id".to_owned())
            })?;
            let versions: Vec<CertifiedDataVersion> = serde_json::from_str(certified_data_versions)
                .map_err(|e| {
                    DataError::Db(format!(
                        "paper_session: bad certified_data_versions JSON: {e}"
                    ))
                })?;
            Ok(Graduation::Certified {
                walk_forward_run_id: WalkForwardRunId::new(run_id),
                data_versions: versions,
            })
        }
        "override" => {
            let reason = override_reason.ok_or_else(|| {
                DataError::Db("paper_session: override row without override_reason".to_owned())
            })?;
            let at = override_at.ok_or_else(|| {
                DataError::Db("paper_session: override row without override_at".to_owned())
            })?;
            Ok(Graduation::Override {
                reason: NonEmptyText::try_new(&reason).map_err(|_| {
                    DataError::Db("paper_session: override reason is empty".to_owned())
                })?,
                at,
            })
        }
        other => Err(DataError::Db(format!(
            "paper_session: unknown graduation {other:?}"
        ))),
    }
}

/// A raw `paper_bar` row (`query_as!` needs a named struct).
struct RawBar {
    open_time: i64,
    close_time: i64,
    open: String,
    high: String,
    low: String,
    close: String,
    volume: String,
    funding_rate: Option<String>,
}

/// A raw `paper_session` row — shared by `get_session` and `list_sessions`
/// so the catalog reuses the single-row decode instead of a second mapping.
#[allow(dead_code)] // every field is read by `session_from_row`; sqlx names them positionally
struct RawSession {
    id: String,
    seq: i64,
    strategy_version_id: String,
    created_at: String,
    pair: String,
    primary_timeframe: String,
    htf_timeframe: Option<String>,
    uses_d1: i64,
    starting_equity: String,
    taker_fee_bps: String,
    slippage_bps: String,
    engine_fingerprint: String,
    graduation: String,
    walk_forward_run_id: Option<String>,
    override_reason: Option<String>,
    override_at: Option<String>,
    certified_data_versions: String,
    fixture: i64,
    min_trades: i64,
    promoted_by: String,
}

/// Decode one raw `paper_session` row into the aggregate — the one mapping
/// both catalog reads share.
fn session_from_row(
    r: RawSession,
) -> Result<crate::domain::paper::session::PaperSession, DataError> {
    Ok(crate::domain::paper::session::PaperSession {
        id: PaperSessionId::new(r.id),
        seq: r.seq,
        strategy_version_id: VersionId::new(r.strategy_version_id),
        created_at: r.created_at,
        pair: Pair::new(&r.pair),
        primary_timeframe: parse_timeframe("primary_timeframe", &r.primary_timeframe)?,
        htf_timeframe: r
            .htf_timeframe
            .map(|tf| parse_timeframe("htf_timeframe", &tf))
            .transpose()?,
        uses_d1: r.uses_d1 != 0,
        starting_equity: parse_decimal("starting_equity", &r.starting_equity)?,
        taker_fee_bps: parse_decimal("taker_fee_bps", &r.taker_fee_bps)?,
        slippage_bps: parse_decimal("slippage_bps", &r.slippage_bps)?,
        engine_fingerprint: EngineFingerprint::from_stored(r.engine_fingerprint),
        graduation: graduation_from_row(
            &r.graduation,
            r.walk_forward_run_id,
            r.override_reason,
            r.override_at,
            &r.certified_data_versions,
        )?,
        fixture: r.fixture != 0,
        min_trades: u32::try_from(r.min_trades)
            .map_err(|e| DataError::Db(format!("paper_session: min_trades out of range: {e}")))?,
        promoted_by: NonEmptyText::try_new(&r.promoted_by)
            .map_err(|_| DataError::Db("paper_session: promoted_by is empty".to_owned()))?,
    })
}

impl<C: Clock + Send + Sync> PaperSessionRepository for SqlitePaperSessionRepo<C> {
    async fn insert_session(&self, draft: &PaperSessionDraft) -> Result<PaperSession, DataError> {
        let (_dt, created_at) = self.now_rfc3339()?;
        let id = PaperSessionId::new(Uuid::new_v4().to_string());
        let graduation_word = match &draft.graduation {
            Graduation::Certified { .. } => "certified",
            Graduation::Override { .. } => "override",
        };
        let (walk_forward_run_id, override_reason, override_at) = match &draft.graduation {
            Graduation::Certified {
                walk_forward_run_id,
                ..
            } => (Some(walk_forward_run_id.as_str().to_owned()), None, None),
            Graduation::Override { reason, at } => {
                (None, Some(reason.as_str().to_owned()), Some(at.clone()))
            }
        };
        let data_versions = Self::data_versions_json(&draft.graduation);
        // The `query!` expansion borrows its arguments inside its own statement
        // scope — derived strings bind to locals first (the house pattern).
        let id_text = id.as_str().to_owned();
        let version_text = draft.strategy_version_id.as_str().to_owned();
        let pair_text = draft.pair.as_str().to_owned();
        let primary_tf_text = draft.primary_timeframe.binance_interval();
        let htf_tf_text = draft.htf_timeframe.map(Timeframe::binance_interval);
        let equity_text = decimal_text(draft.starting_equity);
        let taker_text = decimal_text(draft.taker_fee_bps);
        let slip_text = decimal_text(draft.slippage_bps);
        let fingerprint_text = draft.engine_fingerprint.as_str().to_owned();
        let promoted_by_text = draft.promoted_by.as_str().to_owned();
        let uses_d1_i64 = i64::from(draft.uses_d1);
        let fixture_i64 = i64::from(draft.fixture);
        let min_trades_i64 = i64::from(draft.min_trades);
        let result = sqlx::query!(
            "INSERT INTO paper_session \
             (id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
              htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
              engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
              override_at, certified_data_versions, fixture, min_trades, promoted_by) \
             VALUES (?1, (SELECT COALESCE(MAX(seq), 0) + 1 FROM paper_session), \
                     ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, \
                     ?16, ?17, ?18, ?19)",
            id_text,
            version_text,
            created_at,
            pair_text,
            primary_tf_text,
            htf_tf_text,
            uses_d1_i64,
            equity_text,
            taker_text,
            slip_text,
            fingerprint_text,
            graduation_word,
            walk_forward_run_id,
            override_reason,
            override_at,
            data_versions,
            fixture_i64,
            min_trades_i64,
            promoted_by_text,
        )
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(PaperSession {
                id,
                // The insert above minted `MAX(seq)+1`; read it back so the
                // returned row is the row.
                seq: sqlx::query!("SELECT seq FROM paper_session WHERE id = ?1", id_text)
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|e| DataError::Db(format!("paper_session seq read-back failed: {e}")))?
                    .seq,
                strategy_version_id: draft.strategy_version_id.clone(),
                created_at,
                pair: draft.pair.clone(),
                primary_timeframe: draft.primary_timeframe,
                htf_timeframe: draft.htf_timeframe,
                uses_d1: draft.uses_d1,
                starting_equity: draft.starting_equity,
                taker_fee_bps: draft.taker_fee_bps,
                slippage_bps: draft.slippage_bps,
                engine_fingerprint: draft.engine_fingerprint.clone(),
                graduation: draft.graduation.clone(),
                fixture: draft.fixture,
                min_trades: draft.min_trades,
                promoted_by: draft.promoted_by.clone(),
            }),
            Err(e) => Err(DataError::Db(format!("paper_session insert failed: {e}"))),
        }
    }

    async fn get_session(&self, id: &PaperSessionId) -> Result<Option<PaperSession>, DataError> {
        let id_text = id.as_str();
        let row = sqlx::query_as!(
            RawSession,
            "SELECT id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
                    htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
                    engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
                    override_at, certified_data_versions, fixture, min_trades, promoted_by \
             FROM paper_session WHERE id = ?1",
            id_text
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("paper_session read failed: {e}")))?;
        row.map(session_from_row).transpose()
    }

    async fn list_sessions(&self) -> Result<Vec<PaperSession>, DataError> {
        let rows: Vec<RawSession> = sqlx::query_as!(
            RawSession,
            "SELECT id, seq, strategy_version_id, created_at, pair, primary_timeframe, \
                    htf_timeframe, uses_d1, starting_equity, taker_fee_bps, slippage_bps, \
                    engine_fingerprint, graduation, walk_forward_run_id, override_reason, \
                    override_at, certified_data_versions, fixture, min_trades, promoted_by \
             FROM paper_session ORDER BY seq ASC"
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("paper_session catalog read failed: {e}")))?;
        rows.into_iter().map(session_from_row).collect()
    }

    async fn all_versions_are_fixtures(
        &self,
        versions: &[CertifiedDataVersion],
    ) -> Result<bool, DataError> {
        if versions.is_empty() {
            return Ok(false);
        }
        for version in versions {
            let timeframe_text = version.timeframe.binance_interval();
            let data_version_text = version.data_version.as_str();
            let hit = sqlx::query_scalar!(
                "SELECT COUNT(*) FROM fixture_snapshot \
                 WHERE timeframe = ?1 AND data_version = ?2",
                timeframe_text,
                data_version_text,
            )
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DataError::Db(format!("fixture_snapshot probe failed: {e}")))?;
            if hit == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn insert_fixture_snapshot(
        &self,
        pair: &Pair,
        timeframe: Timeframe,
        data_version: &DataVersion,
    ) -> Result<(), DataError> {
        let (_dt, created_at) = self.now_rfc3339()?;
        let pair_text = pair.as_str();
        let timeframe_text = timeframe.binance_interval();
        let data_version_text = data_version.as_str();
        sqlx::query!(
            "INSERT OR IGNORE INTO fixture_snapshot (pair, timeframe, data_version, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            pair_text,
            timeframe_text,
            data_version_text,
            created_at,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("fixture_snapshot insert failed: {e}")))?;
        Ok(())
    }

    async fn append_bar(
        &self,
        session_id: &PaperSessionId,
        bars: &[(Timeframe, crate::domain::Candle, bool)],
        events: &[crate::domain::paper::event::PaperEvent],
    ) -> Result<Vec<crate::domain::paper::event::PaperEvent>, DataError> {
        let session = session_id.as_str().to_owned();
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| DataError::Db(format!("append_bar begin failed: {e}")))?;

        // The per-session mints, read inside the transaction (the write lock
        // makes them the true order).
        let bar_base: i64 = sqlx::query_scalar!(
            "SELECT COALESCE(MAX(seq), 0) FROM paper_bar WHERE session_id = ?1",
            session
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("paper_bar seq read failed: {e}")))?;
        let event_base: i64 = sqlx::query_scalar!(
            "SELECT COALESCE(MAX(seq), 0) FROM paper_event WHERE session_id = ?1",
            session
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| DataError::Db(format!("paper_event seq read failed: {e}")))?;

        for (offset, (timeframe, candle, lead_in)) in bars.iter().enumerate() {
            let bar_seq = bar_base + i64::try_from(offset).unwrap_or(0) + 1;
            let lead_in_i64 = i64::from(*lead_in);
            let timeframe_text = timeframe.binance_interval();
            let open_text = decimal_text(candle.open);
            let high_text = decimal_text(candle.high);
            let low_text = decimal_text(candle.low);
            let close_text = decimal_text(candle.close);
            let volume_text = decimal_text(candle.volume);
            let funding_text = candle.funding_rate.as_ref().map(|rate| decimal_text(*rate));
            sqlx::query!(
                "INSERT INTO paper_bar \
                 (session_id, timeframe, seq, open_time, close_time, open, high, low, close, \
                  volume, funding_rate, lead_in) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                session,
                timeframe_text,
                bar_seq,
                candle.open_time,
                candle.close_time,
                open_text,
                high_text,
                low_text,
                close_text,
                volume_text,
                funding_text,
                lead_in_i64,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| DataError::Db(format!("paper_bar insert failed: {e}")))?;
        }

        let mut persisted = Vec::with_capacity(events.len());
        for (offset, event) in events.iter().enumerate() {
            let minted_seq = event_base + i64::try_from(offset).unwrap_or(0) + 1;
            let event = event.clone().with_seq(minted_seq);
            let kind = event.kind();
            let at_text = event_at(&event);
            let payload = event
                .payload()
                .map_err(|e| DataError::Db(format!("paper_event payload failed: {e}")))?;
            sqlx::query!(
                "INSERT INTO paper_event (session_id, seq, at, kind, payload) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                session,
                minted_seq,
                at_text,
                kind,
                payload,
            )
            .execute(&mut *tx)
            .await
            .map_err(|e| DataError::Db(format!("paper_event insert failed: {e}")))?;
            persisted.push(event);
        }

        tx.commit()
            .await
            .map_err(|e| DataError::Db(format!("append_bar commit failed: {e}")))?;
        Ok(persisted)
    }

    async fn events(
        &self,
        session_id: &PaperSessionId,
    ) -> Result<Vec<crate::domain::paper::event::PaperEvent>, DataError> {
        /// A raw `paper_event` row (`query_as!` needs a named struct; the `at`
        /// instant rides the payload, so the column is not selected).
        struct RawEvent {
            seq: i64,
            kind: String,
            payload: String,
        }
        let session = session_id.as_str().to_owned();
        let rows: Vec<RawEvent> = sqlx::query_as!(
            RawEvent,
            "SELECT seq, kind, payload FROM paper_event \
             WHERE session_id = ?1 ORDER BY seq ASC",
            session
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("paper_event read failed: {e}")))?;
        rows.iter()
            .map(|row| {
                PaperEvent::decode(&row.kind, &row.payload).map_err(|e| {
                    DataError::Db(format!("paper_event row {} undecodable: {e:?}", row.seq))
                })
            })
            .collect()
    }
}

/// The event's RFC3339 instant, for the row's `at` column.
fn event_at(event: &crate::domain::paper::event::PaperEvent) -> String {
    use crate::domain::paper::event::PaperEvent;
    match event {
        PaperEvent::BarProcessed { at, .. }
        | PaperEvent::Order { at, .. }
        | PaperEvent::Fill { at, .. }
        | PaperEvent::Funding { at, .. }
        | PaperEvent::Stop { at, .. }
        | PaperEvent::DataEvent { at, .. }
        | PaperEvent::EngineUpgraded { at, .. }
        | PaperEvent::ShadowChecked { at, .. } => at.clone(),
    }
}

impl<C: Clock + Send + Sync> SqlitePaperSessionRepo<C> {
    /// Read one session's recorded candles for a timeframe back as `Candle`s,
    /// in `open_time` order, lead-in included — the read half of
    /// [`Self::append_bar`](crate::domain::PaperSessionRepository::append_bar).
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on a corrupt/un-parseable row or a store
    /// failure.
    pub async fn bars(
        &self,
        session_id: &PaperSessionId,
        timeframe: Timeframe,
    ) -> Result<Vec<crate::domain::Candle>, DataError> {
        let session = session_id.as_str().to_owned();
        let timeframe_text = timeframe.binance_interval();
        let rows: Vec<RawBar> = sqlx::query_as!(
            RawBar,
            "SELECT open_time, close_time, open, high, low, close, volume, funding_rate \
             FROM paper_bar WHERE session_id = ?1 AND timeframe = ?2 ORDER BY open_time ASC",
            session,
            timeframe_text
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| DataError::Db(format!("paper_bar read failed: {e}")))?;
        rows.iter()
            .map(|row| {
                Ok(crate::domain::Candle {
                    open_time: row.open_time,
                    close_time: row.close_time,
                    open: parse_decimal("paper_bar.open", &row.open)?,
                    high: parse_decimal("paper_bar.high", &row.high)?,
                    low: parse_decimal("paper_bar.low", &row.low)?,
                    close: parse_decimal("paper_bar.close", &row.close)?,
                    volume: parse_decimal("paper_bar.volume", &row.volume)?,
                    funding_rate: row
                        .funding_rate
                        .as_ref()
                        .map(|rate| parse_decimal("paper_bar.funding_rate", rate))
                        .transpose()?,
                })
            })
            .collect()
    }

    /// Materialise the session's recorded candles into content-addressed
    /// snapshots — one `SnapshotSelection` per timeframe that holds rows (spec
    /// 3b). The version derives with the candle store's `content_version` and
    /// writes through `write_snapshot` ONLY: never a `commit`, never HEAD.
    /// Content-addressed, so the same rows always give the same `data_version`
    /// and a repeat write is a no-op. Runs only at a shadow check (w3
    /// schedules it; this is the mechanism).
    ///
    /// # Errors
    ///
    /// Returns [`DataError::Db`] on a corrupt row, an absent session, or a
    /// store failure.
    pub async fn materialise(
        &self,
        session_id: &PaperSessionId,
        store: &crate::adapters::store::CandleStore,
    ) -> Result<Vec<crate::domain::SnapshotSelection>, DataError> {
        let session = self
            .get_session(session_id)
            .await?
            .ok_or_else(|| DataError::Db("materialise: no such paper session".to_owned()))?;
        let timeframes = [Timeframe::M15, Timeframe::H4, Timeframe::D1];
        let mut out = Vec::new();
        for timeframe in timeframes {
            let candles = self.bars(session_id, timeframe).await?;
            if candles.is_empty() {
                continue;
            }
            let version = crate::adapters::store::CandleStore::content_version(
                &session.pair,
                timeframe,
                &candles,
            );
            store
                .write_snapshot(&crate::domain::CandleSeries {
                    pair: session.pair.clone(),
                    timeframe,
                    version: version.clone(),
                    candles,
                })
                .map_err(|e| DataError::Db(format!("materialise write failed: {e}")))?;
            out.push(crate::domain::SnapshotSelection {
                timeframe,
                data_version: version,
            });
        }
        Ok(out)
    }
}
