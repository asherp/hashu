//! Crediting accepted shares to the redemption ledger. ARCHITECTURE.md §4.3.
//!
//! Every share the upstream pool accepts is worth `difficulty × 2^32`
//! hashes. When the connection that submitted it was serving a redemption
//! (its upstream is the redeemer's pool), those hashes are credited to
//! that redemption in the shared [`RedemptionLedger`]. Shares sent to the
//! operator's default pool are not credited.
//!
//! Which redemption a connection is serving is decided by the pool
//! routing controller (§4.6) through the [`RedemptionBinding`] trait. The
//! binding is sampled when `mining.submit` goes out and used when the pool
//! acks it, so an ack that arrives after a switch still credits the
//! redemption the share was actually mined for.
//!
//! Each credited share is also appended to the redemption's share
//! commitment tree ([`crate::audit::ShareAudit`]), so the root covers
//! exactly the work that filled it.
//!
//! When a credit crosses a redemption's target, [`ShareCrediter`] sends
//! that redemption's id once on a broadcast channel, so the router can
//! move hashrate back to the default pool.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use hashu_core::redemption::{CreditOutcome, LedgerError, RedemptionId, RedemptionLedger};
use tokio::sync::broadcast;

use crate::audit::ShareAudit;
use crate::ProxyMetrics;

/// Listener-assigned id of one accepted miner connection. Unique for the
/// life of the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionId(pub u64);

impl ConnectionId {
    /// Next process-wide id. Ids start at 1.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "conn-{}", self.0)
    }
}

/// Tells the proxy which redemption, if any, a connection's upstream is
/// serving right now. `None` means the default pool: nothing is credited.
/// Implemented by the routing controller.
pub trait RedemptionBinding: Send + Sync {
    fn current(&self, conn: ConnectionId) -> Option<RedemptionId>;

    /// An accepted share of bdiff `difficulty` from `conn`, on any route
    /// (the default pool included). The router uses it for each rig's
    /// hashrate estimate; the default does nothing.
    fn observe_share(&self, _conn: ConnectionId, _difficulty: f64) {}
}

/// The simplest binding: every connection serves the same redemption (or
/// none). Useful for a single-miner demo and for tests.
#[derive(Debug, Default)]
pub struct StaticBinding(Mutex<Option<RedemptionId>>);

impl StaticBinding {
    pub fn new(id: Option<RedemptionId>) -> Self {
        Self(Mutex::new(id))
    }

    pub fn set(&self, id: Option<RedemptionId>) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = id;
    }
}

impl RedemptionBinding for StaticBinding {
    fn current(&self, _conn: ConnectionId) -> Option<RedemptionId> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Default difficulty assumed for a share submitted before any
/// `mining.set_difficulty` (Stratum V1's implicit starting difficulty).
pub const DEFAULT_SHARE_DIFFICULTY: f64 = 1.0;

/// Capacity of the completion channel. Completions are rare (one per
/// redemption), so a slow subscriber only lags in extreme cases.
const COMPLETION_CHANNEL_CAPACITY: usize = 64;

/// Credits accepted shares to the ledger and announces completions.
#[derive(Debug)]
pub struct ShareCrediter {
    ledger: Arc<RedemptionLedger>,
    audit: Arc<ShareAudit>,
    completions: broadcast::Sender<RedemptionId>,
}

/// PoW evidence for one accepted share, as it enters the commitment tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareEvidence {
    /// The reconstructed 80-byte block header.
    pub preimage: Vec<u8>,
    pub target_bits: u32,
    pub ntime: u32,
}

impl ShareCrediter {
    pub fn new(ledger: Arc<RedemptionLedger>) -> Self {
        Self::with_audit(ledger, Arc::new(ShareAudit::new()))
    }

