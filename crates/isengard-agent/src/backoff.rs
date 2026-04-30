//! Jittered exponential backoff for the agent's reconnect loop.
//!
//! Spec §6: "1s, 2s, 4s, 8s, 16s, 32s, 60s (cap), jittered ±20%". Reset to
//! base after a stream stays open ≥ 60s.

use std::time::Duration;

const BASE_MS: u64 = 1_000;
const CAP_MS: u64 = 60_000;
const JITTER_PCT: f64 = 0.20;

#[derive(Debug, Clone)]
pub struct Backoff {
    /// Current attempt number (0 = first attempt, no backoff yet).
    attempt: u32,
}

impl Backoff {
    pub fn new() -> Self {
        Self { attempt: 0 }
    }

    /// Compute the delay for the next attempt and increment the counter.
    /// Returns Duration::ZERO on the very first call (attempt 0).
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn next_delay(&mut self) -> Duration {
        let attempt = self.attempt;
        self.attempt = self.attempt.saturating_add(1);

        if attempt == 0 {
            return Duration::ZERO;
        }

        // Exponential: base * 2^(attempt-1), capped at CAP.
        let exp = (attempt - 1).min(20); // 2^20 is way past the cap; saturate
        let unjittered_ms = BASE_MS.saturating_mul(1u64 << exp).min(CAP_MS);

        // Apply ±20% jitter using a tiny LCG so we don't pull rand as a dep
        // here. The randomness only needs to be "good enough" — not crypto.
        let jitter = jitter_factor();
        let jittered_ms = ((unjittered_ms as f64) * jitter) as u64;
        // Clamp to [1, CAP * (1 + JITTER_PCT)] to avoid 0-ms pathological case.
        let final_ms = jittered_ms
            .max(1)
            .min(((CAP_MS as f64) * (1.0 + JITTER_PCT)) as u64);

        Duration::from_millis(final_ms)
    }

    /// Reset the counter so the next call to `next_delay` returns ZERO.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Current attempt number (0 = haven't attempted yet).
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// Returns a value in `[1.0 - JITTER_PCT, 1.0 + JITTER_PCT]` using a tiny
/// thread-local LCG. Cheap, deterministic-per-call (advances on each call),
/// no extra crate.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn jitter_factor() -> f64 {
    use std::cell::Cell;
    thread_local! {
        // Seed from process start time. Different threads get different
        // seeds because thread_local default-initializes per-thread.
        static STATE: Cell<u64> = Cell::new({
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0xdeadbeef)
                | 1 // odd seed
        });
    }
    STATE.with(|s| {
        // LCG params from Numerical Recipes (Park-Miller-ish).
        let next = s
            .get()
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        s.set(next);
        let normalized = f64::from((next >> 32) as u32) / f64::from(u32::MAX); // [0, 1)
        1.0 - JITTER_PCT + 2.0 * JITTER_PCT * normalized // [1-J, 1+J)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_returns_zero() {
        let mut b = Backoff::new();
        assert_eq!(b.next_delay(), Duration::ZERO);
    }

    #[test]
    fn second_call_is_around_base() {
        let mut b = Backoff::new();
        let _ = b.next_delay(); // attempt 0 → 0
        let d = b.next_delay(); // attempt 1 → ~1s ± 20%
        let ms = d.as_millis() as u64;
        assert!(ms >= 800, "expected ≥800ms, got {ms}ms");
        assert!(ms <= 1200, "expected ≤1200ms, got {ms}ms");
    }

    #[test]
    fn grows_until_cap() {
        let mut b = Backoff::new();
        let mut last = Duration::ZERO;
        for i in 0..20 {
            let d = b.next_delay();
            // d should be monotonically non-decreasing once we leave attempt 0
            if i > 0 {
                assert!(
                    d >= last || d.as_millis() >= (CAP_MS as u128 * 80 / 100),
                    "step {i}: {d:?} < {last:?}"
                );
            }
            last = d;
        }
        // Final delay must be ≤ cap × (1 + jitter)
        let max = ((CAP_MS as f64) * (1.0 + JITTER_PCT)) as u128;
        assert!(
            last.as_millis() <= max,
            "final {last:?} exceeds cap {max}ms"
        );
    }

    #[test]
    fn never_exceeds_cap_plus_jitter() {
        let mut b = Backoff::new();
        let cap = ((CAP_MS as f64) * (1.0 + JITTER_PCT)) as u128;
        for _ in 0..50 {
            assert!(b.next_delay().as_millis() <= cap);
        }
    }

    #[test]
    fn reset_returns_to_zero_for_next_call() {
        let mut b = Backoff::new();
        for _ in 0..10 {
            let _ = b.next_delay();
        }
        b.reset();
        assert_eq!(b.attempt(), 0);
        assert_eq!(b.next_delay(), Duration::ZERO);
    }

    #[test]
    fn jitter_factor_in_expected_range() {
        for _ in 0..1000 {
            let f = jitter_factor();
            assert!(f >= 1.0 - JITTER_PCT, "got {f}");
            assert!(f <= 1.0 + JITTER_PCT, "got {f}");
        }
    }
}
