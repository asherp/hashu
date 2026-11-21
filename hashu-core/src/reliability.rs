//! The mint reliability record: what a mint delivered each hour, in a form
//! anyone can check. Issue #109, ARCHITECTURE.md §4.7.4.
//!
//! Every share credited to a redemption lands in its mint's
//! [`HourBucket`] for the UTC hour it was credited in. A bucket keeps the
//! summed share difficulty (the work delivered) and the 16 hardest shares
//! by actual hash ([`TopShares`]). The top shares are 80-byte block
//! headers: anyone can hash one, read its difficulty, and see whether it
//! solved a block, without trusting the mint. Their difficulties also
//! bound the work claim: the hardest of N shares at difficulty D is
//! expected near N·D.
//!
//! Nothing here names a redeemer: no redemption id, pool or worker.
//! Blocks are always kept; no setting removes a share, since dropping a
//! block would look like withholding.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Shares kept per hour.
pub const TOP_K: usize = 16;

/// Seconds in one record period.
pub const HOUR_S: u64 = 3600;

/// Start of the UTC hour containing `unix_s`.
pub fn hour_start(unix_s: u64) -> u64 {
    unix_s - unix_s % HOUR_S
}

/// Double SHA-256 of an 80-byte header, in internal byte order (the
/// order the bytes come out of the hash, little-endian as a number).
pub fn header_hash(header: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(header);
    Sha256::digest(first).into()
}

/// The hash as block explorers print it (byte-reversed hex).
pub fn display_hash(hash: &[u8; 32]) -> String {
    let mut be = *hash;
    be.reverse();
    hex::encode(be)
}

/// Big-endian form of an internal-order hash, for comparing as a number.
fn be(hash: &[u8; 32]) -> [u8; 32] {
    let mut b = *hash;
    b.reverse();
    b
}

/// Share difficulty a hash meets: `0xffff · 2^208 / hash` (the
/// difficulty-1 target pools use). A hash of zero is reported as
/// `f64::MAX`.
pub fn hash_difficulty(hash: &[u8; 32]) -> f64 {
    let mut v = 0f64;
    for b in be(hash) {
        v = v * 256.0 + b as f64;
    }
    if v == 0.0 {
        return f64::MAX;
    }
    65535.0 * 2f64.powi(208) / v
}

/// The block target an nBits value encodes, big-endian.
pub fn target_from_bits(bits: u32) -> [u8; 32] {
    let exp = (bits >> 24) as usize;
    let mant = bits & 0x007f_ffff;
    let mut t = [0u8; 32];
    if bits & 0x0080_0000 != 0 || mant == 0 {
        return t;
    }
    let m = [(mant >> 16) as u8, (mant >> 8) as u8, mant as u8];
    // The mantissa's 3 bytes sit at byte positions exp-1, exp-2, exp-3
    // counting from the least significant end.
    for (i, byte) in m.iter().enumerate() {
        let pos_from_lsb = exp as isize - 1 - i as isize;
        if (0..32).contains(&pos_from_lsb) {
            t[31 - pos_from_lsb as usize] = *byte;
        }
    }
    t
}