    /// Like [`Self::new`], with a caller-built (e.g. persistent) audit.
    pub fn with_audit(ledger: Arc<RedemptionLedger>, audit: Arc<ShareAudit>) -> Self {
        let (completions, _) = broadcast::channel(COMPLETION_CHANNEL_CAPACITY);
        Self {
            ledger,
            audit,
            completions,
        }
    }

    pub fn ledger(&self) -> &Arc<RedemptionLedger> {
        &self.ledger
    }

    /// Per-redemption share commitment trees filled by
    /// [`Self::credit_with_evidence`].
    pub fn audit(&self) -> &Arc<ShareAudit> {
        &self.audit
    }

    /// Credit one accepted share and, if the ledger counted it, append its
    /// evidence to the redemption's commitment tree. Shares arriving after
    /// completion or settle are ignored by the ledger and kept out of the tree.
    pub fn credit_with_evidence(
        &self,
        id: &RedemptionId,
        difficulty: f64,
        evidence: ShareEvidence,
    ) -> Result<CreditOutcome, LedgerError> {
        let outcome = self.credit(id, difficulty)?;
        let ignored = outcome.state.is_finished() && !outcome.newly_completed;
        if !ignored {
            self.audit
                .append(*id, evidence.preimage, evidence.target_bits, evidence.ntime);
        }
        Ok(outcome)
    }

    /// Receive each redemption id once, when its target is reached.
    pub fn subscribe_completions(&self) -> broadcast::Receiver<RedemptionId> {
        self.completions.subscribe()
    }

    /// Credit one accepted share at bdiff `difficulty` to `id`.
    pub fn credit(&self, id: &RedemptionId, difficulty: f64) -> Result<CreditOutcome, LedgerError> {
        let outcome = self.ledger.credit_share_outcome(id, difficulty)?;
        if outcome.newly_completed {
            tracing::info!(redemption = %id, hashes = %outcome.accumulated_hashes, "redemption completed");
            // No subscribers is fine: the ledger still records completion
            // and the melt check observes it.
            let _ = self.completions.send(*id);
        }
        Ok(outcome)
    }
}

/// What a connection needs to credit its accepted shares.
#[derive(Clone)]
pub struct CreditContext {
    pub crediter: Arc<ShareCrediter>,
    pub binding: Arc<dyn RedemptionBinding>,
}

impl CreditContext {
    pub fn new(crediter: Arc<ShareCrediter>, binding: Arc<dyn RedemptionBinding>) -> Self {
        Self { crediter, binding }
    }

