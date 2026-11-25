//! Redemption state machine. ARCHITECTURE.md §4.3, §5.1.
//!
//! A redemption is opened when a holder melts `TiH` vouchers with the
//! `hashrate` method. It accumulates hashes as the proxy reports accepted
//! shares, and completes once the accumulated hashes cover the amount
//! melted. Progress is kept in *hashes* (exact integers), not in floating
//! point TiH, so completion is decided without rounding drift.
//!
//! A redemption that stops receiving hashrate can instead be *settled*:
//! its delivered work is frozen, and the melt reports only that much as
//! spent, so cdk signs the redeemer's blank outputs (NUT-08) for the rest.
//! The part of a redemption that can come back this way is everything above
//! its [`Redemption::floor_tih`].

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::journal::Journal;

pub use crate::denom::HASHES_PER_TIH;

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

/// Who paused a redemption.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pauser {
    /// The holder of its stop key (the wallet that opened it).
    #[default]
    Redeemer,
    /// The mint operator.
    Operator,
}

impl Pauser {
    /// Lowercase name for status APIs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Redeemer => "redeemer",
            Self::Operator => "operator",
        }
    }
}

/// Most fallback pools a redemption may list after its primary pool.
pub const MAX_FALLBACK_POOLS: usize = 3;

/// A pool a redemption may send its hashrate to: the primary
/// ([`Redemption::pool_url`] and friends) or one of its
/// [`Redemption::fallbacks`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolTarget {
    /// Stratum URL, e.g. `stratum+tcp://pool.example:3333`.
    pub pool_url: String,
    /// Worker name this pool expects.
    pub worker_name: String,
    /// Password for `worker_name`; `None` sends the conventional `x`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_password: Option<String>,
}

impl PoolTarget {
    /// Password to send in `mining.authorize`.
    pub fn authorize_password(&self) -> &str {
        self.worker_password.as_deref().unwrap_or("x")
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
    /// Pools to try, in order, when the primary pool is unreachable or
    /// refuses the work. Shares on any of them credit this redemption.
    pub fallbacks: Vec<PoolTarget>,
    /// `TiH` melted.
    pub amount_tih: u64,
    /// `TiH` the redeemer pays even if the redemption settles early: the
    /// melt quote's amount, with the rest of `amount_tih` in its fee
    /// reserve. Equal to `amount_tih` for redemptions opened without a
    /// reserve, which can't return change.
    pub floor_tih: u64,
    /// `amount_tih × HASHES_PER_TIH`.
    pub target_hashes: u128,
    /// Hashes credited so far from accepted shares.
    pub accumulated_hashes: u128,
    pub state: RedemptionState,
    /// Supplier whose rigs fill this order (the mint it was bought from,
    /// e.g. `alice` for `alice.<domain>`). `None` = the operator's
    /// own mint: filled by the operator's rigs.
    pub supplier: Option<String>,
    /// Hex SHA-256 of the redeemer's stop secret, from the melt request.
    /// Whoever shows the secret may stop, pause and resume this
    /// redemption (see [`Self::stop_key_matches`]). `None`: only the
    /// operator can.
    pub stop_hash: Option<String>,
    /// Who paused delivery, if it is paused: routing skips it until
    /// resumed. It stays owed (and in [`RedemptionLedger::active`])
    /// meanwhile.
    pub paused_by: Option<Pauser>,
    /// Bumped each time the redeemer changes pools
    /// ([`RedemptionLedger::change_pools`]), so the router can forget what
    /// it learned about the old ones. Not journaled.
    pub pools_rev: u32,
    /// Hashes credited per pool (by URL and worker), in the order each
    /// pool first got work. Sums to [`Self::accumulated_hashes`].
    pub pool_hashes: Vec<PoolHashes>,
}

/// Work one pool received for a redemption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolHashes {
    pub pool_url: String,
    pub worker_name: String,
    pub hashes: u128,
}

impl PoolHashes {
    /// Whole `TiH` received, rounded down. See [`Redemption::pool_tih`]
    /// for figures that add up to the redemption's total.
    pub fn tih_floor(&self) -> u64 {
        u64::try_from(self.hashes / HASHES_PER_TIH).unwrap_or(u64::MAX)
    }
}

