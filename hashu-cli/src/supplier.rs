//! A supplier mint's own settings and its price and capacity cap.
//!
//! Each supplier sets, in `<data-dir>/mints/<name>/settings.json`:
//!
//! - `ask_sats_per_eih`: their price, in whole sats per EiH (2^60 hashes =
//!   2^20 TiH; Asher, 2026-11-18). Every mint quotes in the same unit, so
//!   asks compare and orders can be matched across suppliers; a sat per EiH
//!   is fine enough (≈ 0.17% of today's ≈ 600) and the price of any purchase
//!   still comes out to the millisat. ≈ 640 sats/EiH is ≈ 2 sats per THH.
//!   Settings saved with the older `ask_msat_per_pih` load converted;
//! - `horizon_hours`: how far ahead they sell (Asher, 2026-11-11). Every hour
//!   the mint tops up their stock to what their verified rigs produce within
//!   the horizon (EWMA hashrate, half-life horizon / 2, × horizon − TiH
//!   owed); between top-ups the stock only goes down as buyers take it, so the quantity
//!   holds still and the delivery ETA moves (#92, [`hashu_mint::stock`]);
//! - `lnurl`: where buyers pay, the supplier's own LNURL-pay endpoint: a
//!   Lightning address or an `lnurl1…` / `lnurlp://` / `https://` link,
//!   which must support LUD-21 verify (see [`hashu_mint::lnaddress`]).
//!   Settings saved under the older `lightning_address` key still load.
//!
//! A mint with no ask or no payment details sells nothing.
//!
//! With epochs on (#126), the stock sells only the current difficulty
//! epoch: a batch covers the part of the epoch left inside the horizon
//! (the average keeps the horizon's half-life), and each new epoch starts a
//! new batch. Future epochs sell separately, from the supplier's hashrate
//! plan ([`crate::planner`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use hashu_core::denom::{msat_for_tih_at_sats_per_eih, sats_per_eih_from_msat_per_pih};
use hashu_mint::capacity::MIN_PURCHASE_TIH;
use hashu_mint::lnaddress::PayEndpoint;
use hashu_mint::pricing::{PricingError, TihPricer};
use hashu_mint::stock::{half_life_hours, Stock, StockView};
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
    pub ask_sats_per_eih: Option<u64>,
    #[serde(default = "default_horizon")]
    pub horizon_hours: f64,
    /// The supplier's LNURL-pay endpoint, saved in canonical form
    /// ([`PayEndpoint`]'s `Display`).
    #[serde(default, alias = "lightning_address")]
    pub lnurl: Option<String>,
    /// The supplier's own Nostr key (npub). Direct messages to the
    /// supplier's mint go here: it is listed first in the mint's NUT-06
    /// contact and named as the mint's message target. Hashu never holds
    /// its secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npub: Option<String>,
}

impl Default for SupplierSettings {
    fn default() -> Self {
        Self {
            ask_sats_per_eih: None,
            horizon_hours: DEFAULT_HORIZON_HOURS,
            lnurl: None,
            npub: None,
        }
    }
}

