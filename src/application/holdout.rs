//! The holdout guard (r4.s1.w4; grill Q4, G1, F1) — ONE application-layer
//! decision, wired into every entry point.
//!
//! While a freeze is open, no run or export may evaluate data at or after the
//! frozen holdout start. The decision has exactly three outcomes, and this
//! module is the only place it is made:
//!
//! - an **explicit** window whose `from` is at/after the holdout start, or
//!   whose `to` is later than it, is **refused by name** ([`HoldoutRefusal`]
//!   cites the pair, the offending bound and the holdout start) — G1;
//! - a **defaulted** end (`run_backtest` with no window, `run_walk_forward`
//!   with no `to`) is **clamped to the holdout start**, and the caller says so
//!   in its result (the #327 echo) — G1;
//! - everything else passes unchanged, so with no freeze open every entry
//!   point behaves byte-identically to before this item.
//!
//! The clamp is decided *only* when it moves something: when the snapshot
//! already ends before the holdout start, the defaulted window is unchanged
//! (`Pass`) and the echo reports `clamped: false` with the snapshot's own
//! bounds — an accurate effective window, not a decorative flag.
//!
//! EXEMPTIONS (grill Q4, G8), named where they are exempt rather than here:
//! the **certification step** (w5 — the one caller that must evaluate the
//! holdout, and the reason this record exists), the **certify-fixture seed**
//! (`pulse fixture`'s synthetic snapshots, never HEAD — it passes
//! `holdout: None` at its `run_walk_forward` call), and **paper sessions**
//! (they step their own engine session and never enter these use cases).
//!
//! The guard consumes [`HoldoutFreeze`] — the open record's holdout start and
//! nothing else — so the entry points read the freeze once and pass it down;
//! `None` means no freeze is open (or the caller is an exemption), and the
//! guard is then inert.

use crate::domain::{CandleWindow, HoldoutFreeze, Pair};

/// A run or export refused because its window reaches into the frozen holdout.
///
/// Display names the pair, the offending bound and the holdout start (RFC 3339
/// UTC) — the "refused by name" the spec asks for; the MCP surfaces map it to a
/// `from`/`to` field error, the app to its existing error shape (ADR-0020), and
/// the coach gate to its `walk_forward` accept-failure stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HoldoutRefusal {
    /// The pair the refused run or export would have evaluated.
    pub pair: Pair,
    /// The offending request member: `"from"` or `"to"`.
    pub field: &'static str,
    /// The open freeze's holdout start, epoch ms.
    pub holdout_start_ms: i64,
}

impl std::fmt::Display for HoldoutRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the holdout freeze refuses this run on {}: `{}` reaches into the holdout, which \
             starts at {} — the search span ends there",
            self.pair,
            self.field,
            rfc3339(self.holdout_start_ms),
        )
    }
}

impl std::error::Error for HoldoutRefusal {}

/// An epoch-ms instant as RFC 3339 UTC seconds, with the raw ms appended when
/// it cannot be rendered (mirrors the CLI's freeze printout).
fn rfc3339(ms: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(dt) => format!(
            "{} ({ms} ms)",
            dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
        None => format!("{ms} ms"),
    }
}

/// The guard's decision over a both-or-neither backtest window (`None` = the
/// whole snapshot, the app's and CLI's shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowDecision {
    /// Run the caller's window (or the whole snapshot) unchanged.
    Pass,
    /// The defaulted window is clamped to `[snapshot_first_open, holdout_start)`.
    ClampToHoldoutStart {
        /// The open freeze's holdout start, epoch ms.
        holdout_start_ms: i64,
    },
}

/// The guard's decision over a walk-forward span's requested bounds (each
/// independent; an omitted `to` defaults to the snapshot's last close).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanDecision {
    /// Resolve the span as always: the defaulted `to` is the snapshot's end.
    Pass,
    /// The defaulted `to` is clamped to the holdout start.
    ClampToHoldoutStart {
        /// The open freeze's holdout start, epoch ms.
        holdout_start_ms: i64,
    },
}

/// Guard a backtest's requested window.
///
/// `snapshot_last_open_ms` is the loaded primary series' last candle's
/// `open_time` — the run counts bars by `open_time`, so the defaulted run only
/// needs clamping when a bar actually opens at/after the holdout start. It is
/// `None` for an empty series (nothing to clamp).
///
/// # Errors
///
/// Returns [`HoldoutRefusal`] when an explicit window reaches into the holdout.
pub fn guard_backtest_window(
    freeze: Option<HoldoutFreeze>,
    pair: &Pair,
    window: Option<&CandleWindow>,
    snapshot_last_open_ms: Option<i64>,
) -> Result<WindowDecision, HoldoutRefusal> {
    let Some(freeze) = freeze else {
        return Ok(WindowDecision::Pass);
    };
    match window {
        Some(w) => {
            if w.from_ms >= freeze.holdout_start_ms {
                return Err(HoldoutRefusal {
                    pair: pair.clone(),
                    field: "from",
                    holdout_start_ms: freeze.holdout_start_ms,
                });
            }
            if w.to_ms > freeze.holdout_start_ms {
                return Err(HoldoutRefusal {
                    pair: pair.clone(),
                    field: "to",
                    holdout_start_ms: freeze.holdout_start_ms,
                });
            }
            Ok(WindowDecision::Pass)
        }
        None => match snapshot_last_open_ms {
            Some(last) if last >= freeze.holdout_start_ms => {
                Ok(WindowDecision::ClampToHoldoutStart {
                    holdout_start_ms: freeze.holdout_start_ms,
                })
            }
            _ => Ok(WindowDecision::Pass),
        },
    }
}

