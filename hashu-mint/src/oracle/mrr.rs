//! MiningRigRentals (MRR) rental-market hashprice adapter.
//! ARCHITECTURE.md §4.4.
//!
//! Prices hashrate at what it costs to *rent* SHA-256 hashrate right now,
//! independent of which chain it points at. That is the right price for a
//! Testnet4 mint: the block reward there is test sats, but the rigs mining it
//! are paid in real BTC. Pair it with the Esplora oracle's
//! [`real_difficulty`](super::esplora::real_difficulty) to turn a price into
//! the cost of one expected block.
//!
//! Reads the public, unauthenticated rig search
//! `GET /rig?type=<algo>&orderby=price&orderdir=asc&hash.type=ph` and takes the
//! floor: the lowest BTC price (BTC per PH per day, MRR's SHA-256 quote unit)
//! among rigs that are online, available and accept BTC. Same filter as
//! `fx-faucet/miner/btc-rent.py market`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use hashu_core::hashprice::{HashpriceOracle, HashpriceSample, SampleBuffer};
use serde_json::Value;

/// Default MRR API v2 base URL.
pub const DEFAULT_BASE_URL: &str = "https://www.miningrigrentals.com/api/v2";

/// Default MRR algorithm category. `sha256ab` (overt AsicBoost) is the deep,
/// liquid SHA-256 market where modern ASICs list; `sha256` is the older,
/// thinner plain market.
pub const DEFAULT_ALGO: &str = "sha256ab";

/// Default poll cadence. Listings move slowly; 5 minutes is plenty.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(300);

/// Rigs requested per page.
const PAGE_SIZE: usize = 100;

/// Pages walked looking for an eligible rig before giving up.
const MAX_PAGES: usize = 10;

#[derive(Debug, thiserror::Error)]
pub enum MrrError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("mrr returned status {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("mrr error: {0}")]
    Api(String),
    #[error("no online, available rig with a BTC price")]
    NoRigs,
}

/// One rental listing, normalised.
#[derive(Clone, Debug, PartialEq)]
pub struct Rig {
    pub id: u64,
    /// Advertised hashrate in PH/s.
    pub ph: f64,
    /// BTC per PH per day.
    pub btc_per_ph_day: f64,
    /// Minimum rental length in hours.
    pub min_hours: u32,
}

impl Rig {
    /// BTC cost of renting this rig for its minimum length.
    pub fn min_rental_btc(&self) -> f64 {
        self.btc_per_ph_day * self.ph / 24.0 * self.min_hours as f64
    }
}

/// Snapshot of the market's cheap end.
#[derive(Clone, Debug)]
pub struct MarketFloor {
    /// Cheapest eligible rig.
    pub floor: Rig,
    /// Eligible rigs seen while finding the floor.
    pub rigs_seen: usize,
    /// Total listings MRR reported for the query.
    pub total_listed: usize,
}

impl MarketFloor {
    /// The floor as a hashprice sample timestamped `t`.
    pub fn sample(&self, t: SystemTime) -> HashpriceSample {
        HashpriceSample::from_btc_per_ph_per_day(t, self.floor.btc_per_ph_day)
    }
}

/// HTTP client for MRR's public rig search.
#[derive(Clone)]
pub struct MrrClient {
    http: reqwest::Client,
    base: String,
    algo: String,
}

impl Default for MrrClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MrrClient {
    pub fn new() -> Self {
        Self::with_base(DEFAULT_BASE_URL.to_string(), DEFAULT_ALGO.to_string())
    }

    pub fn with_base(base: String, algo: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client build");
        Self { http, base, algo }
    }

    /// Walk the price-ascending rig list until an eligible rig appears.
    pub async fn fetch_floor(&self) -> Result<MarketFloor, MrrError> {
        let url = format!("{}/rig", self.base);
        let mut offset = 0usize;
        let mut seen = Vec::new();
        let mut total = 0usize;
        for _ in 0..MAX_PAGES {
            let resp = self
                .http
                .get(&url)
                .header("Accept", "application/json")
                .query(&[
                    ("type", self.algo.as_str()),
                    ("count", &PAGE_SIZE.to_string()),
                    ("offset", &offset.to_string()),
                    ("orderby", "price"),
                    ("orderdir", "asc"),
                    ("hash.type", "ph"),
                ])
                .send()
                .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(MrrError::Status { status, body });
            }
            let body: Value = resp.json().await?;
            let (records, page_total) = parse_page(&body)?;
            total = page_total;
            offset += records.len();
            seen.extend(records.iter().filter_map(normalise_rig));
            if !seen.is_empty() || records.is_empty() || offset >= total {
                break;
            }
        }
        floor_of(seen, total)
    }
}

