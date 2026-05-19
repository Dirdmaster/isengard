//! Maintenance window evaluator.
//!
//! See spec §"Window evaluator" of
//! `docs/superpowers/specs/2026-05-06-phase-9d-maintenance-windows-design.md`.
//!
//! The evaluator is a thin wrapper over [`croner::Cron`] + [`chrono_tz::Tz`]:
//! we parse the cron expression once per call, resolve the timezone, and
//! consult the previous firing relative to `now`. If `now` falls inside
//! `prev .. prev + WINDOW_DURATION` we are "in window".
//!
//! The evaluator is fail-closed for malformed input: an unparseable cron
//! returns `false` for `is_in_window`, which keeps the cycle from rolling
//! out updates the operator did not authorise. REST validation rejects
//! malformed expressions at write time, so this branch should never fire
//! in production; it is the safety net for hand-edited rows.

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use croner::Cron;
use std::str::FromStr;

use super::MaintenanceWindow;

/// How long after a cron firing the cycle still considers itself
/// in-window. Hard-coded to 1h for v1; configurable via a future
/// `MaintenanceWindow.duration` field without a migration since the type is
/// JSON-encoded.
pub const WINDOW_DURATION: Duration = Duration::hours(1);

/// Parse a cron expression. Returns `Err` with a human-readable message on
/// failure. Used by both the runtime evaluator and REST validation.
///
/// Accepts the standard 5-field syntax `minute hour day-of-month month
/// day-of-week`. `croner` also accepts the 6-field form (with seconds);
/// we do not advertise it but do not reject it either.
pub fn parse_cron(expr: &str) -> Result<Cron, String> {
    Cron::from_str(expr).map_err(|e| format!("{e}"))
}

/// Resolve a `MaintenanceWindow.timezone` into a [`chrono_tz::Tz`]. `None`
/// or unparseable values fall back to UTC. Lenient on purpose: a stale row
/// must never block the cycle.
fn resolve_tz(window: &MaintenanceWindow) -> Tz {
    match window.timezone.as_deref() {
        None => Tz::UTC,
        Some(name) => Tz::from_str(name).unwrap_or(Tz::UTC),
    }
}

/// Decide whether `now` falls inside the current maintenance window.
///
/// Algorithm:
/// 1. Parse the cron. On error, return `false` (fail-closed).
/// 2. Resolve the timezone.
/// 3. Find the previous firing relative to `now` in that timezone.
/// 4. Return `true` if `now - prev < WINDOW_DURATION`.
///
/// The `inclusive` flag on `find_previous_occurrence` is `true` so a `now`
/// that lands exactly on a firing counts as in-window (operator-friendly:
/// 02:00:00 sharp belongs to the 02:00 window).
pub fn is_in_window(window: &MaintenanceWindow, now: DateTime<Utc>) -> bool {
    let cron = match parse_cron(&window.cron_expr) {
        Ok(c) => c,
        Err(_) => return false,
    };
    let tz = resolve_tz(window);
    let now_local = now.with_timezone(&tz);
    let prev_local = match cron.find_previous_occurrence(&now_local, true) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let elapsed = now_local.signed_duration_since(prev_local);
    elapsed >= Duration::zero() && elapsed < WINDOW_DURATION
}

