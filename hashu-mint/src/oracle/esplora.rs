//! Self-derived hashprice oracle via the Blockstream Esplora REST API.
//! ARCHITECTURE.md §4.4.
//!
//! Computes hashprice from public Bitcoin data with no third-party pricing
//! feed:
//!
//! ```text
//! sats / PH / day = 86_400 * 1e15 * Σ(coinbase_sats_i)
//!                                  ─────────────────────────────────────
//!                                   Σ(difficulty_i * 4_295_032_833)
//! ```
//!
//! `4_295_032_833 ≈ 2^48 / 65_535` is the number of expected hashes per unit
//! of bdiff difficulty (Bitcoin's `MaxTarget = 65_535 × 2^208` convention).
//! `dt` and the block count algebraically cancel, so the formula is
//! independent of timestamp jitter and tolerates difficulty adjustments
//! mid-window.
//!
//! Coinbase output total = `subsidy + fees`, so summing coinbase outputs
//! across the window directly yields total miner revenue without needing the
//! halving schedule.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hashu_core::hashprice::{HashpriceOracle, HashpriceSample, SampleBuffer};
use serde::Deserialize;

/// Default Blockstream-hosted Esplora base URL.
pub const DEFAULT_BASE_URL: &str = "https://blockstream.info/api";

/// Default fee/work averaging window. 24 blocks ≈ 4 hours of network history;
/// long enough to smooth fee volatility without making startup expensive.
pub const DEFAULT_WINDOW_BLOCKS: usize = 24;

/// Default poll cadence. Bitcoin blocks average 10 minutes, so 5 minutes
/// catches every new tip without hammering the public Esplora instance.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(300);

/// Hashes expected per unit of bdiff difficulty: `2^48 / 65_535`, rounded.
///
/// Derivation: `Diff = MaxTarget / Target` with `MaxTarget = 65_535 × 2^208`.
/// Expected hashes per block = `2^256 / (Target + 1) ≈ Diff × 2^48 / 65_535`.
pub const HASHES_PER_DIFFICULTY: f64 = 4_295_032_833.0;

#[derive(Debug, thiserror::Error)]
pub enum EsploraError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("esplora returned status {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("block had no transactions (no coinbase)")]
    EmptyBlock,
    #[error("invalid tip hash: {0}")]
    InvalidTip(String),
}

#[derive(Debug, Deserialize)]
struct BlockResponse {
    #[allow(dead_code)]
    id: String,
    height: u32,
    timestamp: u32,
    difficulty: f64,
    previousblockhash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TxResponse {
    vout: Vec<VoutEntry>,
}

#[derive(Debug, Deserialize)]
struct VoutEntry {
    value: u64,
}

/// HTTP client for the Esplora REST API (Blockstream-hosted or self-hosted).
#[derive(Clone)]
pub struct EsploraClient {
    http: reqwest::Client,
    base: String,
}

impl Default for EsploraClient {
    fn default() -> Self {
        Self::new()
    }
}

impl EsploraClient {
    pub fn new() -> Self {
        Self::with_base(DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base(base: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .expect("reqwest client build");
        Self { http, base }
    }

    /// `GET /blocks/tip/hash` → returns the hash as plain text.
    pub async fn tip_hash(&self) -> Result<String, EsploraError> {
        let url = format!("{}/blocks/tip/hash", self.base);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(EsploraError::Status { status, body });
        }
        let trimmed = body.trim();
        if trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(EsploraError::InvalidTip(trimmed.to_string()));
        }
        Ok(trimmed.to_string())
    }

    /// `GET /block/:hash` → block header summary.
    pub async fn block(&self, hash: &str) -> Result<BlockSummary, EsploraError> {
        let url = format!("{}/block/{}", self.base, hash);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(EsploraError::Status { status, body });
        }
        let parsed: BlockResponse = resp.json().await?;
        let coinbase_total_sats = self.coinbase_total(hash).await?;
        Ok(BlockSummary {
            hash: hash.to_string(),
            height: parsed.height,
            timestamp: parsed.timestamp,
            difficulty: parsed.difficulty,
            previousblockhash: parsed.previousblockhash,
            coinbase_total_sats,
        })
    }

    /// `GET /block/:hash/txs/0` → first 25 txs starting at index 0;
    /// `txs[0]` is the coinbase. Returns sum of its vout values.
    pub async fn coinbase_total(&self, hash: &str) -> Result<u64, EsploraError> {
        let url = format!("{}/block/{}/txs/0", self.base, hash);
        let resp = self.http.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(EsploraError::Status { status, body });
        }
        let txs: Vec<TxResponse> = resp.json().await?;
        let coinbase = txs.first().ok_or(EsploraError::EmptyBlock)?;
        Ok(coinbase.vout.iter().map(|v| v.value).sum())
    }
}