fn parse_page(body: &Value) -> Result<(Vec<Value>, usize), MrrError> {
    if body.get("success").and_then(Value::as_bool) != Some(true) {
        let msg = body
            .pointer("/data/message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| body.to_string());
        return Err(MrrError::Api(msg));
    }
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    let records = data
        .get("records")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = data.get("total").and_then(num).unwrap_or(0.0) as usize;
    Ok((records, total))
}

fn floor_of(rigs: Vec<Rig>, total_listed: usize) -> Result<MarketFloor, MrrError> {
    let rigs_seen = rigs.len();
    let floor = rigs
        .into_iter()
        .min_by(|a, b| a.btc_per_ph_day.total_cmp(&b.btc_per_ph_day))
        .ok_or(MrrError::NoRigs)?;
    Ok(MarketFloor {
        floor,
        rigs_seen,
        total_listed,
    })
}

/// MRR sends most numbers as strings; accept either.
fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0),
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true" | "yes"),
        _ => false,
    }
}

fn unit_to_ph(unit: &str) -> Option<f64> {
    Some(match unit.to_ascii_lowercase().as_str() {
        "hash" => 1e-15,
        "kh" => 1e-12,
        "mh" => 1e-9,
        "gh" => 1e-6,
        "th" => 1e-3,
        "ph" => 1.0,
        "eh" => 1e3,
        _ => return None,
    })
}

/// Keep a rig only if it is online, available and has an enabled BTC price.
fn normalise_rig(r: &Value) -> Option<Rig> {
    let price = r.pointer("/price/BTC")?;
    if !truthy(price.get("enabled")) {
        return None;
    }
    if r.pointer("/status/status").and_then(Value::as_str) != Some("available") {
        return None;
    }
    if !truthy(r.get("online")) {
        return None;
    }
    let btc_per_ph_day = num(price.get("price")?)?;
    let adv = r.pointer("/hashrate/advertised")?;
    let unit = adv.get("type").and_then(Value::as_str).unwrap_or("ph");
    let ph = num(adv.get("hash")?)? * unit_to_ph(unit)?;
    if ph <= 0.0 || btc_per_ph_day <= 0.0 {
        return None;
    }
    let min_hours = r
        .get("minhours")
        .and_then(num)
        .or_else(|| price.get("min_rental_length").and_then(num))
        .unwrap_or(3.0) as u32;
    Some(Rig {
        id: r.get("id").and_then(num)? as u64,
        ph,
        btc_per_ph_day,
        min_hours,
    })
}

/// Hashprice oracle backed by an MRR poller writing into a shared buffer.
#[derive(Clone)]
pub struct MrrOracle {
    buffer: Arc<Mutex<SampleBuffer>>,
}

impl MrrOracle {
    pub fn new(capacity: usize, max_staleness: Duration) -> Self {
        Self {
            buffer: Arc::new(Mutex::new(SampleBuffer::new(capacity, max_staleness))),
        }
    }

    /// Insert a sample directly. Mainly for tests and warm-starting.
    pub fn push(&self, sample: HashpriceSample) {
        if let Ok(mut b) = self.buffer.lock() {
            b.push(sample);
        }
    }

    /// Spawn a tokio task that reads the MRR floor every `poll_interval`.
    pub fn spawn_poller(
        &self,
        client: MrrClient,
        poll_interval: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let buffer = self.buffer.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(poll_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                match client.fetch_floor().await {
                    Ok(floor) => {
                        let sample = floor.sample(SystemTime::now());
                        if let Ok(mut b) = buffer.lock() {
                            b.push(sample.clone());
                        }
                        tracing::debug!(
                            btc_per_ph_day = floor.floor.btc_per_ph_day,
                            rig = floor.floor.id,
                            "mrr hashprice sample",
                        );
                    }
                    Err(e) => tracing::warn!(error = %e, "mrr fetch failed"),
                }
            }
        })
    }
}

impl HashpriceOracle for MrrOracle {
    fn sample_at(&self, t: SystemTime) -> Option<f64> {
        self.buffer.lock().ok().and_then(|b| b.sample_at(t))
    }

