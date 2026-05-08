//! On-disk operator state for the manifest publisher. ARCHITECTURE.md §4.7.1.
//!
//! Holds the operator's nostr profile, instance URL, oracle name, relay set,
//! and the append-only history of mint keyset IDs (with retire timestamps).
//! Kind 0 events are replaceable, so this file is the source of truth for
//! historical keyset attribution — without it, a wallet holding a `THH`
//! token issued under a since-rotated keyset would lose its proof of
//! attestation on every rotation.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("attempted to reactivate already-retired keyset {id}")]
    ReactivatingRetiredKeyset { id: String },
    #[error("unsupported state_version: {got} (expected <= {supported})")]
    UnsupportedVersion { got: u32, supported: u32 },
}

/// Current on-disk schema version.
pub const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nip05: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lud16: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeysetStatus {
    Active,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysetEntry {
    pub id: String,
    pub status: KeysetStatus,
    pub recorded_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Initial,
    NoOp,
    Rotated { /* prior active id */ },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestState {
    pub state_version: u32,
    pub instance_url: String,
    #[serde(default)]
    pub profile: Profile,
    pub hashprice_oracle: String,
    /// Base URL the oracle adapter polls at runtime. None falls back to the
    /// adapter's compiled-in default (e.g. blockstream.info for esplora).
    /// Captured here so operators see and edit it, and the daemon doesn't
    /// need a separate config file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashprice_oracle_url: Option<String>,
    pub relays: Vec<String>,
    #[serde(default)]
    pub keysets: Vec<KeysetEntry>,
    /// Unix seconds of the last successful publish; used to keep `created_at`
    /// monotonic under clock skew. None until the first publish.
    #[serde(default)]
    pub last_published_at: Option<u64>,
}

impl ManifestState {
    pub fn new(instance_url: String, hashprice_oracle: String, relays: Vec<String>) -> Self {
        Self {
            state_version: STATE_VERSION,
            instance_url,
            profile: Profile::default(),
            hashprice_oracle,
            hashprice_oracle_url: None,
            relays,
            keysets: Vec::new(),
            last_published_at: None,
        }
    }

    /// Currently-active keyset, if any.
    pub fn active(&self) -> Option<&KeysetEntry> {
        self.keysets
            .iter()
            .find(|k| k.status == KeysetStatus::Active)
    }

    /// Apply an observed current keyset id, mutating self. Returns the
    /// transition that was performed.
    ///
    /// - First-ever observation: append as Active → `Initial`
    /// - Observed equals current Active: nothing changes → `NoOp`
    /// - Observed differs from current Active and is not in the retired set:
    ///   mark Active as Retired, append new Active → `Rotated`
    /// - Observed matches an existing Retired entry: error
    pub fn apply_observed_keyset(
        &mut self,
        observed_id: &str,
        now: u64,
    ) -> Result<Transition, StateError> {
        // Reject reactivating a retired keyset.
        if self
            .keysets
            .iter()
            .any(|k| k.status == KeysetStatus::Retired && k.id == observed_id)
        {
            return Err(StateError::ReactivatingRetiredKeyset {
                id: observed_id.to_string(),
            });
        }

        let active_idx = self
            .keysets
            .iter()
            .position(|k| k.status == KeysetStatus::Active);

        match active_idx {
            None => {
                self.keysets.push(KeysetEntry {
                    id: observed_id.to_string(),
                    status: KeysetStatus::Active,
                    recorded_at: now,
                    retired_at: None,
                });
                Ok(Transition::Initial)
            }
            Some(idx) if self.keysets[idx].id == observed_id => Ok(Transition::NoOp),
            Some(idx) => {
                self.keysets[idx].status = KeysetStatus::Retired;
                self.keysets[idx].retired_at = Some(now);
                self.keysets.push(KeysetEntry {
                    id: observed_id.to_string(),
                    status: KeysetStatus::Active,
                    recorded_at: now,
                    retired_at: None,
                });
                Ok(Transition::Rotated {})
            }
        }
    }

    pub fn load_from_path(p: &Path) -> Result<Self, StateError> {
        let mut f = File::open(p)?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;
        let parsed: ManifestState = serde_json::from_str(&buf)?;
        if parsed.state_version > STATE_VERSION {
            return Err(StateError::UnsupportedVersion {
                got: parsed.state_version,
                supported: STATE_VERSION,
            });
        }
        Ok(parsed)
    }

    /// Atomic save: write to `<path>.tmp`, fsync, rename over `path`.
    /// Mid-write crash leaves the original file untouched.
    pub fn save_atomic_to_path(&self, p: &Path) -> Result<(), StateError> {
        let tmp = tmp_path(p);
        {
            let mut f = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&tmp)?;
            let body = serde_json::to_vec_pretty(self)?;
            f.write_all(&body)?;
            f.write_all(b"\n")?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, p)?;
        Ok(())
    }
}

fn tmp_path(p: &Path) -> PathBuf {
    let mut tmp = p.to_path_buf();
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "manifest_state.json".to_string());
    tmp.set_file_name(format!(".{name}.tmp"));
    tmp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> ManifestState {
        ManifestState::new(
            "https://mint.example.com".to_string(),
            "esplora".to_string(),
            vec!["wss://relay.damus.io".to_string()],
        )
    }

    #[test]
    fn apply_initial_pushes_active_entry() {
        let mut s = fresh();
        let t = s.apply_observed_keyset("00aa", 1_000).unwrap();
        assert_eq!(t, Transition::Initial);
        assert_eq!(s.keysets.len(), 1);
        assert_eq!(s.keysets[0].id, "00aa");
        assert_eq!(s.keysets[0].status, KeysetStatus::Active);
        assert_eq!(s.keysets[0].recorded_at, 1_000);
        assert!(s.keysets[0].retired_at.is_none());
    }

    #[test]
    fn apply_same_keyset_is_noop() {
        let mut s = fresh();
        s.apply_observed_keyset("00aa", 1_000).unwrap();
        let t = s.apply_observed_keyset("00aa", 2_000).unwrap();
        assert_eq!(t, Transition::NoOp);
        assert_eq!(s.keysets.len(), 1);
        assert_eq!(s.keysets[0].recorded_at, 1_000); // unchanged
    }

    #[test]
    fn apply_rotation_marks_old_retired() {
        let mut s = fresh();
        s.apply_observed_keyset("00aa", 1_000).unwrap();
        let t = s.apply_observed_keyset("00bb", 2_000).unwrap();
        assert_eq!(t, Transition::Rotated {});
        assert_eq!(s.keysets.len(), 2);
        assert_eq!(s.keysets[0].id, "00aa");
        assert_eq!(s.keysets[0].status, KeysetStatus::Retired);
        assert_eq!(s.keysets[0].retired_at, Some(2_000));
        assert_eq!(s.keysets[1].id, "00bb");
        assert_eq!(s.keysets[1].status, KeysetStatus::Active);
    }

    #[test]
    fn apply_reactivating_retired_keyset_is_rejected() {
        let mut s = fresh();
        s.apply_observed_keyset("00aa", 1_000).unwrap();
        s.apply_observed_keyset("00bb", 2_000).unwrap();
        let err = s.apply_observed_keyset("00aa", 3_000).unwrap_err();
        assert!(matches!(err, StateError::ReactivatingRetiredKeyset { .. }));
    }

    #[test]
    fn active_returns_current_active() {
        let mut s = fresh();
        assert!(s.active().is_none());
        s.apply_observed_keyset("00aa", 1_000).unwrap();
        assert_eq!(s.active().unwrap().id, "00aa");
        s.apply_observed_keyset("00bb", 2_000).unwrap();
        assert_eq!(s.active().unwrap().id, "00bb");
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = fresh();
        s.profile.name = Some("Alice".to_string());
        s.apply_observed_keyset("00aa", 1_000).unwrap();
        s.save_atomic_to_path(&path).unwrap();

        let loaded = ManifestState::load_from_path(&path).unwrap();
        assert_eq!(loaded, s);
    }

    #[test]
    fn rejects_future_state_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let body = serde_json::json!({
            "state_version": 999,
            "instance_url": "https://x",
            "hashprice_oracle": "esplora",
            "relays": [],
            "keysets": []
        });
        std::fs::write(&path, body.to_string()).unwrap();
        let err = ManifestState::load_from_path(&path).unwrap_err();
        assert!(matches!(err, StateError::UnsupportedVersion { .. }));
    }

    #[test]
    fn atomic_save_uses_dotted_tmp_path() {
        let p = Path::new("/foo/bar/baz.json");
        assert_eq!(tmp_path(p), Path::new("/foo/bar/.baz.json.tmp"));
    }
}
