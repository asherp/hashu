//! A supplier mint's own settings and its price and capacity cap.
//!
//! Each supplier sets, in `<data-dir>/mints/<name>/settings.json`:
//!
//! - `ask_sats_per_thh`: their price;
//! - `horizon_hours`: how far ahead they sell (Asher, 2026-11-11). THH for
//!   sale = measured hashrate of their verified rigs × horizon − THH their
//!   mint already owes;
//! - `lightning_address`: where buyers pay (LUD-16 + LUD-21, see
//!   [`hashu_mint::lnaddress`]).
//!
//! A mint with no ask or no payment details sells nothing.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use hashu_mint::lnaddress::LightningAddress;
use hashu_mint::pricing::{PricingError, ThhPricer};
use serde::{Deserialize, Serialize};

/// Selling horizon until the supplier picks one.
pub const DEFAULT_HORIZON_HOURS: f64 = 24.0;
/// Shortest horizon a supplier may choose (Asher, 2026-11-12).
pub const MIN_HORIZON_HOURS: f64 = 1.0;

fn default_horizon() -> f64 {
    DEFAULT_HORIZON_HOURS
}

/// What a supplier sets for their mint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SupplierSettings {
    #[serde(default)]
    pub ask_sats_per_thh: Option<u64>,
    #[serde(default = "default_horizon")]
    pub horizon_hours: f64,
    #[serde(default)]
    pub lightning_address: Option<String>,
}

impl Default for SupplierSettings {
    fn default() -> Self {
        Self {
            ask_sats_per_thh: None,
            horizon_hours: DEFAULT_HORIZON_HOURS,
            lightning_address: None,
        }
    }
}

impl SupplierSettings {
    /// Check the values (not whether the address works: see
    /// [`hashu_mint::lnaddress::probe`]). Normalizes the address.
    pub fn validated(mut self) -> Result<Self, String> {
        if self.ask_sats_per_thh == Some(0) {
            return Err("ask must be at least 1 sat per THH".into());
        }
        if !self.horizon_hours.is_finite() || self.horizon_hours < MIN_HORIZON_HOURS {
            return Err(format!("horizon must be at least {MIN_HORIZON_HOURS} hour"));
        }
        self.lightning_address = match self.lightning_address.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(a) => Some(a.parse::<LightningAddress>()?.to_string()),
        };
        Ok(self)
    }

    pub fn path(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    /// Settings in `dir`, or the defaults when none are saved yet.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = Self::path(dir);
        match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("read {path:?}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {path:?}")),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = Self::path(dir);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("write {tmp:?}"))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("write {path:?}"))?;
        Ok(())
    }
}

/// Live hashrate (H/s) of a supplier's verified rigs.
pub type HashrateFn = Arc<dyn Fn(&str) -> f64 + Send + Sync>;

/// Prices a supplier's THH at their ask, capped at what their rigs can
/// deliver within their horizon.
pub struct SupplierPricer {
    name: String,
    settings: Arc<RwLock<SupplierSettings>>,
    hashrate: HashrateFn,
    /// THH the mint already owes: in circulation plus open redemptions.
    owed: Arc<Mutex<f64>>,
}

impl std::fmt::Debug for SupplierPricer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupplierPricer")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// A supplier mint's capacity right now.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SupplierCapacity {
    pub hashrate_ths: f64,
    pub horizon_hours: f64,
    pub capacity_thh: f64,
    pub owed_thh: f64,
    pub available_thh: u64,
    /// Some headroom, but under the 1 THH minimum purchase.
    pub below_min: bool,
}

impl SupplierPricer {
    pub fn new(
        name: impl Into<String>,
        settings: Arc<RwLock<SupplierSettings>>,
        hashrate: HashrateFn,
        owed: Arc<Mutex<f64>>,
    ) -> Self {
        Self {
            name: name.into(),
            settings,
            hashrate,
            owed,
        }
    }

    pub fn capacity(&self) -> SupplierCapacity {
        let horizon_hours = self
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .horizon_hours;
        let hashrate_ths = ((self.hashrate)(&self.name) / 1e12).max(0.0);
        let capacity_thh = hashrate_ths * horizon_hours;
        let owed_thh = *self.owed.lock().unwrap_or_else(|e| e.into_inner());
        let headroom = capacity_thh - owed_thh;
        let available = headroom.floor();
        SupplierCapacity {
            hashrate_ths,
            horizon_hours,
            capacity_thh,
            owed_thh,
            available_thh: if available.is_finite() && available > 0.0 {
                available as u64
            } else {
                0
            },
            below_min: headroom > 0.0 && headroom < hashu_mint::capacity::MIN_PURCHASE_THH as f64,
        }
    }
}

