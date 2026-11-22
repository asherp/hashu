//! The miner's hashrate plan: TH/s they expect to run in each future
//! difficulty epoch (build-outs, rigs coming online or going away).
//!
//! With epochs on (#126), each future epoch sells against
//! `hashrate × the part of the epoch inside the horizon − owed`. The
//! hashrate for a future epoch is the miner's plan when they entered one,
//! and otherwise the live average (EWMA) carried forward. The current
//! epoch always uses the measured average: what the rigs do now.
//!
//! A plan is a promise buyers price, so it is kept next to what was
//! delivered. When a planned epoch starts, its plan is frozen into the
//! history (it can no longer be changed), and the epoch's delivered work
//! is the sum of the mint's published hourly reliability record (#109)
//! over the hours the epoch ran. Planned vs delivered per epoch is then
//! checkable from public data.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use hashu_core::epoch::EpochClock;
use serde::{Deserialize, Serialize};

pub use crate::epoch::MAX_EPOCHS_AHEAD;

/// Highest TH/s a plan accepts for one epoch (1 ZH/s): a typo guard.
pub const MAX_PLANNED_THS: f64 = 1e9;

/// One past or running epoch: what was planned for it and when it ran.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EpochRun {
    /// TH/s planned for the epoch when it started; `None`: none was set,
    /// so it sold on the live average.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_ths: Option<f64>,
    /// When this mint first saw the epoch running (unix ms).
    pub start_ms: u64,
    /// When the next epoch started; `None` while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_ms: Option<u64>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Persisted {
    /// Planned TH/s by future epoch.
    #[serde(default)]
    planned: BTreeMap<u64, f64>,
    /// Epochs seen running, with their frozen plan.
    #[serde(default)]
    history: BTreeMap<u64, EpochRun>,
}

/// A mint's hashrate plan, kept in `path` (in memory with `None`).
pub struct HashratePlan {
    inner: Mutex<Persisted>,
    path: Option<PathBuf>,
}

