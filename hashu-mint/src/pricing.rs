//! Pricing `THH` in a payment rail's base unit. ARCHITECTURE.md §5.1.
//!
//! Every payment rail (Lightning, Tether) sells the same thing: `THH`
//! vouchers. A mint quote is always denominated in `THH`; the rail's
//! backend asks a [`ThhPricer`] what that many `THH` cost in the rail's
//! own base unit (sats for Lightning, micro-USDT for Tether) and bills that.
//!
//! [`SatsPerThhFromHashprice`] is the Lightning pricer:
//!
//! ```text
//! sats = ceil(amount_thh × hashprice_now[sats/Th·s] × 3600 × (1 + markup))
//! ```
//!
//! The buyer locks in today's hashprice for future hashrate-time.

use std::fmt::Debug;
use std::sync::Arc;
use std::time::SystemTime;

use hashu_core::hashprice::HashpriceOracle;

/// Seconds in one `THH` (1 Th/s sustained for an hour).
pub const SECS_PER_THH: f64 = 3600.0;

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
    #[error("price overflow for {0} THH")]
    Overflow(u64),
    #[error("amount must be at least 1 THH")]
    ZeroAmount,
}

/// Prices `THH` in one payment rail's base unit.
pub trait ThhPricer: Send + Sync + Debug {
    /// Cost of `amount_thh` in the rail's base unit, rounded up.
    fn price(&self, amount_thh: u64) -> Result<u64, PricingError>;
}

/// Lightning pricer: sats per `THH` from the hashprice oracle (§4.4), plus
/// an operator markup in basis points.
pub struct SatsPerThhFromHashprice {
    oracle: Arc<dyn HashpriceOracle + Send + Sync>,
    markup_bps: u32,
}

impl Debug for SatsPerThhFromHashprice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SatsPerThhFromHashprice")
            .field("markup_bps", &self.markup_bps)
            .finish_non_exhaustive()
    }
}

impl SatsPerThhFromHashprice {
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

    /// Current sats per `THH` before markup, if the oracle is fresh.
    pub fn sats_per_thh_now(&self) -> Option<f64> {
        self.oracle
            .sample_at(SystemTime::now())
            .map(|sats_per_ths| sats_per_ths * SECS_PER_THH)
    }
}

impl ThhPricer for SatsPerThhFromHashprice {
    fn price(&self, amount_thh: u64) -> Result<u64, PricingError> {
        if amount_thh == 0 {
            return Err(PricingError::ZeroAmount);
        }
        let per_thh = self.sats_per_thh_now().ok_or(PricingError::NoPrice)?;
        if !per_thh.is_finite() || per_thh <= 0.0 {
            return Err(PricingError::InvalidPrice(per_thh));
        }
        let sats = amount_thh as f64 * per_thh * (1.0 + self.markup_bps as f64 / BPS);
        let sats = sats.ceil();
        if !sats.is_finite() || sats >= u64::MAX as f64 {
            return Err(PricingError::Overflow(amount_thh));
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
        // 48 sats/Th/day = 2 sats per THH.
        let p = SatsPerThhFromHashprice::new(oracle_at(Some(48.0)));
        assert_eq!(p.price(1).unwrap(), 2);
        assert_eq!(p.price(100).unwrap(), 200);
    }

    #[test]
    fn applies_markup_and_rounds_up() {
        // 2 sats/THH × 1.05 = 2.1 -> 3 for one THH, 210 for 100.
        let p = SatsPerThhFromHashprice::new(oracle_at(Some(48.0))).with_markup_bps(500);
        assert_eq!(p.price(1).unwrap(), 3);
        assert_eq!(p.price(100).unwrap(), 210);
    }

    #[test]
    fn never_bills_zero() {
        let p = SatsPerThhFromHashprice::new(oracle_at(Some(1e-9)));
        assert_eq!(p.price(1).unwrap(), 1);
    }

    #[test]
    fn refuses_without_fresh_price() {
        let p = SatsPerThhFromHashprice::new(oracle_at(None));
        assert_eq!(p.price(1), Err(PricingError::NoPrice));
    }

    #[test]
    fn refuses_zero_amount_and_bad_price() {
        let p = SatsPerThhFromHashprice::new(oracle_at(Some(48.0)));
        assert_eq!(p.price(0), Err(PricingError::ZeroAmount));
        let neg = SatsPerThhFromHashprice::new(oracle_at(Some(-1.0)));
        assert!(matches!(neg.price(1), Err(PricingError::InvalidPrice(_))));
    }
}