/// nBits field of an 80-byte header.
pub fn header_bits(header: &[u8]) -> Option<u32> {
    let b = header.get(72..76)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// ntime field of an 80-byte header.
pub fn header_time(header: &[u8]) -> Option<u32> {
    let b = header.get(68..72)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// One checkable share: its header and what anyone derives from it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TopShare {
    /// The 80-byte block header, hex.
    pub header: String,
    /// Its double SHA-256, as explorers print it.
    pub hash: String,
    /// Difficulty the hash meets.
    pub difficulty: f64,
    /// The hash meets the header's own nBits target: a block.
    pub block: bool,
}

impl TopShare {
    /// Score an 80-byte header. `None` if it is not 80 bytes.
    pub fn from_header(header: &[u8]) -> Option<Self> {
        if header.len() != 80 {
            return None;
        }
        let hash = header_hash(header);
        let block = header_bits(header)
            .map(|bits| be(&hash) <= target_from_bits(bits))
            .unwrap_or(false);
        Some(Self {
            header: hex::encode(header),
            hash: display_hash(&hash),
            difficulty: hash_difficulty(&hash),
            block,
        })
    }

    /// Recompute hash, difficulty and block flag from `header`; true when
    /// they match what this share claims.
    pub fn verify(&self) -> bool {
        let Ok(h) = hex::decode(&self.header) else {
            return false;
        };
        match Self::from_header(&h) {
            Some(again) => {
                again.hash == self.hash
                    && again.block == self.block
                    && (again.difficulty - self.difficulty).abs() <= again.difficulty * 1e-9
            }
            None => false,
        }
    }
}

/// The [`TOP_K`] hardest distinct shares seen, hardest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TopShares(Vec<TopShare>);

impl TopShares {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer one share. Kept if it is among the [`TOP_K`] hardest.
    pub fn offer(&mut self, share: TopShare) {
        if self.0.iter().any(|s| s.hash == share.hash) {
            return;
        }
        if self.0.len() >= TOP_K
            && self
                .0
                .last()
                .is_some_and(|worst| share.difficulty <= worst.difficulty)
        {
            return;
        }
        let at = self
            .0
            .iter()
            .position(|s| cmp_hardest(&share, s) == Ordering::Less)
            .unwrap_or(self.0.len());
        self.0.insert(at, share);
        self.0.truncate(TOP_K);
    }

    pub fn shares(&self) -> &[TopShare] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<TopShare> {
        self.0
    }
}

fn cmp_hardest(a: &TopShare, b: &TopShare) -> Ordering {
    b.difficulty
        .partial_cmp(&a.difficulty)
        .unwrap_or(Ordering::Equal)
}

/// One mint's credited work in one UTC hour.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HourBucket {
    /// Credited shares.
    pub shares: u64,
    /// Summed credited share difficulty.
    pub difficulty: f64,
    pub top: TopShares,
    /// Hashes of every block-solving share (always all of them, even past
    /// [`TOP_K`]; they are also in `top` unless 16 harder shares exist,
    /// which cannot happen in practice).
    pub blocks: Vec<String>,
}

impl HourBucket {
    /// Credit one share at pool difficulty `difficulty` with header `header`.
    pub fn record(&mut self, difficulty: f64, header: &[u8]) {
        self.shares += 1;
        if difficulty.is_finite() && difficulty > 0.0 {
            self.difficulty += difficulty;
        }
        if let Some(s) = TopShare::from_header(header) {
            if s.block && !self.blocks.contains(&s.hash) {
                self.blocks.push(s.hash.clone());
            }
            self.top.offer(s);
        }
    }

    /// Work delivered, in TiH (a difficulty-D share is D/256 TiH).
    pub fn work_tih(&self) -> f64 {
        self.difficulty / 256.0
    }
}

/// Redemption activity for one mint in one hour. Self-reported; holders
/// confirm their own with receipts.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RedemptionStats {
    /// Redemptions that opened this hour.
    pub opened: u64,
    /// Reached their full amount this hour.
    pub completed: u64,
    /// Stopped early by the redeemer (change returned) this hour.
    pub settled: u64,
    /// Still open at the end of the hour.
    pub open: u64,
    /// Open at the end of the hour with no credited work during it.
    pub stalled: u64,
    /// TiH quoted (redeemed) by the redemptions that finished this hour.
    pub finished_quoted_tih: u64,
    /// TiH delivered to them.
    pub finished_delivered_tih: u64,
    /// Minutes from open to finish for redemptions that finished this
    /// hour and opened while the record was running: median and worst.
    pub minutes_to_finish_median: Option<u64>,
    pub minutes_to_finish_worst: Option<u64>,
}

/// Wire version of [`HourRecord`].
pub const RECORD_VERSION: u32 = 1;

/// The content of one hourly event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HourRecord {
    pub v: u32,
    /// The mint's public URL.
    pub mint: String,
    /// Start of the UTC hour, unix seconds.
    pub hour: u64,
    /// Credited shares.
    pub shares: u64,
    /// Summed share difficulty.
    pub difficulty: f64,
    /// Work delivered, TiH.
    pub work_tih: f64,
    /// The 16 hardest shares, hardest first. Blocks included.
    pub top: Vec<TopShare>,
    /// Hashes of block-solving shares this hour.
    pub blocks: Vec<String>,
    pub redemptions: RedemptionStats,
    /// Seconds of the hour the mint was running.
    pub uptime_s: u64,
    /// Live hashrate of the mint's rigs when the hour closed, TH/s.
    /// Self-reported.
    pub hashrate_ths: f64,
}