/// Cached summary of one block as needed for hashprice computation.
#[derive(Clone, Debug, PartialEq)]
pub struct BlockSummary {
    pub hash: String,
    pub height: u32,
    pub timestamp: u32,
    pub difficulty: f64,
    pub previousblockhash: Option<String>,
    /// Sum of the coinbase tx's outputs, i.e. `subsidy + total_fees`.
    pub coinbase_total_sats: u64,
}

/// Pure hashprice computation from a window of block summaries.
/// Returns a sample timestamped at the most recent block.
pub fn compute_hashprice(window: &[BlockSummary]) -> Option<HashpriceSample> {
    if window.is_empty() {
        return None;
    }
    let total_rev: u128 = window.iter().map(|b| b.coinbase_total_sats as u128).sum();
    let total_work: f64 = window
        .iter()
        .map(|b| b.difficulty * HASHES_PER_DIFFICULTY)
        .sum();
    if !total_work.is_finite() || total_work <= 0.0 {
        return None;
    }
    let sats_per_ph_day = 86_400.0 * 1e15 * (total_rev as f64) / total_work;
    let latest = window.iter().max_by_key(|b| b.height)?;
    let t = UNIX_EPOCH + Duration::from_secs(latest.timestamp as u64);
    Some(HashpriceSample::from_sats_per_ph_per_day(t, sats_per_ph_day))
}

/// Hashprice oracle backed by an Esplora poller.
#[derive(Clone)]
pub struct EsploraOracle {
    buffer: Arc<Mutex<SampleBuffer>>,
}

impl EsploraOracle {
    pub fn new(capacity: usize, max_staleness: Duration) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(SampleBuffer::new(capacity, max_staleness))),
        }
    }

    /// Insert a sample directly. Mainly for tests / warm-start.
    pub fn push(&self, sample: HashpriceSample) {
        if let Ok(mut b) = self.buffer.lock() {
            b.push(sample);
        }
    }

    /// Spawn a tokio task that polls Esplora every `poll_interval`, walks back
    /// `window` blocks from tip, computes hashprice, and pushes a sample.
    pub fn spawn_poller(
        &self,
        client: EsploraClient,
        window: usize,
        poll_interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let buffer = self.buffer.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(poll_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match fetch_window(&client, window).await {
                    Ok(window_blocks) => {
                        if let Some(sample) = compute_hashprice(&window_blocks) {
                            if let Ok(mut b) = buffer.lock() {
                                b.push(sample.clone());
                            }
                            tracing::debug!(
                                sats_per_ths = sample.sats_per_ths,
                                t = ?sample.t,
                                blocks = window_blocks.len(),
                                "esplora hashprice sample",
                            );
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "esplora fetch failed"),
                }
            }
        })
    }
}

impl HashpriceOracle for EsploraOracle {
    fn sample_at(&self, t: SystemTime) -> Option<f64> {
        self.buffer.lock().ok().and_then(|b| b.sample_at(t))
    }

    fn latest(&self) -> Option<HashpriceSample> {
        self.buffer.lock().ok().and_then(|b| b.latest())
    }
}