impl Redemption {
    pub fn new(id: RedemptionId, amount_tih: u64, pool_url: String, worker_name: String) -> Self {
        Self {
            id,
            pool_url,
            worker_name,
            worker_password: None,
            fallbacks: Vec::new(),
            supplier: None,
            stop_hash: None,
            paused_by: None,
            pools_rev: 0,
            pool_hashes: Vec::new(),
            amount_tih,
            floor_tih: amount_tih,
            target_hashes: u128::from(amount_tih) * HASHES_PER_TIH,
            accumulated_hashes: 0,
            state: RedemptionState::Pending,
        }
    }

    /// Set the worker password (`None` or empty: the default `x`).
    pub fn with_password(mut self, password: Option<String>) -> Self {
        self.worker_password = password.filter(|p| !p.is_empty());
        self
    }

    /// Set the fallback pools, tried in order after the primary. Entries
    /// with an empty URL or worker are dropped.
    pub fn with_fallbacks(mut self, fallbacks: Vec<PoolTarget>) -> Self {
        self.fallbacks = fallbacks
            .into_iter()
            .filter(|p| !p.pool_url.trim().is_empty() && !p.worker_name.trim().is_empty())
            .map(|mut p| {
                p.worker_password = p.worker_password.filter(|pw| !pw.is_empty());
                p
            })
            .collect();
        self
    }

    /// Every pool this redemption may use, primary first.
    pub fn pools(&self) -> Vec<PoolTarget> {
        std::iter::once(PoolTarget {
            pool_url: self.pool_url.clone(),
            worker_name: self.worker_name.clone(),
            worker_password: self.worker_password.clone(),
        })
        .chain(self.fallbacks.iter().cloned())
        .collect()
    }

    /// Set the supplier whose rigs fill it (see [`Redemption::supplier`]).
    pub fn with_supplier(mut self, supplier: Option<String>) -> Self {
        self.supplier = supplier.filter(|s| !s.is_empty());
        self
    }

    /// Set the redeemer's stop key (see [`Redemption::stop_hash`]).
    pub fn with_stop_hash(mut self, stop_hash: Option<String>) -> Self {
        self.stop_hash = stop_hash.filter(|h| !h.is_empty());
        self
    }

    /// Whether `secret` is the redeemer's stop secret: its SHA-256 is
    /// [`Self::stop_hash`]. Compares digests in constant time. False when
    /// the redemption has no stop key.
    pub fn stop_key_matches(&self, secret: &[u8]) -> bool {
        let Some(want) = &self.stop_hash else {
            return false;
        };
        stop_hash_matches(want, secret)
    }

    /// Point it at `primary` then `fallbacks` (same cleanup as
    /// [`Self::with_password`] and [`Self::with_fallbacks`]).
    fn set_pools(&mut self, primary: PoolTarget, fallbacks: Vec<PoolTarget>) {
        let next = Redemption::new(self.id, 0, primary.pool_url, primary.worker_name)
            .with_password(primary.worker_password)
            .with_fallbacks(fallbacks);
        self.pool_url = next.pool_url;
        self.worker_name = next.worker_name;
        self.worker_password = next.worker_password;
        self.fallbacks = next.fallbacks;
        self.pools_rev = self.pools_rev.wrapping_add(1);
    }

    /// Whether delivery is paused (by anyone).
    pub fn is_paused(&self) -> bool {
        self.paused_by.is_some()
    }

    /// Set the non-refundable part (clamped to `amount_tih`).
    pub fn with_floor(mut self, floor_tih: u64) -> Self {
        self.floor_tih = floor_tih.min(self.amount_tih);
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
        self.credit_hashes_on(hashes, 0)
    }

    /// Credit `hashes` mined for pool `pool` (index into [`Self::pools`];
    /// out of range counts as the primary).
    pub fn credit_hashes_on(&mut self, hashes: u128, pool: usize) -> RedemptionState {
        if self.state.is_finished() {
            return self.state;
        }
        let target = self.pools().into_iter().nth(pool).unwrap_or_else(|| PoolTarget {
            pool_url: self.pool_url.clone(),
            worker_name: self.worker_name.clone(),
            worker_password: None,
        });
        self.add_pool_hashes(&target.pool_url, &target.worker_name, hashes);
        self.accumulated_hashes = self.accumulated_hashes.saturating_add(hashes);
        self.refresh_state();
        self.state
    }

    /// Add to one pool's tally only.
    fn add_pool_hashes(&mut self, pool_url: &str, worker_name: &str, hashes: u128) {
        if hashes == 0 {
            return;
        }
        match self
            .pool_hashes
            .iter_mut()
            .find(|p| p.pool_url == pool_url && p.worker_name == worker_name)
        {
            Some(p) => p.hashes = p.hashes.saturating_add(hashes),
            None => self.pool_hashes.push(PoolHashes {
                pool_url: pool_url.to_string(),
                worker_name: worker_name.to_string(),
                hashes,
            }),
        }
    }

