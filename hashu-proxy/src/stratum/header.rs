//! Stratum V1 ↔ Bitcoin block header reconstruction.
//!
//! Builds an 80-byte block header from a `mining.notify` job, the
//! per-connection extranonce1, and a `mining.submit`'s extranonce2 / ntime /
//! nonce / version-mask. The double-SHA256 of the result is what the share
//! difficulty test runs over, so byte order matters down to the bit.
//!
//! ## Stratum V1 wire conventions
//!
//! These follow slush0's spec and the canonical cgminer reference; pools
//! that diverge from them won't interop with stock miners.
//!
//! - **`prevhash`** in `mining.notify`: 32 bytes of hex, with each 4-byte
//!   word reversed relative to the natural SHA256 internal-byte-order form.
//!   To rebuild the header, hex-decode then reverse each 4-byte chunk.
//! - **`version`, `nbits`, `ntime`** in `mining.notify`, and **`ntime`,
//!   `nonce`, `version_mask`** in `mining.submit`: hex-encoded big-endian
//!   `u32`. The block header stores them little-endian, so we parse to
//!   `u32` and write `to_le_bytes()`.
//! - **`extranonce1`, `extranonce2`, `coinb1`, `coinb2`**: raw hex bytes
//!   that drop straight into the coinbase tx. No swapping.
//! - **`merkle_branch`**: each entry is 32 bytes in natural double-SHA256
//!   internal-byte-order. They fold into the merkle root with no swapping.
//! - **AsicBoost (BIP310)**: when present, `version_actual = job_version
//!   ^ version_mask`. The masked-bits set must be a subset of the version
//!   bits the pool advertised in `mining.configure`; we don't enforce that
//!   here, just compute the XOR.

use sha2::{Digest, Sha256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum HeaderError {
    #[error("hex decode failed for {field}")]
    Hex {
        field: &'static str,
        #[source]
        source: hex::FromHexError,
    },
    #[error("invalid byte length for {field}: expected {expected}, got {got}")]
    Length {
        field: &'static str,
        expected: usize,
        got: usize,
    },
    #[error("u32 hex parse failed for {field}")]
    BadU32 { field: &'static str },
}

/// One `mining.notify` job in the form needed to build headers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobTemplate {
    pub job_id: String,
    /// Natural SHA256 internal-byte-order (already 4-byte-word-reversed from
    /// the wire).
    pub prev_hash: [u8; 32],
    pub coinb1: Vec<u8>,
    pub coinb2: Vec<u8>,
    pub merkle_branch: Vec<[u8; 32]>,
    /// Block version as a `u32` value (BE-decoded from the hex on the wire).
    pub version: u32,
    /// Compact difficulty target (BE-decoded from the hex on the wire).
    pub nbits: u32,
    /// Block timestamp (BE-decoded from the hex on the wire).
    pub ntime: u32,
}

/// One `mining.submit`. ntime here can override the job's ntime within the
/// pool's accepted drift window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubmitFields {
    pub extranonce2: Vec<u8>,
    pub ntime: u32,
    pub nonce: u32,
    /// Optional BIP310 version mask. XOR'd onto the job version.
    pub version_mask: Option<u32>,
}

/// Hex-decode an `nbits`-style 4-byte BE value into a `u32`.
pub fn parse_be_u32_hex(s: &str, field: &'static str) -> Result<u32, HeaderError> {
    let bytes = hex::decode(s).map_err(|e| HeaderError::Hex { field, source: e })?;
    if bytes.len() != 4 {
        return Err(HeaderError::Length {
            field,
            expected: 4,
            got: bytes.len(),
        });
    }
    let arr: [u8; 4] = bytes.try_into().map_err(|_| HeaderError::BadU32 { field })?;
    Ok(u32::from_be_bytes(arr))
}

/// Hex-decode a 32-byte hash and 4-byte-word-reverse it (Stratum
/// `prevhash` convention → natural internal-byte-order).
pub fn parse_prev_hash_hex(s: &str) -> Result<[u8; 32], HeaderError> {
    let bytes = hex::decode(s).map_err(|e| HeaderError::Hex {
        field: "prev_hash",
        source: e,
    })?;
    if bytes.len() != 32 {
        return Err(HeaderError::Length {
            field: "prev_hash",
            expected: 32,
            got: bytes.len(),
        });
    }
    let mut out = [0u8; 32];
    for word in 0..8 {
        let s = word * 4;
        out[s] = bytes[s + 3];
        out[s + 1] = bytes[s + 2];
        out[s + 2] = bytes[s + 1];
        out[s + 3] = bytes[s];
    }
    Ok(out)
}

