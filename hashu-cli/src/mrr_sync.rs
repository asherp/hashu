//! MiningRigRentals capacity sync, with credentials the operator can change
//! while the mint runs.
//!
//! The account's active rentals become capacity (see
//! `hashu_mint::capacity`). Credentials come from `MRR_API_KEY` /
//! `MRR_API_SECRET` / `HASHU_MRR_RENTAL_IDS` at start, or from the operator
//! GUI, which saves them to `mrr.json` in the data dir (mode 0600). Like the
//! other GUI settings a saved choice wins over the env, including a saved
//! "disconnected": delete `mrr.json` to go back to the env.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hashu_mint::capacity::{now_ms, CapacityBook, Rental};
use hashu_mint::oracle::mrr::{MrrAccount, DEFAULT_ALGO};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;

/// An MRR API key pair and which rentals to count.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct MrrCredentials {
    pub key: String,
    pub secret: String,
    /// Only these rental ids; empty counts every active rental.
    #[serde(default)]
    pub rental_ids: Vec<String>,
}

impl std::fmt::Debug for MrrCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MrrCredentials")
            .field("key", &key_hint(&self.key))
            .field("rental_ids", &self.rental_ids)
            .finish_non_exhaustive()
    }
}

impl MrrCredentials {
    /// Trimmed, or `None` when key or secret is blank.
    pub fn new(key: &str, secret: &str, rental_ids: &[String]) -> Option<Self> {
        let (key, secret) = (key.trim(), secret.trim());
        if key.is_empty() || secret.is_empty() {
            return None;
        }
        Some(Self {
            key: key.to_string(),
            secret: secret.to_string(),
            rental_ids: rental_ids
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }
}

/// What `mrr.json` holds: the operator's choice, `None` = disconnected.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    credentials: Option<MrrCredentials>,
}

/// Where the running credentials came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Env,
    Gui,
}

/// What the GUI shows. Never the secret, and only the key's last 4.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MrrStatus {
    pub connected: bool,
    pub source: Option<Source>,
    pub key_hint: Option<String>,
    pub rental_ids: Vec<String>,
}

struct Running {
    creds: Option<MrrCredentials>,
    source: Option<Source>,
    task: Option<JoinHandle<()>>,
}

pub struct MrrSync {
    capacity: Arc<CapacityBook>,
    base: String,
    every: Duration,
    path: Option<PathBuf>,
    running: Mutex<Running>,
}

impl std::fmt::Debug for MrrSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MrrSync")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

fn key_hint(key: &str) -> String {
    let tail: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

impl MrrSync {
    /// Start syncing with the saved credentials if `path` has any choice
    /// saved, else with `env`. Needs a Tokio runtime when there are some.
    pub fn start(
        capacity: Arc<CapacityBook>,
        base: String,
        every: Duration,
        path: Option<PathBuf>,
        env: Option<MrrCredentials>,
    ) -> anyhow::Result<Arc<Self>> {
        let saved = match &path {
            Some(p) => match std::fs::read(p) {
                Ok(b) => Some(
                    serde_json::from_slice::<Saved>(&b)
                        .map_err(|e| anyhow::anyhow!("parse {}: {e}", p.display()))?,
                ),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(anyhow::anyhow!("read {}: {e}", p.display())),
            },
            None => None,
        };
        let (creds, source) = match saved {
            Some(s) => {
                if env.is_some() {
                    tracing::info!(
                        "MRR credentials saved in the operator GUI override MRR_API_KEY"
                    );
                }
                (s.credentials, Some(Source::Gui))
            }
            None => (env, Some(Source::Env)),
        };
        let sync = Arc::new(Self {
            capacity,
            base,
            every: every.max(Duration::from_secs(15)),
            path,
            running: Mutex::new(Running {
                creds: None,
                source: None,
                task: None,
            }),
        });
        sync.run(creds, source);
        Ok(sync)
    }

    /// Replace the running credentials (`None` disconnects) and save the
    /// choice so it survives a restart.
    pub fn set(&self, creds: Option<MrrCredentials>) -> anyhow::Result<()> {
        self.save(&Saved {
            credentials: creds.clone(),
        })?;
        self.run(creds, Some(Source::Gui));
        Ok(())
    }

    pub fn status(&self) -> MrrStatus {
        let r = self.running.lock().unwrap();
        MrrStatus {
            connected: r.creds.is_some(),
            source: r.creds.as_ref().and(r.source),
            key_hint: r.creds.as_ref().map(|c| key_hint(&c.key)),
            rental_ids: r
                .creds
                .as_ref()
                .map(|c| c.rental_ids.clone())
                .unwrap_or_default(),
        }
    }

    /// The running credentials, secret included (server side only).
    pub fn current(&self) -> Option<MrrCredentials> {
        self.running.lock().unwrap().creds.clone()
    }

    /// A client for `creds`, to check them before saving.
    pub fn account(&self, creds: &MrrCredentials) -> MrrAccount {
        MrrAccount::with_base(
            self.base.clone(),
            DEFAULT_ALGO.into(),
            creds.key.clone(),
            creds.secret.clone(),
        )
    }

    fn save(&self, s: &Saved) -> anyhow::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(s)?;
        {
            use std::io::Write;
            let mut o = std::fs::OpenOptions::new();
            o.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                o.mode(0o600);
            }
            let mut f = o
                .open(&tmp)
                .map_err(|e| anyhow::anyhow!("write {}: {e}", tmp.display()))?;
            f.write_all(&body)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
        Ok(())
    }

    fn run(&self, creds: Option<MrrCredentials>, source: Option<Source>) {
        let mut r = self.running.lock().unwrap();
        if let Some(t) = r.task.take() {
            t.abort();
        }
        // A different account (or none): its old rentals no longer count.
        let changed = r.creds.is_some() && r.creds != creds;
        if changed || creds.is_none() {
            self.capacity.reset_mrr(creds.is_some());
        }
        r.source = source;
        r.creds = creds.clone();
        let Some(creds) = creds else {
            tracing::info!("MRR capacity sync off");
            return;
        };
        self.capacity.enable_mrr();
        tracing::info!(key = %key_hint(&creds.key), rental_ids = ?creds.rental_ids, "capacity from MRR active rentals");
        let account = self.account(&creds);
        let capacity = self.capacity.clone();
        let every = self.every;
        let ids = creds.rental_ids;
        r.task = Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            loop {
                tick.tick().await;
                let result = account
                    .active_rentals()
                    .await
                    .map(|list| {
                        list.into_iter()
                            .filter(|r| ids.is_empty() || ids.contains(&r.id))
                            .map(|r| Rental {
                                id: r.id,
                                label: r.rig_name,
                                ths: r.ths,
                                end_ms: (r.end_unix * 1000.0).max(0.0) as u64,
                            })
                            .collect::<Vec<_>>()
                    })
                    .map_err(|e| e.to_string());
                match &result {
                    Ok(r) => tracing::debug!(rentals = r.len(), "mrr rentals synced"),
                    Err(e) => tracing::warn!(error = %e, "mrr rental sync failed"),
                }
                capacity.set_mrr(result, now_ms());
            }
        }));
    }
}

