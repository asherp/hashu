//! Registered suppliers: miners with their own mint (`<name>.<domain>`)
//! whose rigs fill only that mint's orders.
//!
//! A rig proves it belongs to supplier `alice` by authorizing as
//! `alice.<rig>` with `pw=<secret>` in its password (the option is stripped
//! before any pool sees the password). The registry stores only the
//! SHA-256 of each secret.
//!
//! Stratum V1 is plaintext, so anyone on the path can read a rig's `pw=`.
//! That secret therefore only proves a rig's name. The supplier's
//! dashboard (ask, horizon, Lightning address) takes a separate key whose
//! hash is `key_sha256` (see [`SupplierRegistry::check_key`]). An entry
//! without one, from before the split, still accepts the rig secret there.
//!
//! The registry is a JSON file (`HASHU_SUPPLIERS_FILE`), re-read when its
//! modification time changes, so sign-up can add suppliers without a
//! restart:
//!
//! ```json
//! { "suppliers": { "alice": { "secret_sha256": "<64 hex chars>", "key_sha256": "<64 hex chars>" } } }
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

/// Host names the operator needs for itself next to supplier mints
/// (`<name>.<domain>`), or that would let a supplier pass for the
/// operator. Sign-up refuses them.
#[rustfmt::skip]
const RESERVED: &[&str] = &[
    // in use or planned on the operator's domain
    "app", "www", "mint", "mints", "api", "docs", "doc", "status", "pool", "pools", "stratum",
    "sv2", "join", "waitlist", "checkout", "redeem", "compare", "slides", "reliability",
    "control", "mdk", "blog", "news", "dev", "test", "staging", "demo", "beta", "cdn", "static",
    "assets", "img", "media", "files", "download", "downloads", "relay", "nostr", "lnurl", "ln",
    "lightning", "pay", "payments", "invoice", "shop", "store", "market", "explorer", "wiki",
    "forum", "community", "chat", "git", "github",
    // accounts, money and the operator
    "admin", "administrator", "operator", "root", "system", "sys", "owner", "staff", "team",
    "official", "support", "help", "helpdesk", "security", "abuse", "legal", "privacy", "terms",
    "tos", "billing", "account", "accounts", "login", "signin", "signup", "register", "auth",
    "oauth", "sso", "id", "identity", "verify", "wallet", "wallets", "vault", "bank",
    "exchange", "swap", "faucet",
    // mail and DNS
    "mail", "email", "smtp", "imap", "pop", "pop3", "mx", "webmail", "postmaster", "hostmaster",
    "webmaster", "ns", "ns1", "ns2", "dns", "ftp", "ssh", "vpn", "localhost", "local",
    "internal", "intranet", "proxy", "gateway", "autodiscover", "autoconfig", "wpad", "mta-sts",
    "_dmarc",
];

/// Words that make a name look like the operator's when they are any
/// hyphen-separated part of it (`app-hashu`, `hashu-support`, `my-wallet`).
#[rustfmt::skip]
const IMPERSONATING: &[&str] = &[
    "hashu", "admin", "operator", "official", "support", "help", "security", "login", "signin",
    "verify", "wallet", "app", "mint", "account", "billing", "staff",
];

/// Whether `name` is kept from suppliers: a [`RESERVED`] host, anything
/// containing "hashu", or a name with an [`IMPERSONATING`] part.
pub fn reserved_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    RESERVED.contains(&name.as_str())
        || name.contains("hashu")
        || name.split('-').any(|part| IMPERSONATING.contains(&part))
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
    /// Rig secret (`pw=`): proves a rig belongs to the supplier.
    secret_sha256: String,
    /// Dashboard key: changes the supplier's settings. `None` for an entry
    /// from before rig and dashboard secrets were split.
    #[serde(default)]
    key_sha256: Option<String>,
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

