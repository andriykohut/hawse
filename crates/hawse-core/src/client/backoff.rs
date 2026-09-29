use std::hash::{BuildHasher, RandomState};
use std::time::Duration;

const FIRST: Duration = Duration::from_secs(1);
const CAP: Duration = Duration::from_secs(30);
/// Twice the cap, so two processes superseding each other never reset: each session lasts only
/// until the other's next retry, which is at most `CAP`.
const HEALTHY: Duration = Duration::from_secs(60);

/// Doubles the wait after every failure up to `CAP`, and starts over only after a session that
/// stayed up for `HEALTHY`.
#[derive(Debug, Default)]
pub(super) struct Backoff {
    failures: u32,
}

impl Backoff {
    /// `lasted` is how long the session that just ended was up, zero if none was established.
    /// `unit` in [0, 1] places the wait between half the step and all of it.
    pub(super) fn next(&mut self, lasted: Duration, unit: f64) -> Duration {
        if lasted >= HEALTHY {
            self.failures = 0;
        }
        let step = FIRST
            .saturating_mul(2u32.saturating_pow(self.failures))
            .min(CAP);
        self.failures = self.failures.saturating_add(1);
        step.mul_f64(0.5 + 0.5 * unit)
    }
}

/// A draw in [0, 1) from the standard library's per-instance hasher keys, which is all jitter
/// needs and saves a crate. 52 random mantissa bits under the exponent of 1.0 make a float in
/// [1, 2).
pub(super) fn random_unit() -> f64 {
    let bits = RandomState::new().hash_one(()) >> 12;
    f64::from_bits(1f64.to_bits() | bits) - 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(s: f64) -> Duration {
        Duration::from_secs_f64(s)
    }

    #[test]
    fn doubles_from_one_second_up_to_thirty() {
        let mut backoff = Backoff::default();
        let waits: Vec<_> = (0..8).map(|_| backoff.next(Duration::ZERO, 1.0)).collect();
        let want = [1.0, 2.0, 4.0, 8.0, 16.0, 30.0, 30.0, 30.0].map(secs);
        assert_eq!(waits, want);
    }

    #[test]
    fn jitter_waits_between_half_the_step_and_all_of_it() {
        let mut low = Backoff::default();
        let mut mid = Backoff::default();
        low.next(Duration::ZERO, 0.0);
        mid.next(Duration::ZERO, 0.5);
        assert_eq!(low.next(Duration::ZERO, 0.0), secs(1.0));
        assert_eq!(mid.next(Duration::ZERO, 0.5), secs(1.5));
    }

    #[test]
    fn a_session_up_for_a_minute_starts_the_ladder_over() {
        let mut backoff = Backoff::default();
        for _ in 0..6 {
            backoff.next(Duration::ZERO, 1.0);
        }
        assert_eq!(backoff.next(Duration::from_secs(60), 1.0), secs(1.0));
        assert_eq!(backoff.next(Duration::ZERO, 1.0), secs(2.0));
    }

    #[test]
    fn a_shorter_session_keeps_climbing() {
        let mut backoff = Backoff::default();
        backoff.next(Duration::ZERO, 1.0);
        backoff.next(Duration::from_secs(59), 1.0);
        assert_eq!(backoff.next(Duration::from_secs(30), 1.0), secs(4.0));
    }

    #[test]
    fn stays_at_the_cap_after_many_failures() {
        let mut backoff = Backoff::default();
        for _ in 0..100 {
            backoff.next(Duration::ZERO, 1.0);
        }
        assert_eq!(backoff.next(Duration::ZERO, 1.0), secs(30.0));
    }

    #[test]
    fn random_unit_varies_within_zero_to_one() {
        let draws: Vec<f64> = (0..64).map(|_| random_unit()).collect();
        assert!(draws.iter().all(|u| (0.0..1.0).contains(u)));
        assert!(draws.iter().any(|u| u.to_bits() != draws[0].to_bits()));
    }
}