impl ThhPricer for SupplierPricer {
    fn price(&self, amount_thh: u64) -> Result<u64, PricingError> {
        if amount_thh == 0 {
            return Err(PricingError::ZeroAmount);
        }
        let ask = self
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .ask_sats_per_thh
            .ok_or(PricingError::NoPrice)?;
        let available = self.capacity().available_thh;
        if amount_thh > available {
            return Err(PricingError::SoldOut {
                requested: amount_thh,
                available,
            });
        }
        ask.checked_mul(amount_thh)
            .ok_or(PricingError::Overflow(amount_thh))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricer(ths: f64, ask: Option<u64>, horizon: f64, owed: f64) -> SupplierPricer {
        SupplierPricer::new(
            "alice",
            Arc::new(RwLock::new(SupplierSettings {
                ask_sats_per_thh: ask,
                horizon_hours: horizon,
                lightning_address: None,
            })),
            Arc::new(move |n: &str| if n == "alice" { ths * 1e12 } else { 0.0 }),
            Arc::new(Mutex::new(owed)),
        )
    }

    #[test]
    fn sells_hashrate_times_horizon_minus_owed() {
        // 1.2 TH/s for 24 h = 28.8 THH, 5 owed: 23 for sale.
        let p = pricer(1.2, Some(50), 24.0, 5.0);
        assert_eq!(p.capacity().available_thh, 23);
        assert_eq!(p.price(23), Ok(23 * 50));
        assert_eq!(
            p.price(24),
            Err(PricingError::SoldOut {
                requested: 24,
                available: 23
            })
        );
        // A longer horizon sells more of the same rigs.
        assert_eq!(
            pricer(1.2, Some(50), 72.0, 5.0).capacity().available_thh,
            81
        );
    }

    #[test]
    fn no_rigs_or_no_ask_sells_nothing() {
        assert!(matches!(
            pricer(0.0, Some(50), 24.0, 0.0).price(1),
            Err(PricingError::SoldOut { available: 0, .. })
        ));
        assert_eq!(
            pricer(1.0, None, 24.0, 0.0).price(1),
            Err(PricingError::NoPrice)
        );
        // Owing more than the rigs cover is sold out, not negative.
        assert_eq!(pricer(1.0, Some(1), 1.0, 9.0).capacity().available_thh, 0);
        assert!(!pricer(1.0, Some(1), 1.0, 9.0).capacity().below_min);
    }

    #[test]
    fn bitaxe_one_hour_is_below_the_minimum() {
        // 0.6 TH/s for a 1 h horizon = 0.6 THH: nothing for sale, but say why.
        let c = pricer(0.6, Some(40), 1.0, 0.0).capacity();
        assert_eq!(c.available_thh, 0);
        assert!(c.below_min);
        assert!(!pricer(0.6, Some(40), 2.0, 0.0).capacity().below_min);
    }

    #[test]
    fn settings_validate_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            SupplierSettings::load(dir.path()).unwrap(),
            SupplierSettings::default()
        );
        let s = SupplierSettings {
            ask_sats_per_thh: Some(45),
            horizon_hours: 12.0,
            lightning_address: Some(" Alice@Wallet.Example ".into()),
        }
        .validated()
        .unwrap();
        assert_eq!(s.lightning_address.as_deref(), Some("alice@wallet.example"));
        s.save(dir.path()).unwrap();
        assert_eq!(SupplierSettings::load(dir.path()).unwrap(), s);
        let one_hour = SupplierSettings {
            horizon_hours: 1.0,
            ..Default::default()
        };
        assert!(one_hour.validated().is_ok());
        for bad in [
            SupplierSettings {
                ask_sats_per_thh: Some(0),
                ..Default::default()
            },
            SupplierSettings {
                horizon_hours: 0.5,
                ..Default::default()
            },
            SupplierSettings {
                lightning_address: Some("nope".into()),
                ..Default::default()
            },
        ] {
            assert!(bad.validated().is_err());
        }
    }
}