/// Why [`SupplierRegistry::register`] refused a name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    #[error("sign-up is closed on this mint (no supplier registry file)")]
    Closed,
    #[error("names are 3 to 32 lowercase letters, digits and inner hyphens")]
    InvalidName,
    #[error("that name is taken")]
    Taken,
    #[error("that name is reserved; pick another")]
    Reserved,
    #[error("could not save the registry: {0}")]
    Io(String),
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
                        key_sha256: None,
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

    /// Registered names, sorted.
    pub fn names(&self) -> Vec<String> {
        self.refresh();
        let mut v: Vec<String> = self.lock().entries.keys().cloned().collect();
        v.sort();
        v
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

    /// Whether `key` opens supplier `name`'s dashboard: it matches the
    /// entry's dashboard key, or, for an entry without one, its rig secret.
    pub fn check_key(&self, name: &str, key: Option<&str>) -> bool {
        self.refresh();
        let st = self.lock();
        let (Some(e), Some(key)) = (st.entries.get(name), key) else {
            return false;
        };
        let want = e.key_sha256.as_ref().unwrap_or(&e.secret_sha256);
        constant_time_eq(&secret_hash(key), &want.to_ascii_lowercase())
    }

    /// Add supplier `name` to the registry file with the SHA-256 (hex) of
    /// its rig secret and, if given, of its dashboard key. Fails when no
    /// file is configured, the name is invalid or already taken. Other
    /// content in the file is kept.
    pub fn register(
        &self,
        name: &str,
        secret_sha256: &str,
        key_sha256: Option<&str>,
    ) -> Result<(), RegisterError> {
        let Some(path) = &self.path else {
            return Err(RegisterError::Closed);
        };
        if !valid_name(name) {
            return Err(RegisterError::InvalidName);
        }
        if reserved_name(name) {
            return Err(RegisterError::Reserved);
        }
        self.refresh();
        let mut st = self.lock();
        if st.entries.contains_key(name) {
            return Err(RegisterError::Taken);
        }
        let io = |e: std::io::Error| RegisterError::Io(e.to_string());
        let mut doc: serde_json::Value = match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| RegisterError::Io(e.to_string()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(e) => return Err(io(e)),
        };
        let obj = doc
            .as_object_mut()
            .ok_or_else(|| RegisterError::Io("registry file is not a JSON object".into()))?;
        let suppliers = obj
            .entry("suppliers")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| RegisterError::Io("\"suppliers\" is not a JSON object".into()))?;
        let mut entry = serde_json::json!({ "secret_sha256": secret_sha256 });
        if let Some(k) = key_sha256 {
            entry["key_sha256"] = serde_json::json!(k);
        }
        suppliers.insert(name.to_string(), entry);
        let tmp = path.with_extension("tmp");
        let body = serde_json::to_vec_pretty(&doc).map_err(|e| RegisterError::Io(e.to_string()))?;
        std::fs::write(&tmp, body).map_err(io)?;
        std::fs::rename(&tmp, path).map_err(io)?;
        st.entries.insert(
            name.to_string(),
            Entry {
                secret_sha256: secret_sha256.to_string(),
                key_sha256: key_sha256.map(str::to_string),
            },
        );
        st.loaded_mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        Ok(())
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

pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
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
    fn reserved_names_are_refused() {
        for no in [
            "app",
            "www",
            "mint",
            "api",
            "docs",
            "status",
            "pool",
            "wallet",
            "join",
            "admin",
            "operator",
            "mail",
            "app-hashu",
            "hashu-support",
            "myhashu",
            "my-wallet",
            "official-pool",
            "Admin",
        ] {
            assert!(reserved_name(no), "{no}");
        }
        for ok in ["alice", "bob-2", "apple", "mintaka", "poolside"] {
            assert!(!reserved_name(ok), "{ok}");
        }
        let dir = tempfile::tempdir().unwrap();
        let r = SupplierRegistry::from_file(dir.path().join("suppliers.json"));
        assert_eq!(r.register("app", "00", None), Err(RegisterError::Reserved));
        assert_eq!(
            r.register("app-hashu", "00", None),
            Err(RegisterError::Reserved)
        );
        assert!(r.register("alice", "00", None).is_ok());
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
    fn entry_without_a_dashboard_key_accepts_the_rig_secret() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suppliers.json");
        let doc = format!(
            r#"{{"suppliers":{{"alice":{{"secret_sha256":"{}"}}}}}}"#,
            secret_hash("s3cret")
        );
        std::fs::write(&path, doc).unwrap();
        let r = SupplierRegistry::from_file(&path);
        assert!(r.check_key("alice", Some("s3cret")));
        assert!(!r.check_key("alice", Some("nope")));
    }

    #[test]
    fn register_adds_a_name_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("suppliers.json");
        std::fs::write(&path, r#"{"note":"kept","suppliers":{}}"#).unwrap();
        let r = SupplierRegistry::from_file(&path);
        r.register("alice", &secret_hash("s3cret"), Some(&secret_hash("k3y")))
            .unwrap();
        assert_eq!(
            r.check("alice.rig1", Some("s3cret")),
            Check::Verified("alice".into())
        );
        // The dashboard key opens settings; the rig secret, sent in clear
        // over Stratum, does not.
        assert!(r.check_key("alice", Some("k3y")));
        assert!(!r.check_key("alice", Some("s3cret")));
        assert!(!r.check_key("alice", None));
        assert!(!r.check_key("bob", Some("k3y")));
        // Re-read from disk: the key hash was saved.
        let again = SupplierRegistry::from_file(&path);
        assert!(again.check_key("alice", Some("k3y")));
        assert!(!again.check_key("alice", Some("s3cret")));
        assert_eq!(r.register("alice", "00", None), Err(RegisterError::Taken));
        assert_eq!(
            r.register("Bad", "00", None),
            Err(RegisterError::InvalidName)
        );
        // On disk, other fields kept; a fresh reader sees alice.
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["note"], "kept");
        assert!(SupplierRegistry::from_file(&path).contains("alice"));
        assert_eq!(
            SupplierRegistry::empty().register("bob", "00", None),
            Err(RegisterError::Closed)
        );
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
