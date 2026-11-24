//! Operator dashboard telemetry in the ops database, so a restart (every
//! deploy) does not wipe the live feed.
//!
//! The hashrate meter, the proxy counters and per-rig work live in memory.
//! With `--ops-database-url` (`HASHU_OPS_DATABASE_URL`, a Postgres URL) they
//! are written to the ops database every [`FLUSH_EVERY`] and on shutdown,
//! and read back on start:
//!
//! - `hashu_samples`: the meter's 1 s samples (time, total work, hashrate,
//!   per-rig work). The last hour is loaded back into the chart. Older rows
//!   are thinned to one a minute and dropped after [`KEEP_DAYS`] days.
//! - `hashu_tracks`: each redemption's delivered-work curve with its real
//!   opened and completed times (the ledger keeps no timestamps).
//! - `hashu_state`: proxy counters and total work, and each rig's work
//!   with its last share time. Restored so totals carry on. Also when the
//!   daemon was last up (`alive`) and since when rig sessions are recorded
//!   (`sessions_since`).
//! - `hashu_rig_sessions`: when each rig connected and disconnected (see
//!   `hashu_proxy::sessions`), so the chart tells downtime from a low
//!   hashrate. Sessions still open at shutdown are saved open and carried
//!   on if the rig's relay resumes them. Dropped [`KEEP_DAYS`] after they
//!   end.
//!
//! Without a URL nothing is saved and the feed starts empty, as before.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use hashu_proxy::meter::{HashrateMeter, RedemptionTrack, Sample};
use hashu_proxy::router::Router;
use hashu_proxy::sessions::{Session, KEEP_CLOSED};
use hashu_proxy::{ProxyMetrics, ProxyMetricsSnapshot};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls};

/// How often the feed is written.
pub const FLUSH_EVERY: Duration = Duration::from_secs(10);
/// Samples older than this are thinned to one per minute.
const FULL_RES_MS: u64 = 3_600_000;
/// Samples older than this many days are dropped.
pub const KEEP_DAYS: u64 = 30;
/// Prune once every this many flushes (10 minutes).
const PRUNE_EVERY: u64 = 60;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS hashu_samples (
    t_ms BIGINT PRIMARY KEY,
    total_hashes DOUBLE PRECISION NOT NULL,
    hashrate_hs DOUBLE PRECISION NOT NULL,
    rigs TEXT NOT NULL DEFAULT '{}'
);
CREATE TABLE IF NOT EXISTS hashu_tracks (
    id TEXT PRIMARY KEY,
    opened_ms BIGINT NOT NULL,
    completed_ms BIGINT,
    data TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS hashu_state (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS hashu_rig_sessions (
    rig TEXT NOT NULL,
    start_ms BIGINT NOT NULL,
    end_ms BIGINT,
    PRIMARY KEY (rig, start_ms)
);
";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SavedMetrics {
    counters: ProxyMetricsSnapshot,
    accepted_hashes: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct SavedRig {
    rig: String,
    hashes: f64,
    /// Unix ms of its last share.
    last_ms: u64,
}

#[derive(Default)]
struct Progress {
    /// Newest sample written.
    last_t_ms: u64,
    /// Completed tracks already written (they no longer change).
    done: HashSet<String>,
    /// Since when rig sessions are recorded (Unix ms), saved as is.
    sessions_since: u64,
    flushes: u64,
}

/// Time of the newest sample, if any.
fn p_last_sample(samples: &[Sample]) -> Option<u64> {
    samples.last().map(|s| s.t_ms)
}

/// The dashboard feed's link to the ops database.
pub struct Telemetry {
    url: String,
    client: Mutex<Option<Client>>,
    progress: Mutex<Progress>,
    meter: Arc<HashrateMeter>,
    metrics: Arc<ProxyMetrics>,
    router: Arc<Router>,
}

async fn connect(url: &str) -> anyhow::Result<Client> {
    let (client, conn) = tokio_postgres::connect(url, NoTls)
        .await
        .context("connect to the ops database")?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::warn!(error = %e, "ops database connection closed");
        }
    });
    client
        .batch_execute(SCHEMA)
        .await
        .context("create telemetry tables")?;
    Ok(client)
}