impl SupplierSettings {
    /// Check the values (not whether the endpoint works: see
    /// [`hashu_mint::lnaddress::probe`]). Normalizes the endpoint.
    pub fn validated(mut self) -> Result<Self, String> {
        if self.ask_sats_per_eih == Some(0) {
            return Err("ask must be at least 1 sat per EiH".into());
        }
        if !self.horizon_hours.is_finite() || self.horizon_hours < MIN_HORIZON_HOURS {
            return Err(format!("horizon must be at least {MIN_HORIZON_HOURS} hour"));
        }
        self.lnurl = match self.lnurl.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(a) => Some(a.parse::<PayEndpoint>()?.to_string()),
        };
        self.npub = match self.npub.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(n) => Some(normalize_npub(n)?),
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
            Ok(b) => Self::from_json(&b).with_context(|| format!("read {path:?}")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {path:?}")),
        }
    }

    /// Parse saved settings, converting an ask saved in the older msat
    /// per PiH unit.
    fn from_json(b: &[u8]) -> serde_json::Result<Self> {
        #[derive(Deserialize)]
        struct Saved {
            #[serde(flatten)]
            settings: SupplierSettings,
            #[serde(default)]
            ask_msat_per_pih: Option<u64>,
        }
        let Saved {
            mut settings,
            ask_msat_per_pih,
        } = serde_json::from_slice(b)?;
        if settings.ask_sats_per_eih.is_none() {
            settings.ask_sats_per_eih =
                ask_msat_per_pih.map(|m| sats_per_eih_from_msat_per_pih(m).max(1));
        }
        Ok(settings)
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

/// What a supplier mint owes and has sold, and its stock (#92).
#[derive(Debug, Default)]
pub struct SupplierBook {
    /// TiH the mint already owes: in circulation plus open redemptions.
    pub owed_tih: f64,
    /// Running total the stock is drawn down by
    /// ([`crate::operator::Obligations::stock_sold`]).
    pub sold_tih: f64,
    /// `owed`/`sold` have been counted at least once.
    pub counted: bool,
    pub stock: Stock,
    /// With epochs on: the current epoch and the hours left in it.
    pub epoch: Option<(u64, f64)>,
    /// Epoch the stock was sized for (saved with it).
    pub stock_epoch: Option<u64>,
    path: Option<PathBuf>,
    saved_ms: u64,
}

/// `stock.json`: the stock, and the epoch it was sized for.
#[derive(Serialize, Deserialize)]
struct SavedStock {
    #[serde(flatten)]
    stock: Stock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    epoch: Option<u64>,
}

impl SupplierBook {
    /// A book whose stock is kept in `<dir>/stock.json`.
    pub fn load(dir: &Path) -> Result<Self> {
        let path = dir.join("stock.json");
        let saved: SavedStock = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b).with_context(|| format!("read {path:?}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SavedStock {
                stock: Stock::default(),
                epoch: None,
            },
            Err(e) => return Err(e).with_context(|| format!("read {path:?}")),
        };
        Ok(Self {
            stock: saved.stock,
            stock_epoch: saved.epoch,
            path: Some(path),
            ..Self::default()
        })
    }

    fn save(&mut self, now_ms: u64) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("tmp");
        let saved = SavedStock {
            stock: self.stock.clone(),
            epoch: self.stock_epoch,
        };
        let res = serde_json::to_vec_pretty(&saved)
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(std::fs::write(&tmp, b)?))
            .and_then(|_| Ok(std::fs::rename(&tmp, path)?));
        match res {
            Ok(()) => self.saved_ms = now_ms,
            Err(e) => tracing::warn!(?path, error = %e, "could not save stock"),
        }
    }

    /// `(hours a batch covers, half-life of the average)` for `horizon`:
    /// with epochs on, only what is left of the current epoch.
    fn sizing(&self, horizon: f64) -> (f64, f64) {
        let hours = match self.epoch {
            Some((_, left)) => left.min(horizon).max(0.0),
            None => horizon,
        };
        (hours, half_life_hours(horizon))
    }
}

/// How often the hashrate average is written to disk between restocks.
const STOCK_SAVE_EVERY_MS: u64 = 60_000;

/// Prices a supplier's TiH at their ask, from a stock restocked once per
/// horizon to what their rigs produce within it (#92).
pub struct SupplierPricer {
    name: String,
    settings: Arc<RwLock<SupplierSettings>>,
    hashrate: HashrateFn,
    book: Arc<Mutex<SupplierBook>>,
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
    /// Measured hashrate right now (the stock is sized from `stock.ewma_ths`).
    pub hashrate_ths: f64,
    pub horizon_hours: f64,
    /// TiH stocked at the last restock.
    pub capacity_tih: f64,
    pub owed_tih: f64,
    pub available_tih: u64,
    /// Some stock left, but under the minimum purchase.
    pub below_min: bool,
    pub stock: StockView,
    /// With epochs on: the epoch the stock sells, and the hours left in it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch_hours_left: Option<f64>,
}

impl SupplierPricer {
    pub fn new(
        name: impl Into<String>,
        settings: Arc<RwLock<SupplierSettings>>,
        hashrate: HashrateFn,
        book: Arc<Mutex<SupplierBook>>,
    ) -> Self {
        Self {
            name: name.into(),
            settings,
            hashrate,
            book,
        }
    }

    fn horizon(&self) -> f64 {
        self.settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .horizon_hours
    }

