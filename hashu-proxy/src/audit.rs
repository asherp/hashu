//! Per-redemption share commitment trees. ARCHITECTURE.md §4.7.2.
//!
//! Every share credited to a redemption is also appended, as a
//! [`ShareLeaf`], to that redemption's append-only Merkle tree. Leaves from
//! every miner connection serving the redemption land in one tree, in
//! credit order, with a per-redemption `seq`. The root commits to exactly
//! the work that filled the redemption, and [`ShareAudit::proof`] gives an
//! RFC 6962 inclusion proof for any leaf, together with the leaf's
//! contents so the redeemer can re-derive and re-hash it.
//!
//! Leaves carry PoW evidence only (block header, target, ntime, seq): no
//! pool URL or worker name. Kept in memory, and, when built with
//! [`ShareAudit::persistent`], journaled to disk one leaf per line so the
//! trees survive a restart alongside the ledger.
//!
//! A finished redemption's tree can be [evicted](ShareAudit::evict): only
//! its leaf count stays in memory, and [`ShareAudit::head`] and
//! [`ShareAudit::proof`] rebuild it from the journal on request. Without
//! this, memory grew with every share ever credited.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::Mutex;

use hashu_core::journal::Journal;
use hashu_core::redemption::RedemptionId;
use hashu_core::share::{Hash, InclusionProof, MerkleTree, ShareLeaf};
use serde::{Deserialize, Serialize};

/// One redemption's tree plus the leaves behind it.
#[derive(Debug, Default)]
struct RedemptionTree {
    tree: MerkleTree,
    leaves: Vec<ShareLeaf>,
}

/// Current root of one redemption's tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeHead {
    /// `None` until the first leaf is appended.
    pub root: Option<Hash>,
    pub leaf_count: u64,
}

/// Inclusion proof for one leaf against the current root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafProof {
    pub leaf: ShareLeaf,
    pub leaf_hash: Hash,
    pub root: Hash,
    pub proof: InclusionProof,
}

/// One journaled leaf. `seq` is implied by line order per redemption.
#[derive(Debug, Serialize, Deserialize)]
struct LeafRecord {
    id: RedemptionId,
    /// Hex.
    preimage: String,
    target_bits: u32,
    ntime: u32,
}

/// In-memory state of one redemption.
#[derive(Debug)]
enum Entry {
    /// Tree and leaves held in memory.
    Live(RedemptionTree),
    /// Evicted: only the leaf count is held; the leaves are in the journal.
    Evicted { leaf_count: u64 },
}

impl Entry {
    fn leaf_count(&self) -> u64 {
        match self {
            Entry::Live(t) => t.leaves.len() as u64,
            Entry::Evicted { leaf_count } => *leaf_count,
        }
    }
}

/// All redemptions' trees, shared by every connection and the control API.
#[derive(Debug, Default)]
pub struct ShareAudit {
    trees: Mutex<HashMap<RedemptionId, Entry>>,
    journal: Option<Journal>,
}