    fn latest(&self) -> Option<HashpriceSample> {
        self.buffer.lock().ok().and_then(|b| b.latest())
    }
}

// ---- account: active rentals (signed API) -------------------------------

/// An active rental on the account, as capacity for the mint.
#[derive(Clone, Debug, PartialEq)]
pub struct ActiveRental {
    pub id: String,
    pub rig_name: String,
    /// Advertised (paid-for) hashrate, TH/s.
    pub ths: f64,
    /// When the rental ends, unix seconds.
    pub end_unix: f64,
}

/// Signed MRR API v2 client for the renter's own account. Every call carries
/// `x-api-key`, a strictly increasing `x-api-nonce` and
/// `x-api-sign = HMAC-SHA1(secret, key + nonce + endpoint)`, the same scheme
/// as `fx-faucet/miner/btc-rent.py`.
pub struct MrrAccount {
    http: reqwest::Client,
    base: String,
    algo: String,
    key: String,
    secret: String,
    last_nonce: Mutex<u64>,
}

impl std::fmt::Debug for MrrAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MrrAccount")
            .field("base", &self.base)
            .field("algo", &self.algo)
            .finish_non_exhaustive()
    }
}

impl MrrAccount {
    pub fn new(key: String, secret: String) -> Self {
        Self::with_base(DEFAULT_BASE_URL.into(), DEFAULT_ALGO.into(), key, secret)
    }

    pub fn with_base(base: String, algo: String, key: String, secret: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .expect("reqwest client build");
        Self {
            http,
            base,
            algo,
            key,
            secret,
            last_nonce: Mutex::new(0),
        }
    }

    fn nonce(&self) -> u64 {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_micros() as u64)
            .unwrap_or_default();
        let mut last = self.last_nonce.lock().unwrap();
        *last = now.max(*last + 1);
        *last
    }

    fn sign(&self, nonce: u64, endpoint: &str) -> String {
        use hmac::{Hmac, Mac};
        let mut mac = Hmac::<sha1::Sha1>::new_from_slice(self.secret.as_bytes())
            .expect("hmac takes any key length");
        mac.update(format!("{}{nonce}{endpoint}", self.key).as_bytes());
        hex::encode(mac.finalize().into_bytes())
    }

    /// `GET /rental?type=active`: rentals on this account still running.
    pub async fn active_rentals(&self) -> Result<Vec<ActiveRental>, MrrError> {
        let endpoint = "/rental";
        let nonce = self.nonce();
        let resp = self
            .http
            .get(format!("{}{endpoint}", self.base))
            .header("Accept", "application/json")
            .header("x-api-key", &self.key)
            .header("x-api-nonce", nonce.to_string())
            .header("x-api-sign", self.sign(nonce, endpoint))
            .query(&[
                ("type", "active"),
                ("algo", self.algo.as_str()),
                ("limit", "100"),
            ])
            .send()
            .await?;
        let status = resp.status();
        let body: Value = match resp.json().await {
            Ok(b) => b,
            Err(_) if !status.is_success() => {
                return Err(MrrError::Status {
                    status,
                    body: String::new(),
                })
            }
            Err(e) => return Err(e.into()),
        };
        parse_rentals(&body)
    }
}

fn parse_rentals(body: &Value) -> Result<Vec<ActiveRental>, MrrError> {
    if body.get("success").and_then(Value::as_bool) != Some(true) {
        let msg = body
            .pointer("/data/message")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| body.to_string());
        return Err(MrrError::Api(msg));
    }
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    let list = data
        .get("rentals")
        .and_then(Value::as_array)
        .or_else(|| data.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(list.iter().filter_map(normalise_rental).collect())
}