/// Walk back `window` blocks from the current tip and return their summaries.
/// 2 HTTP calls per block; for `window=24` that's ~48 calls per refresh.
async fn fetch_window(
    client: &EsploraClient,
    window: usize,
) -> Result<Vec<BlockSummary>, EsploraError> {
    let mut summaries = Vec::with_capacity(window);
    let mut hash = client.tip_hash().await?;
    for _ in 0..window {
        let summary = client.block(&hash).await?;
        let next = match &summary.previousblockhash {
            Some(p) => p.clone(),
            None => {
                summaries.push(summary);
                break;
            }
        };
        summaries.push(summary);
        hash = next;
    }
    Ok(summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(height: u32, ts: u32, diff: f64, coinbase_sats: u64) -> BlockSummary {
        BlockSummary {
            hash: format!("{:064x}", height),
            height,
            timestamp: ts,
            difficulty: diff,
            previousblockhash: if height == 0 {
                None
            } else {
                Some(format!("{:064x}", height - 1))
            },
            coinbase_total_sats: coinbase_sats,
        }
    }

    fn approx_eq(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn empty_window_returns_none() {
        assert!(compute_hashprice(&[]).is_none());
    }

    #[test]
    fn zero_difficulty_returns_none() {
        let b = block(800_000, 1_700_000_000, 0.0, 312_500_000);
        assert!(compute_hashprice(&[b]).is_none());
    }

    #[test]
    fn realistic_single_block_matches_expected() {
        // Mid-2025-ish numbers: difficulty ≈ 110e12, coinbase ≈ 3.23e8 sats
        // (3.125 BTC subsidy + ~0.05 BTC fees).
        let b = block(870_000, 1_700_000_000, 110e12, 323_000_000);
        let sample = compute_hashprice(&[b]).unwrap();
        let sats_per_ph_day = sample.sats_per_ph_per_day();
        // 86400 * 1e15 * 3.23e8 / (110e12 * 4_295_032_833) ≈ 59_069 sats/PH/day
        let expected = 86_400.0 * 1e15 * 3.23e8 / (110e12 * HASHES_PER_DIFFICULTY);
        assert!(approx_eq(sats_per_ph_day, expected, 1.0));
        // Spot check magnitude.
        assert!((50_000.0..70_000.0).contains(&sats_per_ph_day));
    }

    #[test]
    fn sample_timestamp_is_latest_block() {
        let blocks = vec![
            block(100, 1_700_000_000, 1e10, 312_500_000),
            block(101, 1_700_000_600, 1e10, 312_500_000),
            block(102, 1_700_001_200, 1e10, 312_500_000),
        ];
        let sample = compute_hashprice(&blocks).unwrap();
        assert_eq!(
            sample.t,
            UNIX_EPOCH + Duration::from_secs(1_700_001_200)
        );
    }

    #[test]
    fn averaging_over_window_smooths_fees() {
        // Window of 3 blocks: 0 fees, 0 fees, big-fee block.
        // Expect hashprice to reflect the average revenue.
        let subsidy = 312_500_000u64;
        let big_fees = 100_000_000u64; // 1 BTC fees on block 3
        let diff = 1e13;
        let blocks = vec![
            block(1000, 1_700_000_000, diff, subsidy),
            block(1001, 1_700_000_600, diff, subsidy),
            block(1002, 1_700_001_200, diff, subsidy + big_fees),
        ];
        let sample = compute_hashprice(&blocks).unwrap();
        let total_rev = (3 * subsidy + big_fees) as f64;
        let total_work = 3.0 * diff * HASHES_PER_DIFFICULTY;
        let expected = 86_400.0 * 1e15 * total_rev / total_work;
        assert!(approx_eq(sample.sats_per_ph_per_day(), expected, 1.0));
    }

    #[test]
    fn dt_independence_holds_numerically() {
        // Two windows with same diffs and revenues but different "block times".
        // Hashprice should be identical because dt cancels.
        let make = |spacing: u32| {
            (0..10u32)
                .map(|i| block(2_000_000 + i, 1_700_000_000 + i * spacing, 1e13, 312_500_000))
                .collect::<Vec<_>>()
        };
        let fast = compute_hashprice(&make(60)).unwrap(); // 1-min spacing
        let slow = compute_hashprice(&make(1200)).unwrap(); // 20-min spacing
        assert!(approx_eq(
            fast.sats_per_ph_per_day(),
            slow.sats_per_ph_per_day(),
            1e-9
        ));
    }

    #[test]
    fn parses_block_response() {
        let body = r#"{
            "id": "00000000000000000001abcdef",
            "height": 870000,
            "version": 536870912,
            "timestamp": 1700000000,
            "tx_count": 3000,
            "size": 1500000,
            "weight": 4000000,
            "merkle_root": "deadbeef",
            "previousblockhash": "00000000000000000000fffefffe",
            "mediantime": 1699999000,
            "nonce": 12345,
            "bits": 386024311,
            "difficulty": 110000000000000.0
        }"#;
        let parsed: BlockResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.height, 870_000);
        assert_eq!(parsed.timestamp, 1_700_000_000);
        assert!((parsed.difficulty - 1.1e14).abs() < 1.0);
        assert_eq!(
            parsed.previousblockhash.as_deref(),
            Some("00000000000000000000fffefffe")
        );
    }

    #[test]
    fn parses_coinbase_tx_response() {
        // First-tx-only response for /block/:hash/txs/0
        let body = r#"[
            {
                "txid": "abc",
                "version": 1,
                "vin": [{"is_coinbase": true}],
                "vout": [
                    {"value": 312500000},
                    {"value": 5000000},
                    {"value": 0}
                ]
            }
        ]"#;
        let parsed: Vec<TxResponse> = serde_json::from_str(body).unwrap();
        let coinbase = &parsed[0];
        let total: u64 = coinbase.vout.iter().map(|v| v.value).sum();
        assert_eq!(total, 317_500_000);
    }

    #[test]
    fn oracle_implements_trait_through_lock() {
        let oracle = EsploraOracle::new(8, Duration::from_secs(600));
        assert!(oracle.latest().is_none());
        let blocks = vec![block(870_000, 1_700_000_000, 110e12, 323_000_000)];
        oracle.push(compute_hashprice(&blocks).unwrap());
        let v = HashpriceOracle::sample_at(&oracle, UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();
        // Same magnitude as the realistic test.
        let sats_per_ph_day = v * 1000.0 * 86_400.0;
        assert!((50_000.0..70_000.0).contains(&sats_per_ph_day));
    }
}
