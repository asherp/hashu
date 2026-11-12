//! Redemption state machine. ARCHITECTURE.md §4.3, §5.1.
//!
//! A redemption is opened when a holder melts `THH` vouchers with the
//! `hashrate` method. It accumulates hashes as the proxy reports accepted
//! shares, and completes once the accumulated hashes cover the amount
//! melted. Progress is kept in *hashes* (exact integers), not in floating
//! point THH, so completion is decided without rounding drift.
//!
//! A redemption that stops receiving hashrate can instead be *settled*:
//! its delivered work is frozen, and the melt reports only that much as
//! spent, so cdk signs the redeemer's blank outputs (NUT-08) for the rest.
//! The part of a redemption that can come back this way is everything above
//! its [`Redemption::floor_thh`].

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::journal::Journal;

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

impl std::str::FromStr for RedemptionId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = hex::decode(s).map_err(|e| format!("bad redemption id: {e}"))?;
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| "redemption id must be 16 bytes".to_string())?;
        Ok(Self(bytes))
    }
}

impl Serialize for RedemptionId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RedemptionId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
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
    /// Stopped before reaching target; the undelivered part goes back to
    /// the redeemer as change. Terminal.
    Settled,
}

impl RedemptionState {
    /// Lowercase name for status APIs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Settled => "settled",
        }
    }

    /// Completed or settled: no more routing or crediting.
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Settled)
    }
}

/// One redemption's target and progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redemption {
    pub id: RedemptionId,
    /// Redeemer's pool endpoint (stratum URL). Used by routing, not here.
    pub pool_url: String,
    /// Worker name the redeemer's pool expects.
    pub worker_name: String,
    /// Password for `worker_name` in `mining.authorize`; `None` sends the
    /// conventional `x`.
    pub worker_password: Option<String>,
    /// `THH` melted.
    pub amount_thh: u64,
    /// `THH` the redeemer pays even if the redemption settles early: the
    /// melt quote's amount, with the rest of `amount_thh` in its fee
    /// reserve. Equal to `amount_thh` for redemptions opened without a
    /// reserve, which can't return change.
    pub floor_thh: u64,
    /// `amount_thh × HASHES_PER_THH`.
    pub target_hashes: u128,
    /// Hashes credited so far from accepted shares.
    pub accumulated_hashes: u128,
    pub state: RedemptionState,
    /// Supplier whose rigs fill this order (the mint it was bought from,
    /// e.g. `alice` for `alice.mint.<domain>`). `None` = the operator's
    /// own mint: filled by the operator's rigs.
    pub supplier: Option<String>,
}

impl Redemption {
    pub fn new(id: RedemptionId, amount_thh: u64, pool_url: String, worker_name: String) -> Self {
        Self {
            id,
            pool_url,
            worker_name,
            worker_password: None,
            supplier: None,
            amount_thh,
            floor_thh: amount_thh,
            target_hashes: u128::from(amount_thh) * HASHES_PER_THH,
            accumulated_hashes: 0,
            state: RedemptionState::Pending,
        }
    }

    /// Set the worker password (`None` or empty: the default `x`).
    pub fn with_password(mut self, password: Option<String>) -> Self {
        self.worker_password = password.filter(|p| !p.is_empty());
        self
    }

    /// Set the supplier whose rigs fill it (see [`Redemption::supplier`]).
    pub fn with_supplier(mut self, supplier: Option<String>) -> Self {
        self.supplier = supplier.filter(|s| !s.is_empty());
        self
    }

    /// Set the non-refundable part (clamped to `amount_thh`).
    pub fn with_floor(mut self, floor_thh: u64) -> Self {
        self.floor_thh = floor_thh.min(self.amount_thh);
        self
    }

    /// Password to send in `mining.authorize`.
    pub fn authorize_password(&self) -> &str {
        self.worker_password.as_deref().unwrap_or("x")
    }

    /// Credit one accepted share at the given bdiff. Returns the state
    /// after crediting. Credits to a completed redemption are ignored, so
    /// shares in flight at completion do not move the total past target.
    pub fn credit_share(&mut self, difficulty: f64) -> RedemptionState {
        self.credit_hashes(hashes_for_difficulty(difficulty))
    }

