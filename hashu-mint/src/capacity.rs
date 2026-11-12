//! How many `THH` the mint can still sell.
//!
//! A Hashu mint's hashrate comes from rented machines (MiningRigRentals rigs
//! pointed at the proxy). Each rental delivers its hashrate until it ends, so
//! what the mint can still deliver is
//!
//! ```text
//! capacity_thh  = Σ rental TH/s × hours left
//! available_thh = capacity_thh − THH already owed
//! ```
//!
//! where "owed" is ecash still in holders' hands, paid-but-unminted quotes
//! and the undelivered rest of open redemptions (the operator API's
//! obligations). A mint quote for more than `available_thh` is refused.
//!
//! Rentals come from the operator (control API / GUI) and, when an MRR API
//! key is configured, from the account's active MRR rentals. Both are kept
//! in `capacity.json` in the data dir so a restart keeps the limit.
//!
//! With no rentals configured nothing can be sold: a mint must not take
//! payment for hashrate it has no machines to deliver. The operator can
//! turn the limit off explicitly (`limit: false`) to sell uncapped.
//!
//! `capacity_thh` is *declared* capacity (rentals × hours left), not the
//! hashrate the proxy measures right now. And because THH amounts are whole
//! numbers, headroom below [`MIN_PURCHASE_THH`] cannot be sold: a 1 TH/s
//! rental with 55 minutes left is 0.92 THH of capacity and 0 available.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::pricing::PricingError;

/// Smallest purchase: Cashu amounts are integers in the `THH` unit, so a
/// buyer gets at least 1 THH and fractional headroom is unsellable.
pub const MIN_PURCHASE_THH: u64 = 1;

const MS_PER_HOUR: f64 = 3_600_000.0;

/// One rented machine (or group of machines) mining on the proxy.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rental {
    /// Free-form id: the MRR rental id, or one the operator chose.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub label: String,
    /// Hashrate in TH/s.
    pub ths: f64,
    /// When the rental stops mining, unix ms.
    pub end_ms: u64,
}