impl HashratePlan {
    pub fn open(path: Option<PathBuf>) -> anyhow::Result<Self> {
        let p = match &path {
            Some(path) => match std::fs::read(path) {
                Ok(b) => serde_json::from_slice(&b)
                    .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
                Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
            },
            None => Persisted::default(),
        };
        Ok(Self {
            inner: Mutex::new(p),
            path,
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
            tracing::error!(path = %path.display(), error = %e, "could not save hashrate plan");
        }
    }

    /// Planned TH/s for `epoch`, if the miner set one. For the current or
    /// a past epoch this is the plan frozen when it started.
    pub fn planned(&self, epoch: u64) -> Option<f64> {
        let g = self.inner.lock().unwrap();
        g.planned
            .get(&epoch)
            .copied()
            .or_else(|| g.history.get(&epoch).and_then(|r| r.planned_ths))
    }

    /// Every future epoch's plan.
    pub fn future(&self) -> BTreeMap<u64, f64> {
        self.inner.lock().unwrap().planned.clone()
    }

    /// Epochs seen running, oldest first.
    pub fn history(&self) -> BTreeMap<u64, EpochRun> {
        self.inner.lock().unwrap().history.clone()
    }

    /// Check a plan change before it is made: only future epochs, up to
    /// [`MAX_EPOCHS_AHEAD`], and a sane, non-negative TH/s.
    pub fn check(clock: &EpochClock, epoch: u64, ths: Option<f64>) -> Result<(), String> {
        let current = clock.current();
        if epoch <= current {
            return Err(format!(
                "epoch {epoch} has started; the current epoch sells on measured hashrate"
            ));
        }
        if epoch > current + MAX_EPOCHS_AHEAD {
            return Err(format!(
                "epoch {epoch} is too far ahead (up to {})",
                current + MAX_EPOCHS_AHEAD
            ));
        }
        if let Some(t) = ths {
            if !t.is_finite() || t < 0.0 {
                return Err("planned hashrate must be a number of TH/s, 0 or more".into());
            }
            if t > MAX_PLANNED_THS {
                return Err(format!("planned hashrate is over {MAX_PLANNED_THS} TH/s"));
            }
        }
        Ok(())
    }

    /// Set (`Some`) or clear (`None`) the plan for future epochs. All or
    /// nothing: one bad entry changes nothing.
    pub fn set(&self, clock: &EpochClock, changes: &[(u64, Option<f64>)]) -> Result<(), String> {
        for (e, t) in changes {
            Self::check(clock, *e, *t)?;
        }
        let mut g = self.inner.lock().unwrap();
        for (e, t) in changes {
            match t {
                Some(t) => g.planned.insert(*e, *t),
                None => g.planned.remove(e),
            };
        }
        self.save(&g);
        Ok(())
    }

    /// What the plan would be after `changes`, without making them (to
    /// check that a lower plan still covers what is sold).
    pub fn with(&self, changes: &[(u64, Option<f64>)]) -> BTreeMap<u64, f64> {
        let mut m = self.future();
        for (e, t) in changes {
            match t {
                Some(t) => m.insert(*e, *t),
                None => m.remove(e),
            };
        }
        m
    }

    /// Follow the chain: when a new epoch starts, close the last one, open
    /// the new one and freeze its plan into the history.
    pub fn observe(&self, clock: &EpochClock, now_ms: u64) {
        let current = clock.current();
        let mut g = self.inner.lock().unwrap();
        if g.history.contains_key(&current) {
            return;
        }
        let ended: Vec<u64> = g
            .history
            .iter()
            .filter(|(e, r)| **e < current && r.end_ms.is_none())
            .map(|(e, _)| *e)
            .collect();
        for e in ended {
            if let Some(r) = g.history.get_mut(&e) {
                r.end_ms = Some(now_ms);
            }
        }
        let planned_ths = g.planned.remove(&current);
        g.history.insert(
            current,
            EpochRun {
                planned_ths,
                start_ms: now_ms,
                end_ms: None,
            },
        );
        // Plans for epochs that have passed unseen (the mint was down).
        g.planned.retain(|e, _| *e > current);
        self.save(&g);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::epoch::BLOCKS_PER_EPOCH;

    fn clock(epoch: u64) -> EpochClock {
        // Halfway through `epoch`.
        EpochClock::new(epoch * BLOCKS_PER_EPOCH + 1000)
    }

    #[test]
    fn plans_only_future_epochs() {
        let p = HashratePlan::open(None).unwrap();
        let c = clock(10);
        assert!(p.set(&c, &[(10, Some(5.0))]).is_err());
        assert!(p.set(&c, &[(9, Some(5.0))]).is_err());
        assert!(p
            .set(&c, &[(10 + MAX_EPOCHS_AHEAD + 1, Some(5.0))])
            .is_err());
        assert!(p.set(&c, &[(11, Some(-1.0))]).is_err());
        assert!(p.set(&c, &[(11, Some(f64::NAN))]).is_err());
        // All or nothing.
        assert!(p.set(&c, &[(11, Some(5.0)), (9, Some(1.0))]).is_err());
        assert_eq!(p.planned(11), None);
        p.set(&c, &[(11, Some(5.0)), (12, Some(20.0))]).unwrap();
        assert_eq!(p.planned(12), Some(20.0));
        p.set(&c, &[(12, None)]).unwrap();
        assert_eq!(p.planned(12), None);
    }

    #[test]
    fn a_starting_epoch_freezes_its_plan() {
        let p = HashratePlan::open(None).unwrap();
        p.observe(&clock(10), 1_000);
        p.set(&clock(10), &[(11, Some(5.0)), (12, Some(8.0))])
            .unwrap();
        p.observe(&clock(11), 2_000);
        // Epoch 11 is running: its plan is history and can't change.
        assert!(p.set(&clock(11), &[(11, Some(50.0))]).is_err());
        assert_eq!(p.planned(11), Some(5.0));
        let h = p.history();
        assert_eq!(h[&10].end_ms, Some(2_000));
        assert_eq!(h[&10].planned_ths, None);
        assert_eq!(h[&11].planned_ths, Some(5.0));
        assert_eq!(h[&11].start_ms, 2_000);
        assert_eq!(h[&11].end_ms, None);
        assert_eq!(p.future().into_keys().collect::<Vec<_>>(), vec![12]);
        // Seeing the same epoch again changes nothing.
        p.observe(&clock(11), 3_000);
        assert_eq!(p.history()[&11].start_ms, 2_000);
    }

    #[test]
    fn plan_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("hashu-plan-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plan.json");
        let _ = std::fs::remove_file(&path);
        let p = HashratePlan::open(Some(path.clone())).unwrap();
        p.set(&clock(10), &[(11, Some(5.0))]).unwrap();
        p.observe(&clock(10), 1);
        drop(p);
        let p = HashratePlan::open(Some(path)).unwrap();
        assert_eq!(p.planned(11), Some(5.0));
        assert!(p.history().contains_key(&10));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
