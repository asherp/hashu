//! Share commitment tree — append-only Merkle tree of PoW share leaves.
//! ARCHITECTURE.md §4.7.2.
//!
//! Leaf format: `SHA256(u32_be(len(preimage)) || preimage || target_bits ||
//! ntime || seq)`. Integer fields are big-endian. The u32 length prefix on
//! `preimage` removes ambiguity at the variable/fixed-width boundary. No pool
//! URL or worker info ever enters a leaf.
//!
//! Inner nodes: `SHA256(left || right)`. Odd nodes at any level are promoted
//! unchanged to the next level (RFC 6962 / Certificate Transparency style),
//! avoiding the duplicate-last-leaf second-preimage issue from Bitcoin's
//! scheme.
//!
//! Inclusion proofs follow RFC 6962 §2.1; verification uses the audit-path
//! algorithm from §2.1.1.

use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

/// PoW evidence for a single accepted share. Carries no routing information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShareLeaf {
    /// Bytes that hash to satisfy the share difficulty (e.g. block header).
    pub preimage: Vec<u8>,
    /// Compact-encoded difficulty target the share was claimed against.
    pub target_bits: u32,
    /// `ntime` from the share header.
    pub ntime: u32,
    /// Monotonic per-redemption sequence number.
    pub seq: u64,
}

impl ShareLeaf {
    pub fn leaf_hash(&self) -> Hash {
        let mut h = Sha256::new();
        h.update((self.preimage.len() as u32).to_be_bytes());
        h.update(&self.preimage);
        h.update(self.target_bits.to_be_bytes());
        h.update(self.ntime.to_be_bytes());
        h.update(self.seq.to_be_bytes());
        h.finalize().into()
    }
}

fn hash_pair(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Append-only Merkle tree over share-leaf hashes.
#[derive(Clone, Debug, Default)]
pub struct MerkleTree {
    leaves: Vec<Hash>,
}

impl MerkleTree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.leaves.len()
    }

    pub fn is_empty(&self) -> bool {
        self.leaves.is_empty()
    }

    /// Append a share. Returns the leaf index assigned.
    pub fn push(&mut self, leaf: &ShareLeaf) -> u64 {
        self.leaves.push(leaf.leaf_hash());
        (self.leaves.len() - 1) as u64
    }

    /// Append a precomputed leaf hash (for re-import or testing).
    pub fn push_hash(&mut self, h: Hash) -> u64 {
        self.leaves.push(h);
        (self.leaves.len() - 1) as u64
    }

    pub fn leaf_at(&self, idx: usize) -> Option<&Hash> {
        self.leaves.get(idx)
    }

    /// Root of the current tree, or `None` if empty.
    pub fn root(&self) -> Option<Hash> {
        if self.leaves.is_empty() {
            return None;
        }
        Some(merkle_tree_hash(&self.leaves))
    }

    /// Inclusion proof for the leaf at `idx` against the current tree state.
    pub fn inclusion_proof(&self, idx: u64) -> Option<InclusionProof> {
        if idx as usize >= self.leaves.len() {
            return None;
        }
        Some(InclusionProof {
            leaf_index: idx,
            tree_size: self.leaves.len() as u64,
            siblings: audit_path(idx as usize, &self.leaves),
        })
    }
}

fn merkle_tree_hash(layer: &[Hash]) -> Hash {
    debug_assert!(!layer.is_empty());
    if layer.len() == 1 {
        return layer[0];
    }
    let k = largest_pow2_less_than(layer.len());
    hash_pair(
        &merkle_tree_hash(&layer[..k]),
        &merkle_tree_hash(&layer[k..]),
    )
}

fn audit_path(idx: usize, layer: &[Hash]) -> Vec<Hash> {
    if layer.len() == 1 {
        return Vec::new();
    }
    let k = largest_pow2_less_than(layer.len());
    if idx < k {
        let mut p = audit_path(idx, &layer[..k]);
        p.push(merkle_tree_hash(&layer[k..]));
        p
    } else {
        let mut p = audit_path(idx - k, &layer[k..]);
        p.push(merkle_tree_hash(&layer[..k]));
        p
    }
}

