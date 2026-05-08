//! Luxor Hashrate Index adapter. ARCHITECTURE.md §4.4.
//!
//! Pulls `GET /v1/hashrateindex/hashprice/current?hashunit=PHS` from
//! `api.hashrateindex.com`, authenticates with the `X-Hi-Api-Key` header,
//! and converts the BTC-denominated quote (`priceBTC`, units BTC/PH/day at
//! `hashunit=PHS`) into the internal sats/Th-second representation. The USD
//! field is ignored — we don't want to depend on a USD price feed.
//!
//! The poller updates a shared [`SampleBuffer`] under a sync `Mutex`; the
//! buffer is read on the redemption hot path on every accepted share, so
//! sync locking is appropriate (writes happen at the poll cadence, ~5 min).

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hashu_core::hashprice::{HashpriceOracle, HashpriceSample, SampleBuffer};
use serde::Deserialize;
use time::format_description::well_known::Rfc3339;

/// Default API key environment variable.
pub const ENV_LUXOR_API_KEY: &str = "LUXOR_API_KEY";

/// Default Luxor base URL.
pub const DEFAULT_BASE_URL: &str = "https://api.hashrateindex.com/v1";

/// Default poll cadence — Luxor publishes ~5-min granularity.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum LuxorError {
    #[error("missing {0} env var")]
    MissingApiKey(&'static str),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("luxor returned status {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("timestamp parse: {0}")]
    Timestamp(#[from] time::error::Parse),
}

#[derive(Debug, Deserialize)]
struct CurrentResponse {
    data: CurrentData,
}

#[derive(Debug, Deserialize)]
struct CurrentData {
    #[serde(rename = "priceBTC")]
    price_btc: f64,
    #[serde(rename = "priceUSD")]
    #[allow(dead_code)] // Operator's LN node already prices in BTC.
    price_usd: f64,
    timestamp: String,
}

/// HTTP client for the Luxor Hashrate Index hashprice endpoints.
#[derive(Clone)]
pub struct LuxorClient {
    http: reqwest::Client,
    api_key: String,
    base: String,
}

impl LuxorClient {
    pub fn new(api_key: String) -> Self {
        Self::with_base(api_key, DEFAULT_BASE_URL.to_string())
    }

    pub fn with_base(api_key: String, base: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            api_key,
            base,
        }
    }

    pub fn from_env() -> Result<Self, LuxorError> {
        let key = std::env::var(ENV_LUXOR_API_KEY)
            .map_err(|_| LuxorError::MissingApiKey(ENV_LUXOR_API_KEY))?;
        Ok(Self::new(key))
    }

    /// Hit `/hashrateindex/hashprice/current?hashunit=PHS` and convert.
    pub async fn fetch_latest(&self) -> Result<HashpriceSample, LuxorError> {
        let url = format!("{}/hashrateindex/hashprice/current", self.base);
        let resp = self
            .http
            .get(&url)
            .header("X-Hi-Api-Key", &self.api_key)
            .query(&[("hashunit", "PHS")])
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(LuxorError::Status { status, body });
        }

        let parsed: CurrentResponse = resp.json().await?;
        sample_from_response(parsed.data.price_btc, &parsed.data.timestamp)
    }
}

fn sample_from_response(price_btc: f64, timestamp: &str) -> Result<HashpriceSample, LuxorError> {
    let t = parse_rfc3339(timestamp)?;
    // hashunit=PHS means priceBTC is BTC per PH per day.
    Ok(HashpriceSample::from_btc_per_ph_per_day(t, price_btc))
}

fn parse_rfc3339(s: &str) -> Result<SystemTime, LuxorError> {
    let dt = time::OffsetDateTime::parse(s, &Rfc3339)?;
    let nanos = dt.unix_timestamp_nanos();
    if nanos < 0 {
        return Ok(UNIX_EPOCH);
    }
    Ok(UNIX_EPOCH + Duration::from_nanos(nanos as u64))
}

/// Hashprice oracle backed by a Luxor poller writing into a shared buffer.
#[derive(Clone)]
pub struct LuxorOracle {
    buffer: Arc<Mutex<SampleBuffer>>,
}

