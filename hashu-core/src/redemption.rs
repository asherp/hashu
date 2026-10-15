//! Redemption state machine. ARCHITECTURE.md §4.3, §5.1.
//!
//! A redemption is opened when a holder melts `THH` vouchers with the
//! `hashrate` method. It accumulates hashes as the proxy reports accepted
//! shares, and completes once the accumulated hashes cover the amount
//! melted. Progress is kept in *hashes* (exact integers), not in floating
//! point THH, so completion is decided without rounding drift.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

/// Hashes in one `THH` unit: 1 TH/s sustained for 1 hour = 10^12 × 3600.
pub const HASHES_PER_THH: u128 = 3_600_000_000_000_000;

/// Expected hashes per share at difficulty 1 (bdiff): 2^32.
pub const HASHES_PER_DIFF_ONE: u128 = 1 << 32;

/// Opaque 16-byte redemption handle. Random at creation (the mint
/// generates it); printed as lowercase hex. Never derived from the pool
/// or worker, so it leaks nothing about routing (§4.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RedemptionId([u8; 16]);

impl RedemptionId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for RedemptionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// Lifecycle of a redemption.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RedemptionState {
    /// Opened by a melt; no accepted shares credited yet.
    Pending,
    /// At least one share has been credited, target not yet reached.
    Active,
    /// Accumulated hashes ≥ target. Terminal.
    Completed,
}

/// One redemption's target and progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redemption {
    pub id: RedemptionId,
    /// Redeemer's pool endpoint (stratum URL). Used by routing, not here.
    pub pool_url: String,
    /// Worker name the redeemer's pool expects.
    pub worker_name: String,
    /// `THH` melted.
    pub amount_thh: u64,
    /// `amount_thh × HASHES_PER_THH`.
    pub target_hashes: u128,
    /// Hashes credited so far from accepted shares.
    pub accumulated_hashes: u128,
    pub state: RedemptionState,
}

impl Redemption {
    pub fn new(id: RedemptionId, amount_thh: u64, pool_url: String, worker_name: String) -> Self {
        Self {
            id,
            pool_url,
            worker_name,
            amount_thh,
            target_hashes: u128::from(amount_thh) * HASHES_PER_THH,
            accumulated_hashes: 0,
            state: RedemptionState::Pending,
        }
    }

    /// Credit one accepted share at the given bdiff. Returns the state
    /// after crediting. Credits to a completed redemption are ignored, so
    /// shares in flight at completion do not move the total past target.
    pub fn credit_share(&mut self, difficulty: f64) -> RedemptionState {
        if self.state == RedemptionState::Completed {
            return self.state;
        }
        self.accumulated_hashes = self
            .accumulated_hashes
            .saturating_add(hashes_for_difficulty(difficulty));
        self.state = if self.accumulated_hashes >= self.target_hashes {
            RedemptionState::Completed
        } else {
            RedemptionState::Active
        };
        self.state
    }

    pub fn is_complete(&self) -> bool {
        self.state == RedemptionState::Completed
    }
}

/// Expected hashes behind one accepted share at bdiff `difficulty`.
/// Non-finite or non-positive difficulties contribute nothing.
pub fn hashes_for_difficulty(difficulty: f64) -> u128 {
    if !difficulty.is_finite() || difficulty <= 0.0 {
        return 0;
    }
    (difficulty * HASHES_PER_DIFF_ONE as f64).round() as u128
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("redemption {0} already exists")]
    Duplicate(RedemptionId),
    #[error("unknown redemption {0}")]
    Unknown(RedemptionId),
}

/// In-memory store of redemptions, shared between the melt method (which
/// opens them) and the proxy path (which credits shares). Persistence is
/// not here yet; a restart forgets open redemptions.
#[derive(Debug, Default)]
pub struct RedemptionLedger {
    inner: Mutex<HashMap<RedemptionId, Redemption>>,
}