/// Compute the next firing time after `now`, expressed in UTC. Returns
/// `None` if the cron has no future occurrences (rare; effectively
/// "never") or if the expression is malformed.
///
/// Used by the updater to populate the `next_window` field on the
/// `update.deferred` event so notifier consumers can quote a concrete
/// "back-online" time.
pub fn next_window_after(window: &MaintenanceWindow, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let cron = parse_cron(&window.cron_expr).ok()?;
    let tz = resolve_tz(window);
    let now_local = now.with_timezone(&tz);
    let next_local = cron.find_next_occurrence(&now_local, false).ok()?;
    Some(next_local.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// 30 min after the 02:00 firing on a Sunday: in window.
    #[test]
    fn in_window_30_min_after_firing() {
        let w = MaintenanceWindow {
            cron_expr: "0 2 * * 0".to_string(),
            timezone: None,
        };
        // 2026-05-03 is a Sunday. 02:30 UTC is 30 min after the 02:00 firing.
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 2, 30, 0).unwrap();
        assert!(is_in_window(&w, now));
    }

    /// 90 min after the 02:00 firing: out of window (1h envelope expired).
    #[test]
    fn out_of_window_90_min_after_firing() {
        let w = MaintenanceWindow {
            cron_expr: "0 2 * * 0".to_string(),
            timezone: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 3, 30, 0).unwrap();
        assert!(!is_in_window(&w, now));
    }

    /// Before the very first occurrence (cron starts in the future): out of
    /// window. Picked a date well before any plausible Sunday occurrence.
    #[test]
    fn before_first_occurrence_is_out_of_window() {
        let w = MaintenanceWindow {
            // Only fires on Feb 30: an impossible date, so croner returns
            // no past occurrence. Falls back to false.
            cron_expr: "0 0 30 2 *".to_string(),
            timezone: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 12, 0, 0).unwrap();
        assert!(!is_in_window(&w, now));
    }

    /// Timezone honored: `0 2 * * 0` in Europe/Zurich while now is 02:30
    /// Zurich (00:30 UTC summer / 01:30 UTC winter) returns `true`.
    /// 2026-05-03 is in CEST (UTC+2), so 02:30 Zurich == 00:30 UTC.
    #[test]
    fn timezone_zurich_in_window() {
        let w = MaintenanceWindow {
            cron_expr: "0 2 * * 0".to_string(),
            timezone: Some("Europe/Zurich".to_string()),
        };
        // 00:30 UTC on 2026-05-03 (Sunday).
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 0, 30, 0).unwrap();
        assert!(is_in_window(&w, now));
    }

    /// Same UTC instant against a UTC-tz window: out of window (the
    /// 02:00 UTC firing has not happened yet at 00:30 UTC).
    #[test]
    fn timezone_utc_out_of_window_at_same_instant() {
        let w = MaintenanceWindow {
            cron_expr: "0 2 * * 0".to_string(),
            timezone: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 0, 30, 0).unwrap();
        assert!(!is_in_window(&w, now));
    }

    /// Malformed cron: returns false (fail-closed).
    #[test]
    fn malformed_cron_is_out_of_window() {
        let w = MaintenanceWindow {
            cron_expr: "this is not a cron".to_string(),
            timezone: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 2, 30, 0).unwrap();
        assert!(!is_in_window(&w, now));
    }

    /// Unknown timezone falls back to UTC. The same Sunday 02:30 UTC instant
    /// must still be in window when the cron fires at 02:00 UTC.
    #[test]
    fn unknown_timezone_falls_back_to_utc() {
        let w = MaintenanceWindow {
            cron_expr: "0 2 * * 0".to_string(),
            timezone: Some("Mars/Olympus".to_string()),
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 3, 2, 30, 0).unwrap();
        assert!(is_in_window(&w, now));
    }

    /// `next_window_after` returns the upcoming Sunday 02:00 UTC.
    #[test]
    fn next_window_after_returns_upcoming_occurrence() {
        let w = MaintenanceWindow {
            cron_expr: "0 2 * * 0".to_string(),
            timezone: None,
        };
        // Tuesday 2026-05-05 12:00 UTC: next Sunday is 2026-05-10 02:00 UTC.
        let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
        let next = next_window_after(&w, now).expect("next");
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 5, 10, 2, 0, 0).unwrap());
    }

    /// `next_window_after` returns None for a malformed cron.
    #[test]
    fn next_window_after_returns_none_for_malformed_cron() {
        let w = MaintenanceWindow {
            cron_expr: "garbage".to_string(),
            timezone: None,
        };
        let now = Utc.with_ymd_and_hms(2026, 5, 5, 12, 0, 0).unwrap();
        assert!(next_window_after(&w, now).is_none());
    }

    /// `parse_cron` round-trips a valid 5-field expression.
    #[test]
    fn parse_cron_accepts_5_field_syntax() {
        assert!(parse_cron("0 2 * * 0").is_ok());
        assert!(parse_cron("*/15 * * * *").is_ok());
    }

    /// `parse_cron` returns Err with a non-empty message on garbage input.
    #[test]
    fn parse_cron_rejects_garbage() {
        let err = parse_cron("not a cron").unwrap_err();
        assert!(!err.is_empty());
    }
}
