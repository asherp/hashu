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

use std::collections::HashMap;
use std::io;
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

/// All redemptions' trees, shared by every connection and the control API.
#[derive(Debug, Default)]
pub struct ShareAudit {
    trees: Mutex<HashMap<RedemptionId, RedemptionTree>>,
    journal: Option<Journal>,
}

impl ShareAudit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trees backed by the journal at `path`: rebuilt from it now, and
    /// every later leaf appended to it.
    pub fn persistent(path: &Path) -> io::Result<Self> {
        let (journal, records, skipped) = Journal::open::<LeafRecord>(path)?;
        if skipped > 0 {
            tracing::warn!(?path, skipped, "skipped unreadable share audit lines");
        }
        let audit = Self::new();
        let mut leaves = 0usize;
        for r in records {
            let Ok(preimage) = hex::decode(&r.preimage) else {
                continue;
            };
            audit.push(r.id, preimage, r.target_bits, r.ntime);
            leaves += 1;
        }
        tracing::info!(?path, leaves, "restored share audit trees");
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

    fn push(&self, id: RedemptionId, preimage: Vec<u8>, target_bits: u32, ntime: u32) -> u64 {
        push(&mut self.lock(), id, preimage, target_bits, ntime)
    }

    /// Root and leaf count for `id`. A redemption with no credited shares
    /// yet (or one this proxy has never seen) has no root.
    pub fn head(&self, id: &RedemptionId) -> TreeHead {
        let trees = self.lock();
        match trees.get(id) {
            Some(t) => TreeHead {
                root: t.tree.root(),
                leaf_count: t.leaves.len() as u64,
            },
            None => TreeHead {
                root: None,
                leaf_count: 0,
            },
        }
    }

    /// Inclusion proof for leaf `index` of `id`'s tree, against the
    /// current root. `None` if there is no such leaf.
    pub fn proof(&self, id: &RedemptionId, index: u64) -> Option<LeafProof> {
        let trees = self.lock();
        let t = trees.get(id)?;
        let leaf = t.leaves.get(usize::try_from(index).ok()?)?.clone();
        Some(LeafProof {
            leaf_hash: leaf.leaf_hash(),
            root: t.tree.root()?,
            proof: t.tree.inclusion_proof(index)?,
            leaf,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<RedemptionId, RedemptionTree>> {
        self.trees.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn push(
    trees: &mut HashMap<RedemptionId, RedemptionTree>,
    id: RedemptionId,
    preimage: Vec<u8>,
    target_bits: u32,
    ntime: u32,
) -> u64 {
    let t = trees.entry(id).or_default();
    let seq = t.leaves.len() as u64;
    let leaf = ShareLeaf {
        preimage,
        target_bits,
        ntime,
        seq,
    };
    t.tree.push(&leaf);
    t.leaves.push(leaf);
    seq
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
}