fn largest_pow2_less_than(n: usize) -> usize {
    debug_assert!(n >= 2);
    let mut k: usize = 1;
    while k.saturating_mul(2) < n {
        k *= 2;
    }
    k
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InclusionProof {
    pub leaf_index: u64,
    pub tree_size: u64,
    pub siblings: Vec<Hash>,
}

impl InclusionProof {
    pub fn verify(&self, leaf_hash: &Hash, expected_root: &Hash) -> bool {
        verify_inclusion(leaf_hash, self, expected_root)
    }
}

/// RFC 6962 §2.1.1 audit-path verification.
pub fn verify_inclusion(
    leaf_hash: &Hash,
    proof: &InclusionProof,
    expected_root: &Hash,
) -> bool {
    if proof.tree_size == 0 || proof.leaf_index >= proof.tree_size {
        return false;
    }

    // Single-leaf tree: the leaf is the root and the proof is empty.
    if proof.tree_size == 1 {
        return proof.siblings.is_empty() && leaf_hash == expected_root;
    }

    let mut fn_ = proof.leaf_index;
    let mut sn = proof.tree_size - 1;
    let mut r = *leaf_hash;

    for sibling in &proof.siblings {
        if sn == 0 {
            return false; // proof too long
        }
        if (fn_ & 1) == 1 || fn_ == sn {
            r = hash_pair(sibling, &r);
            // If the node was at the rightmost edge but not on an odd index,
            // it was promoted up some levels — collapse them.
            while (fn_ & 1) == 0 && fn_ != 0 {
                fn_ >>= 1;
                sn >>= 1;
            }
        } else {
            r = hash_pair(&r, sibling);
        }
        fn_ >>= 1;
        sn >>= 1;
    }

    sn == 0 && r == *expected_root
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_leaf(seq: u64) -> ShareLeaf {
        ShareLeaf {
            preimage: vec![seq as u8; 80],
            target_bits: 0x1d00ffff,
            ntime: 1_700_000_000 + seq as u32,
            seq,
        }
    }

    #[test]
    fn leaf_hash_is_deterministic() {
        let l = mk_leaf(7);
        assert_eq!(l.leaf_hash(), l.clone().leaf_hash());
    }

    #[test]
    fn leaf_hash_changes_with_each_field() {
        let base = mk_leaf(0);
        let h0 = base.leaf_hash();

        let mut a = base.clone();
        a.preimage[0] ^= 1;
        assert_ne!(a.leaf_hash(), h0);

        let mut b = base.clone();
        b.target_bits ^= 1;
        assert_ne!(b.leaf_hash(), h0);

        let mut c = base.clone();
        c.ntime ^= 1;
        assert_ne!(c.leaf_hash(), h0);

        let mut d = base.clone();
        d.seq ^= 1;
        assert_ne!(d.leaf_hash(), h0);
    }

    #[test]
    fn empty_tree_has_no_root() {
        let t = MerkleTree::new();
        assert!(t.is_empty());
        assert_eq!(t.root(), None);
        assert!(t.inclusion_proof(0).is_none());
    }

    #[test]
    fn single_leaf_is_root() {
        let mut t = MerkleTree::new();
        let l = mk_leaf(0);
        t.push(&l);
        assert_eq!(t.root(), Some(l.leaf_hash()));

        let proof = t.inclusion_proof(0).unwrap();
        assert!(proof.siblings.is_empty());
        assert!(proof.verify(&l.leaf_hash(), &t.root().unwrap()));
    }

    #[test]
    fn two_leaves_root_is_pair_hash() {
        let mut t = MerkleTree::new();
        let a = mk_leaf(0);
        let b = mk_leaf(1);
        t.push(&a);
        t.push(&b);
        let expected = hash_pair(&a.leaf_hash(), &b.leaf_hash());
        assert_eq!(t.root(), Some(expected));
    }

    #[test]
    fn all_leaves_verify_for_various_sizes() {
        // Cover power-of-two and odd sizes, including the RFC 6962 example
        // shape (n=5) where promotion kicks in.
        for n in [1u64, 2, 3, 4, 5, 7, 8, 13, 16, 33, 100] {
            let mut t = MerkleTree::new();
            let leaves: Vec<ShareLeaf> = (0..n).map(mk_leaf).collect();
            for l in &leaves {
                t.push(l);
            }
            let root = t.root().unwrap();
            for (i, l) in leaves.iter().enumerate() {
                let proof = t.inclusion_proof(i as u64).unwrap();
                assert!(
                    proof.verify(&l.leaf_hash(), &root),
                    "leaf {i} of {n} failed to verify",
                );
            }
        }
    }

    #[test]
    fn proof_fails_with_wrong_leaf() {
        let mut t = MerkleTree::new();
        for i in 0..10 {
            t.push(&mk_leaf(i));
        }
        let root = t.root().unwrap();
        let proof = t.inclusion_proof(3).unwrap();

        let wrong = mk_leaf(99).leaf_hash();
        assert!(!proof.verify(&wrong, &root));
    }

    #[test]
    fn proof_fails_with_wrong_root() {
        let mut t = MerkleTree::new();
        for i in 0..10 {
            t.push(&mk_leaf(i));
        }
        let proof = t.inclusion_proof(3).unwrap();
        let leaf3 = mk_leaf(3).leaf_hash();

        let mut bad_root = t.root().unwrap();
        bad_root[0] ^= 1;
        assert!(!proof.verify(&leaf3, &bad_root));
    }

    #[test]
    fn proof_fails_with_tampered_sibling() {
        let mut t = MerkleTree::new();
        for i in 0..10 {
            t.push(&mk_leaf(i));
        }
        let root = t.root().unwrap();
        let mut proof = t.inclusion_proof(3).unwrap();
        let leaf3 = mk_leaf(3).leaf_hash();

        proof.siblings[0][0] ^= 1;
        assert!(!proof.verify(&leaf3, &root));
    }

    #[test]
    fn proof_fails_with_wrong_index() {
        let mut t = MerkleTree::new();
        for i in 0..8 {
            t.push(&mk_leaf(i));
        }
        let root = t.root().unwrap();
        let mut proof = t.inclusion_proof(2).unwrap();
        let leaf2 = mk_leaf(2).leaf_hash();
        assert!(proof.verify(&leaf2, &root));

        proof.leaf_index = 5;
        assert!(!proof.verify(&leaf2, &root));
    }

    #[test]
    fn append_does_not_mutate_earlier_leaves() {
        let mut t = MerkleTree::new();
        for i in 0..5 {
            t.push(&mk_leaf(i));
        }
        let root_before = t.root().unwrap();

        for i in 5..10 {
            t.push(&mk_leaf(i));
        }
        // Earlier leaves are byte-identical.
        for i in 0..5 {
            assert_eq!(*t.leaf_at(i).unwrap(), mk_leaf(i as u64).leaf_hash());
        }
        // New root differs.
        assert_ne!(t.root().unwrap(), root_before);
    }

    #[test]
    fn rfc6962_largest_pow2() {
        assert_eq!(largest_pow2_less_than(2), 1);
        assert_eq!(largest_pow2_less_than(3), 2);
        assert_eq!(largest_pow2_less_than(4), 2);
        assert_eq!(largest_pow2_less_than(5), 4);
        assert_eq!(largest_pow2_less_than(8), 4);
        assert_eq!(largest_pow2_less_than(9), 8);
    }
}