    fn refresh_state(&mut self) {
        if self.state.is_finished() {
            return;
        }
        self.state = if self.accumulated_hashes >= self.target_hashes {
            RedemptionState::Completed
        } else {
            RedemptionState::Active
        };
    }

    /// Whole `TiH` per pool, in [`Self::pool_hashes`] order, adding up to
    /// exactly [`Self::delivered_tih`]: each pool's share rounded down,
    /// then the units rounding leaves over go to the largest remainders.
    pub fn pool_tih(&self) -> Vec<(&PoolHashes, u64)> {
        let mut out: Vec<(&PoolHashes, u64)> =
            self.pool_hashes.iter().map(|p| (p, p.tih_floor())).collect();
        let sum: u64 = out.iter().map(|(_, t)| *t).sum();
        let mut left = self.delivered_tih().saturating_sub(sum);
        let mut by_rem: Vec<usize> = (0..out.len()).collect();
        by_rem.sort_by_key(|&i| std::cmp::Reverse(out[i].0.hashes % HASHES_PER_TIH));
        for i in by_rem {
            if left == 0 {
                break;
            }
            if out[i].0.hashes % HASHES_PER_TIH > 0 {
                out[i].1 += 1;
                left -= 1;
            }
        }
        out
    }

    pub fn is_complete(&self) -> bool {
        self.state == RedemptionState::Completed
    }

    /// Completed or settled.
    pub fn is_finished(&self) -> bool {
        self.state.is_finished()
    }

    /// Delivered work in whole `TiH`, rounded up: a started unit counts as
    /// spent.
    pub fn delivered_tih(&self) -> u64 {
        let whole = self.accumulated_hashes.div_ceil(HASHES_PER_TIH);
        u64::try_from(whole).unwrap_or(u64::MAX)
    }

    /// `TiH` the melt reports as spent. A settled redemption is charged for
    /// what it was delivered, but never less than its floor; anything else
    /// is charged the full amount.
    pub fn spent_tih(&self) -> u64 {
        match self.state {
            RedemptionState::Settled => self.delivered_tih().clamp(self.floor_tih, self.amount_tih),
            _ => self.amount_tih,
        }
    }

    /// `TiH` returned to the redeemer as change: `amount_tih − spent_tih`.
    pub fn change_tih(&self) -> u64 {
        self.amount_tih - self.spent_tih()
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
        let charged = self.delivered_tih().clamp(self.floor_tih, self.amount_tih);
        Some((charged, self.amount_tih - charged))
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

/// Whether hex `want` is the SHA-256 of `secret`, compared in constant
/// time so the timing says nothing about the secret.
pub fn stop_hash_matches(want: &str, secret: &[u8]) -> bool {
    use sha2::{Digest, Sha256};
    let got = hex::encode(Sha256::digest(secret));
    got.len() == want.len()
        && got
            .bytes()
            .zip(want.bytes())
            .fold(0, |a, (x, y)| a | (x ^ y))
            == 0
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
    #[error("the mint operator paused redemption {0}; only the operator can resume it")]
    PausedByOperator(RedemptionId),
}

/// Result of [`RedemptionLedger::settle`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettleOutcome {
    /// State after the call: `Settled`, or `Completed` if it had already
    /// reached target.
    pub state: RedemptionState,
    /// True only for the call that settled it.
    pub newly_settled: bool,
    pub spent_tih: u64,
    pub change_tih: u64,
}

/// One line of the ledger journal. Credits record hashes, not difficulty,
/// so replay is exact.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum LedgerRecord {
    Open {
        id: RedemptionId,
        amount_tih: u64,
        pool_url: String,
        worker_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_password: Option<String>,
        /// Absent in journals from before fallback pools: none.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fallbacks: Vec<PoolTarget>,
        /// Absent in journals from before settling existed: no reserve.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        floor_tih: Option<u64>,
        /// Absent in journals from before supplier mints: the operator's.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        supplier: Option<String>,
        /// Absent in journals from before redeemer stop keys: none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_hash: Option<String>,
    },
    Credit {
        id: RedemptionId,
        /// Decimal string: u128 does not fit a JSON number portably.
        hashes: String,
        /// Which of the redemption's pools (index into
        /// [`Redemption::pools`] as they stood then) got the share.
        /// Absent in journals from before per-pool totals: the primary.
        #[serde(default, skip_serializing_if = "is_zero")]
        pool: usize,
    },
    /// Hashes one pool got, written by compaction in place of the credits
    /// it sums. Counts toward the total like a credit.
    PoolCredit {
        id: RedemptionId,
        pool_url: String,
        worker_name: String,
        hashes: String,
    },
    /// Frozen at the hashes credited before this record.
    Settle { id: RedemptionId },
    /// Delivery paused, by the redeemer unless `by` says otherwise.
    Pause {
        id: RedemptionId,
        #[serde(default)]
        by: Pauser,
    },
    /// Delivery resumed.
    Resume { id: RedemptionId },
    /// The redeemer sent its hashrate to other pools.
    Pools {
        id: RedemptionId,
        pool_url: String,
        worker_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worker_password: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        fallbacks: Vec<PoolTarget>,
    },
}