impl ShareAudit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trees backed by the journal at `path`: rebuilt from it now, and
    /// every later leaf appended to it.
    pub fn persistent(path: &Path) -> io::Result<Self> {
        Self::persistent_evicting(path, |_| false)
    }

    /// Like [`Self::persistent`], but redemptions for which `evicted`
    /// returns true (finished ones) are restored evicted: only their leaf
    /// count is kept in memory.
    pub fn persistent_evicting(
        path: &Path,
        evicted: impl Fn(&RedemptionId) -> bool,
    ) -> io::Result<Self> {
        let (journal, records, skipped) = Journal::open::<LeafRecord>(path)?;
        if skipped > 0 {
            tracing::warn!(?path, skipped, "skipped unreadable share audit lines");
        }
        let audit = Self::new();
        let (mut live, mut archived) = (0usize, 0usize);
        {
            let mut trees = audit.lock();
            for r in records {
                let Ok(preimage) = hex::decode(&r.preimage) else {
                    continue;
                };
                if !trees.contains_key(&r.id) && evicted(&r.id) {
                    trees.insert(r.id, Entry::Evicted { leaf_count: 0 });
                }
                push(&mut trees, r.id, preimage, r.target_bits, r.ntime);
                match trees.get(&r.id) {
                    Some(Entry::Evicted { .. }) => archived += 1,
                    _ => live += 1,
                }
            }
        }
        tracing::info!(?path, live, archived, "restored share audit trees");
        Ok(Self {
            journal: Some(journal),
            ..audit
        })
    }

    /// Append one credited share to `id`'s tree. `seq` is assigned here
    /// (0, 1, 2, … per redemption) and returned.
    pub fn append(&self, id: RedemptionId, preimage: Vec<u8>, target_bits: u32, ntime: u32) -> u64 {
        let mut trees = self.lock();
        if let Some(j) = &self.journal {
            // Written under the tree lock, so file order = seq order.
            let record = LeafRecord {
                id,
                preimage: hex::encode(&preimage),
                target_bits,
                ntime,
            };
            if let Err(e) = j.append(&record) {
                tracing::error!(error = %e, path = ?j.path(), "share audit journal write failed");
            }
        }
        push(&mut trees, id, preimage, target_bits, ntime)
    }

    /// Drop `id`'s tree and leaves from memory, keeping only its leaf
    /// count. Later [`Self::head`] and [`Self::proof`] calls rebuild it from
    /// the journal. Does nothing without a journal (the leaves would be
    /// lost) or for a redemption this audit has not seen. Returns true if a
    /// tree was evicted.
    pub fn evict(&self, id: &RedemptionId) -> bool {
        if self.journal.is_none() {
            return false;
        }
        let mut trees = self.lock();
        match trees.get_mut(id) {
            Some(e @ Entry::Live(_)) => {
                *e = Entry::Evicted {
                    leaf_count: e.leaf_count(),
                };
                true
            }
            _ => false,
        }
    }

    /// Redemptions whose trees are held in memory.
    pub fn live_ids(&self) -> Vec<RedemptionId> {
        self.lock()
            .iter()
            .filter(|(_, e)| matches!(e, Entry::Live(_)))
            .map(|(id, _)| *id)
            .collect()
    }

    /// Root and leaf count for `id`. A redemption with no credited shares
    /// yet (or one this proxy has never seen) has no root.
    pub fn head(&self, id: &RedemptionId) -> TreeHead {
        let empty = TreeHead {
            root: None,
            leaf_count: 0,
        };
        let head = |t: &RedemptionTree| TreeHead {
            root: t.tree.root(),
            leaf_count: t.leaves.len() as u64,
        };
        match self.lock().get(id) {
            Some(Entry::Live(t)) => return head(t),
            Some(Entry::Evicted { .. }) => {}
            None => return empty,
        }
        self.load(id).as_ref().map(head).unwrap_or(empty)
    }

    /// Inclusion proof for leaf `index` of `id`'s tree, against the
    /// current root. `None` if there is no such leaf.
    pub fn proof(&self, id: &RedemptionId, index: u64) -> Option<LeafProof> {
        let prove = |t: &RedemptionTree| {
            let leaf = t.leaves.get(usize::try_from(index).ok()?)?.clone();
            Some(LeafProof {
                leaf_hash: leaf.leaf_hash(),
                root: t.tree.root()?,
                proof: t.tree.inclusion_proof(index)?,
                leaf,
            })
        };
        match self.lock().get(id)? {
            Entry::Live(t) => return prove(t),
            Entry::Evicted { leaf_count } if index >= *leaf_count => return None,
            Entry::Evicted { .. } => {}
        }
        prove(&self.load(id)?)
    }

    /// Rebuild an evicted redemption's tree from the journal (not cached).
    fn load(&self, id: &RedemptionId) -> Option<RedemptionTree> {
        let path = self.journal.as_ref()?.path();
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!(error = %e, ?path, "share audit journal read failed");
                return None;
            }
        };
        let mut t = RedemptionTree::default();
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            let Ok(r) = serde_json::from_str::<LeafRecord>(&line) else {
                continue;
            };
            if r.id != *id {
                continue;
            }
            let Ok(preimage) = hex::decode(&r.preimage) else {
                continue;
            };
            t.push(preimage, r.target_bits, r.ntime);
        }
        Some(t)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RedemptionId, Entry>> {
        self.trees.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl RedemptionTree {
    fn push(&mut self, preimage: Vec<u8>, target_bits: u32, ntime: u32) -> u64 {
        let seq = self.leaves.len() as u64;
        let leaf = ShareLeaf {
            preimage,
            target_bits,
            ntime,
            seq,
        };
        self.tree.push(&leaf);
        self.leaves.push(leaf);
        seq
    }
}

fn push(
    trees: &mut HashMap<RedemptionId, Entry>,
    id: RedemptionId,
    preimage: Vec<u8>,
    target_bits: u32,
    ntime: u32,
) -> u64 {
    match trees
        .entry(id)
        .or_insert_with(|| Entry::Live(RedemptionTree::default()))
    {
        Entry::Live(t) => t.push(preimage, target_bits, ntime),
        // A late leaf for an evicted redemption: it is in the journal (the
        // caller wrote it first); only the count moves.
        Entry::Evicted { leaf_count } => {
            *leaf_count += 1;
            *leaf_count - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::share::verify_inclusion;

    fn id(n: u8) -> RedemptionId {
        RedemptionId::from_bytes([n; 16])
    }

    #[test]
    fn unknown_redemption_has_no_root() {
        let a = ShareAudit::new();
        assert_eq!(
            a.head(&id(1)),
            TreeHead {
                root: None,
                leaf_count: 0
            }
        );
        assert!(a.proof(&id(1), 0).is_none());
    }

    #[test]
    fn seq_is_per_redemption() {
        let a = ShareAudit::new();
        assert_eq!(a.append(id(1), vec![1; 80], 0x1d00_ffff, 10), 0);
        assert_eq!(a.append(id(2), vec![2; 80], 0x1d00_ffff, 10), 0);
        assert_eq!(a.append(id(1), vec![3; 80], 0x1d00_ffff, 11), 1);
        assert_eq!(a.head(&id(1)).leaf_count, 2);
        assert_eq!(a.head(&id(2)).leaf_count, 1);
        assert_ne!(a.head(&id(1)).root, a.head(&id(2)).root);
    }

    #[test]
    fn every_leaf_proves_against_the_current_root() {
        let a = ShareAudit::new();
        for i in 0..7u8 {
            a.append(id(1), vec![i; 80], 0x1d00_ffff, u32::from(i));
        }
        let root = a.head(&id(1)).root.unwrap();
        for i in 0..7 {
            let p = a.proof(&id(1), i).unwrap();
            assert_eq!(p.root, root);
            assert_eq!(p.leaf.seq, i);
            assert_eq!(p.leaf.leaf_hash(), p.leaf_hash);
            assert!(verify_inclusion(&p.leaf_hash, &p.proof, &root));
        }
        assert!(a.proof(&id(1), 7).is_none());
    }

    #[test]
    fn persistent_trees_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("share-audit.jsonl");
        let root = {
            let a = ShareAudit::persistent(&path).unwrap();
            for i in 0..5u8 {
                a.append(id(1), vec![i; 80], 0x1d00_ffff, u32::from(i));
            }
            a.append(id(2), vec![9; 80], 0x1d00_ffff, 1);
            a.head(&id(1)).root.unwrap()
        };
        let a = ShareAudit::persistent(&path).unwrap();
        assert_eq!(a.head(&id(1)).root, Some(root));
        assert_eq!(a.head(&id(1)).leaf_count, 5);
        assert_eq!(a.head(&id(2)).leaf_count, 1);
        // Appends continue the sequence.
        assert_eq!(a.append(id(1), vec![7; 80], 0x1d00_ffff, 7), 5);
        drop(a);
        let a = ShareAudit::persistent(&path).unwrap();
        let p = a.proof(&id(1), 5).unwrap();
        assert_eq!(p.leaf.seq, 5);
        assert!(verify_inclusion(&p.leaf_hash, &p.proof, &p.root));
    }

    #[test]
    fn evicted_tree_proves_from_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("share-audit.jsonl");
        let a = ShareAudit::persistent(&path).unwrap();
        for i in 0..6u8 {
            a.append(id(1), vec![i; 80], 0x1d00_ffff, u32::from(i));
        }
        a.append(id(2), vec![9; 80], 0x1d00_ffff, 1);
        let before = a.head(&id(1));
        let proof_before = a.proof(&id(1), 4).unwrap();

        assert!(a.evict(&id(1)));
        assert!(!a.evict(&id(1)), "already evicted");
        assert_eq!(a.live_ids(), vec![id(2)]);
        assert_eq!(a.head(&id(1)), before);
        assert_eq!(a.proof(&id(1), 4).unwrap(), proof_before);
        assert!(a.proof(&id(1), 6).is_none());

        // A late leaf keeps the sequence and lands in the rebuilt tree.
        assert_eq!(a.append(id(1), vec![7; 80], 0x1d00_ffff, 7), 6);
        let p = a.proof(&id(1), 6).unwrap();
        assert_eq!(p.leaf.seq, 6);
        assert!(verify_inclusion(&p.leaf_hash, &p.proof, &p.root));
        assert_eq!(a.head(&id(1)).leaf_count, 7);
    }

    #[test]
    fn restore_can_start_evicted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("share-audit.jsonl");
        let root = {
            let a = ShareAudit::persistent(&path).unwrap();
            for i in 0..3u8 {
                a.append(id(1), vec![i; 80], 0x1d00_ffff, u32::from(i));
            }
            a.append(id(2), vec![9; 80], 0x1d00_ffff, 1);
            a.head(&id(1)).root
        };
        let a = ShareAudit::persistent_evicting(&path, |r| *r == id(1)).unwrap();
        assert_eq!(a.live_ids(), vec![id(2)]);
        assert_eq!(a.head(&id(1)).root, root);
        assert_eq!(a.head(&id(1)).leaf_count, 3);
    }

    #[test]
    fn memory_only_audit_never_evicts() {
        let a = ShareAudit::new();
        a.append(id(1), vec![1; 80], 0x1d00_ffff, 1);
        assert!(!a.evict(&id(1)));
        assert_eq!(a.head(&id(1)).leaf_count, 1);
    }
}
