//! How many `TiH` the mint can still sell.
//!
//! A Hashu mint's hashrate comes from rented machines (MiningRigRentals rigs
//! pointed at the proxy). Each rental delivers its hashrate until it ends, so
//! what the mint can still deliver is
//!
//! ```text
//! capacity_tih  = Σ rental TH/s × hours left × 3,274.2 TiH per THH
//! available_tih = capacity_tih − TiH already owed
//! ```
//!
//! where "owed" is ecash still in holders' hands, paid-but-unminted quotes
//! and the undelivered rest of open redemptions (the operator API's
//! obligations). A mint quote for more than `available_tih` is refused.
//!
//! Rentals come from the operator (control API / GUI) and, when an MRR API
//! key is configured, from the account's active MRR rentals. Both are kept
//! in `capacity.json` in the data dir so a restart keeps the limit.
//!
//! With no rentals configured nothing can be sold: a mint must not take
//! payment for hashrate it has no machines to deliver. The operator can
//! turn the limit off explicitly (`limit: false`) to sell uncapped.
//!
//! The operator can also count the mint's own connected rigs (a Bitaxe at
//! home, say) by setting a live horizon: their measured hashrate × that
//! horizon is added, the same rule supplier mints sell by. To avoid counting
//! rented rigs twice (they mine on the proxy too), only measured hashrate
//! above the running rentals' declared TH/s is counted:
//!
//! ```text
//! live_tih = max(0, measured TH/s − rented TH/s) × live horizon (in TiH)
//! ```
//!
//! Live capacity rolls forward: it never ends, but it drops as soon as the
//! rigs go quiet, so a mint that sold against it can end up oversold.
//!
//! Rental `capacity_tih` is *declared* capacity (rentals × hours left), not the
//! hashrate the proxy measures right now. Headroom below
//! [`MIN_PURCHASE_TIH`] (2^12 TiH ≈ 1.25 THH) cannot be sold: a 1 TH/s rental
//! with 55 minutes left is ≈ 3,000 TiH of capacity and nothing to sell.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hashu_core::denom::tih_from_ths_hours;
use serde::{Deserialize, Serialize};

use crate::pricing::PricingError;

/// Smallest purchase: 2^12 TiH (≈ 1.25 THH, #90), the power of two closest
/// to the old 1 THH floor. Headroom below it is unsellable.
pub const MIN_PURCHASE_TIH: u64 = 1 << 12;

const MS_PER_HOUR: f64 = 3_600_000.0;

/// Shortest live horizon, the same floor supplier mints use.
pub const MIN_LIVE_HORIZON_HOURS: f64 = 1.0;

/// Measured hashrate (TH/s) of the rigs that fill this mint's orders.
pub type LiveHashrate = Arc<dyn Fn() -> f64 + Send + Sync>;

struct LiveSource(LiveHashrate);

impl std::fmt::Debug for LiveSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LiveSource")
    }
}

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
    /// TiH this rental can still deliver.
    pub fn tih_left(&self, now_ms: u64) -> f64 {
        tih_from_ths_hours(self.ths.max(0.0), self.hours_left(now_ms))
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
    /// Count connected rigs' live hashrate × this many hours; `None` is off.
    #[serde(default)]
    live_horizon_hours: Option<f64>,
}

#[derive(Debug, Default)]
struct Inner {
    p: Persisted,
    mrr_enabled: bool,
    mrr_error: Option<String>,
    owed_tih: f64,
    owed_ms: Option<u64>,
}

/// Rented capacity and what is already owed against it.
#[derive(Debug, Default)]
pub struct CapacityBook {
    inner: Mutex<Inner>,
    live: Mutex<Option<LiveSource>>,
    path: Option<PathBuf>,
}

