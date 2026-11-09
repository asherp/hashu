//! Start a new pool connection near the miner's real difficulty.
//!
//! A pool's vardiff starts every new connection at its own default (8,192
//! on Braiins, 262,144 on OCEAN, …) and only converges after minutes of
//! shares. When the proxy moves a miner to another pool, it already knows
//! the miner's hashrate from the shares it relayed, so it tells the new
//! pool up front, in the two standard ways:
//!
//! - `mining.suggest_difficulty`, sent *after* the authorize result (ckpool
//!   ignores it from a client that isn't authorized yet);
//! - `d=N` in the worker password, when the redeemer left it at the
//!   default `x` (public-pool, ViaBTC, NiceHash and others read it).
//!
//! Pools that support neither just run their vardiff as before.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How far back the hashrate estimate looks.
pub const WINDOW: Duration = Duration::from_secs(600);
/// Accepted shares needed before the estimate is trusted.
pub const MIN_SHARES: usize = 8;
/// Time needed before the estimate is trusted.
pub const MIN_ELAPSED: Duration = Duration::from_secs(1);
/// Seconds per share the suggested difficulty aims at. The suggestion is
/// rounded down to a power of two, so the miner lands at one share every
/// 2.5 to 5 s: ckpool treats a suggestion as a floor for its vardiff, so
/// erring low keeps it free to adjust.
pub const SECS_PER_SHARE: f64 = 5.0;

/// Hashrate from the shares a pool accepted on this connection.
#[derive(Debug)]
pub struct WorkRate {
    start: Instant,
    shares: VecDeque<(Instant, f64)>,
}

impl WorkRate {
    pub fn new(start: Instant) -> Self {
        Self {
            start,
            shares: VecDeque::new(),
        }
    }

    /// A pool accepted a share of difficulty `diff` at `now`.
    pub fn accepted(&mut self, now: Instant, diff: f64) {
        if diff.is_finite() && diff > 0.0 {
            self.shares.push_back((now, diff));
        }
        while self
            .shares
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > WINDOW)
        {
            self.shares.pop_front();
        }
    }

    /// Hashes per second over the window, once there is enough to go on.
    pub fn hashes_per_sec(&self, now: Instant) -> Option<f64> {
        let elapsed = now.saturating_duration_since(self.start).min(WINDOW);
        let recent: Vec<f64> = self
            .shares
            .iter()
            .filter(|(t, _)| now.saturating_duration_since(*t) <= WINDOW)
            .map(|(_, d)| *d)
            .collect();
        if recent.len() < MIN_SHARES || elapsed < MIN_ELAPSED {
            return None;
        }
        let work: f64 = recent.iter().sum();
        Some(work * 4_294_967_296.0 / elapsed.as_secs_f64())
    }

    /// The difficulty to ask a new pool for, if the estimate is ready.
    pub fn start_difficulty(&self, now: Instant) -> Option<u64> {
        self.hashes_per_sec(now).and_then(start_difficulty)
    }
}

/// Difficulty for one share per [`SECS_PER_SHARE`] at `hashes_per_sec`,
/// rounded down to a power of two, at least 1.
pub fn start_difficulty(hashes_per_sec: f64) -> Option<u64> {
    if !hashes_per_sec.is_finite() || hashes_per_sec <= 0.0 {
        return None;
    }
    let d = hashes_per_sec * SECS_PER_SHARE / 4_294_967_296.0;
    if d < 1.0 {
        return Some(1);
    }
    // Below 2^63, so the shift can't overflow.
    let bits = d.min(9.2e18).log2().floor() as u32;
    Some(1u64 << bits.min(62))
}

/// The worker password with `d=N` added, when the redeemer left it at the
/// default (`x` or empty). A password the redeemer chose is theirs: it
/// may already carry a difficulty, or mean something to their pool.
pub fn password_with_difficulty(password: &str, difficulty: u64) -> String {
    match password.trim() {
        "" | "x" | "X" => format!("x,d={difficulty}"),
        _ => password.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_rounds_down_to_a_power_of_two() {
        // 100 TH/s at one share per 5 s: 116,415 → 65,536.
        assert_eq!(start_difficulty(100e12), Some(65_536));
        // 1 TH/s: 1,164 → 1,024.
        assert_eq!(start_difficulty(1e12), Some(1_024));
        assert_eq!(start_difficulty(1e3), Some(1));
        assert_eq!(start_difficulty(0.0), None);
        assert_eq!(start_difficulty(f64::NAN), None);
        assert_eq!(start_difficulty(f64::INFINITY), None);
        assert!(start_difficulty(1e40).unwrap().is_power_of_two());
    }

    #[test]
    fn estimate_needs_enough_shares_and_time() {
        let t0 = Instant::now();
        let mut r = WorkRate::new(t0);
        for i in 0..MIN_SHARES as u64 - 1 {
            r.accepted(t0 + Duration::from_secs(i), 1024.0);
        }
        assert_eq!(r.hashes_per_sec(t0 + Duration::from_secs(10)), None);
        r.accepted(t0 + Duration::from_secs(9), 1024.0);
        // 8 × 1024 × 2^32 hashes over 10 s.
        let hps = r.hashes_per_sec(t0 + Duration::from_secs(10)).unwrap();
        assert!((hps - 8.0 * 1024.0 * 4_294_967_296.0 / 10.0).abs() < 1.0);
        assert_eq!(
            r.start_difficulty(t0 + Duration::from_secs(10)),
            start_difficulty(hps)
        );
        // Too soon after the connection opened: not trusted yet.
        let mut fast = WorkRate::new(t0);
        for _ in 0..MIN_SHARES {
            fast.accepted(t0, 1.0);
        }
        assert_eq!(fast.hashes_per_sec(t0 + Duration::from_millis(500)), None);
        assert!(fast.hashes_per_sec(t0 + MIN_ELAPSED).is_some());
    }

    #[test]
    fn old_shares_leave_the_window() {
        let t0 = Instant::now();
        let mut r = WorkRate::new(t0);
        for _ in 0..MIN_SHARES {
            r.accepted(t0, 1.0);
        }
        let later = t0 + WINDOW + Duration::from_secs(1);
        assert_eq!(r.hashes_per_sec(later), None);
        r.accepted(later, 1.0);
        assert_eq!(r.shares.len(), 1);
    }

    #[test]
    fn only_default_passwords_get_a_difficulty() {
        assert_eq!(password_with_difficulty("x", 4096), "x,d=4096");
        assert_eq!(password_with_difficulty("", 4096), "x,d=4096");
        assert_eq!(password_with_difficulty("secret", 4096), "secret");
        assert_eq!(password_with_difficulty("d=512", 4096), "d=512");
    }
}
