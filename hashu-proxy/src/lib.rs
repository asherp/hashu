//! Stratum proxy. Transparent passthrough to the operator's default pool;
//! redirects to a redeemer's pool while a redemption is active (later
//! commits). See ARCHITECTURE.md §4.2, §4.6.

use std::sync::atomic::{AtomicU64, Ordering};

pub mod router;
pub mod stratum;

/// Counters maintained by the proxy. Updated atomically; readable
/// concurrently from any task.
///
/// `shares_accepted` is what the upstream pool ack'd. `shares_committed`
/// is the subset for which we successfully reconstructed a share preimage
/// from session state and appended it to the commitment tree — these can
/// diverge when the proxy missed the relevant `mining.subscribe` /
/// `mining.notify` (e.g. attaching mid-session).
#[derive(Debug, Default)]
pub struct ProxyMetrics {
    pub connections_accepted: AtomicU64,
    pub shares_submitted: AtomicU64,
    pub shares_accepted: AtomicU64,
    pub shares_rejected: AtomicU64,
    pub shares_committed: AtomicU64,
}

impl ProxyMetrics {
    pub fn snapshot(&self) -> ProxyMetricsSnapshot {
        ProxyMetricsSnapshot {
            connections_accepted: self.connections_accepted.load(Ordering::Relaxed),
            shares_submitted: self.shares_submitted.load(Ordering::Relaxed),
            shares_accepted: self.shares_accepted.load(Ordering::Relaxed),
            shares_rejected: self.shares_rejected.load(Ordering::Relaxed),
            shares_committed: self.shares_committed.load(Ordering::Relaxed),
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
}