/// What the GUI and checkout show.
#[derive(Debug, Clone, Serialize)]
pub struct CapacitySnapshot {
    /// Whether quotes beyond `available_tih` are refused.
    pub limit: bool,
    /// The operator set `limit` explicitly (otherwise it is on).
    pub limit_explicit: bool,
    pub rentals: Vec<RentalView>,
    /// TH/s of rentals still running.
    pub rented_ths: f64,
    /// Latest end among running rentals.
    pub ends_ms: Option<u64>,
    pub capacity_tih: f64,
    pub owed_tih: f64,
    pub owed_updated_ms: Option<u64>,
    /// `capacity − owed`, negative when oversold.
    pub headroom_tih: f64,
    /// Whole TiH a buyer can still get; `None` while the limit is off.
    pub available_tih: Option<u64>,
    /// Capped, with some headroom but less than [`MIN_PURCHASE_TIH`]: the
    /// mint reads sold out only because TiH is sold in whole units.
    pub below_min: bool,
    pub mrr_enabled: bool,
    pub mrr_synced_ms: Option<u64>,
    pub mrr_error: Option<String>,
    /// The mint's own connected rigs (see the module docs).
    pub live: LiveView,
}

/// Live hashrate × horizon, the part of capacity that is measured.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LiveView {
    /// The proxy reports hashrate to this mint (false in tests or tools).
    pub available: bool,
    /// Counted in capacity: a horizon is set.
    pub on: bool,
    pub horizon_hours: Option<f64>,
    /// Measured TH/s of the rigs that fill this mint's orders.
    pub measured_ths: f64,
    /// Of that, TH/s already declared by running rentals.
    pub rented_ths: f64,
    /// `max(0, measured − rented)`: what the horizon multiplies.
    pub extra_ths: f64,
    /// TiH this adds to capacity (0 while off).
    pub tih: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RentalView {
    pub source: &'static str,
    #[serde(flatten)]
    pub rental: Rental,
    pub hours_left: f64,
    pub tih_left: f64,
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
            live: Mutex::new(None),
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

    /// MRR sync stopped or switched account: forget its rentals, so they
    /// stop counting. `enabled` says whether a new sync is starting.
    pub fn reset_mrr(&self, enabled: bool) {
        let mut g = self.inner.lock().unwrap();
        g.mrr_enabled = enabled;
        g.mrr_error = None;
        g.p.mrr.clear();
        g.p.mrr_synced_ms = None;
        self.save(&g.p);
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

    /// Where live hashrate comes from (the proxy's operator rigs, TH/s).
    pub fn set_live_source(&self, f: LiveHashrate) {
        *self.live.lock().unwrap() = Some(LiveSource(f));
    }

    /// Count live hashrate × `hours`, or stop counting it with `None`.
    pub fn set_live_horizon(&self, hours: Option<f64>) -> Result<(), String> {
        if let Some(h) = hours {
            if !h.is_finite() || h < MIN_LIVE_HORIZON_HOURS {
                return Err(format!(
                    "live horizon must be at least {MIN_LIVE_HORIZON_HOURS} hour"
                ));
            }
        }
        let mut g = self.inner.lock().unwrap();
        g.p.live_horizon_hours = hours;
        self.save(&g.p);
        Ok(())
    }

    /// TiH already owed (from the mint's obligations).
    pub fn set_owed(&self, owed_tih: f64, now_ms: u64) {
        let mut g = self.inner.lock().unwrap();
        g.owed_tih = owed_tih.max(0.0);
        g.owed_ms = Some(now_ms);
    }

    pub fn snapshot(&self, now_ms: u64) -> CapacitySnapshot {
        // Read the proxy before taking our own lock.
        let measured = self.live.lock().unwrap().as_ref().map(|LiveSource(f)| f());
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
                    tih_left: r.tih_left(now_ms),
                })
                .collect();
        let running = rentals.iter().filter(|r| r.rental.end_ms > now_ms);
        let rented_ths: f64 = running.clone().map(|r| r.rental.ths.max(0.0)).sum();
        let ends_ms = running.map(|r| r.rental.end_ms).max();
        let live = live_view(measured, rented_ths, g.p.live_horizon_hours);
        let capacity_tih: f64 = rentals.iter().map(|r| r.tih_left).sum::<f64>() + live.tih;
        let headroom_tih = capacity_tih - g.owed_tih;
        CapacitySnapshot {
            limit,
            limit_explicit: g.p.limit.is_some(),
            rentals,
            rented_ths,
            ends_ms,
            capacity_tih,
            owed_tih: g.owed_tih,
            owed_updated_ms: g.owed_ms,
            headroom_tih,
            available_tih: limit.then(|| {
                let a = headroom_tih.max(0.0).floor() as u64;
                if a < MIN_PURCHASE_TIH {
                    0
                } else {
                    a
                }
            }),
            below_min: limit && headroom_tih > 0.0 && headroom_tih < MIN_PURCHASE_TIH as f64,
            mrr_enabled: g.mrr_enabled,
            mrr_synced_ms: g.p.mrr_synced_ms,
            mrr_error: g.mrr_error.clone(),
            live,
        }
    }

    /// Refuse a quote for more TiH than the rentals can still deliver.
    pub fn check(&self, amount_tih: u64, now_ms: u64) -> Result<(), PricingError> {
        match self.snapshot(now_ms).available_tih {
            Some(available) if amount_tih > available => Err(PricingError::SoldOut {
                requested: amount_tih,
                available,
            }),
            _ => Ok(()),
        }
    }
}

