//! A fixed stock of TiH for sale, restocked once per horizon (#92).
//!
//! Selling `measured hashrate × horizon − owed` made the quantity on sale
//! jump with every share. A stock holds still between restocks and only
//! goes down as buyers take it, so it is the delivery ETA that moves
//! instead (Asher, 2026-11-13):
//!
//! ```text
//! at each restock:  batch = max(left, ewma TH/s × horizon − owed)
//! between restocks: left  = batch − TiH sold since the restock
//! ```
//!
//! - The hashrate is an EWMA with half-life = horizon / 2, so one quiet
//!   hour does not shrink the next batch.
//! - A restock happens once per horizon (or when the seller asks). It
//!   never takes away stock already listed, and since the batch is sized to
//!   one horizon's production, unsold stock cannot pile up beyond that.
//! - Until a first non-empty batch has been stocked, the stock refills as
//!   soon as there is hashrate, so a newly connected rig does not wait a
//!   whole horizon.
//! - "Sold" counts paid quotes and open, unexpired ones (a quote reserves
//!   stock until it expires), as a running total from the mint's quotes.
//!   Delivering a redemption does not refill the stock.

use serde::{Deserialize, Serialize};

use hashu_core::denom::tih_from_ths_hours;

use crate::capacity::MIN_PURCHASE_TIH;

const MS_PER_HOUR: f64 = 3_600_000.0;

/// EWMA of hashrate with a half-life that follows the horizon.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ewma {
    /// TH/s; `None` until the first sample.
    #[serde(default)]
    pub ths: Option<f64>,
    #[serde(default)]
    pub at_ms: u64,
}

impl Ewma {
    /// Fold in a sample `ths` taken at `now_ms`, with the given half-life.
    /// The first sample seeds the average.
    pub fn observe(&mut self, ths: f64, half_life_hours: f64, now_ms: u64) {
        let ths = if ths.is_finite() { ths.max(0.0) } else { 0.0 };
        match self.ths {
            None => self.ths = Some(ths),
            Some(avg) => {
                let dt_h = now_ms.saturating_sub(self.at_ms) as f64 / MS_PER_HOUR;
                let hl = half_life_hours.max(1e-9);
                let k = 1.0 - (-dt_h / hl).exp2();
                self.ths = Some(avg + k * (ths - avg));
            }
        }
        self.at_ms = now_ms.max(self.at_ms);
    }

    pub fn ths(&self) -> f64 {
        self.ths.unwrap_or(0.0)
    }
}

/// Half-life of the hashrate average for a given horizon.
pub fn half_life_hours(horizon_hours: f64) -> f64 {
    horizon_hours / 2.0
}

/// The stock itself; persisted so a restart does not refill it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stock {
    #[serde(default)]
    pub ewma: Ewma,
    /// TiH put up for sale at the last restock.
    #[serde(default)]
    pub batch_tih: f64,
    /// Running "sold" total at the last restock.
    #[serde(default)]
    pub sold_at_restock_tih: f64,
    #[serde(default)]
    pub restocked_ms: Option<u64>,
    /// A batch of at least the minimum purchase has been stocked; until
    /// then the stock refills as soon as there is hashrate.
    #[serde(default)]
    pub stocked: bool,
}

/// What a seller and the GUI see of a stock.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct StockView {
    /// Hashrate average the next batch is sized from.
    pub ewma_ths: f64,
    pub half_life_hours: f64,
    pub batch_tih: f64,
    /// TiH of this batch sold (or reserved by open quotes) since restock.
    pub sold_tih: f64,
    /// Still for sale.
    pub left_tih: f64,
    pub restocked_ms: Option<u64>,
    pub next_restock_ms: Option<u64>,
}

impl Stock {
    /// TiH of the current batch taken since the restock.
    pub fn sold(&self, sold_total_tih: f64) -> f64 {
        (sold_total_tih - self.sold_at_restock_tih)
            .max(0.0)
            .min(self.batch_tih.max(0.0))
    }