impl Drop for MrrSync {
    fn drop(&mut self) {
        if let Some(t) = self.running.get_mut().unwrap().task.take() {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(key: &str) -> MrrCredentials {
        MrrCredentials::new(key, "s3cret", &[" 12 ".into(), "".into()]).unwrap()
    }

    // An unroutable base so the sync task never reaches a real API.
    const BASE: &str = "http://127.0.0.1:9";

    #[tokio::test]
    async fn env_then_gui_override_then_disconnect_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mrr.json");
        let cap = Arc::new(CapacityBook::new());
        let every = Duration::from_secs(3600);
        let s = MrrSync::start(
            cap.clone(),
            BASE.into(),
            every,
            Some(path.clone()),
            Some(creds("env-key-AAAA")),
        )
        .unwrap();
        let st = s.status();
        assert!(st.connected);
        assert_eq!(st.source, Some(Source::Env));
        assert_eq!(st.key_hint.as_deref(), Some("…AAAA"));
        assert_eq!(st.rental_ids, vec!["12".to_string()]);
        assert!(cap.snapshot(0).mrr_enabled);

        s.set(Some(creds("gui-key-BBBB"))).unwrap();
        assert_eq!(s.status().source, Some(Source::Gui));
        assert_eq!(s.status().key_hint.as_deref(), Some("…BBBB"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The secret is saved but never in the status.
        assert!(!serde_json::to_string(&s.status())
            .unwrap()
            .contains("s3cret"));
        drop(s);

        // Restart: the saved key wins over the env.
        let s = MrrSync::start(
            cap.clone(),
            BASE.into(),
            every,
            Some(path.clone()),
            Some(creds("env-key-AAAA")),
        )
        .unwrap();
        assert_eq!(s.status().key_hint.as_deref(), Some("…BBBB"));

        // Disconnect: stays off across a restart even with env set.
        s.set(None).unwrap();
        assert!(!s.status().connected);
        assert!(!cap.snapshot(0).mrr_enabled);
        drop(s);
        let s = MrrSync::start(
            cap,
            BASE.into(),
            every,
            Some(path),
            Some(creds("env-key-AAAA")),
        )
        .unwrap();
        assert!(!s.status().connected);
    }

    #[test]
    fn blank_credentials_are_none() {
        assert!(MrrCredentials::new(" ", "x", &[]).is_none());
        assert!(MrrCredentials::new("k", "", &[]).is_none());
        assert!(!format!("{:?}", creds("abcdWXYZ")).contains("s3cret"));
    }
}
