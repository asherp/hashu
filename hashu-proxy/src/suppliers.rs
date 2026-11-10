//! Registered suppliers: miners with their own mint (`<name>.mint.<domain>`)
//! whose rigs fill only that mint's orders.
//!
//! A rig proves it belongs to supplier `alice` by authorizing as
//! `alice.<rig>` with `pw=<secret>` in its password (the option is stripped
//! before any pool sees the password). The registry stores only the
//! SHA-256 of each secret.
//!
//! The registry is a JSON file (`HASHU_SUPPLIERS_FILE`), re-read when its
//! modification time changes, so sign-up can add suppliers without a
//! restart:
//!
//! ```json
//! { "suppliers": { "alice": { "secret_sha256": "<64 hex chars>" } } }
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Whether `name` is a valid supplier name: a DNS label of 3 to 32
/// lowercase letters, digits and inner hyphens.
pub fn valid_name(name: &str) -> bool {
    (3..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Hex SHA-256 of a worker secret, as stored in the registry.
pub fn secret_hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

#[derive(Debug, Deserialize)]
struct FileFormat {
    #[serde(default)]
    suppliers: HashMap<String, Entry>,
}

#[derive(Debug, Clone, Deserialize)]
struct Entry {
    secret_sha256: String,
}

/// What [`SupplierRegistry::check`] says about a rig.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check {
    /// `name` is registered and the secret matches.
    Verified(String),
    /// `name` is registered but the secret is missing or wrong.
    WrongSecret(String),
    /// No registered supplier by that name.
    Unregistered,
}

/// The registered suppliers. Empty when no file is configured.
#[derive(Debug, Default)]
pub struct SupplierRegistry {
    path: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    loaded_mtime: Option<SystemTime>,
    entries: HashMap<String, Entry>,
}

impl SupplierRegistry {
    /// No suppliers.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Suppliers from `path`, re-read when it changes. A missing file is
    /// an empty registry until it appears.
    pub fn from_file(path: impl AsRef<Path>) -> Self {
        let r = Self {
            path: Some(path.as_ref().to_path_buf()),
            state: Mutex::default(),
        };
        r.refresh();
        r
    }

    /// Suppliers from `(name, secret)` pairs, for tests.
    pub fn from_secrets<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let entries = pairs
            .into_iter()
            .map(|(n, s)| {
                (
                    n.to_string(),
                    Entry {
                        secret_sha256: secret_hash(s),
                    },
                )
            })
            .collect();
        Self {
            path: None,
            state: Mutex::new(State {
                loaded_mtime: None,
                entries,
            }),
        }
    }

    /// Whether a registry file is configured (it may not exist yet).
    pub fn path_configured(&self) -> bool {
        self.path.is_some() || !self.lock().entries.is_empty()
    }

    /// Whether `name` is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.refresh();
        self.lock().entries.contains_key(name)
    }

    /// Check rig `user` (`<name>.<rig>`) with `secret` from its `pw=`.
    pub fn check(&self, user: &str, secret: Option<&str>) -> Check {
        self.refresh();
        let name = user.trim().split('.').next().unwrap_or_default();
        let st = self.lock();
        let Some(e) = st.entries.get(name) else {
            return Check::Unregistered;
        };
        match secret {
            Some(s) if constant_time_eq(&secret_hash(s), &e.secret_sha256.to_ascii_lowercase()) => {
                Check::Verified(name.to_string())
            }
            _ => Check::WrongSecret(name.to_string()),
        }
    }

    fn refresh(&self) {
        let Some(path) = &self.path else { return };
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let mut st = self.lock();
        if mtime.is_some() && mtime == st.loaded_mtime {
            return;
        }
        let Some(mtime) = mtime else {
            st.entries.clear();
            st.loaded_mtime = None;
            return;
        };
        match std::fs::read(path)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice::<FileFormat>(&b).map_err(|e| e.to_string()))
        {
            Ok(f) => {
                let (ok, bad): (HashMap<_, _>, HashMap<_, _>) =
                    f.suppliers.into_iter().partition(|(n, _)| valid_name(n));
                for n in bad.keys() {
                    tracing::warn!(name = %n, "ignoring supplier with an invalid name");
                }
                tracing::info!(?path, suppliers = ok.len(), "loaded supplier registry");
                st.entries = ok;
                st.loaded_mtime = Some(mtime);
            }
            // Keep the last good copy; retry on the next change.
            Err(e) => {
                tracing::warn!(?path, error = %e, "supplier registry unreadable; keeping the last copy");
                st.loaded_mtime = Some(mtime);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_dns_labels() {
        for ok in ["alice", "bob-2", "a1b"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in [
            "al",
            "Alice",
            "-bob",
            "bob-",
            "a.b",
            "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn checks_secret_by_name_prefix() {
        let r = SupplierRegistry::from_secrets([("alice", "s3cret")]);
        assert_eq!(
            r.check("alice.rig1", Some("s3cret")),
            Check::Verified("alice".into())
        );
        assert_eq!(
            r.check("alice", Some("s3cret")),
            Check::Verified("alice".into())
        );
        assert_eq!(
            r.check("alice.rig1", Some("nope")),
            Check::WrongSecret("alice".into())
        );
        assert_eq!(
            r.check("alice.rig1", None),
            Check::WrongSecret("alice".into())
        );
        assert_eq!(r.check("bob.rig1", Some("s3cret")), Check::Unregistered);
        assert_eq!(r.check("alicex.rig1", Some("s3cret")), Check::Unregistered);
    }

    #[test]
    fn file_is_reread_when_it_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suppliers.json");
        let r = SupplierRegistry::from_file(&path);
        assert!(!r.contains("alice"));
        let body = format!(
            r#"{{"suppliers":{{"alice":{{"secret_sha256":"{}"}},"Bad Name":{{"secret_sha256":"00"}}}}}}"#,
            secret_hash("s3cret")
        );
        std::fs::write(&path, body).unwrap();
        assert!(r.contains("alice"));
        assert!(!r.contains("Bad Name"));
        // A broken rewrite keeps the last good copy.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, "{not json").unwrap();
        let t = SystemTime::now() + std::time::Duration::from_secs(5);
        let f = std::fs::File::options().write(true).open(&path).unwrap();
        f.set_modified(t).unwrap();
        assert!(r.contains("alice"));
    }
}