    fn measured_ths(&self) -> f64 {
        ((self.hashrate)(&self.name) / 1e12).max(0.0)
    }

    /// Record what the mint owes and has sold, sample the hashrate, and
    /// restock if a horizon has passed (or `force`). Returns whether it
    /// restocked.
    pub fn refresh(&self, owed_tih: f64, sold_tih: f64, now_ms: u64, force: bool) -> bool {
        let horizon = self.horizon();
        let ths = self.measured_ths();
        let mut b = self.book.lock().unwrap_or_else(|e| e.into_inner());
        b.owed_tih = owed_tih.max(0.0);
        b.sold_tih = sold_tih.max(0.0);
        b.counted = true;
        let (owed, sold) = (b.owed_tih, b.sold_tih);
        let (hours, half_life) = b.sizing(horizon);
        // A new epoch starts a new batch sized for it.
        let mut restocked = false;
        if let Some((epoch, _)) = b.epoch {
            if b.stock_epoch != Some(epoch) {
                if b.stock.ewma.ths.is_some() {
                    b.stock.resize(hours, owed, sold, now_ms);
                    restocked = true;
                }
                b.stock_epoch = Some(epoch);
            }
        }
        restocked |= b
            .stock
            .tick_sized(Some(ths), half_life, hours, owed, sold, now_ms, force);
        if restocked {
            tracing::info!(supplier = %self.name, batch_tih = b.stock.batch_tih, "restocked");
        }
        if restocked || now_ms.saturating_sub(b.saved_ms) >= STOCK_SAVE_EVERY_MS {
            b.save(now_ms);
        }
        restocked
    }

    /// Restock now, at the current average (the supplier asked).
    /// Refused until owed and sold have been counted.
    pub fn restock_now(&self, now_ms: u64) -> Result<SupplierCapacity, String> {
        let (owed, sold, counted) = {
            let b = self.book.lock().unwrap_or_else(|e| e.into_inner());
            (b.owed_tih, b.sold_tih, b.counted)
        };
        if !counted {
            return Err("still counting what this mine owes; try again in a few seconds".into());
        }
        self.refresh(owed, sold, now_ms, true);
        Ok(self.capacity())
    }

    /// The horizon changed: size a new batch for it (may shrink). Before
    /// the first count there is nothing to resize; the first refresh
    /// stocks at the new horizon.
    pub fn horizon_changed(&self, now_ms: u64) {
        let horizon = self.horizon();
        let mut b = self.book.lock().unwrap_or_else(|e| e.into_inner());
        if !b.counted || b.stock.ewma.ths.is_none() {
            return;
        }
        let (owed, sold) = (b.owed_tih, b.sold_tih);
        let (hours, _) = b.sizing(horizon);
        b.stock.resize(hours, owed, sold, now_ms);
        b.save(now_ms);
    }

    /// With epochs on: the current epoch and the hours left in it. The
    /// stock then sells only that epoch.
    pub fn set_epoch(&self, epoch: u64, hours_left: f64) {
        self.book.lock().unwrap_or_else(|e| e.into_inner()).epoch = Some((epoch, hours_left));
    }

    /// The live hashrate average the stock is sized from, in TH/s (`None`
    /// before the first sample).
    pub fn average_ths(&self) -> Option<f64> {
        self.book
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .stock
            .ewma
            .ths
    }

    /// The supplier's horizon in hours.
    pub fn horizon_hours(&self) -> f64 {
        self.horizon()
    }

    pub fn capacity(&self) -> SupplierCapacity {
        let horizon_hours = self.horizon();
        let hashrate_ths = self.measured_ths();
        let b = self.book.lock().unwrap_or_else(|e| e.into_inner());
        let stock = b.stock.view(horizon_hours, b.sold_tih);
        let left = if b.counted { stock.left_tih } else { 0.0 };
        let below_min = left > 0.0 && left < MIN_PURCHASE_TIH as f64;
        let available = left.floor();
        SupplierCapacity {
            hashrate_ths,
            horizon_hours,
            capacity_tih: stock.batch_tih,
            owed_tih: b.owed_tih,
            available_tih: if available.is_finite() && !below_min && available > 0.0 {
                available as u64
            } else {
                0
            },
            below_min,
            stock,
            epoch: b.epoch.map(|(e, _)| e),
            epoch_hours_left: b.epoch.map(|(_, h)| h),
        }
    }
}

