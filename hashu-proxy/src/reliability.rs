//! Hourly buckets of credited work per mint, the input to the mint
//! reliability record (issue #109, [`hashu_core::reliability`]).
//!
//! [`crate::credit::ShareCrediter`] offers every share it credits to a
//! redemption. The share lands in the bucket of the redemption's mint
//! (`""` for the operator's mint, else the supplier's name) for the UTC
//! hour it was credited in. The publisher takes closed hours with
//! [`ReliabilityRecorder::take_closed`].
//!
//! With [`ReliabilityRecorder::persistent`], [`ReliabilityRecorder::save`]
//! writes the open buckets to disk so a restart mid-hour keeps the hour's
//! work, and closed hours not yet published survive to be published late.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hashu_core::reliability::{hour_start, HourBucket};
use serde::{Deserialize, Serialize};

/// Mint key of the operator's own mint.
pub const OPERATOR_MINT: &str = "";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Snapshot {
    /// (mint, hour) → bucket. A list, since JSON keys are strings.
    buckets: Vec<(String, u64, HourBucket)>,
}

#[derive(Debug, Default)]
pub struct ReliabilityRecorder {
    buckets: Mutex<BTreeMap<(String, u64), HourBucket>>,
    path: Option<PathBuf>,
}

impl ReliabilityRecorder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Buckets restored from `path` (if it exists) and saved back there by
    /// [`Self::save`].
    pub fn persistent(path: &Path) -> io::Result<Self> {
        let buckets = match std::fs::read(path) {
            Ok(b) => match serde_json::from_slice::<Snapshot>(&b) {
                Ok(s) => s.buckets.into_iter().map(|(m, h, b)| ((m, h), b)).collect(),
                Err(e) => {
                    tracing::warn!(?path, error = %e, "unreadable reliability buckets, starting empty");
                    BTreeMap::new()
                }
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => BTreeMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            buckets: Mutex::new(buckets),
            path: Some(path.to_path_buf()),
        })
    }

    /// Credit one share to `mint`'s bucket for the hour of `now_s`.
    pub fn record(&self, mint: &str, now_s: u64, difficulty: f64, header: &[u8]) {
        self.lock()
            .entry((mint.to_string(), hour_start(now_s)))
            .or_default()
            .record(difficulty, header);
    }

    /// Remove and return every bucket for an hour that started before
    /// `before_hour`, oldest first.
    pub fn take_closed(&self, before_hour: u64) -> Vec<(String, u64, HourBucket)> {
        let mut b = self.lock();
        let closed: Vec<(String, u64)> = b
            .keys()
            .filter(|(_, h)| *h < before_hour)
            .cloned()
            .collect();
        let mut out: Vec<(String, u64, HourBucket)> = closed
            .into_iter()
            .filter_map(|k| b.remove(&k).map(|v| (k.0, k.1, v)))
            .collect();
        out.sort_by_key(|(_, h, _)| *h);
        out
    }

    /// Credited work so far in `mint`'s bucket for `hour`.
    pub fn peek(&self, mint: &str, hour: u64) -> Option<HourBucket> {
        self.lock().get(&(mint.to_string(), hour)).cloned()
    }

    /// Put a bucket back (a publish that failed, to retry later).
    pub fn restore(&self, mint: String, hour: u64, bucket: HourBucket) {
        let mut b = self.lock();
        let slot = b.entry((mint, hour)).or_default();
        // Shares credited after the take land in a fresh bucket: fold them.
        slot.shares += bucket.shares;
        slot.difficulty += bucket.difficulty;
        for h in bucket.blocks {
            if !slot.blocks.contains(&h) {
                slot.blocks.push(h);
            }
        }
        for s in bucket.top.into_vec() {
            slot.top.offer(s);
        }
    }

    /// Write the buckets to the persistent path, if any.
    pub fn save(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let snap = Snapshot {
            buckets: self
                .lock()
                .iter()
                .map(|((m, h), b)| (m.clone(), *h, b.clone()))
                .collect(),
        };
        let bytes = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, u64), HourBucket>> {
        self.buckets.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_by_mint_and_hour() {
        let r = ReliabilityRecorder::new();
        r.record("", 3600, 256.0, &[0; 80]);
        r.record("", 3700, 256.0, &[1; 80]);
        r.record("alice", 3700, 512.0, &[2; 80]);
        r.record("", 7300, 256.0, &[3; 80]);
        let closed = r.take_closed(7200);
        assert_eq!(closed.len(), 2);
        assert_eq!(closed[0].2.shares + closed[1].2.shares, 3);
        assert!(r.take_closed(7200).is_empty());
        assert_eq!(r.peek("", 7200).unwrap().shares, 1);
    }

    #[test]
    fn survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buckets.json");
        let r = ReliabilityRecorder::persistent(&path).unwrap();
        r.record("bob", 10, 256.0, &[0; 80]);
        r.save().unwrap();
        let again = ReliabilityRecorder::persistent(&path).unwrap();
        assert_eq!(again.peek("bob", 0).unwrap().shares, 1);
    }

    #[test]
    fn restore_folds_into_new_shares() {
        let r = ReliabilityRecorder::new();
        r.record("", 10, 256.0, &[0; 80]);
        let (m, h, b) = r.take_closed(u64::MAX).remove(0);
        r.record("", 20, 256.0, &[1; 80]);
        r.restore(m, h, b);
        assert_eq!(r.peek("", 0).unwrap().shares, 2);
    }
}