    /// Credit an accepted share that was submitted while `bound_to` was
    /// the connection's redemption. Updates metrics; never fails the
    /// connection.
    pub(crate) fn credit_accepted(
        &self,
        conn: ConnectionId,
        bound_to: Option<RedemptionId>,
        difficulty: Option<f64>,
        evidence: ShareEvidence,
        metrics: &ProxyMetrics,
    ) {
        let difficulty = difficulty.unwrap_or(DEFAULT_SHARE_DIFFICULTY);
        self.binding.observe_share(conn, difficulty);
        let Some(id) = bound_to else {
            return;
        };
        match self
            .crediter
            .credit_with_evidence(&id, difficulty, evidence)
        {
            Ok(outcome) => {
                metrics.shares_credited.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    %conn,
                    redemption = %id,
                    difficulty,
                    accumulated = %outcome.accumulated_hashes,
                    target = %outcome.target_hashes,
                    "share credited",
                );
            }
            Err(e) => {
                tracing::warn!(%conn, redemption = %id, error = %e, "share not credited");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::redemption::{Redemption, RedemptionState, HASHES_PER_DIFF_ONE};

    fn id(n: u8) -> RedemptionId {
        RedemptionId::from_bytes([n; 16])
    }

    fn ev() -> ShareEvidence {
        ShareEvidence {
            preimage: vec![0xab; 80],
            target_bits: 0x1d00_ffff,
            ntime: 0,
        }
    }

    fn ctx_with(redemption: RedemptionId, thh: u64) -> (CreditContext, Arc<StaticBinding>) {
        let ledger = Arc::new(RedemptionLedger::new());
        ledger
            .open(Redemption::new(redemption, thh, "p".into(), "w".into()))
            .unwrap();
        let binding = Arc::new(StaticBinding::default());
        let ctx = CreditContext::new(Arc::new(ShareCrediter::new(ledger)), binding.clone());
        (ctx, binding)
    }

    #[test]
    fn connection_ids_are_unique() {
        assert_ne!(ConnectionId::next(), ConnectionId::next());
    }

    #[test]
    fn unbound_share_is_not_credited() {
        let (ctx, _) = ctx_with(id(1), 1);
        let m = ProxyMetrics::default();
        ctx.credit_accepted(ConnectionId(1), None, Some(1.0), ev(), &m);
        assert_eq!(m.snapshot().shares_credited, 0);
        let r = ctx.crediter.ledger().get(&id(1)).unwrap();
        assert_eq!(r.accumulated_hashes, 0);
        assert_eq!(ctx.crediter.audit().head(&id(1)).leaf_count, 0);
    }

    #[test]
    fn bound_share_credits_difficulty_times_2_32() {
        let (ctx, _) = ctx_with(id(1), 1);
        let m = ProxyMetrics::default();
        ctx.credit_accepted(ConnectionId(1), Some(id(1)), Some(8.0), ev(), &m);
        assert_eq!(m.snapshot().shares_credited, 1);
        let r = ctx.crediter.ledger().get(&id(1)).unwrap();
        assert_eq!(r.accumulated_hashes, 8 * HASHES_PER_DIFF_ONE);
        assert_eq!(r.state, RedemptionState::Active);
    }

    #[test]
    fn missing_difficulty_defaults_to_one() {
        let (ctx, _) = ctx_with(id(1), 1);
        let m = ProxyMetrics::default();
        ctx.credit_accepted(ConnectionId(1), Some(id(1)), None, ev(), &m);
        let r = ctx.crediter.ledger().get(&id(1)).unwrap();
        assert_eq!(r.accumulated_hashes, HASHES_PER_DIFF_ONE);
    }

    #[test]
    fn unknown_redemption_is_logged_not_counted() {
        let (ctx, _) = ctx_with(id(1), 1);
        let m = ProxyMetrics::default();
        ctx.credit_accepted(ConnectionId(1), Some(id(9)), Some(1.0), ev(), &m);
        assert_eq!(m.snapshot().shares_credited, 0);
    }

    #[test]
    fn completion_is_broadcast_once() {
        let (ctx, _) = ctx_with(id(2), 1);
        let mut rx = ctx.crediter.subscribe_completions();
        let m = ProxyMetrics::default();
        // 1 THH = 3.6e15 hashes ≈ 838_190.3 × 2^32. Two diff-5e5 shares
        // cross the target on the second one.
        ctx.credit_accepted(ConnectionId(1), Some(id(2)), Some(5e5), ev(), &m);
        assert!(rx.try_recv().is_err());
        ctx.credit_accepted(ConnectionId(1), Some(id(2)), Some(5e5), ev(), &m);
        assert_eq!(rx.try_recv().unwrap(), id(2));
        ctx.credit_accepted(ConnectionId(1), Some(id(2)), Some(5e5), ev(), &m);
        assert!(rx.try_recv().is_err());
        assert!(ctx.crediter.ledger().get(&id(2)).unwrap().is_complete());
        // The share after completion is not in the tree.
        assert_eq!(ctx.crediter.audit().head(&id(2)).leaf_count, 2);
    }

    #[test]
    fn static_binding_set_and_clear() {
        let b = StaticBinding::default();
        assert_eq!(b.current(ConnectionId(1)), None);
        b.set(Some(id(3)));
        assert_eq!(b.current(ConnectionId(7)), Some(id(3)));
        b.set(None);
        assert_eq!(b.current(ConnectionId(1)), None);
    }
}