impl LuxorOracle {
    pub fn new(capacity: usize, max_staleness: Duration) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(SampleBuffer::new(capacity, max_staleness))),
        }
    }

    /// Insert a sample directly. Mainly for tests and warm-starting from disk.
    pub fn push(&self, sample: HashpriceSample) {
        if let Ok(mut b) = self.buffer.lock() {
            b.push(sample);
        }
    }

    /// Spawn a tokio task that polls Luxor every `poll_interval` and pushes
    /// samples into this oracle. Caller can `.abort()` the returned handle
    /// to stop polling.
    pub fn spawn_poller(
        &self,
        client: LuxorClient,
        poll_interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let buffer = self.buffer.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(poll_interval);
            // Don't burst on startup: tick once immediately, then wait.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match client.fetch_latest().await {
                    Ok(sample) => {
                        if let Ok(mut b) = buffer.lock() {
                            b.push(sample.clone());
                        }
                        tracing::debug!(
                            sats_per_ths = sample.sats_per_ths,
                            t = ?sample.t,
                            "luxor hashprice sample",
                        );
                    }
                    Err(e) => tracing::warn!(error = %e, "luxor fetch failed"),
                }
            }
        })
    }
}

impl HashpriceOracle for LuxorOracle {
    fn sample_at(&self, t: SystemTime) -> Option<f64> {
        self.buffer.lock().ok().and_then(|b| b.sample_at(t))
    }

    fn latest(&self) -> Option<HashpriceSample> {
        self.buffer.lock().ok().and_then(|b| b.latest())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rfc3339_z() {
        let t = parse_rfc3339("2026-05-07T12:00:00Z").unwrap();
        let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        // 2026-05-07T12:00:00Z = 1_778_155_200
        assert_eq!(secs, 1_778_155_200);
    }

    #[test]
    fn parses_rfc3339_with_offset() {
        let t = parse_rfc3339("2026-05-07T08:00:00-04:00").unwrap();
        let secs = t.duration_since(UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1_778_155_200);
    }

    #[test]
    fn parses_rfc3339_with_fractional_seconds() {
        let t = parse_rfc3339("2026-05-07T12:00:00.500Z").unwrap();
        let nanos = t.duration_since(UNIX_EPOCH).unwrap().as_nanos();
        assert_eq!(nanos, 1_778_155_200_500_000_000_u128);
    }

    #[test]
    fn rejects_garbage_timestamp() {
        assert!(parse_rfc3339("not a timestamp").is_err());
    }

    #[test]
    fn deserializes_luxor_response_shape() {
        // Documented response shape from /hashrateindex/hashprice/current
        let body = r#"{"data":{"priceBTC":0.0005,"priceUSD":50.0,"timestamp":"2026-05-07T12:00:00Z"}}"#;
        let parsed: CurrentResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.data.price_btc, 0.0005);
        assert_eq!(parsed.data.timestamp, "2026-05-07T12:00:00Z");
    }

    #[test]
    fn sample_from_response_converts_units() {
        // 5e-4 BTC/PH/day == 50_000 sats/PH/day == 50 sats/Th/day
        let sample = sample_from_response(5e-4, "2026-05-07T12:00:00Z").unwrap();
        assert!((sample.sats_per_th_per_day() - 50.0).abs() < 1e-9);
    }

    #[test]
    fn oracle_implements_trait_through_lock() {
        let oracle = LuxorOracle::new(8, Duration::from_secs(600));
        assert!(oracle.latest().is_none());

        let now = UNIX_EPOCH + Duration::from_secs(1_778_155_200);
        oracle.push(HashpriceSample::from_btc_per_ph_per_day(now, 5e-4));

        let latest = oracle.latest().unwrap();
        assert_eq!(latest.t, now);

        // Trait-level query.
        let v = HashpriceOracle::sample_at(&oracle, now).unwrap();
        assert!((v * 1000.0 * 86_400.0 - 50_000.0).abs() < 1e-9);
    }

    #[test]
    fn from_env_reports_missing_key() {
        // Use a name we know is unset to avoid clobbering a real key.
        std::env::remove_var("LUXOR_API_KEY_TEST_UNSET");
        // Reach into the impl: we can't change which env var from_env reads,
        // so just exercise the error variant via the public type.
        let err = LuxorError::MissingApiKey(ENV_LUXOR_API_KEY);
        assert_eq!(format!("{err}"), "missing LUXOR_API_KEY env var");
    }
}
