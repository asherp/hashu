//! Pricing `TiH` in a payment rail's base unit. ARCHITECTURE.md §5.1.
//!
//! Every payment rail (Lightning, Tether) sells the same thing: `TiH`
//! vouchers. A mint quote is always denominated in `TiH`; the rail's
//! backend asks a [`TihPricer`] what that many `TiH` cost in the rail's
//! own base unit (sats or msat for Lightning, micro-USDT for Tether) and
//! bills that.
//!
//! Prices are quoted in **msat per PiH** (2^50 hashes = 1,024 TiH; #90),
//! because a single TiH costs well under a millisat. [`MsatPerPihFromHashprice`]
//! is the operator mint's Lightning pricer:
//!
//! ```text
//! msat_per_pih = hashprice_now[sats/TH·s] × 1000 × 2^50 / 10^12 × (1 + markup)
//! sats         = ceil(amount_tih × msat_per_pih / 1024 / 1000)
//! ```
//!
//! The buyer locks in today's hashprice for future hashes.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::SystemTime;

use hashu_core::denom::{msat_per_pih_from_sats_per_ths, TIH_PER_PIH};
use hashu_core::hashprice::HashpriceOracle;

/// Basis points in 100%.
const BPS: f64 = 10_000.0;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PricingError {
    /// The oracle has no fresh price (no samples yet, or stale).
    #[error("no fresh price available")]
    NoPrice,
    /// The oracle returned a value that cannot be a price.
    #[error("invalid price: {0}")]
    InvalidPrice(f64),
    /// The priced amount does not fit in a u64.
    #[error("price overflow for {0} TiH")]
    Overflow(u64),
    #[error("amount must be at least 1 TiH")]
    ZeroAmount,
    /// The operator has paused issuance.
    #[error("issuance is paused by the mint operator")]
    Paused,
    /// More TiH than the mint's hashrate (rented, or a supplier's rigs) can
    /// still deliver.
    #[error("{}", sold_out_message(*requested, *available))]
    SoldOut { requested: u64, available: u64 },
}

fn sold_out_message(requested: u64, available: u64) -> String {
    if available == 0 {
        "sold out: the mint has no hashrate available to sell right now".to_string()
    } else {
        format!("only {available} TiH available from the mint's hashrate (asked for {requested})")
    }
}

/// Prices `TiH` in one payment rail's base unit.
pub trait TihPricer: Send + Sync + Debug {
    /// Cost of `amount_tih` in the rail's base unit, rounded up.
    fn price(&self, amount_tih: u64) -> Result<u64, PricingError>;
}

/// Lightning pricer: msat per PiH from the hashprice oracle (§4.4), plus
/// an operator markup in basis points. Bills whole sats.
pub struct MsatPerPihFromHashprice {
    oracle: Arc<dyn HashpriceOracle + Send + Sync>,
    markup_bps: u32,
}

impl Debug for MsatPerPihFromHashprice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsatPerPihFromHashprice")
            .field("markup_bps", &self.markup_bps)
            .finish_non_exhaustive()
    }
}

impl MsatPerPihFromHashprice {
    pub fn new(oracle: Arc<dyn HashpriceOracle + Send + Sync>) -> Self {
        Self {
            oracle,
            markup_bps: 0,
        }
    }

    /// Operator markup over hashprice, e.g. `500` = 5%.
    pub fn with_markup_bps(mut self, markup_bps: u32) -> Self {
        self.markup_bps = markup_bps;
        self
    }

    pub fn markup_bps(&self) -> u32 {
        self.markup_bps
    }

    /// Current msat per PiH before markup, if the oracle is fresh.
    pub fn msat_per_pih_now(&self) -> Option<f64> {
        self.oracle
            .sample_at(SystemTime::now())
            .map(msat_per_pih_from_sats_per_ths)
    }