fn normalise_rental(r: &Value) -> Option<ActiveRental> {
    if truthy(r.get("ended")) {
        return None;
    }
    let adv = r.pointer("/hashrate/advertised")?;
    let unit = adv.get("type").and_then(Value::as_str).unwrap_or("th");
    let ths = num(adv.get("hash")?)? * unit_to_ph(unit)? * 1e3;
    let end_unix = r.get("end_unix").and_then(num).or_else(|| {
        // Some records only carry hours left.
        let left = r
            .get("length_left")
            .or_else(|| r.get("remaining"))
            .and_then(num)?;
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        Some(now + left * 3600.0)
    })?;
    if !ths.is_finite() || ths <= 0.0 {
        return None;
    }
    let id = match r.get("id")? {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    };
    let rig_name = r
        .pointer("/rig/name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    Some(ActiveRental {
        id,
        rig_name,
        ths,
        end_unix,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rig(id: u64, price: &str, hash: &str, unit: &str, online: bool, status: &str) -> Value {
        json!({
            "id": id.to_string(),
            "online": online,
            "minhours": "3",
            "status": { "status": status },
            "price": { "BTC": { "enabled": true, "price": price } },
            "hashrate": { "advertised": { "hash": hash, "type": unit } },
        })
    }

    #[test]
    fn floor_skips_offline_rented_and_non_btc_rigs() {
        let mut no_btc = rig(4, "0.00010", "1", "ph", true, "available");
        no_btc["price"]["BTC"]["enabled"] = json!(false);
        let body = json!({
            "success": true,
            "data": { "total": "5", "records": [
                rig(1, "0.00020", "1", "ph", false, "available"),
                rig(2, "0.00030", "1", "ph", true, "rented"),
                no_btc,
                rig(5, "0.00045", "430", "th", true, "available"),
                rig(6, "0.00050", "2", "ph", true, "available"),
            ]}
        });
        let (records, total) = parse_page(&body).unwrap();
        assert_eq!(total, 5);
        let rigs: Vec<Rig> = records.iter().filter_map(normalise_rig).collect();
        let floor = floor_of(rigs, total).unwrap();
        assert_eq!(floor.rigs_seen, 2);
        assert_eq!(floor.floor.id, 5);
        assert!((floor.floor.ph - 0.43).abs() < 1e-12);
        assert_eq!(floor.floor.btc_per_ph_day, 0.00045);
    }

    #[test]
    fn floor_converts_to_hashprice() {
        let floor = floor_of(
            vec![Rig {
                id: 1,
                ph: 0.19,
                btc_per_ph_day: 0.00045,
                min_hours: 24,
            }],
            1,
        )
        .unwrap();
        let s = floor.sample(SystemTime::UNIX_EPOCH);
        assert!((s.btc_per_ph_per_day() - 0.00045).abs() < 1e-12);
        assert!((s.sats_per_ph_per_day() - 45_000.0).abs() < 1e-6);
        // 0.19 PH/s for 24h at the floor.
        assert!((floor.floor.min_rental_btc() - 0.0000855).abs() < 1e-12);
    }

    #[test]
    fn api_error_is_reported() {
        let body = json!({ "success": false, "data": { "message": "Bad type" } });
        assert!(matches!(parse_page(&body), Err(MrrError::Api(m)) if m == "Bad type"));
    }

    #[test]
    fn empty_market_is_no_rigs() {
        assert!(matches!(floor_of(vec![], 0), Err(MrrError::NoRigs)));
    }

    #[test]
    fn parses_active_rentals() {
        let body = json!({"success": true, "data": {"total": "2", "rentals": [
            {"id": "123", "rig": {"name": " S19 "}, "hashrate": {"advertised": {"hash": "0.1", "type": "ph"}}, "end_unix": "1800000000"},
            {"id": 124, "ended": true, "hashrate": {"advertised": {"hash": "90", "type": "th"}}, "end_unix": "1"},
            {"id": 125, "hashrate": {"advertised": {"hash": "500", "type": "gh"}}, "end_unix": 1800003600}
        ]}});
        let r = parse_rentals(&body).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].id, "123");
        assert_eq!(r[0].rig_name, "S19");
        assert!((r[0].ths - 100.0).abs() < 1e-9);
        assert_eq!(r[0].end_unix, 1_800_000_000.0);
        assert_eq!(r[1].id, "125");
        assert!((r[1].ths - 0.5).abs() < 1e-9);
        let err = parse_rentals(&json!({"success": false, "data": {"message": "Bad Nonce"}}));
        assert!(matches!(err, Err(MrrError::Api(m)) if m == "Bad Nonce"));
    }

    #[test]
    fn signs_like_btc_rent() {
        let a = MrrAccount::new("key".into(), "secret".into());
        // python3 -c "import hmac,hashlib;print(hmac.new(b'secret',b'key1/rental',hashlib.sha1).hexdigest())"
        assert_eq!(
            a.sign(1, "/rental"),
            "cf0e9cfe875177e377b41e80640d1ee924806c10"
        );
    }
}