    /// Credit `hashes` directly. Same rules as [`Self::credit_share`];
    /// settled redemptions ignore credits too.
    pub fn credit_hashes(&mut self, hashes: u128) -> RedemptionState {
        if self.state.is_finished() {
            return self.state;
        }
        self.accumulated_hashes = self.accumulated_hashes.saturating_add(hashes);
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

    /// Completed or settled.
    pub fn is_finished(&self) -> bool {
        self.state.is_finished()
    }

    /// Delivered work in whole `THH`, rounded up: a started unit counts as
    /// spent.
    pub fn delivered_thh(&self) -> u64 {
        let whole = self.accumulated_hashes.div_ceil(HASHES_PER_THH);
        u64::try_from(whole).unwrap_or(u64::MAX)
    }

    /// `THH` the melt reports as spent. A settled redemption is charged for
    /// what it was delivered, but never less than its floor; anything else
    /// is charged the full amount.
    pub fn spent_thh(&self) -> u64 {
        match self.state {
            RedemptionState::Settled => self.delivered_thh().clamp(self.floor_thh, self.amount_thh),
            _ => self.amount_thh,
        }
    }

    /// `THH` returned to the redeemer as change: `amount_thh − spent_thh`.
    pub fn change_thh(&self) -> u64 {
        self.amount_thh - self.spent_thh()
    }

    /// `(charged, returned)` if the redemption were settled now: delivered
    /// work rounded up, never below the floor. `None` once it has finished.
    /// A stop is only worth offering while `returned > 0`; below that (for
    /// example nothing delivered on a redemption of exactly its floor) it
    /// would burn the whole amount.
    pub fn settle_preview(&self) -> Option<(u64, u64)> {
        if self.is_finished() {
            return None;
        }
        let charged = self.delivered_thh().clamp(self.floor_thh, self.amount_thh);
        Some((charged, self.amount_thh - charged))
    }

    /// Stop the redemption where it stands. Returns false if it had
    /// already finished.
    fn settle(&mut self) -> bool {
        if self.is_finished() {
            return false;
        }
        self.state = RedemptionState::Settled;
        true
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

/// Result of crediting one share through the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreditOutcome {
    /// State after the credit.
    pub state: RedemptionState,
    /// True only for the credit that crossed the target.
    pub newly_completed: bool,
    pub accumulated_hashes: u128,
    pub target_hashes: u128,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LedgerError {
    #[error("redemption {0} already exists")]
    Duplicate(RedemptionId),
    #[error("unknown redemption {0}")]
    Unknown(RedemptionId),
}

/// Result of [`RedemptionLedger::settle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettleOutcome {
    /// State after the call: `Settled`, or `Completed` if it had already
    /// reached target.
    pub state: RedemptionState,
    /// True only for the call that settled it.
    pub newly_settled: bool,
    pub spent_thh: u64,
    pub change_thh: u64,
}

/// One line of the ledger journal. Credits record hashes, not difficulty,
/// so replay is exact.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum LedgerRecord {
    Open {
        id: RedemptionId,
        amount_thh: u64,
        pool_url: String,
        worker_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_password: Option<String>,
        /// Absent in journals from before settling existed: no reserve.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        floor_thh: Option<u64>,
        /// Absent in journals from before supplier mints: the operator's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supplier: Option<String>,
    },
    Credit {
        id: RedemptionId,
        /// Decimal string: u128 does not fit a JSON number portably.
        hashes: String,
    },
    /// Frozen at the hashes credited before this record.
    Settle { id: RedemptionId },
}

/// Store of redemptions, shared between the melt method (which opens
/// them) and the proxy path (which credits shares). [`Self::new`] keeps
/// them in memory only; [`Self::persistent`] also journals every change
/// to disk and restores them on the next start.
#[derive(Debug, Default)]
pub struct RedemptionLedger {
    inner: Mutex<HashMap<RedemptionId, Redemption>>,
    /// Ids in the order they were opened, so routing can serve the oldest
    /// redemption first. Never shrinks; completed ids are skipped on read.
    order: Mutex<Vec<RedemptionId>>,
    journal: Option<Journal>,
}

impl RedemptionLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ledger backed by the journal at `path`: replays it, compacts it to
    /// one open + one credit record per redemption, and journals every
    /// later open and credit. Missing file = empty ledger.
    pub fn persistent(path: &Path) -> io::Result<Self> {
        let (journal, records, skipped) = Journal::open::<LedgerRecord>(path)?;
        if skipped > 0 {
            tracing::warn!(?path, skipped, "skipped unreadable ledger journal lines");
        }
        let mut ledger = Self::new();
        for record in records {
            match record {
                LedgerRecord::Open {
                    id,
                    amount_thh,
                    pool_url,
                    worker_name,
                    worker_password,
                    floor_thh,
                    supplier,
                } => {
                    // A duplicate open can only come from a hand-edited
                    // file; keep the first.
                    let _ = ledger.open(
                        Redemption::new(id, amount_thh, pool_url, worker_name)
                            .with_password(worker_password)
                            .with_supplier(supplier)
                            .with_floor(floor_thh.unwrap_or(amount_thh)),
                    );
                }
                LedgerRecord::Settle { id } => {
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.settle();
                    }
                }
                LedgerRecord::Credit { id, hashes } => {
                    let Ok(hashes) = hashes.parse::<u128>() else {
                        continue;
                    };
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.credit_hashes(hashes);
                    }
                }
            }
        }
        let snapshot = ledger.all();
        let compact: Vec<LedgerRecord> = snapshot
            .iter()
            .flat_map(|r| {
                let open = LedgerRecord::Open {
                    id: r.id,
                    amount_thh: r.amount_thh,
                    pool_url: r.pool_url.clone(),
                    worker_name: r.worker_name.clone(),
                    worker_password: r.worker_password.clone(),
                    floor_thh: Some(r.floor_thh),
                    supplier: r.supplier.clone(),
                };
                let credit = (r.accumulated_hashes > 0).then(|| LedgerRecord::Credit {
                    id: r.id,
                    hashes: r.accumulated_hashes.to_string(),
                });
                let settle = (r.state == RedemptionState::Settled)
                    .then_some(LedgerRecord::Settle { id: r.id });
                std::iter::once(open).chain(credit).chain(settle)
            })
            .collect();
        journal.rewrite(&compact)?;
        tracing::info!(
            path = ?journal.path(),
            redemptions = snapshot.len(),
            open = snapshot.iter().filter(|r| !r.is_finished()).count(),
            "restored redemption ledger"
        );
        ledger.journal = Some(journal);
        Ok(ledger)
    }

    fn record(&self, record: &LedgerRecord) {
        if let Some(j) = &self.journal {
            if let Err(e) = j.append(record) {
                tracing::error!(error = %e, path = ?j.path(), "ledger journal write failed");
            }
        }
    }

    pub fn open(&self, redemption: Redemption) -> Result<(), LedgerError> {
        let mut map = self.lock();
        if map.contains_key(&redemption.id) {
            return Err(LedgerError::Duplicate(redemption.id));
        }
        let id = redemption.id;
        // Journal while holding the lock so records keep apply order.
        self.record(&LedgerRecord::Open {
            id,
            amount_thh: redemption.amount_thh,
            pool_url: redemption.pool_url.clone(),
            worker_name: redemption.worker_name.clone(),
            worker_password: redemption.worker_password.clone(),
            floor_thh: Some(redemption.floor_thh),
            supplier: redemption.supplier.clone(),
        });
        map.insert(id, redemption);
        // Take `order` while still holding `inner` so the two never
        // disagree about which ids exist. Lock order: inner, then order.
        self.order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(id);
        Ok(())
    }

    /// Snapshots of every redemption that is not yet finished, oldest
    /// first. The pool router polls this to decide where to send hashrate.
    pub fn active(&self) -> Vec<Redemption> {
        let map = self.lock();
        let order = self.order.lock().unwrap_or_else(|e| e.into_inner());
        order
            .iter()
            .filter_map(|id| map.get(id))
            .filter(|r| !r.is_finished())
            .cloned()
            .collect()
    }

    /// Snapshots of every redemption, completed ones included, oldest
    /// first. For status reporting; routing uses [`Self::active`].
    pub fn all(&self) -> Vec<Redemption> {
        let map = self.lock();
        let order = self.order.lock().unwrap_or_else(|e| e.into_inner());
        order.iter().filter_map(|id| map.get(id)).cloned().collect()
    }

    /// Credit an accepted share to a redemption and return its new state.
    pub fn credit_share(
        &self,
        id: &RedemptionId,
        difficulty: f64,
    ) -> Result<RedemptionState, LedgerError> {
        self.credit_share_outcome(id, difficulty).map(|o| o.state)
    }

    /// Like [`Self::credit_share`], but also reports whether this credit is
    /// the one that moved the redemption to `Completed`. Exactly one credit
    /// per redemption returns `newly_completed = true`, which lets callers
    /// fire a completion signal once without racing other crediters.
    pub fn credit_share_outcome(
        &self,
        id: &RedemptionId,
        difficulty: f64,
    ) -> Result<CreditOutcome, LedgerError> {
        let mut map = self.lock();
        let redemption = map.get_mut(id).ok_or(LedgerError::Unknown(*id))?;
        let was_finished = redemption.is_finished();
        let hashes = hashes_for_difficulty(difficulty);
        let state = redemption.credit_hashes(hashes);
        if !was_finished && hashes > 0 {
            self.record(&LedgerRecord::Credit {
                id: *id,
                hashes: hashes.to_string(),
            });
        }
        Ok(CreditOutcome {
            state,
            newly_completed: !was_finished && state == RedemptionState::Completed,
            accumulated_hashes: redemption.accumulated_hashes,
            target_hashes: redemption.target_hashes,
        })
    }

    /// Stop crediting `id` and charge it only for the work delivered so
    /// far; the melt then returns the rest as change. Settling a finished
    /// redemption changes nothing.
    pub fn settle(&self, id: &RedemptionId) -> Result<SettleOutcome, LedgerError> {
        let mut map = self.lock();
        let redemption = map.get_mut(id).ok_or(LedgerError::Unknown(*id))?;
        let newly_settled = redemption.settle();
        if newly_settled {
            self.record(&LedgerRecord::Settle { id: *id });
            tracing::info!(
                redemption = %id,
                spent_thh = redemption.spent_thh(),
                change_thh = redemption.change_thh(),
                "redemption settled"
            );
        }
        Ok(SettleOutcome {
            state: redemption.state,
            newly_settled,
            spent_thh: redemption.spent_thh(),
            change_thh: redemption.change_thh(),
        })
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
    fn credit_outcome_flags_completion_exactly_once() {
        let ledger = RedemptionLedger::new();
        ledger
            .open(Redemption::new(id(4), 1, "p".into(), "w".into()))
            .unwrap();
        let first = ledger.credit_share_outcome(&id(4), 1.0).unwrap();
        assert_eq!(first.state, RedemptionState::Active);
        assert!(!first.newly_completed);
        assert_eq!(first.accumulated_hashes, HASHES_PER_DIFF_ONE);
        let crossing = ledger.credit_share_outcome(&id(4), 1e9).unwrap();
        assert_eq!(crossing.state, RedemptionState::Completed);
        assert!(crossing.newly_completed);
        let after = ledger.credit_share_outcome(&id(4), 1e9).unwrap();
        assert_eq!(after.state, RedemptionState::Completed);
        assert!(!after.newly_completed);
    }

    #[test]
    fn active_lists_open_redemptions_oldest_first() {
        let ledger = RedemptionLedger::new();
        for n in [5u8, 2, 9] {
            ledger
                .open(Redemption::new(id(n), 1, "p".into(), "w".into()))
                .unwrap();
        }
        let ids: Vec<_> = ledger.active().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![id(5), id(2), id(9)]);

        ledger.credit_share(&id(2), 1e9).unwrap();
        let ids: Vec<_> = ledger.active().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![id(5), id(9)]);
    }

    #[test]
    fn supplier_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            ledger
                .open(
                    Redemption::new(id(1), 1, "stratum+tcp://p:1".into(), "w".into())
                        .with_supplier(Some("alice".into())),
                )
                .unwrap();
            ledger
                .open(Redemption::new(
                    id(2),
                    1,
                    "stratum+tcp://p:1".into(),
                    "w".into(),
                ))
                .unwrap();
        }
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        assert_eq!(
            ledger.get(&id(1)).unwrap().supplier.as_deref(),
            Some("alice")
        );
        assert_eq!(ledger.get(&id(2)).unwrap().supplier, None);
    }

    #[test]
    fn persistent_ledger_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            for n in [5u8, 2, 9] {
                ledger
                    .open(Redemption::new(
                        id(n),
                        1,
                        "stratum+tcp://p:1".into(),
                        format!("w{n}"),
                    ))
                    .unwrap();
            }
            ledger.credit_share(&id(5), 1.0).unwrap();
            ledger.credit_share(&id(5), 3.0).unwrap();
            ledger.credit_share(&id(2), 1e9).unwrap(); // completes
            ledger.credit_share(&id(2), 1e9).unwrap(); // ignored
        }
        // Torn last line, as from a kill mid-write.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(br#"{"op":"credit","id":"09"#).unwrap();
        drop(f);

        let ledger = RedemptionLedger::persistent(&path).unwrap();
        let ids: Vec<_> = ledger.active().iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![id(5), id(9)], "order kept, completed skipped");
        let r5 = ledger.get(&id(5)).unwrap();
        assert_eq!(r5.accumulated_hashes, 4 * HASHES_PER_DIFF_ONE);
        assert_eq!(r5.state, RedemptionState::Active);
        assert_eq!(r5.worker_name, "w5");
        assert_eq!(
            ledger.get(&id(2)).unwrap().state,
            RedemptionState::Completed
        );
        assert_eq!(ledger.get(&id(9)).unwrap().state, RedemptionState::Pending);

        // Keeps journaling after restore, and a third start sees it.
        let done = ledger.credit_share_outcome(&id(5), 1e9).unwrap();
        assert!(done.newly_completed);
        drop(ledger);
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        assert_eq!(ledger.active().len(), 1);
        assert!(ledger.get(&id(5)).unwrap().is_complete());
    }

    #[test]
    fn settle_charges_delivered_work_rounded_up_above_the_floor() {
        let ledger = RedemptionLedger::new();
        let open = |n: u8, amount: u64, floor: u64| {
            ledger
                .open(Redemption::new(id(n), amount, "p".into(), "w".into()).with_floor(floor))
                .unwrap()
        };
        // 10 THH, floor 1, 3.5 THH delivered: spent 4, change 6.
        open(1, 10, 1);
        ledger
            .lock()
            .get_mut(&id(1))
            .unwrap()
            .credit_hashes(HASHES_PER_THH * 7 / 2);
        let s = ledger.settle(&id(1)).unwrap();
        assert_eq!(
            s,
            SettleOutcome {
                state: RedemptionState::Settled,
                newly_settled: true,
                spent_thh: 4,
                change_thh: 6,
            }
        );
        // Nothing delivered: charged the floor.
        open(2, 10, 1);
        assert_eq!(ledger.settle(&id(2)).unwrap().change_thh, 9);
        // No reserve (floor = amount): settles, but nothing comes back.
        open(3, 10, 10);
        assert_eq!(ledger.settle(&id(3)).unwrap().change_thh, 0);
        // Floor above amount is clamped.
        assert_eq!(
            Redemption::new(id(4), 2, "p".into(), "w".into())
                .with_floor(5)
                .floor_thh,
            2
        );
    }

    #[test]
    fn settled_redemption_is_terminal() {
        let ledger = RedemptionLedger::new();
        ledger
            .open(Redemption::new(id(1), 2, "p".into(), "w".into()).with_floor(1))
            .unwrap();
        ledger.credit_share(&id(1), 1.0).unwrap();
        assert!(ledger.settle(&id(1)).unwrap().newly_settled);
        // Late shares are ignored, so the charge can't move after settle.
        let late = ledger.credit_share_outcome(&id(1), 1e9).unwrap();
        assert_eq!(late.state, RedemptionState::Settled);
        assert!(!late.newly_completed);
        assert_eq!(late.accumulated_hashes, HASHES_PER_DIFF_ONE);
        assert!(ledger.active().is_empty(), "settled is not routed");
        // A second settle is a no-op.
        let again = ledger.settle(&id(1)).unwrap();
        assert!(!again.newly_settled);
        assert_eq!((again.spent_thh, again.change_thh), (1, 1));
        // Settling a completed redemption leaves it completed, no change.
        ledger
            .open(Redemption::new(id(2), 1, "p".into(), "w".into()).with_floor(0))
            .unwrap();
        ledger.credit_share(&id(2), 1e9).unwrap();
        let done = ledger.settle(&id(2)).unwrap();
        assert_eq!(done.state, RedemptionState::Completed);
        assert_eq!(done.change_thh, 0);
        assert_eq!(ledger.settle(&id(9)), Err(LedgerError::Unknown(id(9))));
    }

    #[test]
    fn settle_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            ledger
                .open(Redemption::new(id(1), 8, "p".into(), "w".into()).with_floor(1))
                .unwrap();
            ledger.lock().get_mut(&id(1)).unwrap().state = RedemptionState::Active;
            ledger.credit_share(&id(1), 2_000_000.0).unwrap();
            ledger.settle(&id(1)).unwrap();
            ledger.credit_share(&id(1), 1e9).unwrap(); // ignored, not journaled
        }
        for _ in 0..2 {
            // Twice: once from the raw journal, once from the compacted one.
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            let r = ledger.get(&id(1)).unwrap();
            assert_eq!(r.state, RedemptionState::Settled);
            assert_eq!(r.floor_thh, 1);
            assert_eq!(r.accumulated_hashes, hashes_for_difficulty(2_000_000.0));
            assert_eq!((r.spent_thh(), r.change_thh()), (3, 5));
        }
    }

    #[test]
    fn journal_from_before_settling_has_no_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        let line = format!(
            r#"{{"op":"open","id":"{}","amount_thh":4,"pool_url":"p","worker_name":"w"}}"#,
            id(1)
        );
        std::fs::write(&path, line + "\n").unwrap();
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        assert_eq!(ledger.get(&id(1)).unwrap().floor_thh, 4);
        assert_eq!(ledger.settle(&id(1)).unwrap().change_thh, 0);
    }

    #[test]
    fn redemption_id_round_trips_through_json() {
        let json = serde_json::to_string(&id(0xab)).unwrap();
        assert_eq!(json, format!("\"{}\"", "ab".repeat(16)));
        assert_eq!(
            serde_json::from_str::<RedemptionId>(&json).unwrap(),
            id(0xab)
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

    #[test]
    fn settle_preview_says_what_a_stop_returns() {
        // 1 THH at a 1 THH floor: a stop can never return anything.
        let r = Redemption::new(id(1), 1, "p".into(), "w".into()).with_floor(1);
        assert_eq!(r.settle_preview(), Some((1, 0)));
        // 10 THH, floor 1, 0.5 delivered: charged the floor, 9 back.
        let mut r = Redemption::new(id(2), 10, "p".into(), "w".into()).with_floor(1);
        r.credit_hashes(HASHES_PER_THH / 2);
        assert_eq!(r.settle_preview(), Some((1, 9)));
        // 9.2 delivered rounds up to all 10: nothing back.
        r.credit_hashes(HASHES_PER_THH * 87 / 10);
        assert_eq!(r.settle_preview(), Some((10, 0)));
        // Finished: no preview.
        r.settle();
        assert_eq!(r.settle_preview(), None);
    }
}