impl TihPricer for SupplierPricer {
    fn price(&self, amount_tih: u64) -> Result<u64, PricingError> {
        if amount_tih == 0 {
            return Err(PricingError::ZeroAmount);
        }
        let ask = self
            .settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .ask_sats_per_eih
            .ok_or(PricingError::NoPrice)?;
        // A future-epoch purchase sells against that epoch's capacity,
        // checked by the epoch endpoint (#126), not the live stock.
        let future = crate::epochs::FUTURE_EPOCH_QUOTE.try_with(|_| ()).is_ok();
        let available = self.capacity().available_tih;
        if !future && amount_tih > available {
            return Err(PricingError::SoldOut {
                requested: amount_tih,
                available,
            });
        }
        msat_for_tih_at_sats_per_eih(amount_tih, ask).ok_or(PricingError::Overflow(amount_tih))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: u64 = 3_600_000;

    fn live_pricer(ths: Arc<Mutex<f64>>, ask: Option<u64>, horizon: f64) -> SupplierPricer {
        SupplierPricer::new(
            "alice",
            Arc::new(RwLock::new(SupplierSettings {
                ask_sats_per_eih: ask,
                horizon_hours: horizon,
                lnurl: None,
                npub: None,
            })),
            Arc::new(move |n: &str| {
                if n == "alice" {
                    *ths.lock().unwrap() * 1e12
                } else {
                    0.0
                }
            }),
            Arc::new(Mutex::new(SupplierBook::default())),
        )
    }

    /// A pricer after its first count, so the first batch is stocked.
    fn pricer(ths: f64, ask: Option<u64>, horizon: f64, owed: f64) -> SupplierPricer {
        let p = live_pricer(Arc::new(Mutex::new(ths)), ask, horizon);
        p.refresh(owed, 0.0, 0, false);
        p
    }

    #[test]
    fn nothing_for_sale_before_the_first_count() {
        let p = live_pricer(Arc::new(Mutex::new(1.0)), Some(600), 24.0);
        assert_eq!(p.capacity().available_tih, 0);
        assert!(p.restock_now(0).is_err());
    }

    #[test]
    fn stock_holds_still_while_hashrate_moves_and_tops_up_hourly() {
        const M: u64 = 60_000;
        let ths = Arc::new(Mutex::new(1.0));
        let p = live_pricer(ths.clone(), Some(600), 24.0);
        p.refresh(0.0, 0.0, 0, false);
        let first = p.capacity().available_tih;
        // 1 TH/s × 24 h ≈ 78,580 TiH.
        assert_eq!(first, 78_580);
        // Hashrate swings within the hour: same quantity on sale.
        for (h, t) in [(1, 3.0), (2, 0.1), (5, 2.0), (23, 0.0)] {
            *ths.lock().unwrap() = t;
            p.refresh(0.0, 0.0, h * M, false);
            assert_eq!(p.capacity().available_tih, first);
        }
        // A buyer takes 10,000; a redemption delivering (owed falling) does
        // not put it back.
        p.refresh(10_000.0, 10_000.0, 23 * M + 1, false);
        assert_eq!(p.capacity().available_tih, first - 10_000);
        p.refresh(2_000.0, 10_000.0, 23 * M + 2, false);
        assert_eq!(p.capacity().available_tih, first - 10_000);
        // On the hour it tops up from the average, minus what is owed.
        *ths.lock().unwrap() = 1.0;
        assert!(p.refresh(2_000.0, 10_000.0, H, false));
        let c = p.capacity();
        assert_eq!(c.stock.restocked_ms, Some(H));
        assert!(c.stock.ewma_ths > 0.0 && c.stock.ewma_ths < 3.0);
        let fresh = hashu_core::denom::tih_from_ths_hours(c.stock.ewma_ths, 24.0) - 2_000.0;
        let left = (first - 10_000) as f64;
        assert_eq!(c.available_tih, fresh.max(left).floor() as u64);
    }

    #[test]
    fn stock_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let ths = 1.0;
        let mk = || {
            SupplierPricer::new(
                "alice",
                Arc::new(RwLock::new(SupplierSettings {
                    ask_sats_per_eih: Some(600),
                    ..Default::default()
                })),
                Arc::new(move |_: &str| ths * 1e12),
                Arc::new(Mutex::new(SupplierBook::load(dir.path()).unwrap())),
            )
        };
        let p = mk();
        p.refresh(0.0, 0.0, 0, false);
        p.refresh(0.0, 5_000.0, H, false);
        let left = p.capacity().available_tih;
        drop(p);
        let again = mk();
        // Same running total after the restart: no refill.
        again.refresh(0.0, 5_000.0, 2 * H, false);
        assert_eq!(again.capacity().available_tih, left);
    }