    /// TiH still for sale.
    pub fn left(&self, sold_total_tih: f64) -> f64 {
        (self.batch_tih - self.sold(sold_total_tih)).max(0.0)
    }

    /// Whether a restock is due at `now_ms`.
    pub fn due(&self, horizon_hours: f64, now_ms: u64) -> bool {
        match self.restocked_ms {
            // Never stocked anything yet: fill as soon as there is hashrate.
            _ if !self.stocked => true,
            None => true,
            Some(t) => now_ms >= t + (horizon_hours * MS_PER_HOUR) as u64,
        }
    }

    /// Refill to what the average hashrate produces within the horizon,
    /// less `owed` (TiH already promised that this stock must also cover).
    /// Never shrinks what is left. Returns the new batch.
    pub fn restock(
        &mut self,
        horizon_hours: f64,
        owed_tih: f64,
        sold_total_tih: f64,
        now_ms: u64,
    ) -> f64 {
        let left = self.left(sold_total_tih);
        let fresh = tih_from_ths_hours(self.ewma.ths(), horizon_hours) - owed_tih.max(0.0);
        self.batch_tih = left.max(fresh).max(0.0);
        self.sold_at_restock_tih = sold_total_tih;
        self.restocked_ms = Some(now_ms);
        self.stocked |= self.batch_tih >= MIN_PURCHASE_TIH as f64;
        self.batch_tih
    }

    /// Start a new batch sized for a changed horizon: unlike a restock this
    /// may shrink the stock, since the seller chose to sell less far ahead.
    pub fn resize(
        &mut self,
        horizon_hours: f64,
        owed_tih: f64,
        sold_total_tih: f64,
        now_ms: u64,
    ) -> f64 {
        let fresh = tih_from_ths_hours(self.ewma.ths(), horizon_hours) - owed_tih.max(0.0);
        self.batch_tih = fresh.max(0.0);
        self.sold_at_restock_tih = sold_total_tih;
        self.restocked_ms = Some(now_ms);
        self.stocked |= self.batch_tih >= MIN_PURCHASE_TIH as f64;
        self.batch_tih
    }

    /// Sample the hashrate and restock if due (or if `force`). Returns
    /// whether it restocked.
    pub fn tick(
        &mut self,
        measured_ths: Option<f64>,
        horizon_hours: f64,
        owed_tih: f64,
        sold_total_tih: f64,
        now_ms: u64,
        force: bool,
    ) -> bool {
        if let Some(ths) = measured_ths {
            self.ewma
                .observe(ths, half_life_hours(horizon_hours), now_ms);
        }
        if self.ewma.ths.is_none() && !force {
            return false;
        }
        // While nothing has been stocked yet, wait for real hashrate rather
        // than locking in an empty batch for a whole horizon.
        let empty_first = !self.stocked && self.ewma.ths() <= 0.0;
        if force || (self.due(horizon_hours, now_ms) && !empty_first) {
            self.restock(horizon_hours, owed_tih, sold_total_tih, now_ms);
            return true;
        }
        false
    }