impl Telemetry {
    /// Connect (retrying for up to `wait`, the database may still be
    /// starting) and restore the saved feed into `meter`, `metrics` and
    /// `router`. Call before the meter's first sample and before the proxy
    /// takes miners. If the database cannot be reached the feed starts
    /// empty and saving is retried on each flush.
    pub async fn open(
        url: &str,
        meter: Arc<HashrateMeter>,
        metrics: Arc<ProxyMetrics>,
        router: Arc<Router>,
        wait: Duration,
    ) -> Arc<Self> {
        let t = Arc::new(Self {
            url: url.to_string(),
            client: Mutex::new(None),
            progress: Mutex::new(Progress::default()),
            meter,
            metrics,
            router,
        });
        let deadline = tokio::time::Instant::now() + wait;
        let client = loop {
            match connect(url).await {
                Ok(c) => break Some(c),
                Err(e) if tokio::time::Instant::now() >= deadline => {
                    tracing::warn!(error = %format!("{e:#}"), "ops database unreachable; dashboard feed starts empty");
                    break None;
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        };
        if let Some(c) = client {
            match t.restore(&c).await {
                Ok(n) => tracing::info!(
                    samples = n.0,
                    tracks = n.1,
                    rigs = n.2,
                    "dashboard feed restored"
                ),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "could not restore dashboard feed")
                }
            }
            *t.client.lock().await = Some(c);
        }
        t
    }

    async fn restore(&self, c: &Client) -> anyhow::Result<(usize, usize, usize)> {
        let now = now_ms();
        let since = now.saturating_sub(FULL_RES_MS) as i64;
        let rows = c
            .query(
                "SELECT t_ms, total_hashes, hashrate_hs, rigs FROM hashu_samples WHERE t_ms > $1 ORDER BY t_ms",
                &[&since],
            )
            .await?;
        let samples: Vec<Sample> = rows
            .iter()
            .map(|r| Sample {
                t_ms: r.get::<_, i64>(0) as u64,
                total_hashes: r.get(1),
                hashrate_hs: r.get(2),
                rigs: serde_json::from_str(r.get::<_, &str>(3)).unwrap_or_default(),
            })
            .collect();
        let rows = c.query("SELECT id, data FROM hashu_tracks", &[]).await?;
        let mut done = HashSet::new();
        let tracks: Vec<RedemptionTrack> = rows
            .iter()
            .filter_map(|r| {
                let t: RedemptionTrack = serde_json::from_str(r.get::<_, &str>(1)).ok()?;
                if t.completed_ms.is_some() {
                    done.insert(t.id.clone());
                }
                Some(t)
            })
            .collect();
        let state = |key: &'static str| async move {
            c.query_opt("SELECT value FROM hashu_state WHERE key = $1", &[&key])
                .await
                .map(|r| r.map(|r| r.get::<_, String>(0)))
        };
        if let Some(v) = state("metrics").await? {
            if let Ok(m) = serde_json::from_str::<SavedMetrics>(&v) {
                self.metrics.restore(&m.counters, m.accepted_hashes);
            }
        }
        let mut n_rigs = 0;
        if let Some(v) = state("rigs").await? {
            if let Ok(rigs) = serde_json::from_str::<Vec<SavedRig>>(&v) {
                n_rigs = rigs.len();
                self.router.restore_rig_work(
                    rigs.into_iter()
                        .map(|r| {
                            (
                                r.rig,
                                r.hashes,
                                Duration::from_millis(now.saturating_sub(r.last_ms)),
                            )
                        })
                        .collect(),
                );
            }
        }
        // Rig sessions: every open one, and those that ended recently
        // enough to matter to the chart.
        let from = now.saturating_sub(KEEP_CLOSED.as_millis() as u64) as i64;
        let rows = c
            .query(
                "SELECT rig, start_ms, end_ms FROM hashu_rig_sessions \
                 WHERE end_ms IS NULL OR end_ms >= $1 ORDER BY start_ms",
                &[&from],
            )
            .await?;
        let sessions: Vec<Session> = rows
            .iter()
            .map(|r| Session {
                rig: r.get(0),
                start_ms: r.get::<_, i64>(1) as u64,
                end_ms: r.get::<_, Option<i64>>(2).map(|v| v as u64),
            })
            .collect();
        let alive = state("alive").await?.and_then(|v| v.parse::<u64>().ok());
        let since = state("sessions_since")
            .await?
            .and_then(|v| v.parse::<u64>().ok());
        let (_, own_since) = self.router.rig_sessions(now);
        // With nothing saved yet, sessions are recorded from this start.
        let since = since.unwrap_or(own_since);
        self.progress.lock().await.sessions_since = since;
        let alive = alive.unwrap_or_else(|| p_last_sample(&samples).unwrap_or(now));
        self.router.restore_sessions(sessions, alive, since);