/// Hex-decode a 32-byte hash without any reordering (merkle branch entries
/// arrive in natural internal-byte-order).
pub fn parse_branch_entry_hex(s: &str) -> Result<[u8; 32], HeaderError> {
    let bytes = hex::decode(s).map_err(|e| HeaderError::Hex {
        field: "merkle_branch",
        source: e,
    })?;
    if bytes.len() != 32 {
        return Err(HeaderError::Length {
            field: "merkle_branch",
            expected: 32,
            got: bytes.len(),
        });
    }
    Ok(bytes.try_into().unwrap())
}

fn sha256d(data: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(data);
    Sha256::digest(first).into()
}

/// `coinb1 || extranonce1 || extranonce2 || coinb2` → double SHA256.
pub fn coinbase_tx_hash(
    coinb1: &[u8],
    extranonce1: &[u8],
    extranonce2: &[u8],
    coinb2: &[u8],
) -> [u8; 32] {
    let total = coinb1.len() + extranonce1.len() + extranonce2.len() + coinb2.len();
    let mut buf = Vec::with_capacity(total);
    buf.extend_from_slice(coinb1);
    buf.extend_from_slice(extranonce1);
    buf.extend_from_slice(extranonce2);
    buf.extend_from_slice(coinb2);
    sha256d(&buf)
}

/// Fold the coinbase hash up through the supplied merkle branch.
///
/// Stratum branches are always *left*-side, since the coinbase is leaf 0:
/// at each level the running hash is concatenated *before* the branch entry,
/// then double-hashed.
pub fn merkle_root_from_branch(coinbase_hash: &[u8; 32], branch: &[[u8; 32]]) -> [u8; 32] {
    let mut acc = *coinbase_hash;
    for sibling in branch {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&acc);
        buf[32..].copy_from_slice(sibling);
        acc = sha256d(&buf);
    }
    acc
}