fn live_view(measured_ths: Option<f64>, rented_ths: f64, horizon: Option<f64>) -> LiveView {
    let measured = measured_ths
        .filter(|t| t.is_finite())
        .unwrap_or(0.0)
        .max(0.0);
    let extra = (measured - rented_ths).max(0.0);
    let on = horizon.is_some() && measured_ths.is_some();
    LiveView {
        available: measured_ths.is_some(),
        on,
        horizon_hours: horizon,
        measured_ths: measured,
        rented_ths: rented_ths.min(measured),
        extra_ths: extra,
        tih: if on {
            tih_from_ths_hours(extra, horizon.unwrap_or(0.0))
        } else {
            0.0
        },
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
    /// TiH in one TH/s for an hour.
    const T: f64 = hashu_core::denom::TIH_PER_THH;

    fn rental(ths: f64, end_ms: u64) -> Rental {
        Rental {
            id: String::new(),
            label: String::new(),
            ths,
            end_ms,
        }
    }

    #[test]
    fn headroom_under_one_tih_is_below_min() {
        // A 1 TH/s rental with ~56 minutes left: 0.929 THH ≈ 3,042 TiH
        // declared, under the 4,096 TiH minimum: 0 to sell.
        let b = CapacityBook::new();
        b.set_manual(vec![rental(1.0, 3_344_400)]);
        let s = b.snapshot(0);
        assert!((s.headroom_tih - 0.929 * T).abs() < 1e-6);
        assert_eq!(s.available_tih, Some(0));
        assert!(s.below_min);
        // Nothing rented, or the minimum or more, is not "below the minimum".
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
        assert_eq!(s.available_tih, Some(0));
        assert_eq!(
            b.check(1, 0),
            Err(PricingError::SoldOut {
                requested: 1,
                available: 0
            })
        );
        // Only an explicit "off" sells without rentals.
        b.set_limit(Some(false));
        assert_eq!(b.snapshot(0).available_tih, None);
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
        // 350 THH ≈ 1,145,963 TiH; 120 THH owed.
        b.set_owed(120.0 * T, 0);
        let s = b.snapshot(0);
        assert!(s.limit);
        assert_eq!(s.rented_ths, 150.0);
        assert_eq!(s.ends_ms, Some(3 * H));
        assert!((s.capacity_tih - 350.0 * T).abs() < 1e-6);
        let avail = (230.0 * T).floor() as u64;
        assert_eq!(s.available_tih, Some(avail));
        assert!(b.check(avail, 0).is_ok());
        assert_eq!(
            b.check(avail + 1, 0),
            Err(PricingError::SoldOut {
                requested: avail + 1,
                available: avail
            })
        );
        // An hour later: 200 THH left, 120 owed.
        assert_eq!(b.snapshot(H).available_tih, Some((80.0 * T).floor() as u64));
        // After every rental ends nothing can be sold.
        assert_eq!(b.snapshot(4 * H).available_tih, Some(0));
        assert!(b.check(1, 4 * H).is_err());
    }

    #[test]
    fn oversold_shows_negative_headroom() {
        let b = CapacityBook::new();
        b.set_manual(vec![rental(10.0, H)]);
        b.set_owed(25.0 * T, 0);
        let s = b.snapshot(0);
        assert!((s.headroom_tih + 15.0 * T).abs() < 1e-6);
        assert_eq!(s.available_tih, Some(0));
    }

    #[test]
    fn explicit_limit_overrides_auto() {
        let b = CapacityBook::new();
        b.set_manual(vec![rental(10.0, H)]);
        b.set_limit(Some(false));
        assert!(b.check(100_000, 0).is_ok());
        b.set_limit(None);
        assert!(b.check(100_000, 0).is_err());
        // MRR configured but not yet synced: limit on, nothing to sell.
        let m = CapacityBook::new();
        m.enable_mrr();
        assert_eq!(m.snapshot(0).available_tih, Some(0));
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
    fn reset_mrr_drops_its_rentals() {
        let b = CapacityBook::new();
        b.enable_mrr();
        b.set_mrr(Ok(vec![rental(40.0, 2 * H)]), 0);
        b.reset_mrr(false);
        let s = b.snapshot(0);
        assert!(!s.mrr_enabled);
        assert_eq!(s.rented_ths, 0.0);
        assert_eq!(s.mrr_synced_ms, None);
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
        assert!((s.capacity_tih - 62.5 * T).abs() < 1e-6);
    }

    #[test]
    fn live_hashrate_times_horizon_backs_sales() {
        let b = CapacityBook::new();
        let ths = Arc::new(Mutex::new(1.2));
        let t = ths.clone();
        b.set_live_source(Arc::new(move || *t.lock().unwrap()));
        // Measured, but off until the operator sets a horizon.
        let s = b.snapshot(0);
        assert!(s.live.available && !s.live.on);
        assert_eq!(s.available_tih, Some(0));
        b.set_live_horizon(Some(24.0)).unwrap();
        let s = b.snapshot(0);
        assert!((s.live.tih - 28.8 * T).abs() < 1e-6);
        assert_eq!(s.available_tih, Some((28.8 * T).floor() as u64));
        b.set_owed(10.0 * T, 0);
        assert_eq!(b.snapshot(0).available_tih, Some((18.8 * T).floor() as u64));
        // The rig goes quiet: nothing left to sell, and the mint is oversold.
        *ths.lock().unwrap() = 0.0;
        let s = b.snapshot(0);
        assert_eq!(s.available_tih, Some(0));
        assert!(s.headroom_tih < 0.0);
        // Off again.
        b.set_live_horizon(None).unwrap();
        assert!(!b.snapshot(0).live.on);
    }

    #[test]
    fn live_does_not_count_rented_rigs_twice() {
        let b = CapacityBook::new();
        // 10 TH/s rented for 2 h; the proxy measures 11 TH/s in all.
        b.set_manual(vec![rental(10.0, 2 * H)]);
        b.set_live_source(Arc::new(|| 11.0));
        b.set_live_horizon(Some(5.0)).unwrap();
        let s = b.snapshot(0);
        assert!((s.live.extra_ths - 1.0).abs() < 1e-9);
        assert!((s.capacity_tih - 25.0 * T).abs() < 1e-6);
        // Measuring less than rented adds nothing (and takes nothing away).
        b.set_live_source(Arc::new(|| 7.0));
        let s = b.snapshot(0);
        assert_eq!(s.live.extra_ths, 0.0);
        assert!((s.capacity_tih - 20.0 * T).abs() < 1e-6);
    }

    #[test]
    fn live_horizon_validated_and_persisted() {
        let b = CapacityBook::new();
        assert!(b.set_live_horizon(Some(0.5)).is_err());
        assert!(b.set_live_horizon(Some(f64::NAN)).is_err());
        // Without a proxy the horizon is kept but nothing is counted.
        b.set_live_horizon(Some(3.0)).unwrap();
        let s = b.snapshot(0);
        assert!(!s.live.available && !s.live.on && s.live.tih == 0.0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("capacity.json");
        CapacityBook::load(path.clone())
            .unwrap()
            .set_live_horizon(Some(12.0))
            .unwrap();
        let again = CapacityBook::load(path).unwrap();
        assert_eq!(again.snapshot(0).live.horizon_hours, Some(12.0));
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