/// Guard a walk-forward span's requested bounds.
///
/// `snapshot_end_ms` is the resolved default for an omitted `to` (the
/// snapshot's last candle's `close_time`): the clamp fires only when that end
/// is later than the holdout start, so a snapshot that already ends before the
/// holdout passes unchanged.
///
/// # Errors
///
/// Returns [`HoldoutRefusal`] when an explicit bound reaches into the holdout.
pub fn guard_span(
    freeze: Option<HoldoutFreeze>,
    pair: &Pair,
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    snapshot_end_ms: i64,
) -> Result<SpanDecision, HoldoutRefusal> {
    let Some(freeze) = freeze else {
        return Ok(SpanDecision::Pass);
    };
    if let Some(from) = from_ms
        && from >= freeze.holdout_start_ms
    {
        return Err(HoldoutRefusal {
            pair: pair.clone(),
            field: "from",
            holdout_start_ms: freeze.holdout_start_ms,
        });
    }
    if let Some(to) = to_ms
        && to > freeze.holdout_start_ms
    {
        return Err(HoldoutRefusal {
            pair: pair.clone(),
            field: "to",
            holdout_start_ms: freeze.holdout_start_ms,
        });
    }
    if to_ms.is_none() && snapshot_end_ms > freeze.holdout_start_ms {
        Ok(SpanDecision::ClampToHoldoutStart {
            holdout_start_ms: freeze.holdout_start_ms,
        })
    } else {
        Ok(SpanDecision::Pass)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{SpanDecision, WindowDecision, guard_backtest_window, guard_span};
    use crate::domain::{CandleWindow, HoldoutFreeze, Pair};

    const H: i64 = 1_751_328_000_000; // 2025-07-01T00:00:00Z

    fn freeze() -> HoldoutFreeze {
        HoldoutFreeze {
            holdout_start_ms: H,
        }
    }

    #[test]
    fn no_freeze_passes_everything() {
        let pair = Pair::new("BTCUSDT");
        let window = CandleWindow::new(H - 10, H + 10).unwrap();
        assert_eq!(
            guard_backtest_window(None, &pair, Some(&window), Some(H + 1)).unwrap(),
            WindowDecision::Pass
        );
        assert_eq!(
            guard_span(None, &pair, Some(H), Some(H + 1), H + 2).unwrap(),
            SpanDecision::Pass
        );
    }

    #[test]
    fn explicit_windows_into_the_holdout_are_refused_by_name() {
        let pair = Pair::new("SOLUSDT");
        // `to` later than the holdout start.
        let window = CandleWindow::new(H - 10, H + 1).unwrap();
        let refusal =
            guard_backtest_window(Some(freeze()), &pair, Some(&window), None).unwrap_err();
        assert_eq!(refusal.field, "to");
        assert!(refusal.to_string().contains("SOLUSDT"));
        assert!(refusal.to_string().contains("2025-07-01"));
        // `from` at the holdout start.
        let window = CandleWindow::new(H, H + 10).unwrap();
        let refusal =
            guard_backtest_window(Some(freeze()), &pair, Some(&window), None).unwrap_err();
        assert_eq!(refusal.field, "from");
        // A span's explicit bounds follow the same law.
        let refusal = guard_span(Some(freeze()), &pair, None, Some(H + 1), H + 2).unwrap_err();
        assert_eq!(refusal.field, "to");
        let refusal = guard_span(Some(freeze()), &pair, Some(H), None, H + 2).unwrap_err();
        assert_eq!(refusal.field, "from");
    }

    #[test]
    fn windows_that_clear_the_holdout_pass() {
        let pair = Pair::new("BTCUSDT");
        // `to` == the holdout start is legal: `to` is exclusive.
        let window = CandleWindow::new(H - 10, H).unwrap();
        assert_eq!(
            guard_backtest_window(Some(freeze()), &pair, Some(&window), None).unwrap(),
            WindowDecision::Pass
        );
        assert_eq!(
            guard_span(Some(freeze()), &pair, Some(H - 10), Some(H), H + 2).unwrap(),
            SpanDecision::Pass
        );
    }

    #[test]
    fn defaulted_windows_clamp_only_when_they_would_reach_the_holdout() {
        let pair = Pair::new("XRPUSDT");
        assert_eq!(
            guard_backtest_window(Some(freeze()), &pair, None, Some(H + 1)).unwrap(),
            WindowDecision::ClampToHoldoutStart {
                holdout_start_ms: H
            }
        );
        // The snapshot already ends before the holdout start: nothing to move.
        assert_eq!(
            guard_backtest_window(Some(freeze()), &pair, None, Some(H - 1)).unwrap(),
            WindowDecision::Pass
        );
        assert_eq!(
            guard_span(Some(freeze()), &pair, None, None, H + 1).unwrap(),
            SpanDecision::ClampToHoldoutStart {
                holdout_start_ms: H
            }
        );
        assert_eq!(
            guard_span(Some(freeze()), &pair, None, None, H - 1).unwrap(),
            SpanDecision::Pass
        );
        // An explicit `to` never clamps — it passes or refuses.
        assert_eq!(
            guard_span(Some(freeze()), &pair, None, Some(H - 1), H + 1).unwrap(),
            SpanDecision::Pass
        );
    }
}