    /// Current msat per PiH including markup, if the oracle is fresh.
    pub fn quoted_msat_per_pih(&self) -> Option<f64> {
        self.msat_per_pih_now()
            .map(|p| p * (1.0 + self.markup_bps as f64 / BPS))
    }
}

impl TihPricer for MsatPerPihFromHashprice {
    fn price(&self, amount_tih: u64) -> Result<u64, PricingError> {
        if amount_tih == 0 {
            return Err(PricingError::ZeroAmount);
        }
        let per_pih = self.quoted_msat_per_pih().ok_or(PricingError::NoPrice)?;
        if !per_pih.is_finite() || per_pih <= 0.0 {
            return Err(PricingError::InvalidPrice(per_pih));
        }
        let msat = amount_tih as f64 * per_pih / TIH_PER_PIH as f64;
        let sats = (msat / 1000.0).ceil();
        if !sats.is_finite() || sats >= u64::MAX as f64 {
            return Err(PricingError::Overflow(amount_tih));
        }
        // Never bill zero: an invoice for 0 sats would mint for free.
        Ok((sats as u64).max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::hashprice::{HashpriceSample, SampleBuffer};
    use std::sync::Mutex;
    use std::time::Duration;

    /// Oracle wrapper so a test can feed samples behind a shared handle.
    #[derive(Debug)]
    struct TestOracle(Mutex<SampleBuffer>);

    impl HashpriceOracle for TestOracle {
        fn sample_at(&self, t: SystemTime) -> Option<f64> {
            self.0.lock().unwrap().sample_at(t)
        }
        fn latest(&self) -> Option<HashpriceSample> {
            self.0.lock().unwrap().latest()
        }
    }

    fn oracle_at(sats_per_th_day: Option<f64>) -> Arc<TestOracle> {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        if let Some(p) = sats_per_th_day {
            buf.push(HashpriceSample::from_sats_per_th_per_day(
                SystemTime::now(),
                p,
            ));
        }
        Arc::new(TestOracle(Mutex::new(buf)))
    }

    #[test]
    fn prices_from_hashprice() {
        // 48 sats/TH/day ≈ 625.5 msat per PiH (≈ 2 sats per THH).
        let p = MsatPerPihFromHashprice::new(oracle_at(Some(48.0)));
        assert!((p.msat_per_pih_now().unwrap() - 625.5).abs() < 0.01);
        // 100 PiH = 62.55 sats, billed 63.
        assert_eq!(p.price(100 * 1024).unwrap(), 63);
        // One THH's worth (3,274 TiH) ≈ 2 sats.
        assert_eq!(p.price(3274).unwrap(), 2);
    }

    #[test]
    fn applies_markup_and_rounds_up() {
        // 625.5 msat/PiH × 1.05 ≈ 656.8; 100 PiH = 65.68 sats -> 66.
        let p = MsatPerPihFromHashprice::new(oracle_at(Some(48.0))).with_markup_bps(500);
        assert_eq!(p.price(100 * 1024).unwrap(), 66);
        // A single TiH still bills a whole sat.
        assert_eq!(p.price(1).unwrap(), 1);
    }

    #[test]
    fn never_bills_zero() {
        let p = MsatPerPihFromHashprice::new(oracle_at(Some(1e-9)));
        assert_eq!(p.price(1).unwrap(), 1);
    }

    #[test]
    fn refuses_without_fresh_price() {
        let p = MsatPerPihFromHashprice::new(oracle_at(None));
        assert_eq!(p.price(1), Err(PricingError::NoPrice));
    }

    #[test]
    fn refuses_zero_amount_and_bad_price() {
        let p = MsatPerPihFromHashprice::new(oracle_at(Some(48.0)));
        assert_eq!(p.price(0), Err(PricingError::ZeroAmount));
        let neg = MsatPerPihFromHashprice::new(oracle_at(Some(-1.0)));
        assert!(matches!(neg.price(1), Err(PricingError::InvalidPrice(_))));
    }
}
