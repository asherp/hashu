//! Hashrate history for the operator GUI.
//!
//! The proxy only sees shares, not hashes. Each accepted share of
//! difficulty `D` stands for `D × 2^32` hashes on average, so the work
//! through the proxy is the running sum of accepted share difficulty
//! ([`ProxyMetrics::accepted_hashes`]). [`HashrateMeter`] samples that sum
//! once a tick and keeps:
//!
//! - a rolling series of `(time, total hashes, hashrate)`, where hashrate is
//!   the work over the trailing window divided by its length; and
//! - per redemption, the ledger's `accumulated_hashes` over time: the
//!   hashrate integrated since the redemption opened, which is what the
//!   redeemer has been delivered so far against its `target_hashes`.
//!
//! Share-based hashrate is noisy over short windows (a share every few
//! seconds per miner is normal), so callers can recompute it over any
//! window from the cumulative series.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hashu_core::redemption::{RedemptionLedger, HASHES_PER_TIH};
use serde::Serialize;

use crate::ProxyMetrics;

/// Samples kept in the rolling series (one hour at the default 1 s tick).
pub const MAX_SAMPLES: usize = 3600;
/// Points kept per redemption before the series is thinned out.
pub const MAX_REDEMPTION_POINTS: usize = 2000;

/// One tick of the rolling series.
#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Sample {
    /// Unix time, milliseconds.
    pub t_ms: u64,
    /// Work of every accepted share since the proxy started, in hashes.
    pub total_hashes: f64,
    /// Hashes per second over the trailing window.
    pub hashrate_hs: f64,
}

/// A redemption's delivered work over time.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RedemptionTrack {
    pub id: String,
    pub amount_tih: u64,
    pub target_hashes: f64,
    /// When the meter first saw it (Unix ms); close to when it opened.
    pub opened_ms: u64,
    pub completed_ms: Option<u64>,
    /// `(Unix ms, accumulated hashes)`, oldest first.
    pub points: Vec<(u64, f64)>,
}

impl RedemptionTrack {
    pub fn delivered_tih(&self) -> f64 {
        self.points.last().map(|p| p.1).unwrap_or(0.0) / HASHES_PER_TIH as f64
    }
}

#[derive(Default)]
struct Inner {
    samples: VecDeque<Sample>,
    tracks: HashMap<String, RedemptionTrack>,
    /// Track ids in the order they were first seen.
    order: Vec<String>,
}