    pub fn view(&self, horizon_hours: f64, sold_total_tih: f64) -> StockView {
        StockView {
            ewma_ths: self.ewma.ths(),
            half_life_hours: half_life_hours(horizon_hours),
            batch_tih: self.batch_tih,
            sold_tih: self.sold(sold_total_tih),
            left_tih: self.left(sold_total_tih),
            restocked_ms: self.restocked_ms,
            next_restock_ms: self
                .restocked_ms
                .map(|t| t + (horizon_hours * MS_PER_HOUR) as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::denom::TIH_PER_THH;

    const H: u64 = 3_600_000;

    #[test]
    fn ewma_halves_the_gap_each_half_life() {
        let mut e = Ewma::default();
        e.observe(10.0, 12.0, 0);
        assert_eq!(e.ths(), 10.0);
        e.observe(0.0, 12.0, 12 * H);
        assert!((e.ths() - 5.0).abs() < 1e-9);
        e.observe(0.0, 12.0, 24 * H);
        assert!((e.ths() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn stock_holds_still_while_hashrate_swings() {
        let mut s = Stock::default();
        // 1 TH/s, 24 h horizon: first batch as soon as there is hashrate.
        assert!(s.tick(Some(1.0), 24.0, 0.0, 0.0, 0, false));
        let batch = 24.0 * TIH_PER_THH;
        assert!((s.left(0.0) - batch).abs() < 1e-6);
        // Hashrate jumps around: nothing on sale changes.
        for (i, ths) in [3.0, 0.0, 0.2, 5.0].iter().enumerate() {
            assert!(!s.tick(Some(*ths), 24.0, 0.0, 0.0, (i as u64 + 1) * H, false));
            assert!((s.left(0.0) - batch).abs() < 1e-6);
        }
    }

    #[test]
    fn sales_draw_down_and_deliveries_do_not_refill() {
        let mut s = Stock::default();
        s.tick(Some(1.0), 24.0, 0.0, 100.0, 0, false);
        let batch = s.batch_tih;
        // 5,000 TiH sold since (running total 100 → 5,100).
        assert!((s.left(5_100.0) - (batch - 5_000.0)).abs() < 1e-6);
        // Owed falling as redemptions fill does not enter `left` at all;
        // only the running sold total does.
        assert!((s.left(5_100.0) - (batch - 5_000.0)).abs() < 1e-6);
        // Sold past the batch: empty, not negative.
        assert_eq!(s.left(batch * 3.0), 0.0);
    }

    #[test]
    fn restocks_once_per_horizon_to_production_minus_owed_and_keeps_leftovers() {
        let mut s = Stock::default();
        s.tick(Some(1.0), 24.0, 0.0, 0.0, 0, false);
        let batch = s.batch_tih;
        // Sell half, then a day passes at 1 TH/s with 1,000 TiH owed.
        let sold = batch / 2.0;
        assert!(!s.tick(Some(1.0), 24.0, 1_000.0, sold, 23 * H, false));
        assert!(s.tick(Some(1.0), 24.0, 1_000.0, sold, 24 * H, false));
        assert!((s.left(sold) - (batch - 1_000.0)).abs() < 1e-6);
        // Rigs gone: the average decays, but a restock never takes away
        // what is still listed.
        for h in 25..48 {
            s.tick(Some(0.0), 24.0, 0.0, sold, h * H, false);
        }
        let before = s.left(sold);
        assert!(s.tick(Some(0.0), 24.0, 0.0, sold, 48 * H, false));
        assert!((s.left(sold) - before).abs() < 1e-6);
    }

    #[test]
    fn waits_for_hashrate_before_the_first_batch() {
        let mut s = Stock::default();
        assert!(!s.tick(Some(0.0), 24.0, 0.0, 0.0, 0, false));
        assert_eq!(s.left(0.0), 0.0);
        assert!(s.tick(Some(2.0), 24.0, 0.0, 0.0, H, false));
        assert!(s.left(0.0) > 0.0);
    }

    #[test]
    fn forced_restock_and_expired_quotes() {
        let mut s = Stock::default();
        s.tick(Some(1.0), 10.0, 0.0, 0.0, 0, false);
        let batch = s.batch_tih;
        // A 2,000 TiH quote reserves stock, then expires: back on sale.
        assert!((s.left(2_000.0) - (batch - 2_000.0)).abs() < 1e-6);
        assert!((s.left(0.0) - batch).abs() < 1e-6);
        // The seller asks for a restock with more hashrate measured.
        s.ewma.ths = Some(2.0);
        assert!(s.tick(None, 10.0, 0.0, 0.0, H, true));
        assert!((s.left(0.0) - 2.0 * 10.0 * TIH_PER_THH).abs() < 1e-6);
        // A shorter horizon shrinks it.
        s.resize(1.0, 0.0, 0.0, 2 * H);
        assert!((s.left(0.0) - 2.0 * TIH_PER_THH).abs() < 1e-6);
    }
}