impl Rental {
    pub fn hours_left(&self, now_ms: u64) -> f64 {
        self.end_ms.saturating_sub(now_ms) as f64 / MS_PER_HOUR
    }
    /// THH this rental can still deliver.
    pub fn thh_left(&self, now_ms: u64) -> f64 {
        self.ths.max(0.0) * self.hours_left(now_ms)
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Persisted {
    /// `None` (auto): the limit is on, so without rentals nothing is sold.
    #[serde(default)]
    limit: Option<bool>,
    #[serde(default)]
    manual: Vec<Rental>,
    #[serde(default)]
    mrr: Vec<Rental>,
    #[serde(default)]
    mrr_synced_ms: Option<u64>,
}

#[derive(Debug, Default)]
struct Inner {
    p: Persisted,
    mrr_enabled: bool,
    mrr_error: Option<String>,
    owed_thh: f64,
    owed_ms: Option<u64>,
}

/// Rented capacity and what is already owed against it.
#[derive(Debug, Default)]
pub struct CapacityBook {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
}

/// What the GUI and checkout show.
#[derive(Debug, Clone, Serialize)]
pub struct CapacitySnapshot {
    /// Whether quotes beyond `available_thh` are refused.
    pub limit: bool,
    /// The operator set `limit` explicitly (otherwise it is on).
    pub limit_explicit: bool,
    pub rentals: Vec<RentalView>,
    /// TH/s of rentals still running.
    pub rented_ths: f64,
    /// Latest end among running rentals.
    pub ends_ms: Option<u64>,
    pub capacity_thh: f64,
    pub owed_thh: f64,
    pub owed_updated_ms: Option<u64>,
    /// `capacity − owed`, negative when oversold.
    pub headroom_thh: f64,
    /// Whole THH a buyer can still get; `None` while the limit is off.
    pub available_thh: Option<u64>,
    /// Capped, with some headroom but less than [`MIN_PURCHASE_THH`]: the
    /// mint reads sold out only because THH is sold in whole units.
    pub below_min: bool,
    pub mrr_enabled: bool,
    pub mrr_synced_ms: Option<u64>,
    pub mrr_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RentalView {
    pub source: &'static str,
    #[serde(flatten)]
    pub rental: Rental,
    pub hours_left: f64,
    pub thh_left: f64,
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl CapacityBook {
    /// In memory only, limit on: nothing to sell until a rental is set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load `path` if it exists; changes are written back to it.
    pub fn load(path: PathBuf) -> anyhow::Result<Self> {
        let p = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
        };
        Ok(Self {
            inner: Mutex::new(Inner {
                p,
                ..Default::default()
            }),
            path: Some(path),
        })
    }

    fn save(&self, p: &Persisted) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        let r = serde_json::to_vec_pretty(p)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            tracing::warn!(path = %path.display(), error = %e, "could not save capacity");
        }
    }

    /// Replace the operator's rentals.
    pub fn set_manual(&self, rentals: Vec<Rental>) {
        let mut g = self.inner.lock().unwrap();
        g.p.manual = rentals;
        self.save(&g.p);
    }

    /// Turn the limit on or off, or back to automatic with `None`.
    pub fn set_limit(&self, limit: Option<bool>) {
        let mut g = self.inner.lock().unwrap();
        g.p.limit = limit;
        self.save(&g.p);
    }

    /// MRR sync is configured: the limit turns on even before the first sync.
    pub fn enable_mrr(&self) {
        self.inner.lock().unwrap().mrr_enabled = true;
    }

    /// Result of one MRR sync. On error the last good list is kept.
    pub fn set_mrr(&self, result: Result<Vec<Rental>, String>, now_ms: u64) {
        let mut g = self.inner.lock().unwrap();
        match result {
            Ok(rentals) => {
                g.p.mrr = rentals;
                g.p.mrr_synced_ms = Some(now_ms);
                g.mrr_error = None;
                self.save(&g.p);
            }
            Err(e) => g.mrr_error = Some(e),
        }
    }

    /// THH already owed (from the mint's obligations).
    pub fn set_owed(&self, owed_thh: f64, now_ms: u64) {
        let mut g = self.inner.lock().unwrap();
        g.owed_thh = owed_thh.max(0.0);
        g.owed_ms = Some(now_ms);
    }

    pub fn snapshot(&self, now_ms: u64) -> CapacitySnapshot {
        let g = self.inner.lock().unwrap();
        // Auto means on: no rented hashrate, nothing to sell.
        let limit = g.p.limit.unwrap_or(true);
        let rentals: Vec<RentalView> =
            g.p.manual
                .iter()
                .map(|r| ("manual", r))
                .chain(g.p.mrr.iter().map(|r| ("mrr", r)))
                .map(|(source, r)| RentalView {
                    source,
                    rental: r.clone(),
                    hours_left: r.hours_left(now_ms),
                    thh_left: r.thh_left(now_ms),
                })
                .collect();
        let running = rentals.iter().filter(|r| r.rental.end_ms > now_ms);
        let rented_ths = running.clone().map(|r| r.rental.ths.max(0.0)).sum();
        let ends_ms = running.map(|r| r.rental.end_ms).max();
        let capacity_thh: f64 = rentals.iter().map(|r| r.thh_left).sum();
        let headroom_thh = capacity_thh - g.owed_thh;
        CapacitySnapshot {
            limit,
            limit_explicit: g.p.limit.is_some(),
            rentals,
            rented_ths,
            ends_ms,
            capacity_thh,
            owed_thh: g.owed_thh,
            owed_updated_ms: g.owed_ms,
            headroom_thh,
            available_thh: limit.then(|| headroom_thh.max(0.0).floor() as u64),
            below_min: limit && headroom_thh > 0.0 && headroom_thh < MIN_PURCHASE_THH as f64,
            mrr_enabled: g.mrr_enabled,
            mrr_synced_ms: g.p.mrr_synced_ms,
            mrr_error: g.mrr_error.clone(),
        }
    }

    /// Refuse a quote for more THH than the rentals can still deliver.
    pub fn check(&self, amount_thh: u64, now_ms: u64) -> Result<(), PricingError> {
        match self.snapshot(now_ms).available_thh {
            Some(available) if amount_thh > available => Err(PricingError::SoldOut {
                requested: amount_thh,
                available,
            }),
            _ => Ok(()),
        }
    }
}

/// Parse an operator rental: `{ "ths", "end_ms" }` or `{ "ths", "hours" }`
/// (from now), with optional `id` and `label`.
pub fn rental_from_json(v: &serde_json::Value, now_ms: u64) -> Result<Rental, String> {
    let ths = v
        .get("ths")
        .and_then(|x| x.as_f64())
        .filter(|x| x.is_finite() && *x >= 0.0)
        .ok_or("each rental needs ths, a non-negative number of TH/s")?;
    let end_ms = match (v.get("end_ms").and_then(|x| x.as_u64()), v.get("hours")) {
        (Some(end), _) => end,
        (None, Some(h)) => {
            let h = h
                .as_f64()
                .filter(|h| h.is_finite() && *h >= 0.0 && *h < 24.0 * 366.0)
                .ok_or("hours must be a non-negative number")?;
            now_ms + (h * MS_PER_HOUR) as u64
        }
        (None, None) => return Err("each rental needs end_ms or hours".into()),
    };
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    Ok(Rental {
        id: s("id"),
        label: s("label"),
        ths,
        end_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3_600_000;

    fn rental(ths: f64, end_ms: u64) -> Rental {
        Rental {
            id: String::new(),
            label: String::new(),
            ths,
            end_ms,
        }
    }

    #[test]
    fn headroom_under_one_thh_is_below_min() {
        // A 1 TH/s rental with ~56 minutes left: 0.93 THH declared, 0 to sell.
        let b = CapacityBook::new();
        b.set_manual(vec![rental(1.0, 3_344_400)]);
        let s = b.snapshot(0);
        assert!((s.headroom_thh - 0.929).abs() < 1e-9);
        assert_eq!(s.available_thh, Some(0));
        assert!(s.below_min);
        // Nothing rented, or a whole THH or more, is not "below the minimum".
        assert!(!CapacityBook::new().snapshot(0).below_min);
        b.set_manual(vec![rental(1.0, 2 * H)]);
        assert!(!b.snapshot(0).below_min);
        // Uncapped never reads sold out.
        b.set_manual(vec![rental(1.0, 3_344_400)]);
        b.set_limit(Some(false));
        assert!(!b.snapshot(0).below_min);
    }

    #[test]
    fn no_rentals_means_nothing_to_sell() {
        let b = CapacityBook::new();
        let s = b.snapshot(0);
        assert!(s.limit);
        assert_eq!(s.available_thh, Some(0));
        assert_eq!(
            b.check(1, 0),
            Err(PricingError::SoldOut {
                requested: 1,
                available: 0
            })
        );
        // Only an explicit "off" sells without rentals.
        b.set_limit(Some(false));
        assert_eq!(b.snapshot(0).available_thh, None);
        assert!(b.check(1_000_000, 0).is_ok());
    }

    #[test]
    fn capacity_is_hashrate_times_time_left_minus_owed() {
        let b = CapacityBook::new();
        // 100 TH/s for 3 h, 50 TH/s for 1 h, one already over.
        b.set_manual(vec![
            rental(100.0, 3 * H),
            rental(50.0, H),
            rental(999.0, 0),
        ]);
        b.set_owed(120.0, 0);
        let s = b.snapshot(0);
        assert!(s.limit);
        assert_eq!(s.rented_ths, 150.0);
        assert_eq!(s.ends_ms, Some(3 * H));
        assert!((s.capacity_thh - 350.0).abs() < 1e-9);
        assert_eq!(s.available_thh, Some(230));
        assert!(b.check(230, 0).is_ok());
        assert_eq!(
            b.check(231, 0),
            Err(PricingError::SoldOut {
                requested: 231,
                available: 230
            })
        );
        // An hour later: 200 THH left, 120 owed.
        assert_eq!(b.snapshot(H).available_thh, Some(80));
        // After every rental ends nothing can be sold.
        assert_eq!(b.snapshot(4 * H).available_thh, Some(0));
        assert!(b.check(1, 4 * H).is_err());
    }

    #[test]
    fn oversold_shows_negative_headroom() {
        let b = CapacityBook::new();
        b.set_manual(vec![rental(10.0, H)]);
        b.set_owed(25.0, 0);
        let s = b.snapshot(0);
        assert!((s.headroom_thh + 15.0).abs() < 1e-9);
        assert_eq!(s.available_thh, Some(0));
    }

    #[test]
    fn explicit_limit_overrides_auto() {
        let b = CapacityBook::new();
        b.set_manual(vec![rental(10.0, H)]);
        b.set_limit(Some(false));
        assert!(b.check(1_000, 0).is_ok());
        b.set_limit(None);
        assert!(b.check(1_000, 0).is_err());
        // MRR configured but not yet synced: limit on, nothing to sell.
        let m = CapacityBook::new();
        m.enable_mrr();
        assert_eq!(m.snapshot(0).available_thh, Some(0));
    }

    #[test]
    fn mrr_error_keeps_last_list() {
        let b = CapacityBook::new();
        b.set_mrr(Ok(vec![rental(40.0, 2 * H)]), 0);
        b.set_mrr(Err("Bad Nonce".into()), 1);
        let s = b.snapshot(0);
        assert_eq!(s.rented_ths, 40.0);
        assert_eq!(s.mrr_error.as_deref(), Some("Bad Nonce"));
        assert_eq!(s.mrr_synced_ms, Some(0));
    }

    #[test]
    fn persists_across_loads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capacity.json");
        let b = CapacityBook::load(path.clone()).unwrap();
        b.set_manual(vec![rental(12.5, 5 * H)]);
        b.set_limit(Some(true));
        let again = CapacityBook::load(path).unwrap();
        let s = again.snapshot(0);
        assert!(s.limit_explicit);
        assert!((s.capacity_thh - 62.5).abs() < 1e-9);
    }

    #[test]
    fn parses_hours_or_end() {
        let r = rental_from_json(
            &serde_json::json!({"ths": 40, "hours": 2.5, "label": "bitaxe"}),
            1_000,
        )
        .unwrap();
        assert_eq!(r.end_ms, 1_000 + 9_000_000);
        assert_eq!(r.label, "bitaxe");
        let r = rental_from_json(&serde_json::json!({"ths": 1.5, "end_ms": 77}), 0).unwrap();
        assert_eq!(r.end_ms, 77);
        assert!(rental_from_json(&serde_json::json!({"ths": -1, "hours": 1}), 0).is_err());
        assert!(rental_from_json(&serde_json::json!({"ths": 1}), 0).is_err());
    }
}