/// Samples the proxy's accepted work and the ledger. See the module docs.
pub struct HashrateMeter {
    metrics: Arc<ProxyMetrics>,
    ledger: Arc<RedemptionLedger>,
    window: Duration,
    inner: Mutex<Inner>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

impl HashrateMeter {
    /// `window` is the trailing span the reported hashrate averages over.
    pub fn new(
        metrics: Arc<ProxyMetrics>,
        ledger: Arc<RedemptionLedger>,
        window: Duration,
    ) -> Self {
        Self {
            metrics,
            ledger,
            window,
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn window(&self) -> Duration {
        self.window
    }

    /// Sample now. [`run`](Self::run) calls this every tick.
    pub fn sample(&self) {
        self.sample_at(now_ms());
    }

    /// Sample with an explicit clock, for tests.
    pub fn sample_at(&self, t_ms: u64) {
        let total = self.metrics.accepted_hashes();
        let redemptions = self.ledger.all();
        let mut inner = self.inner.lock().unwrap();

        // Rolling hashrate: work since the oldest sample inside the window.
        let window_ms = self.window.as_millis() as u64;
        let since = t_ms.saturating_sub(window_ms);
        let base = inner.samples.iter().find(|s| s.t_ms >= since).copied();
        let hashrate_hs = match base {
            Some(b) if t_ms > b.t_ms => {
                (total - b.total_hashes).max(0.0) * 1000.0 / (t_ms - b.t_ms) as f64
            }
            _ => 0.0,
        };
        inner.samples.push_back(Sample {
            t_ms,
            total_hashes: total,
            hashrate_hs,
        });
        while inner.samples.len() > MAX_SAMPLES {
            inner.samples.pop_front();
        }

        for r in redemptions {
            let id = r.id.to_string();
            let acc = r.accumulated_hashes as f64;
            if !inner.tracks.contains_key(&id) {
                inner.order.push(id.clone());
                inner.tracks.insert(
                    id.clone(),
                    RedemptionTrack {
                        id: id.clone(),
                        amount_tih: r.amount_tih,
                        target_hashes: r.target_hashes as f64,
                        opened_ms: t_ms,
                        completed_ms: None,
                        // Start the curve where the redemption stands when
                        // first seen: zero for a new one, its restored
                        // progress after a restart (no false jump).
                        points: vec![(t_ms, acc)],
                    },
                );
            }
            let track = inner.tracks.get_mut(&id).expect("inserted above");
            if track.completed_ms.is_some() {
                continue;
            }
            if acc != track.points.last().map(|p| p.1).unwrap_or(-1.0) || !r.state.is_finished() {
                track.points.push((t_ms, acc));
            }
            if r.state.is_finished() {
                track.completed_ms = Some(t_ms);
            }
            if track.points.len() > MAX_REDEMPTION_POINTS {
                thin(&mut track.points);
            }
        }
    }

    /// Sample every `tick` until the task is dropped.
    pub async fn run(self: Arc<Self>, tick: Duration) {
        let mut t = tokio::time::interval(tick);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            self.sample();
        }
    }

    /// Samples newer than `since_ms` (all of them for 0).
    pub fn samples_since(&self, since_ms: u64) -> Vec<Sample> {
        let inner = self.inner.lock().unwrap();
        inner
            .samples
            .iter()
            .filter(|s| s.t_ms > since_ms)
            .copied()
            .collect()
    }

    /// Average hashrate over the trailing `window`, from the cumulative
    /// work (steadier than the per-tick rate). `None` before two samples.
    pub fn rate_over(&self, window: Duration) -> Option<f64> {
        let inner = self.inner.lock().unwrap();
        let last = *inner.samples.back()?;
        let since = last.t_ms.saturating_sub(window.as_millis() as u64);
        let first = *inner.samples.iter().find(|x| x.t_ms >= since)?;
        if last.t_ms <= first.t_ms {
            return None;
        }
        Some(
            (last.total_hashes - first.total_hashes).max(0.0) * 1000.0
                / (last.t_ms - first.t_ms) as f64,
        )
    }

    /// The latest sample, if any.
    pub fn latest(&self) -> Option<Sample> {
        self.inner.lock().unwrap().samples.back().copied()
    }

    /// Every redemption seen, in the order they opened.
    pub fn tracks(&self) -> Vec<RedemptionTrack> {
        let inner = self.inner.lock().unwrap();
        inner
            .order
            .iter()
            .filter_map(|id| inner.tracks.get(id).cloned())
            .collect()
    }

    /// The redemption the GUI should chart: the oldest still open, else the
    /// most recently opened one.
    pub fn current_track(&self) -> Option<RedemptionTrack> {
        let tracks = self.tracks();
        tracks
            .iter()
            .find(|t| t.completed_ms.is_none())
            .or_else(|| tracks.last())
            .cloned()
    }
}

/// Halve a series, keeping the first and last points.
fn thin(points: &mut Vec<(u64, f64)>) {
    let last = *points.last().expect("non-empty");
    let mut i = 0;
    points.retain(|_| {
        i += 1;
        i % 2 == 1
    });
    if points.last() != Some(&last) {
        points.push(last);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::redemption::{Redemption, RedemptionId, HASHES_PER_DIFF_ONE};

    fn meter() -> (Arc<ProxyMetrics>, Arc<RedemptionLedger>, HashrateMeter) {
        let m = Arc::new(ProxyMetrics::default());
        let l = Arc::new(RedemptionLedger::new());
        let h = HashrateMeter::new(m.clone(), l.clone(), Duration::from_secs(10));
        (m, l, h)
    }

    #[test]
    fn hashrate_is_work_over_the_window() {
        let (m, _, h) = meter();
        h.sample_at(1_000);
        // 1000 shares of difficulty 1000 over 10 s.
        for _ in 0..1000 {
            m.add_accepted_work(1000.0);
        }
        h.sample_at(11_000);
        let s = h.latest().unwrap();
        let expected = 1000.0 * 1000.0 * HASHES_PER_DIFF_ONE as f64 / 10.0;
        assert!((s.hashrate_hs - expected).abs() / expected < 1e-9);
        assert_eq!(s.total_hashes, 1000.0 * 1000.0 * HASHES_PER_DIFF_ONE as f64);
    }

    #[test]
    fn old_work_falls_out_of_the_window() {
        let (m, _, h) = meter();
        h.sample_at(0);
        m.add_accepted_work(1e6);
        h.sample_at(5_000);
        h.sample_at(20_000);
        h.sample_at(30_000);
        assert_eq!(h.latest().unwrap().hashrate_hs, 0.0);
    }

    #[test]
    fn rate_over_averages_the_window() {
        let (m, _, h) = meter();
        assert_eq!(h.rate_over(Duration::from_secs(60)), None);
        h.sample_at(0);
        m.add_accepted_work(1.0);
        h.sample_at(1_000);
        let r = h.rate_over(Duration::from_secs(60)).unwrap();
        assert_eq!(r, HASHES_PER_DIFF_ONE as f64);
    }

    #[test]
    fn ignores_bad_difficulty() {
        let (m, _, _) = meter();
        m.add_accepted_work(f64::NAN);
        m.add_accepted_work(-1.0);
        assert_eq!(m.accepted_hashes(), 0.0);
    }

    #[test]
    fn tracks_a_redemption_until_it_completes() {
        let (_, l, h) = meter();
        let id = RedemptionId::from_bytes([7; 16]);
        l.open(Redemption::new(id, 1, "pool:1".into(), "w".into()))
            .unwrap();
        h.sample_at(1_000);
        // Half a TiH at a time; difficulty d credits d × 2^32 hashes.
        let half = HASHES_PER_TIH as f64 / 2.0 / HASHES_PER_DIFF_ONE as f64;
        l.credit_share(&id, half).unwrap();
        h.sample_at(2_000);
        l.credit_share(&id, half * 1.01).unwrap();
        h.sample_at(3_000);
        h.sample_at(4_000);

        let t = h.current_track().unwrap();
        assert_eq!(t.opened_ms, 1_000);
        assert_eq!(t.completed_ms, Some(3_000));
        assert_eq!(t.points.first().unwrap(), &(1_000, 0.0));
        assert_eq!(t.points.last().unwrap().0, 3_000);
        assert!(t.delivered_tih() >= 1.0);
    }

    #[test]
    fn current_track_prefers_an_open_redemption() {
        let (_, l, h) = meter();
        let a = RedemptionId::from_bytes([1; 16]);
        let b = RedemptionId::from_bytes([2; 16]);
        l.open(Redemption::new(a, 1, "pool:1".into(), "w".into()))
            .unwrap();
        l.open(Redemption::new(b, 1, "pool:1".into(), "w".into()))
            .unwrap();
        l.credit_share(&a, 1e9).unwrap();
        h.sample_at(1_000);
        assert_eq!(h.current_track().unwrap().id, b.to_string());
    }

    #[test]
    fn thinning_keeps_the_ends() {
        let mut p: Vec<(u64, f64)> = (0..10).map(|i| (i, i as f64)).collect();
        thin(&mut p);
        assert_eq!(p.first(), Some(&(0, 0.0)));
        assert_eq!(p.last(), Some(&(9, 9.0)));
        assert!(p.len() <= 6);
    }
}