    #[test]
    fn sells_hashrate_times_horizon_minus_owed() {
        // 1.2 TH/s for 24 h = 28.8 THH ≈ 94,296 TiH, 5,000 owed: 89,296.
        let p = pricer(1.2, Some(600), 24.0, 5_000.0);
        assert_eq!(p.capacity().available_tih, 89_296);
        // 600 sats per 2^20 TiH, to the millisat, rounded up.
        assert_eq!(p.price(89_296), Ok(51_096));
        assert_eq!(
            p.price(89_297),
            Err(PricingError::SoldOut {
                requested: 89_297,
                available: 89_296
            })
        );
        // A longer horizon sells more of the same rigs.
        assert_eq!(
            pricer(1.2, Some(600), 72.0, 5_000.0)
                .capacity()
                .available_tih,
            277_889
        );
    }

    #[test]
    fn with_epochs_the_stock_sells_only_the_current_epoch() {
        let dir = tempfile::tempdir().unwrap();
        let p = SupplierPricer::new(
            "alice",
            Arc::new(RwLock::new(SupplierSettings {
                ask_sats_per_eih: Some(600),
                horizon_hours: 400.0,
                lnurl: None,
                npub: None,
            })),
            Arc::new(|_: &str| 1e12),
            Arc::new(Mutex::new(SupplierBook::load(dir.path()).unwrap())),
        );
        let tih = hashu_core::denom::tih_from_ths_hours;
        // 10 h left in epoch 10: 1 TH/s × 10 h, not × the 400 h horizon.
        p.set_epoch(10, 10.0);
        p.refresh(0.0, 0.0, 0, false);
        let c = p.capacity();
        assert_eq!((c.epoch, c.epoch_hours_left), (Some(10), Some(10.0)));
        assert_eq!(c.available_tih, tih(1.0, 10.0).floor() as u64);
        // Hourly top-ups never grow it past what is left of the epoch...
        p.set_epoch(10, 5.0);
        p.refresh(0.0, 1_000.0, H, false);
        assert!(p.capacity().available_tih <= tih(1.0, 10.0) as u64);
        // ...and a new epoch starts a new batch for its whole length.
        p.set_epoch(11, 336.0);
        assert!(p.refresh(0.0, 1_000.0, H + 1, false));
        let c = p.capacity();
        assert_eq!(c.epoch, Some(11));
        assert!((c.available_tih as f64 - tih(c.stock.ewma_ths, 336.0)).abs() < 2.0);
        // The epoch the stock was sized for survives a restart.
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("stock.json")).unwrap()).unwrap();
        assert_eq!(saved["epoch"], 11);
        assert_eq!(
            SupplierBook::load(dir.path()).unwrap().stock_epoch,
            Some(11)
        );
    }

    #[test]
    fn whole_sat_asks_price_to_the_millisat() {
        // 640 sats per EiH: 3 PiH cost 1,875 msat; one more TiH than the
        // 4,096 TiH minimum rounds up to the next millisat.
        let p = pricer(1.0, Some(640), 24.0, 0.0);
        assert_eq!(p.price(3 * 1024), Ok(1_875));
        assert_eq!(p.price(4_096 + 1), Ok(2_501));
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
        let over = pricer(1.0, Some(1), 1.0, 9_000.0).capacity();
        assert_eq!(over.available_tih, 0);
        assert!(!over.below_min);
    }

    #[test]
    fn small_rig_one_hour_is_below_the_minimum() {
        // 0.25 TH/s for a 1 h horizon ≈ 819 TiH, under the 1,024 TiH
        // minimum: nothing for sale, but say why. 2 h is enough.
        let c = pricer(0.25, Some(40), 1.0, 0.0).capacity();
        assert_eq!(c.available_tih, 0);
        assert!(c.below_min);
        let c = pricer(0.25, Some(40), 2.0, 0.0).capacity();
        assert!(!c.below_min);
        assert_eq!(c.available_tih, 1_637);
    }

    #[test]
    fn settings_validate_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            SupplierSettings::load(dir.path()).unwrap(),
            SupplierSettings::default()
        );
        let s = SupplierSettings {
            ask_sats_per_eih: Some(45),
            horizon_hours: 12.0,
            lnurl: Some(" Alice@Wallet.Example ".into()),
            npub: None,
        }
        .validated()
        .unwrap();
        assert_eq!(s.lnurl.as_deref(), Some("alice@wallet.example"));
        s.save(dir.path()).unwrap();
        assert_eq!(SupplierSettings::load(dir.path()).unwrap(), s);
        // A miner's own LNURL-pay endpoint is kept as its URL.
        let own = SupplierSettings {
            lnurl: Some("lnurlp://pay.alice.example/lnurlp/1".into()),
            ..Default::default()
        }
        .validated()
        .unwrap();
        assert_eq!(
            own.lnurl.as_deref(),
            Some("https://pay.alice.example/lnurlp/1")
        );
        // Settings saved before the rename still load.
        std::fs::write(
            SupplierSettings::path(dir.path()),
            br#"{"ask_sats_per_eih": 600, "lightning_address": "alice@wallet.example"}"#,
        )
        .unwrap();
        assert_eq!(
            SupplierSettings::load(dir.path()).unwrap().lnurl.as_deref(),
            Some("alice@wallet.example")
        );
        // An ask saved as msat per PiH loads as sats per EiH.
        std::fs::write(
            SupplierSettings::path(dir.path()),
            br#"{"ask_msat_per_pih": 590, "horizon_hours": 6}"#,
        )
        .unwrap();
        let old = SupplierSettings::load(dir.path()).unwrap();
        assert_eq!((old.ask_sats_per_eih, old.horizon_hours), (Some(604), 6.0));
        let one_hour = SupplierSettings {
            horizon_hours: 1.0,
            ..Default::default()
        };
        assert!(one_hour.validated().is_ok());
        for bad in [
            SupplierSettings {
                ask_sats_per_eih: Some(0),
                ..Default::default()
            },
            SupplierSettings {
                horizon_hours: 0.5,
                ..Default::default()
            },
            SupplierSettings {
                lnurl: Some("nope".into()),
                ..Default::default()
            },
        ] {
            assert!(bad.validated().is_err());
        }
    }
}