impl RedemptionLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, redemption: Redemption) -> Result<(), LedgerError> {
        let mut map = self.lock();
        if map.contains_key(&redemption.id) {
            return Err(LedgerError::Duplicate(redemption.id));
        }
        map.insert(redemption.id, redemption);
        Ok(())
    }

    /// Credit an accepted share to a redemption and return its new state.
    pub fn credit_share(
        &self,
        id: &RedemptionId,
        difficulty: f64,
    ) -> Result<RedemptionState, LedgerError> {
        let mut map = self.lock();
        let redemption = map.get_mut(id).ok_or(LedgerError::Unknown(*id))?;
        Ok(redemption.credit_share(difficulty))
    }

    /// Snapshot of one redemption, or `None` if unknown.
    pub fn get(&self, id: &RedemptionId) -> Option<Redemption> {
        self.lock().get(id).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RedemptionId, Redemption>> {
        // A poisoned lock only means another thread panicked mid-update;
        // the map is still structurally valid, so keep serving it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> RedemptionId {
        RedemptionId::from_bytes([n; 16])
    }

    #[test]
    fn one_thh_is_3_6e15_hashes() {
        assert_eq!(HASHES_PER_THH, 3_600_000_000_000_000);
    }

    #[test]
    fn id_displays_as_32_hex_chars() {
        let s = id(0xab).to_string();
        assert_eq!(s.len(), 32);
        assert_eq!(s, "ab".repeat(16));
    }

    #[test]
    fn difficulty_to_hashes() {
        assert_eq!(hashes_for_difficulty(1.0), 1 << 32);
        assert_eq!(hashes_for_difficulty(0.0), 0);
        assert_eq!(hashes_for_difficulty(-5.0), 0);
        assert_eq!(hashes_for_difficulty(f64::NAN), 0);
        assert_eq!(hashes_for_difficulty(f64::INFINITY), 0);
    }

    #[test]
    fn new_redemption_targets_amount_in_hashes() {
        let r = Redemption::new(id(1), 2, "stratum+tcp://p".into(), "w".into());
        assert_eq!(r.target_hashes, 2 * HASHES_PER_THH);
        assert_eq!(r.state, RedemptionState::Pending);
        assert_eq!(r.accumulated_hashes, 0);
    }

    #[test]
    fn credits_move_pending_to_active_then_completed() {
        // Target is 1 THH = 3.6e15 hashes. A diff-1 share is 2^32 hashes
        // (stays Active); a diff-2^20 share adds 2^52 and crosses target.
        let mut r = Redemption::new(id(1), 1, "p".into(), "w".into());
        assert_eq!(r.credit_share(1.0), RedemptionState::Active);
        assert!(!r.is_complete());
        assert_eq!(r.credit_share(1_048_576.0), RedemptionState::Completed);
        assert!(r.is_complete());
    }

    #[test]
    fn completion_is_exact_at_the_boundary() {
        // Target 1 THH. Crediting exactly target−1 hashes must not
        // complete; one more hash must.
        let mut r = Redemption::new(id(1), 1, "p".into(), "w".into());
        r.accumulated_hashes = HASHES_PER_THH - 1;
        r.state = RedemptionState::Active;
        assert_eq!(r.accumulated_hashes + 1, r.target_hashes);
        // A zero-difficulty share adds nothing and does not complete.
        assert_eq!(r.credit_share(0.0), RedemptionState::Active);
        // Credit the final hash via a difficulty that rounds to exactly 1.
        r.accumulated_hashes = HASHES_PER_THH - 1;
        r.target_hashes = HASHES_PER_THH;
        assert_eq!(
            r.credit_share(1.0 / (1u64 << 32) as f64),
            RedemptionState::Completed
        );
    }

    #[test]
    fn completed_redemption_ignores_further_credits() {
        let mut r = Redemption::new(id(1), 1, "p".into(), "w".into());
        r.credit_share(1e9);
        assert!(r.is_complete());
        let before = r.accumulated_hashes;
        assert_eq!(r.credit_share(1e9), RedemptionState::Completed);
        assert_eq!(r.accumulated_hashes, before);
    }

    #[test]
    fn ledger_rejects_duplicates_and_unknown_ids() {
        let ledger = RedemptionLedger::new();
        let r = Redemption::new(id(7), 1, "p".into(), "w".into());
        ledger.open(r.clone()).unwrap();
        assert_eq!(ledger.open(r), Err(LedgerError::Duplicate(id(7))));
        assert_eq!(
            ledger.credit_share(&id(9), 1.0),
            Err(LedgerError::Unknown(id(9)))
        );
    }

    #[test]
    fn ledger_credit_is_visible_through_get() {
        let ledger = RedemptionLedger::new();
        ledger
            .open(Redemption::new(id(3), 1, "p".into(), "w".into()))
            .unwrap();
        assert_eq!(
            ledger.credit_share(&id(3), 1e9),
            Ok(RedemptionState::Completed)
        );
        assert_eq!(
            ledger.get(&id(3)).map(|r| r.state),
            Some(RedemptionState::Completed)
        );
    }
}
