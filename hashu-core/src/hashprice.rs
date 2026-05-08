//! Hashprice oracle trait + in-memory sample buffer with linear interpolation.
//! ARCHITECTURE.md §4.4.
//!
//! Hashprice unit: **sats per Th-second** — sats earned by 1 Th/s of hashrate
//! over 1 second under current network/fee conditions. Multiplying by hashrate
//! (Th/s) and integrating over time yields sats earned.
//!
//! Luxor and Hashrate Index quote hashprice in `{USD, BTC} / PH / day`. We
//! consume the BTC-denominated form to avoid pulling in a USD price feed.
//! Conversion: `sats/Th-sec = sats/PH/day / 1000 / 86_400`. Use the
//! `from_*` constructors on `HashpriceSample` to convert at ingest.

use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

/// One observation of bitcoin's hashprice.
#[derive(Clone, Debug, PartialEq)]
pub struct HashpriceSample {
    pub t: SystemTime,
    /// Sats per Th-second.
    pub sats_per_ths: f64,
}

/// Sats in one BTC.
pub const SATS_PER_BTC: f64 = 100_000_000.0;
/// Th in one PH.
pub const TH_PER_PH: f64 = 1000.0;
/// Seconds in a day.
pub const SECS_PER_DAY: f64 = 86_400.0;

impl HashpriceSample {
    pub fn from_sats_per_ths(t: SystemTime, sats_per_ths: f64) -> Self {
        Self { t, sats_per_ths }
    }

    pub fn from_sats_per_th_per_day(t: SystemTime, sats_per_th_day: f64) -> Self {
        Self {
            t,
            sats_per_ths: sats_per_th_day / SECS_PER_DAY,
        }
    }

    /// Luxor's native USD-free quote: sats per PH per day.
    pub fn from_sats_per_ph_per_day(t: SystemTime, sats_per_ph_day: f64) -> Self {
        Self {
            t,
            sats_per_ths: sats_per_ph_day / TH_PER_PH / SECS_PER_DAY,
        }
    }

    /// Luxor's BTC quote: BTC per PH per day.
    pub fn from_btc_per_ph_per_day(t: SystemTime, btc_per_ph_day: f64) -> Self {
        Self::from_sats_per_ph_per_day(t, btc_per_ph_day * SATS_PER_BTC)
    }

    pub fn sats_per_th_per_day(&self) -> f64 {
        self.sats_per_ths * SECS_PER_DAY
    }

    pub fn sats_per_ph_per_day(&self) -> f64 {
        self.sats_per_ths * TH_PER_PH * SECS_PER_DAY
    }

    pub fn btc_per_ph_per_day(&self) -> f64 {
        self.sats_per_ph_per_day() / SATS_PER_BTC
    }
}

/// Source of hashprice values. Pluggable per ARCHITECTURE.md §4.4.
pub trait HashpriceOracle {
    /// Hashprice in sats per Th-second at time `t`, or `None` if no value is
    /// available (no samples, query before earliest, or stale).
    fn sample_at(&self, t: SystemTime) -> Option<f64>;

    /// Most recently observed sample, if any.
    fn latest(&self) -> Option<HashpriceSample>;
}

/// Bounded ring buffer of hashprice samples with linear interpolation.
///
/// Samples are kept sorted by time ascending. The capacity is enforced by
/// evicting the oldest sample when full. Out-of-order pushes are handled
/// (insertion-sorted) but slow — Luxor pulls are monotonic in practice.
#[derive(Clone, Debug)]
pub struct SampleBuffer {
    samples: VecDeque<HashpriceSample>,
    capacity: usize,
    /// How long after the latest sample we still treat the buffer as fresh.
    /// Queries past `latest.t + max_staleness` return `None`.
    max_staleness: Duration,
}