        let n = (samples.len(), tracks.len(), n_rigs);
        let mut p = self.progress.lock().await;
        p.last_t_ms = samples.last().map(|s| s.t_ms).unwrap_or(0);
        p.done = done;
        drop(p);
        self.meter.restore(samples, tracks);
        Ok(n)
    }

    /// Write what changed since the last flush. Reconnects if needed.
    pub async fn flush(&self) -> anyhow::Result<()> {
        let mut guard = self.client.lock().await;
        if guard.as_ref().map(|c| c.is_closed()).unwrap_or(true) {
            *guard = Some(connect(&self.url).await?);
        }
        let c = guard.as_mut().expect("connected above");
        let mut p = self.progress.lock().await;
        let samples = self.meter.samples_since(p.last_t_ms);
        let tracks: Vec<RedemptionTrack> = self
            .meter
            .tracks()
            .into_iter()
            .filter(|t| !p.done.contains(&t.id))
            .collect();
        let now = now_ms();
        let (sessions, since) = self.router.rig_sessions(0);
        if p.sessions_since == 0 {
            p.sessions_since = since;
        }
        let metrics = serde_json::to_string(&SavedMetrics {
            counters: self.metrics.snapshot(),
            accepted_hashes: self.metrics.accepted_hashes(),
        })?;
        let rigs = serde_json::to_string(
            &self
                .router
                .rig_work_saved()
                .into_iter()
                .map(|(rig, hashes, idle)| SavedRig {
                    rig,
                    hashes,
                    last_ms: now.saturating_sub(idle.as_millis() as u64),
                })
                .collect::<Vec<_>>(),
        )?;

        let tx = c.transaction().await?;
        let ins = tx
            .prepare(
                "INSERT INTO hashu_samples (t_ms, total_hashes, hashrate_hs, rigs) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (t_ms) DO NOTHING",
            )
            .await?;
        for s in &samples {
            let rigs = serde_json::to_string(&s.rigs)?;
            tx.execute(
                &ins,
                &[&(s.t_ms as i64), &s.total_hashes, &s.hashrate_hs, &rigs],
            )
            .await?;
        }
        let up = tx
            .prepare(
                "INSERT INTO hashu_tracks (id, opened_ms, completed_ms, data) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (id) DO UPDATE SET opened_ms = $2, completed_ms = $3, data = $4",
            )
            .await?;
        for t in &tracks {
            let data = serde_json::to_string(t)?;
            let completed = t.completed_ms.map(|m| m as i64);
            tx.execute(&up, &[&t.id, &(t.opened_ms as i64), &completed, &data])
                .await?;
        }
        let kv = tx
            .prepare(
                "INSERT INTO hashu_state (key, value) VALUES ($1, $2) \
                 ON CONFLICT (key) DO UPDATE SET value = $2",
            )
            .await?;
        tx.execute(&kv, &[&"metrics", &metrics]).await?;
        tx.execute(&kv, &[&"rigs", &rigs]).await?;
        tx.execute(&kv, &[&"alive", &now.to_string()]).await?;
        tx.execute(&kv, &[&"sessions_since", &p.sessions_since.to_string()])
            .await?;
        let ses = tx
            .prepare(
                "INSERT INTO hashu_rig_sessions (rig, start_ms, end_ms) VALUES ($1, $2, $3) \
                 ON CONFLICT (rig, start_ms) DO UPDATE SET end_ms = $3",
            )
            .await?;
        for s in &sessions {
            tx.execute(
                &ses,
                &[&s.rig, &(s.start_ms as i64), &s.end_ms.map(|e| e as i64)],
            )
            .await?;
        }
        if p.flushes % PRUNE_EVERY == 0 {
            let cut = now.saturating_sub(FULL_RES_MS) as i64;
            let floor = now.saturating_sub(KEEP_DAYS * 86_400_000) as i64;
            tx.execute("DELETE FROM hashu_samples WHERE t_ms < $1", &[&floor])
                .await?;
            tx.execute(
                "DELETE FROM hashu_rig_sessions WHERE end_ms IS NOT NULL AND end_ms < $1",
                &[&floor],
            )
            .await?;
            // One sample per minute past the first hour; only look back a
            // day so each prune stays small.
            tx.execute(
                "DELETE FROM hashu_samples WHERE t_ms < $1 AND t_ms > $2 AND t_ms NOT IN ( \
                   SELECT MIN(t_ms) FROM hashu_samples WHERE t_ms < $1 AND t_ms > $2 GROUP BY t_ms / 60000)",
                &[&cut, &(cut - 86_400_000)],
            )
            .await?;
        }
        tx.commit().await?;

        if let Some(s) = samples.last() {
            p.last_t_ms = s.t_ms;
        }
        for t in tracks {
            if t.completed_ms.is_some() {
                p.done.insert(t.id);
            }
        }
        p.flushes += 1;
        Ok(())
    }

    /// Flush every [`FLUSH_EVERY`] until the task is dropped.
    pub async fn run(self: Arc<Self>) {
        let mut t = tokio::time::interval(FLUSH_EVERY);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t.tick().await;
        let mut failing = false;
        loop {
            t.tick().await;
            match self.flush().await {
                Ok(()) if failing => {
                    failing = false;
                    tracing::info!("saving the dashboard feed again");
                }
                Ok(()) => {}
                Err(e) if !failing => {
                    failing = true;
                    tracing::warn!(error = %format!("{e:#}"), "could not save the dashboard feed");
                }
                Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::redemption::RedemptionLedger;
    use hashu_proxy::credit::ConnectionId;

    fn parts() -> (Arc<HashrateMeter>, Arc<ProxyMetrics>, Arc<Router>) {
        let metrics = Arc::new(ProxyMetrics::default());
        let ledger = Arc::new(RedemptionLedger::new());
        let meter = Arc::new(HashrateMeter::new(
            metrics.clone(),
            ledger.clone(),
            Duration::from_secs(60),
        ));
        let router = Arc::new(Router::new("127.0.0.1:1", ledger));
        (meter, metrics, router)
    }

    /// Needs a Postgres: `HASHU_TEST_OPS_DB=postgres://...`; skipped
    /// without one.
    #[tokio::test]
    async fn rig_sessions_survive_a_restart() {
        let Ok(url) = std::env::var("HASHU_TEST_OPS_DB") else {
            return;
        };
        {
            let c = connect(&url).await.unwrap();
            c.batch_execute("DELETE FROM hashu_rig_sessions; DELETE FROM hashu_state")
                .await
                .unwrap();
        }
        let (meter, metrics, router) = parts();
        let t = Telemetry::open(&url, meter, metrics, router.clone(), Duration::from_secs(5)).await;
        let conn = ConnectionId(1);
        router.assign_with(conn, Some(&serde_json::json!(["alice.rig1", "x"])));
        router.begin_shutdown();
        router.release(conn);
        t.flush().await.unwrap();
        let (before, since) = router.rig_sessions(0);
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].end_ms, None, "saved open across the restart");

        let (meter, metrics, router2) = parts();
        let _t2 = Telemetry::open(
            &url,
            meter,
            metrics,
            router2.clone(),
            Duration::from_secs(5),
        )
        .await;
        let (after, since2) = router2.rig_sessions(0);
        assert_eq!(after, before);
        assert_eq!(since2, since, "recording start carried over");
    }
}
