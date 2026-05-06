//! Retry backoff schedule for webhook deliveries.
//!
//! One initial attempt plus up to four retries. After attempt N (1-indexed)
//! we wait the duration at `SCHEDULE[N - 1]` before attempt N+1. Attempt 5
//! has no successor, so we return `None` and the worker marks the row
//! `exhausted`.
//!
//! The spec lists `30s, 1m, 5m, 30m, 2h`. With the `max 5 attempts` cap, the
//! `2h` slot would be the wait AFTER attempt 5, which has no successor; it
//! is intentionally omitted. Bumping `MAX_ATTEMPTS` to 6 in a follow-up
//! reactivates it.

use std::time::Duration;

/// Maximum number of attempts before giving up.
pub const MAX_ATTEMPTS: i64 = 5;

/// Schedule of waits between attempts. Index `i` is the wait AFTER attempt
/// `i + 1`. Length is `MAX_ATTEMPTS - 1` because the final attempt has no
/// retry slot.
pub const SCHEDULE: [Duration; (MAX_ATTEMPTS - 1) as usize] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
];

/// Return the wait before the NEXT attempt given that `attempts_so_far`
/// attempts have already been made. Returns `None` once attempts have
/// reached `MAX_ATTEMPTS`.
pub fn next_delay(attempts_so_far: i64) -> Option<Duration> {
    if attempts_so_far <= 0 {
        return Some(SCHEDULE[0]);
    }
    if attempts_so_far >= MAX_ATTEMPTS {
        return None;
    }
    SCHEDULE.get(attempts_so_far as usize - 1).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_matches_spec() {
        assert_eq!(next_delay(1), Some(Duration::from_secs(30)));
        assert_eq!(next_delay(2), Some(Duration::from_secs(60)));
        assert_eq!(next_delay(3), Some(Duration::from_secs(5 * 60)));
        assert_eq!(next_delay(4), Some(Duration::from_secs(30 * 60)));
        // After 5 attempts we have no retry left.
        assert_eq!(next_delay(5), None);
        assert_eq!(next_delay(99), None);
    }

    #[test]
    fn zero_or_negative_attempts_get_first_slot() {
        assert_eq!(next_delay(0), Some(Duration::from_secs(30)));
        assert_eq!(next_delay(-1), Some(Duration::from_secs(30)));
    }
}