fn is_zero(n: &usize) -> bool {
    *n == 0
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
    /// Which of each redemption's pools (0 = primary, then its fallbacks)
    /// is receiving its hashrate right now, as the router last reported.
    /// Not journaled.
    serving: Mutex<HashMap<RedemptionId, usize>>,
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
                    amount_tih,
                    pool_url,
                    worker_name,
                    worker_password,
                    fallbacks,
                    floor_tih,
                    supplier,
                    stop_hash,
                } => {
                    // A duplicate open can only come from a hand-edited
                    // file; keep the first.
                    let _ = ledger.open(
                        Redemption::new(id, amount_tih, pool_url, worker_name)
                            .with_password(worker_password)
                            .with_fallbacks(fallbacks)
                            .with_supplier(supplier)
                            .with_stop_hash(stop_hash)
                            .with_floor(floor_tih.unwrap_or(amount_tih)),
                    );
                }
                LedgerRecord::Pause { id, by } => {
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.paused_by = Some(by);
                    }
                }
                LedgerRecord::Resume { id } => {
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.paused_by = None;
                    }
                }
                LedgerRecord::Pools {
                    id,
                    pool_url,
                    worker_name,
                    worker_password,
                    fallbacks,
                } => {
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.set_pools(
                            PoolTarget {
                                pool_url,
                                worker_name,
                                worker_password,
                            },
                            fallbacks,
                        );
                    }
                }
                LedgerRecord::Settle { id } => {
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.settle();
                    }
                }
                LedgerRecord::Credit { id, hashes, pool } => {
                    let Ok(hashes) = hashes.parse::<u128>() else {
                        continue;
                    };
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.credit_hashes_on(hashes, pool);
                    }
                }
                LedgerRecord::PoolCredit {
                    id,
                    pool_url,
                    worker_name,
                    hashes,
                } => {
                    let Ok(hashes) = hashes.parse::<u128>() else {
                        continue;
                    };
                    // Compaction writes these before any settle record,
                    // and their sum is what was credited, so add them
                    // whole even past the target.
                    if let Some(r) = ledger.lock().get_mut(&id) {
                        r.add_pool_hashes(&pool_url, &worker_name, hashes);
                        r.accumulated_hashes = r.accumulated_hashes.saturating_add(hashes);
                        r.refresh_state();
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
                    amount_tih: r.amount_tih,
                    pool_url: r.pool_url.clone(),
                    worker_name: r.worker_name.clone(),
                    worker_password: r.worker_password.clone(),
                    fallbacks: r.fallbacks.clone(),
                    floor_tih: Some(r.floor_tih),
                    supplier: r.supplier.clone(),
                    stop_hash: r.stop_hash.clone(),
                };
                let credits = r.pool_hashes.iter().map(|p| LedgerRecord::PoolCredit {
                    id: r.id,
                    pool_url: p.pool_url.clone(),
                    worker_name: p.worker_name.clone(),
                    hashes: p.hashes.to_string(),
                });
                let settle = (r.state == RedemptionState::Settled)
                    .then_some(LedgerRecord::Settle { id: r.id });
                let pause = r.paused_by.map(|by| LedgerRecord::Pause { id: r.id, by });
                std::iter::once(open)
                    .chain(credits)
                    .chain(settle)
                    .chain(pause)
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
            amount_tih: redemption.amount_tih,
            pool_url: redemption.pool_url.clone(),
            worker_name: redemption.worker_name.clone(),
            worker_password: redemption.worker_password.clone(),
            fallbacks: redemption.fallbacks.clone(),
            floor_tih: Some(redemption.floor_tih),
            supplier: redemption.supplier.clone(),
            stop_hash: redemption.stop_hash.clone(),
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
        self.credit_share_on(id, difficulty, None)
    }

    /// Like [`Self::credit_share_outcome`], for a share sent to pool
    /// `pool` (index into [`Redemption::pools`]). `None`: the pool the
    /// router last reported serving it, else the primary.
    pub fn credit_share_on(
        &self,
        id: &RedemptionId,
        difficulty: f64,
        pool: Option<usize>,
    ) -> Result<CreditOutcome, LedgerError> {
        let pool = pool.or_else(|| self.serving_pool(id)).unwrap_or(0);
        let mut map = self.lock();
        let redemption = map.get_mut(id).ok_or(LedgerError::Unknown(*id))?;
        let was_finished = redemption.is_finished();
        let hashes = hashes_for_difficulty(difficulty);
        let state = redemption.credit_hashes_on(hashes, pool);
        if !was_finished && hashes > 0 {
            self.record(&LedgerRecord::Credit {
                id: *id,
                hashes: hashes.to_string(),
                pool,
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
                spent_tih = redemption.spent_tih(),
                change_tih = redemption.change_tih(),
                "redemption settled"
            );
        }
        Ok(SettleOutcome {
            state: redemption.state,
            newly_settled,
            spent_tih: redemption.spent_tih(),
            change_tih: redemption.change_tih(),
        })
    }

    /// Pause delivery to `id`, as `by`. A paused redemption keeps its
    /// place and what it is owed, but the router sends it no hashrate
    /// until it is resumed. An operator's pause takes over a redeemer's;
    /// a redeemer's pause leaves an operator's in place. Returns whether
    /// this call changed anything; finished redemptions are left as they
    /// are.
    pub fn pause(&self, id: &RedemptionId, by: Pauser) -> Result<bool, LedgerError> {
        let mut map = self.lock();
        let redemption = map.get_mut(id).ok_or(LedgerError::Unknown(*id))?;
        if redemption.is_finished()
            || redemption.paused_by == Some(by)
            || (by == Pauser::Redeemer && redemption.paused_by == Some(Pauser::Operator))
        {
            return Ok(false);
        }
        redemption.paused_by = Some(by);
        self.record(&LedgerRecord::Pause { id: *id, by });
        tracing::info!(redemption = %id, by = by.as_str(), "redemption paused");
        Ok(true)
    }

    /// Resume delivery to `id`, as `by`. The operator may resume any pause;
    /// the redeemer only their own ([`LedgerError::PausedByOperator`]
    /// otherwise). Returns whether this call changed anything.
    pub fn resume(&self, id: &RedemptionId, by: Pauser) -> Result<bool, LedgerError> {
        let mut map = self.lock();
        let redemption = map.get_mut(id).ok_or(LedgerError::Unknown(*id))?;
        match redemption.paused_by {
            None => return Ok(false),
            Some(Pauser::Operator) if by == Pauser::Redeemer => {
                return Err(LedgerError::PausedByOperator(*id))
            }
            Some(_) => {}
        }
        redemption.paused_by = None;
        self.record(&LedgerRecord::Resume { id: *id });
        tracing::info!(redemption = %id, by = by.as_str(), "redemption resumed");
        Ok(true)
    }

    /// Send `id`'s hashrate to `primary` (then `fallbacks`) from now on.
    /// Work already delivered stays credited; the router moves its rigs on
    /// its next pass. Returns false if it had finished.
    pub fn change_pools(
        &self,
        id: &RedemptionId,
        primary: PoolTarget,
        fallbacks: Vec<PoolTarget>,
    ) -> Result<bool, LedgerError> {
        let mut map = self.lock();
        let redemption = map.get_mut(id).ok_or(LedgerError::Unknown(*id))?;
        if redemption.is_finished() {
            return Ok(false);
        }
        redemption.set_pools(primary, fallbacks);
        self.record(&LedgerRecord::Pools {
            id: *id,
            pool_url: redemption.pool_url.clone(),
            worker_name: redemption.worker_name.clone(),
            worker_password: redemption.worker_password.clone(),
            fallbacks: redemption.fallbacks.clone(),
        });
        tracing::info!(redemption = %id, pool_url = %redemption.pool_url, "redemption pools changed");
        Ok(true)
    }

    /// Record which pool each redemption's rigs are on (index into
    /// [`Redemption::pools`]), replacing the previous report. Redemptions
    /// missing from `serving` have no rig right now.
    pub fn set_serving_pools(&self, serving: HashMap<RedemptionId, usize>) {
        *self.serving.lock().unwrap_or_else(|e| e.into_inner()) = serving;
    }

    /// The pool (index into [`Redemption::pools`]) `id`'s hashrate is
    /// going to, or `None` while no rig is on it.
    pub fn serving_pool(&self, id: &RedemptionId) -> Option<usize> {
        self.serving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .copied()
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
    fn a_difficulty_256_share_is_exactly_one_tih() {
        assert_eq!(hashes_for_difficulty(256.0), HASHES_PER_TIH);
        let mut r = Redemption::new(id(1), 2, "p".into(), "w".into());
        r.credit_share(256.0);
        r.credit_share(128.0);
        // 1.5 TiH delivered, carried exactly; settle rounds up to 2.
        assert_eq!(r.accumulated_hashes, HASHES_PER_TIH * 3 / 2);
        assert_eq!(r.delivered_tih(), 2);
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
        assert_eq!(r.target_hashes, 2 * HASHES_PER_TIH);
        assert_eq!(r.state, RedemptionState::Pending);
        assert_eq!(r.accumulated_hashes, 0);
    }

    #[test]
    fn credits_move_pending_to_active_then_completed() {
        // Target is 1 TiH = 2^40 hashes. A diff-1 share is 2^32 hashes
        // (stays Active); a diff-2^20 share adds 2^52 and crosses target.
        let mut r = Redemption::new(id(1), 1, "p".into(), "w".into());
        assert_eq!(r.credit_share(1.0), RedemptionState::Active);
        assert!(!r.is_complete());
        assert_eq!(r.credit_share(1_048_576.0), RedemptionState::Completed);
        assert!(r.is_complete());
    }

    #[test]
    fn completion_is_exact_at_the_boundary() {
        // Target 1 TiH. Crediting exactly target−1 hashes must not
        // complete; one more hash must.
        let mut r = Redemption::new(id(1), 1, "p".into(), "w".into());
        r.accumulated_hashes = HASHES_PER_TIH - 1;
        r.state = RedemptionState::Active;
        assert_eq!(r.accumulated_hashes + 1, r.target_hashes);
        // A zero-difficulty share adds nothing and does not complete.
        assert_eq!(r.credit_share(0.0), RedemptionState::Active);
        // Credit the final hash via a difficulty that rounds to exactly 1.
        r.accumulated_hashes = HASHES_PER_TIH - 1;
        r.target_hashes = HASHES_PER_TIH;
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
    fn stop_key_and_pause_survive_reopen() {
        use sha2::{Digest, Sha256};
        let secret = [9u8; 32];
        let hash = hex::encode(Sha256::digest(secret));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            ledger
                .open(
                    Redemption::new(id(1), 4, "stratum+tcp://p:1".into(), "w".into())
                        .with_stop_hash(Some(hash.clone())),
                )
                .unwrap();
            assert_eq!(ledger.pause(&id(1), Pauser::Redeemer), Ok(true));
            assert_eq!(ledger.pause(&id(1), Pauser::Redeemer), Ok(false));
            // Paused redemptions are still owed.
            assert_eq!(ledger.active().len(), 1);
        }
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            let r = ledger.get(&id(1)).unwrap();
            assert_eq!(r.paused_by, Some(Pauser::Redeemer));
            assert!(r.stop_key_matches(&secret));
            assert!(!r.stop_key_matches(&[8u8; 32]));
            // The operator's pause takes over; the redeemer can't lift it
            // and their own pause doesn't replace it.
            assert_eq!(ledger.pause(&id(1), Pauser::Operator), Ok(true));
            assert_eq!(ledger.pause(&id(1), Pauser::Redeemer), Ok(false));
            assert_eq!(
                ledger.resume(&id(1), Pauser::Redeemer),
                Err(LedgerError::PausedByOperator(id(1)))
            );
        }
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            let r = ledger.get(&id(1)).unwrap();
            assert_eq!(r.paused_by, Some(Pauser::Operator));
            assert_eq!(ledger.resume(&id(1), Pauser::Operator), Ok(true));
            assert_eq!(ledger.resume(&id(1), Pauser::Operator), Ok(false));
        }
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        assert!(!ledger.get(&id(1)).unwrap().is_paused());
        // New pools survive a reopen too (compaction folds them into the
        // open record).
        ledger
            .change_pools(
                &id(1),
                PoolTarget {
                    pool_url: "stratum+tcp://q:2".into(),
                    worker_name: "w2".into(),
                    worker_password: Some("".into()),
                },
                vec![],
            )
            .unwrap();
        drop(ledger);
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        let r = ledger.get(&id(1)).unwrap();
        assert_eq!(
            (r.pool_url.as_str(), r.worker_name.as_str()),
            ("stratum+tcp://q:2", "w2")
        );
        assert_eq!(r.worker_password, None);
        // A finished redemption can't be paused.
        ledger.settle(&id(1)).unwrap();
        assert_eq!(ledger.pause(&id(1), Pauser::Operator), Ok(false));
        // No stop key: no secret matches.
        assert!(!Redemption::new(id(2), 1, "p".into(), "w".into()).stop_key_matches(&secret));
    }

    #[test]
    fn hashes_are_totalled_per_pool_across_changes_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        let tih = HASHES_PER_TIH as f64 / HASHES_PER_DIFF_ONE as f64;
        let backup = PoolTarget {
            pool_url: "stratum+tcp://b:2".into(),
            worker_name: "wb".into(),
            worker_password: None,
        };
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            ledger
                .open(
                    Redemption::new(id(1), 100, "stratum+tcp://a:1".into(), "wa".into())
                        .with_fallbacks(vec![backup.clone()]),
                )
                .unwrap();
            // 3 TiH on the primary, 2.5 on its fallback.
            ledger.credit_share_on(&id(1), 3.0 * tih, Some(0)).unwrap();
            ledger.credit_share_on(&id(1), 2.5 * tih, Some(1)).unwrap();
            // The redeemer moves to pool c; 4 more TiH there.
            ledger
                .change_pools(
                    &id(1),
                    PoolTarget {
                        pool_url: "stratum+tcp://c:3".into(),
                        worker_name: "wc".into(),
                        worker_password: None,
                    },
                    vec![backup.clone()],
                )
                .unwrap();
            ledger.credit_share_on(&id(1), 4.0 * tih, Some(0)).unwrap();
            // Unknown pool index: the router's serving pool (none
            // reported here), else the primary.
            ledger.credit_share_on(&id(1), 0.5 * tih, None).unwrap();
        }
        let want = |r: &Redemption| {
            r.pool_tih()
                .iter()
                .map(|(p, t)| (p.pool_url.clone(), p.worker_name.clone(), *t))
                .collect::<Vec<_>>()
        };
        let expect = vec![
            ("stratum+tcp://a:1".to_string(), "wa".to_string(), 3),
            ("stratum+tcp://b:2".to_string(), "wb".to_string(), 3),
            ("stratum+tcp://c:3".to_string(), "wc".to_string(), 4),
        ];
        // Replayed from the full journal, then again from the compacted one.
        for _ in 0..2 {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            let r = ledger.get(&id(1)).unwrap();
            assert_eq!(r.delivered_tih(), 10);
            assert_eq!(want(&r), expect);
            let sum: u128 = r.pool_hashes.iter().map(|p| p.hashes).sum();
            assert_eq!(sum, r.accumulated_hashes);
        }
    }

    #[test]
    fn journal_from_before_pool_totals_credits_the_primary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        let rid = id(1).to_string();
        std::fs::write(
            &path,
            format!(
                "{{\"op\":\"open\",\"id\":\"{rid}\",\"amount_tih\":4,\"pool_url\":\"p\",\"worker_name\":\"w\"}}\n\
                 {{\"op\":\"credit\",\"id\":\"{rid}\",\"hashes\":\"{}\"}}\n",
                2 * HASHES_PER_TIH
            ),
        )
        .unwrap();
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        let r = ledger.get(&id(1)).unwrap();
        assert_eq!(r.pool_tih().len(), 1);
        assert_eq!(r.pool_tih()[0].1, 2);
        assert_eq!(r.pool_tih()[0].0.pool_url, "p");
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
        // 10 TiH, floor 1, 3.5 TiH delivered: spent 4, change 6.
        open(1, 10, 1);
        ledger
            .lock()
            .get_mut(&id(1))
            .unwrap()
            .credit_hashes(HASHES_PER_TIH * 7 / 2);
        let s = ledger.settle(&id(1)).unwrap();
        assert_eq!(
            s,
            SettleOutcome {
                state: RedemptionState::Settled,
                newly_settled: true,
                spent_tih: 4,
                change_tih: 6,
            }
        );
        // Nothing delivered: charged the floor.
        open(2, 10, 1);
        assert_eq!(ledger.settle(&id(2)).unwrap().change_tih, 9);
        // No reserve (floor = amount): settles, but nothing comes back.
        open(3, 10, 10);
        assert_eq!(ledger.settle(&id(3)).unwrap().change_tih, 0);
        // Floor above amount is clamped.
        assert_eq!(
            Redemption::new(id(4), 2, "p".into(), "w".into())
                .with_floor(5)
                .floor_tih,
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
        assert_eq!((again.spent_tih, again.change_tih), (1, 1));
        // Settling a completed redemption leaves it completed, no change.
        ledger
            .open(Redemption::new(id(2), 1, "p".into(), "w".into()).with_floor(0))
            .unwrap();
        ledger.credit_share(&id(2), 1e9).unwrap();
        let done = ledger.settle(&id(2)).unwrap();
        assert_eq!(done.state, RedemptionState::Completed);
        assert_eq!(done.change_tih, 0);
        assert_eq!(ledger.settle(&id(9)), Err(LedgerError::Unknown(id(9))));
    }

    #[test]
    fn settle_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            ledger
                .open(Redemption::new(id(1), 32_768, "p".into(), "w".into()).with_floor(1))
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
            assert_eq!(r.floor_tih, 1);
            assert_eq!(r.accumulated_hashes, hashes_for_difficulty(2_000_000.0));
            // 2M × 2^32 hashes = 7,812.5 TiH, charged 7,813.
            assert_eq!((r.spent_tih(), r.change_tih()), (7_813, 24_955));
        }
    }

    #[test]
    fn journal_from_before_settling_has_no_reserve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("redemptions.jsonl");
        let line = format!(
            r#"{{"op":"open","id":"{}","amount_tih":4,"pool_url":"p","worker_name":"w"}}"#,
            id(1)
        );
        std::fs::write(&path, line + "\n").unwrap();
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        assert_eq!(ledger.get(&id(1)).unwrap().floor_tih, 4);
        assert_eq!(ledger.settle(&id(1)).unwrap().change_tih, 0);
    }

    #[test]
    fn fallback_pools_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        let fallback = PoolTarget {
            pool_url: "stratum+tcp://backup:3333".into(),
            worker_name: "me.backup".into(),
            worker_password: Some("pw".into()),
        };
        {
            let ledger = RedemptionLedger::persistent(&path).unwrap();
            let r = Redemption::new(id(1), 8, "stratum+tcp://main:3333".into(), "me".into())
                .with_fallbacks(vec![
                    fallback.clone(),
                    PoolTarget {
                        pool_url: "".into(),
                        worker_name: "dropped".into(),
                        worker_password: None,
                    },
                ]);
            ledger.open(r).unwrap();
        }
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        let r = ledger.get(&id(1)).unwrap();
        assert_eq!(r.fallbacks, vec![fallback.clone()]);
        let pools = r.pools();
        assert_eq!(pools.len(), 2);
        assert_eq!(pools[0].pool_url, "stratum+tcp://main:3333");
        assert_eq!(pools[0].authorize_password(), "x");
        assert_eq!(pools[1], fallback);
    }

    #[test]
    fn journal_from_before_fallbacks_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        std::fs::write(
            &path,
            format!(
                r#"{{"op":"open","id":"{}","amount_tih":4,"pool_url":"p","worker_name":"w"}}"#,
                id(2)
            ) + "\n",
        )
        .unwrap();
        let ledger = RedemptionLedger::persistent(&path).unwrap();
        assert!(ledger.get(&id(2)).unwrap().fallbacks.is_empty());
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
        // 1 TiH at a 1 TiH floor: a stop can never return anything.
        let r = Redemption::new(id(1), 1, "p".into(), "w".into()).with_floor(1);
        assert_eq!(r.settle_preview(), Some((1, 0)));
        // 10 TiH, floor 1, 0.5 delivered: charged the floor, 9 back.
        let mut r = Redemption::new(id(2), 10, "p".into(), "w".into()).with_floor(1);
        r.credit_hashes(HASHES_PER_TIH / 2);
        assert_eq!(r.settle_preview(), Some((1, 9)));
        // 9.2 delivered rounds up to all 10: nothing back.
        r.credit_hashes(HASHES_PER_TIH * 87 / 10);
        assert_eq!(r.settle_preview(), Some((10, 0)));
        // Finished: no preview.
        r.settle();
        assert_eq!(r.settle_preview(), None);
    }
}
