//! Stratum proxy. Transparent passthrough to the operator's default pool;
//! redirects to a redeemer's pool while a redemption is active (later
//! commits). See ARCHITECTURE.md §4.2, §4.6.

use std::sync::atomic::{AtomicU64, Ordering};

pub mod audit;
pub mod control;
pub mod credit;
pub mod home;
pub mod meter;
pub mod router;
pub mod stratum;
pub mod upstream;

/// Counters maintained by the proxy. Updated atomically; readable
/// concurrently from any task.
///
/// `shares_accepted` is what the upstream pool ack'd. `shares_committed`
/// is the subset for which we successfully reconstructed a share preimage
/// from session state and appended it to the commitment tree — these can
/// diverge when the proxy missed the relevant `mining.subscribe` /
/// `mining.notify` (e.g. attaching mid-session). `shares_credited` is the
/// subset of accepted shares credited to an open redemption in the ledger
/// (shares sent to the default pool are accepted but not credited).
#[derive(Debug, Default)]
pub struct ProxyMetrics {
    pub connections_accepted: AtomicU64,
    pub shares_submitted: AtomicU64,
    pub shares_accepted: AtomicU64,
    pub shares_rejected: AtomicU64,
    pub shares_committed: AtomicU64,
    pub shares_credited: AtomicU64,
    /// Sum of the difficulty of every committed accepted share, as `f64`
    /// bits. Times 2^32 this is the work (hashes) miners delivered through
    /// the proxy, wherever it was routed; [`meter::HashrateMeter`] turns it
    /// into a hashrate.
    accepted_difficulty_bits: AtomicU64,
}

impl ProxyMetrics {
    /// Record the work of one accepted share of `difficulty`.
    pub fn add_accepted_work(&self, difficulty: f64) {
        if !difficulty.is_finite() || difficulty <= 0.0 {
            return;
        }
        let _ = self.accepted_difficulty_bits.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |bits| Some((f64::from_bits(bits) + difficulty).to_bits()),
        );
    }

    /// Total work of accepted shares, in hashes (difficulty × 2^32).
    pub fn accepted_hashes(&self) -> f64 {
        f64::from_bits(self.accepted_difficulty_bits.load(Ordering::Relaxed))
            * hashu_core::redemption::HASHES_PER_DIFF_ONE as f64
    }

    pub fn snapshot(&self) -> ProxyMetricsSnapshot {
        ProxyMetricsSnapshot {
            connections_accepted: self.connections_accepted.load(Ordering::Relaxed),
            shares_submitted: self.shares_submitted.load(Ordering::Relaxed),
            shares_accepted: self.shares_accepted.load(Ordering::Relaxed),
            shares_rejected: self.shares_rejected.load(Ordering::Relaxed),
            shares_committed: self.shares_committed.load(Ordering::Relaxed),
            shares_credited: self.shares_credited.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxyMetricsSnapshot {
    pub connections_accepted: u64,
    pub shares_submitted: u64,
    pub shares_accepted: u64,
    pub shares_rejected: u64,
    pub shares_committed: u64,
    pub shares_credited: u64,
}
