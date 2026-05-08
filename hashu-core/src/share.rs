//! Share commitment tree — append-only Merkle tree of PoW share leaves.
//! ARCHITECTURE.md §4.7.2.
//!
//! Leaf format: SHA256(share_preimage || target_bits || ntime || seq).
//! No pool URL or worker info ever enters a leaf.

// TODO: ShareLeaf { preimage: Vec<u8>, target_bits: u32, ntime: u32, seq: u64 }
// TODO: append-only MerkleTree (binary, SHA-256), incremental root.
// TODO: inclusion-proof generator for selective disclosure to redeemer.