impl HourRecord {
    pub fn new(
        mint: String,
        hour: u64,
        bucket: HourBucket,
        redemptions: RedemptionStats,
        uptime_s: u64,
        hashrate_ths: f64,
    ) -> Self {
        let work_tih = bucket.work_tih();
        Self {
            v: RECORD_VERSION,
            mint,
            hour,
            shares: bucket.shares,
            difficulty: bucket.difficulty,
            work_tih,
            top: bucket.top.into_vec(),
            blocks: bucket.blocks,
            redemptions,
            uptime_s: uptime_s.min(HOUR_S),
            hashrate_ths,
        }
    }
}

/// Median and worst of `minutes`.
pub fn median_and_worst(mut minutes: Vec<u64>) -> (Option<u64>, Option<u64>) {
    if minutes.is_empty() {
        return (None, None);
    }
    minutes.sort_unstable();
    (Some(minutes[minutes.len() / 2]), minutes.last().copied())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bitcoin's genesis block header.
    const GENESIS: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";

    #[test]
    fn genesis_is_a_block() {
        let h = hex::decode(GENESIS).unwrap();
        let s = TopShare::from_header(&h).unwrap();
        assert_eq!(
            s.hash,
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
        assert!(s.block);
        // ~2,536 (the genesis hash beats its difficulty-1 target).
        assert!((s.difficulty - 2536.0).abs() < 1.0, "{}", s.difficulty);
        assert!(s.verify());
    }

    #[test]
    fn tampered_share_fails_verify() {
        let h = hex::decode(GENESIS).unwrap();
        let mut s = TopShare::from_header(&h).unwrap();
        s.difficulty *= 2.0;
        assert!(!s.verify());
        let mut s = TopShare::from_header(&h).unwrap();
        s.block = false;
        assert!(!s.verify());
    }

    #[test]
    fn non_block_share() {
        let mut h = hex::decode(GENESIS).unwrap();
        h[76] ^= 1; // change the nonce
        let s = TopShare::from_header(&h).unwrap();
        assert!(!s.block);
        assert!(s.difficulty < 1.0);
    }

    #[test]
    fn target_from_bits_matches_diff_one() {
        let t = target_from_bits(0x1d00ffff);
        let mut want = [0u8; 32];
        want[4] = 0xff;
        want[5] = 0xff;
        assert_eq!(t, want);
    }

    fn share(d: f64, tag: u8) -> TopShare {
        TopShare {
            header: String::new(),
            hash: format!("{tag:02x}"),
            difficulty: d,
            block: false,
        }
    }

    #[test]
    fn keeps_top_16_hardest_first() {
        let mut t = TopShares::new();
        for i in 0..40u8 {
            t.offer(share(((i as u64 * 37) % 41) as f64, i));
        }
        let d: Vec<f64> = t.shares().iter().map(|s| s.difficulty).collect();
        assert_eq!(d.len(), TOP_K);
        assert!(d.windows(2).all(|w| w[0] >= w[1]));
        assert_eq!(d[0], 40.0);
        // Duplicates are ignored.
        let before = t.clone();
        let mut dup = t.shares()[3].clone();
        dup.difficulty = 1000.0;
        t.offer(dup);
        assert_eq!(t, before);
    }

    #[test]
    fn bucket_counts_work_and_blocks() {
        let mut b = HourBucket::default();
        let g = hex::decode(GENESIS).unwrap();
        b.record(256.0, &g);
        b.record(256.0, &g);
        assert_eq!(b.shares, 2);
        assert_eq!(b.work_tih(), 2.0);
        assert_eq!(b.blocks.len(), 1);
        assert_eq!(b.top.shares().len(), 1);
    }

    #[test]
    fn hours_and_medians() {
        assert_eq!(hour_start(7205), 7200);
        assert_eq!(median_and_worst(vec![]), (None, None));
        assert_eq!(median_and_worst(vec![9, 1, 5]), (Some(5), Some(9)));
    }
}