/// Build the 80-byte block header given a job + extranonce1 + submit.
pub fn build_block_header(
    job: &JobTemplate,
    extranonce1: &[u8],
    submit: &SubmitFields,
) -> [u8; 80] {
    let coinbase_hash = coinbase_tx_hash(
        &job.coinb1,
        extranonce1,
        &submit.extranonce2,
        &job.coinb2,
    );
    let merkle_root = merkle_root_from_branch(&coinbase_hash, &job.merkle_branch);

    let version = match submit.version_mask {
        Some(mask) => job.version ^ mask,
        None => job.version,
    };

    let mut header = [0u8; 80];
    header[0..4].copy_from_slice(&version.to_le_bytes());
    header[4..36].copy_from_slice(&job.prev_hash);
    header[36..68].copy_from_slice(&merkle_root);
    header[68..72].copy_from_slice(&submit.ntime.to_le_bytes());
    header[72..76].copy_from_slice(&job.nbits.to_le_bytes());
    header[76..80].copy_from_slice(&submit.nonce.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(merkle_branch: Vec<[u8; 32]>) -> JobTemplate {
        JobTemplate {
            job_id: "j1".to_string(),
            prev_hash: [0x11; 32],
            coinb1: vec![0xAA; 4],
            coinb2: vec![0xBB; 4],
            merkle_branch,
            version: 0x2000_0000,
            nbits: 0x1d00_ffff,
            ntime: 0x6000_0000,
        }
    }

    #[test]
    fn parse_be_u32_hex_roundtrip() {
        assert_eq!(parse_be_u32_hex("1d00ffff", "nbits").unwrap(), 0x1d00_ffff);
        assert_eq!(parse_be_u32_hex("00000001", "version").unwrap(), 1);
        assert_eq!(parse_be_u32_hex("ffffffff", "nonce").unwrap(), 0xffff_ffff);
    }

    #[test]
    fn parse_be_u32_hex_rejects_wrong_length() {
        assert!(matches!(
            parse_be_u32_hex("1d00ff", "nbits"),
            Err(HeaderError::Length { .. })
        ));
    }

    #[test]
    fn parse_prev_hash_reverses_each_4_byte_word() {
        // wire = "01020304 05060708 ..."  → natural = "04030201 08070605 ..."
        let wire: String = (0..8u8)
            .map(|w| {
                format!(
                    "{:02x}{:02x}{:02x}{:02x}",
                    4 * w + 1,
                    4 * w + 2,
                    4 * w + 3,
                    4 * w + 4,
                )
            })
            .collect();
        let parsed = parse_prev_hash_hex(&wire).unwrap();
        for w in 0..8 {
            let s = w * 4;
            assert_eq!(parsed[s], (4 * w as u8) + 4);
            assert_eq!(parsed[s + 1], (4 * w as u8) + 3);
            assert_eq!(parsed[s + 2], (4 * w as u8) + 2);
            assert_eq!(parsed[s + 3], (4 * w as u8) + 1);
        }
    }

    #[test]
    fn parse_prev_hash_rejects_wrong_length() {
        assert!(matches!(
            parse_prev_hash_hex("11"),
            Err(HeaderError::Length { .. })
        ));
    }

    #[test]
    fn coinbase_hash_concatenates_in_order() {
        let h1 = coinbase_tx_hash(b"AA", b"BB", b"CC", b"DD");
        let h2 = sha256d(b"AABBCCDD");
        assert_eq!(h1, h2);
    }

    #[test]
    fn merkle_root_with_empty_branch_is_coinbase_hash() {
        let cb = sha256d(b"hello");
        let root = merkle_root_from_branch(&cb, &[]);
        assert_eq!(root, cb);
    }

    #[test]
    fn merkle_root_folds_left_at_each_level() {
        let cb = [0x01u8; 32];
        let s1 = [0x02u8; 32];
        let s2 = [0x03u8; 32];
        let want = {
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(&cb);
            buf[32..].copy_from_slice(&s1);
            let lvl1 = sha256d(&buf);
            buf[..32].copy_from_slice(&lvl1);
            buf[32..].copy_from_slice(&s2);
            sha256d(&buf)
        };
        assert_eq!(merkle_root_from_branch(&cb, &[s1, s2]), want);
    }

    #[test]
    fn build_header_field_layout() {
        // No version mask, no merkle branch.
        let j = job(vec![]);
        let s = SubmitFields {
            extranonce2: vec![0xC1, 0xC2, 0xC3, 0xC4],
            ntime: 0x6111_2233,
            nonce: 0xDEAD_BEEF,
            version_mask: None,
        };
        let en1 = [0xE1, 0xE2];
        let header = build_block_header(&j, &en1, &s);

        // version → LE
        assert_eq!(&header[0..4], &[0x00, 0x00, 0x00, 0x20]);
        // prev_hash copied verbatim
        assert_eq!(&header[4..36], &[0x11; 32]);
        // merkle_root = double_sha256("AAAA E1E2 C1C2C3C4 BBBB")
        let mut cb = Vec::new();
        cb.extend_from_slice(&[0xAA; 4]);
        cb.extend_from_slice(&en1);
        cb.extend_from_slice(&[0xC1, 0xC2, 0xC3, 0xC4]);
        cb.extend_from_slice(&[0xBB; 4]);
        let want_root = sha256d(&cb);
        assert_eq!(&header[36..68], want_root.as_slice());
        // ntime → LE
        assert_eq!(&header[68..72], &[0x33, 0x22, 0x11, 0x61]);
        // nbits → LE
        assert_eq!(&header[72..76], &[0xff, 0xff, 0x00, 0x1d]);
        // nonce → LE
        assert_eq!(&header[76..80], &[0xef, 0xbe, 0xad, 0xde]);
    }

    #[test]
    fn build_header_applies_version_mask() {
        let j = job(vec![]);
        let s = SubmitFields {
            extranonce2: vec![],
            ntime: 0,
            nonce: 0,
            version_mask: Some(0x0000_0004),
        };
        let header = build_block_header(&j, &[], &s);
        let v = u32::from_le_bytes(header[0..4].try_into().unwrap());
        assert_eq!(v, 0x2000_0000 ^ 0x0000_0004);
    }
}