/// A Nostr public key as npub (accepts npub, `nostr:npub…` or hex).
pub fn normalize_npub(n: &str) -> Result<String, String> {
    use nostr::ToBech32;
    let n = n.trim();
    let n = n.strip_prefix("nostr:").unwrap_or(n);
    nostr::PublicKey::parse(n)
        .ok()
        .and_then(|p| p.to_bech32().ok())
        .ok_or_else(|| "npub: a Nostr public key starting npub1".to_string())
}

#[cfg(test)]
mod npub_tests {
    use super::*;

    #[test]
    fn npub_is_optional_and_normalized() {
        let npub = "npub1sg6plzptd64u62a878hep2kev88swjh3tw00gjsfl8f237lmu63q0uf63m";
        let hex = nostr::PublicKey::parse(npub).unwrap().to_hex();
        for given in [
            npub.to_string(),
            format!("nostr:{npub}"),
            hex,
            format!(" {npub} "),
        ] {
            let s = SupplierSettings {
                npub: Some(given),
                ..Default::default()
            }
            .validated()
            .unwrap();
            assert_eq!(s.npub.as_deref(), Some(npub));
        }
        let blank = SupplierSettings {
            npub: Some("  ".into()),
            ..Default::default()
        };
        assert_eq!(blank.validated().unwrap().npub, None);
        let bad = SupplierSettings {
            npub: Some("npub1nope".into()),
            ..Default::default()
        };
        assert!(bad.validated().is_err());
        // Older settings files have no npub.
        let old: SupplierSettings = serde_json::from_str(r#"{"horizon_hours":24}"#).unwrap();
        assert_eq!(old.npub, None);
    }
}
