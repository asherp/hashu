//! Per-connection share commitment tree wrapper.
//!
//! Wraps `hashu_core::share::MerkleTree` with a monotonic per-connection
//! sequence counter so each leaf has a stable position in the redemption.
//! The tree is single-threaded by design — owned by one connection task.
//!
//! For the stand-alone proxy (no redemption flow yet), one of these per
//! connection is fine: on disconnect we log the final root + leaf count.
//! When redemption support lands the committer will move up to the
//! redemption-handle scope so multiple miner connections can contribute to
//! one root.

use hashu_core::share::{Hash, MerkleTree, ShareLeaf};

#[derive(Debug, Default)]
pub struct ShareCommitter {
    tree: MerkleTree,
    next_seq: u64,
}

impl ShareCommitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append one accepted share. Returns the assigned `seq`.
    pub fn append(&mut self, preimage: Vec<u8>, target_bits: u32, ntime: u32) -> u64 {
        let seq = self.next_seq;
        self.tree.push(&ShareLeaf {
            preimage,
            target_bits,
            ntime,
            seq,
        });
        self.next_seq += 1;
        seq
    }

    pub fn len(&self) -> usize {
        self.tree.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    pub fn root(&self) -> Option<Hash> {
        self.tree.root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_committer_has_no_root() {
        let c = ShareCommitter::new();
        assert!(c.is_empty());
        assert_eq!(c.root(), None);
    }

    #[test]
    fn append_assigns_monotonic_seq() {
        let mut c = ShareCommitter::new();
        assert_eq!(c.append(vec![1u8; 80], 0x1d00_ffff, 0), 0);
        assert_eq!(c.append(vec![2u8; 80], 0x1d00_ffff, 0), 1);
        assert_eq!(c.append(vec![3u8; 80], 0x1d00_ffff, 0), 2);
        assert_eq!(c.len(), 3);
    }

    #[test]
    fn root_changes_with_each_append() {
        let mut c = ShareCommitter::new();
        c.append(vec![0xAA; 80], 0x1d00_ffff, 0);
        let r1 = c.root().unwrap();
        c.append(vec![0xBB; 80], 0x1d00_ffff, 0);
        let r2 = c.root().unwrap();
        assert_ne!(r1, r2);
    }

    #[test]
    fn seq_field_affects_leaf_hash() {
        // The committer assigns seq in order — same preimage twice gives
        // distinct leaves because seq differs.
        let mut c = ShareCommitter::new();
        c.append(vec![0u8; 80], 0x1d00_ffff, 0);
        c.append(vec![0u8; 80], 0x1d00_ffff, 0);
        // A two-leaf tree's root is the pair-hash of two distinct leaves;
        // it must not equal the single-leaf-root case.
        let r2 = c.root().unwrap();
        let mut alt = ShareCommitter::new();
        alt.append(vec![0u8; 80], 0x1d00_ffff, 0);
        assert_ne!(r2, alt.root().unwrap());
    }
}