impl SampleBuffer {
    pub fn new(capacity: usize, max_staleness: Duration) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            max_staleness,
        }
    }

    pub fn push(&mut self, sample: HashpriceSample) {
        let pos = self
            .samples
            .iter()
            .rposition(|s| s.t <= sample.t)
            .map(|i| i + 1)
            .unwrap_or(0);
        self.samples.insert(pos, sample);
        while self.samples.len() > self.capacity {
            self.samples.pop_front();
        }
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn max_staleness(&self) -> Duration {
        self.max_staleness
    }
}

impl HashpriceOracle for SampleBuffer {
    fn latest(&self) -> Option<HashpriceSample> {
        self.samples.back().cloned()
    }

    fn sample_at(&self, t: SystemTime) -> Option<f64> {
        let first = self.samples.front()?;
        let last = self.samples.back()?;

        if t < first.t {
            return None;
        }

        if t > last.t {
            return match t.duration_since(last.t) {
                Ok(gap) if gap <= self.max_staleness => Some(last.sats_per_ths),
                _ => None,
            };
        }

        let mut prev = first;
        for s in self.samples.iter() {
            if s.t >= t {
                if s.t == t || s.t == prev.t {
                    return Some(s.sats_per_ths);
                }
                let span = s.t.duration_since(prev.t).ok()?.as_nanos() as f64;
                let into = t.duration_since(prev.t).ok()?.as_nanos() as f64;
                let frac = into / span;
                return Some(prev.sats_per_ths + frac * (s.sats_per_ths - prev.sats_per_ths));
            }
            prev = s;
        }
        Some(last.sats_per_ths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn t(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn empty_buffer_returns_none() {
        let buf = SampleBuffer::new(8, Duration::from_secs(600));
        assert!(buf.is_empty());
        assert_eq!(buf.latest(), None);
        assert_eq!(buf.sample_at(t(0)), None);
    }

    #[test]
    fn single_sample_exact_match() {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        buf.push(HashpriceSample {
            t: t(1000),
            sats_per_ths: 1.5e-7,
        });
        assert!(approx_eq(buf.sample_at(t(1000)).unwrap(), 1.5e-7));
    }

    #[test]
    fn before_first_sample_is_none() {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        buf.push(HashpriceSample {
            t: t(1000),
            sats_per_ths: 1.0,
        });
        assert_eq!(buf.sample_at(t(999)), None);
    }

    #[test]
    fn after_latest_within_staleness_returns_latest() {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        buf.push(HashpriceSample {
            t: t(1000),
            sats_per_ths: 2.0,
        });
        // 599s past latest: still fresh.
        assert!(approx_eq(buf.sample_at(t(1599)).unwrap(), 2.0));
        // 600s past latest: still fresh (boundary inclusive).
        assert!(approx_eq(buf.sample_at(t(1600)).unwrap(), 2.0));
    }

    #[test]
    fn after_latest_beyond_staleness_is_none() {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        buf.push(HashpriceSample {
            t: t(1000),
            sats_per_ths: 2.0,
        });
        assert_eq!(buf.sample_at(t(1601)), None);
    }

    #[test]
    fn interpolates_linearly_between_samples() {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        buf.push(HashpriceSample {
            t: t(1000),
            sats_per_ths: 1.0,
        });
        buf.push(HashpriceSample {
            t: t(2000),
            sats_per_ths: 3.0,
        });
        // Midpoint → 2.0
        assert!(approx_eq(buf.sample_at(t(1500)).unwrap(), 2.0));
        // Quarter → 1.5
        assert!(approx_eq(buf.sample_at(t(1250)).unwrap(), 1.5));
        // Three-quarters → 2.5
        assert!(approx_eq(buf.sample_at(t(1750)).unwrap(), 2.5));
    }

    #[test]
    fn boundary_equality_returns_endpoint() {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        buf.push(HashpriceSample {
            t: t(1000),
            sats_per_ths: 1.0,
        });
        buf.push(HashpriceSample {
            t: t(2000),
            sats_per_ths: 3.0,
        });
        assert!(approx_eq(buf.sample_at(t(1000)).unwrap(), 1.0));
        assert!(approx_eq(buf.sample_at(t(2000)).unwrap(), 3.0));
    }

    #[test]
    fn capacity_evicts_oldest() {
        let mut buf = SampleBuffer::new(3, Duration::from_secs(600));
        for i in 0..5u64 {
            buf.push(HashpriceSample {
                t: t(1000 + i * 100),
                sats_per_ths: i as f64,
            });
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.latest().unwrap().sats_per_ths, 4.0);
        // Oldest two were evicted; querying at their times now fails.
        assert_eq!(buf.sample_at(t(1000)), None);
        assert_eq!(buf.sample_at(t(1100)), None);
        // Earliest retained sample is at t=1200.
        assert!(approx_eq(buf.sample_at(t(1200)).unwrap(), 2.0));
    }

    #[test]
    fn out_of_order_push_is_sorted() {
        let mut buf = SampleBuffer::new(8, Duration::from_secs(600));
        buf.push(HashpriceSample {
            t: t(2000),
            sats_per_ths: 3.0,
        });
        buf.push(HashpriceSample {
            t: t(1000),
            sats_per_ths: 1.0,
        });
        buf.push(HashpriceSample {
            t: t(1500),
            sats_per_ths: 2.0,
        });
        assert!(approx_eq(buf.sample_at(t(1250)).unwrap(), 1.5));
        assert!(approx_eq(buf.sample_at(t(1750)).unwrap(), 2.5));
        assert_eq!(buf.latest().unwrap().t, t(2000));
    }

    #[test]
    fn th_per_day_round_trips() {
        let s = HashpriceSample::from_sats_per_th_per_day(t(0), 86_400.0);
        assert!(approx_eq(s.sats_per_ths, 1.0));
        assert!(approx_eq(s.sats_per_th_per_day(), 86_400.0));
    }

    #[test]
    fn ph_per_day_round_trips() {
        // 1000 sats/PH/day == 1 sat/Th/day == 1/86400 sats/Th-sec
        let s = HashpriceSample::from_sats_per_ph_per_day(t(0), 1000.0);
        assert!(approx_eq(s.sats_per_th_per_day(), 1.0));
        assert!(approx_eq(s.sats_per_ph_per_day(), 1000.0));
    }

    #[test]
    fn btc_per_ph_per_day_round_trips() {
        // 1e-5 BTC/PH/day == 1000 sats/PH/day
        let s = HashpriceSample::from_btc_per_ph_per_day(t(0), 1e-5);
        assert!(approx_eq(s.sats_per_ph_per_day(), 1000.0));
        assert!(approx_eq(s.btc_per_ph_per_day(), 1e-5));
    }

    #[test]
    fn matches_luxor_worked_example() {
        // Luxor docs: at $50/PH/day, a 100 TH/s ASIC earns ~$5/day.
        // Equivalent BTC quote at $100k/BTC: 50 / 100_000 = 5e-4 BTC/PH/day.
        let s = HashpriceSample::from_btc_per_ph_per_day(t(0), 5e-4);
        // Earnings of a 100 Th/s rig over 1 day:
        //   sats/day = sats_per_ths * 100 Th/s * 86400 s
        let day_sats_for_100_ths = s.sats_per_ths * 100.0 * SECS_PER_DAY;
        // Should equal 5e-5 BTC = 5000 sats (the $5 worth).
        assert!(approx_eq(day_sats_for_100_ths, 5000.0));
    }

    #[test]
    fn one_thh_earnings_at_known_hashprice() {
        // 1 THH = 1 Th sustained for 1 hour.
        // At hashprice = 50 sats/Th/day, 1 THH earns 50 * (1/24) ≈ 2.083 sats.
        let s = HashpriceSample::from_sats_per_th_per_day(t(0), 50.0);
        let one_thh_sats = s.sats_per_ths * 1.0 * 3600.0; // 1 Th/s * 3600 s
        assert!(approx_eq(one_thh_sats, 50.0 / 24.0));
    }
}
