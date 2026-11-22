//! The mint reliability record, live (issue #109): every UTC hour the
//! daemon was up, each mint it serves (the operator's and every supplier's)
//! publishes one signed record of the work it delivered.
//!
//! - [`Reliability::run`] ticks every minute: it counts the minute toward
//!   the hour's uptime, follows redemptions in the ledger (opened,
//!   finished, stalled), saves the open share buckets, and publishes every
//!   closed hour not yet published.
//! - [`router`] serves the record:
//!   - `GET /v1/hashu/reliability`: index (pubkey, relays, totals, latest
//!     closed hours). The hour in progress is never shown: live progress
//!     would let anyone follow a running redemption.
//!   - `GET /v1/hashu/reliability/key`, `/key.ots`: key document and its proof
//!   - `GET /v1/hashu/reliability/{hour}`: the signed Nostr event
//!   - `GET /v1/hashu/reliability/{hour}.ots`: its OpenTimestamps proof
//!   - `GET /reliability/`: a page that checks it all in the browser
//!
//! An hour is published only if the daemon ran during it, so downtime
//! shows as a missing hour, which nobody can fill in later.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use hashu_core::redemption::{RedemptionLedger, RedemptionState};
use hashu_core::reliability::{hour_start, median_and_worst, HourRecord, RedemptionStats, HOUR_S};
use hashu_mint::discovery::{Announcement, ANNOUNCE_KIND};
use hashu_mint::reliability::{PublishConfig, RecordStore, RECORD_KIND, RECORD_TAG};
use hashu_proxy::reliability::{ReliabilityRecorder, OPERATOR_MINT};
use serde::{Deserialize, Serialize};
use serde_json::json;

const PAGE: &str = include_str!("reliability.html");
const TICK: Duration = Duration::from_secs(60);
/// How often pending timestamps are upgraded.
const MAINTAIN_EVERY_TICKS: u64 = 30;
/// Most hours the index lists.
const MAX_LIMIT: usize = 24 * 31;

/// Live hashrate of a mint's rigs in H/s (`""` = operator).
pub type HashrateFn = Arc<dyn Fn(&str) -> f64 + Send + Sync>;
/// Registered supplier names.
pub type SuppliersFn = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

/// How supplier mints are named and stored.
pub struct SupplierMints {
    /// Supplier `alice` is at `https://alice.<base>`.
    pub base: String,
    /// `<data-dir>/mints`.
    pub dir: PathBuf,
    pub names: SuppliersFn,
}

/// One redemption as the record follows it.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Tracked {
    mint: String,
    first_seen_s: u64,
    /// Already open when the record started: not counted as opened, and no
    /// time to finish.
    baseline: bool,
    finished_s: Option<u64>,
    settled: bool,
    amount_tih: u64,
    delivered_tih: u64,
    hashes: String,
    last_progress_s: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Tracker {
    redemptions: HashMap<String, Tracked>,
    /// Seconds the daemon ran in each hour.
    uptime: BTreeMap<u64, u64>,
    /// Set once the first scan has run.
    started: bool,
}

pub struct Reliability {
    recorder: Arc<ReliabilityRecorder>,
    ledger: Arc<RedemptionLedger>,
    cfg: PublishConfig,
    dir: PathBuf,
    operator: Arc<RecordStore>,
    suppliers: Option<SupplierMints>,
    supplier_stores: Mutex<HashMap<String, Arc<RecordStore>>>,
    hashrate: HashrateFn,
    tracker: Mutex<Tracker>,
    keysets: Mutex<HashMap<String, Vec<String>>>,
    /// NIP-87 announcement of each mint with a public URL (#116).
    announcements: Mutex<HashMap<String, Announcement>>,
}

impl Reliability {
    /// State under `<data-dir>/reliability`; the operator's mint is at `url`.
    pub fn open(
        data_dir: &Path,
        url: &str,
        cfg: PublishConfig,
        ledger: Arc<RedemptionLedger>,
        hashrate: HashrateFn,
        suppliers: Option<SupplierMints>,
    ) -> Result<Arc<Self>> {
        let dir = data_dir.join("reliability");
        let recorder = Arc::new(ReliabilityRecorder::persistent(&dir.join("buckets.json"))?);
        let operator = Arc::new(RecordStore::open(&dir, url, cfg.clone())?);
        let tracker = std::fs::read(dir.join("tracker.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        tracing::info!(
            npub = %operator.npub(),
            relays = cfg.relays.len(),
            calendars = cfg.calendars.len(),
            "reliability record on: GET /v1/hashu/reliability"
        );
        Ok(Arc::new(Self {
            recorder,
            ledger,
            cfg,
            dir,
            operator,
            suppliers,
            supplier_stores: Mutex::new(HashMap::new()),
            hashrate,
            tracker: Mutex::new(tracker),
            keysets: Mutex::new(HashMap::new()),
            announcements: Mutex::new(HashMap::new()),
        }))
    }

    /// Hand this to the proxy's share crediter.
    pub fn recorder(&self) -> Arc<ReliabilityRecorder> {
        self.recorder.clone()
    }

    /// The keysets `mint` (`""` = operator) signs with, for its key document.
    pub fn set_keysets(&self, mint: &str, ids: Vec<String>) {
        self.keysets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(mint.to_string(), ids);
    }

    /// What `mint` (`""` = operator) announces on Nostr (NIP-87 kind
    /// 38172, signed with its receipt key). Only mints with a public URL.
    pub fn set_announcement(&self, mint: &str, a: Announcement) {
        self.announcements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(mint.to_string(), a);
    }

    /// The store for `mint` (`""` = operator); `None` for a supplier that
    /// is not registered.
    pub fn store(&self, mint: &str) -> Option<Arc<RecordStore>> {
        if mint == OPERATOR_MINT {
            return Some(self.operator.clone());
        }
        let s = self.suppliers.as_ref()?;
        if !(s.names)().iter().any(|n| n == mint) {
            return None;
        }
        let mut stores = self
            .supplier_stores
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(st) = stores.get(mint) {
            return Some(st.clone());
        }
        let url = format!("https://{mint}.{}", s.base);
        match RecordStore::open(
            &s.dir.join(mint).join("reliability"),
            &url,
            self.cfg.clone(),
        ) {
            Ok(st) => {
                let st = Arc::new(st);
                stores.insert(mint.to_string(), st.clone());
                Some(st)
            }
            Err(e) => {
                tracing::error!(supplier = %mint, error = %e, "could not open reliability store");
                None
            }
        }
    }

    fn mints(&self) -> Vec<String> {
        let mut m = vec![OPERATOR_MINT.to_string()];
        if let Some(s) = &self.suppliers {
            m.extend((s.names)());
        }
        m
    }

    /// Run forever.
    pub async fn run(self: Arc<Self>) {
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut n: u64 = 0;
        loop {
            tick.tick().await;
            let now = now_s();
            self.step(now, n == 0).await;
            if n % MAINTAIN_EVERY_TICKS == 1 {
                for m in self.mints() {
                    if let Some(st) = self.store(&m) {
                        st.maintain(now).await;
                    }
                }
            }
            n += 1;
        }
    }

    /// One tick at `now`. `first` = the first tick since start (counts no
    /// uptime: the minute before it may not have been up).
    pub async fn step(&self, now: u64, first: bool) {
        self.scan(now, if first { 0 } else { TICK.as_secs() });
        if let Err(e) = self.recorder.save() {
            tracing::warn!(error = %e, "could not save reliability buckets");
        }
        // Key documents (cheap once written).
        let keysets = self
            .keysets
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        for (m, ids) in keysets {
            if let Some(st) = self.store(&m) {
                if let Err(e) = st.ensure_key_document(ids, now).await {
                    tracing::warn!(mint = %st.url(), error = %e, "key document");
                }
            }
        }
        self.announce(now).await;
        self.publish_closed(now).await;
    }

    /// Keep every mint's NIP-87 announcement current, and the operator's
    /// recommendation of each supplier mint.
    async fn announce(&self, now: u64) {
        let all = self
            .announcements
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let operator_announces = all.contains_key(OPERATOR_MINT);
        for (m, a) in all {
            let Some(st) = self.store(&m) else { continue };
            if let Err(e) = st.ensure_announcement(&a, now).await {
                tracing::warn!(mint = %st.url(), error = %e, "NIP-87 announcement");
            }
            if m != OPERATOR_MINT && operator_announces {
                let content = format!(
                    "Hashu supplier mint: TiH backed by {m}'s own rigs, redeemed as hashrate"
                );
                if let Err(e) = self
                    .operator
                    .ensure_recommendation(&m, &a, &st.pubkey(), content, now)
                    .await
                {
                    tracing::warn!(supplier = %m, error = %e, "NIP-87 recommendation");
                }
            }
        }
    }

    /// Follow the ledger and count uptime.
    fn scan(&self, now: u64, up_s: u64) {
        let all = self.ledger.all();
        let mut t = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        let hour = hour_start(now);
        // Uptime: the minute just ended, split across an hour boundary.
        let slot = t.uptime.entry(hour).or_insert(0);
        let into_hour = now - hour;
        *slot = (*slot + up_s.min(into_hour)).min(HOUR_S);
        if up_s > into_hour && hour >= HOUR_S {
            if let Some(prev) = t.uptime.get_mut(&(hour - HOUR_S)) {
                *prev = (*prev + (up_s - into_hour)).min(HOUR_S);
            }
        }
        let first_scan = !t.started;
        t.started = true;
        for r in all {
            let key = r.id.to_string();
            let hashes = r.accumulated_hashes.to_string();
            let finished = r.state.is_finished();
            match t.redemptions.get_mut(&key) {
                Some(tr) => {
                    if tr.hashes != hashes {
                        tr.hashes = hashes;
                        tr.last_progress_s = Some(now);
                    }
                    tr.delivered_tih = r.delivered_tih();
                    if finished && tr.finished_s.is_none() {
                        tr.finished_s = Some(now);
                        tr.settled = r.state == RedemptionState::Settled;
                    }
                }
                None => {
                    // Finished before we ever saw it open: only count it
                    // once the record is running (a fast redemption).
                    if finished && first_scan {
                        continue;
                    }
                    t.redemptions.insert(
                        key,
                        Tracked {
                            mint: r.supplier.clone().unwrap_or_default(),
                            first_seen_s: now,
                            baseline: first_scan,
                            finished_s: finished.then_some(now),
                            settled: r.state == RedemptionState::Settled,
                            amount_tih: r.amount_tih,
                            delivered_tih: r.delivered_tih(),
                            last_progress_s: (r.accumulated_hashes > 0).then_some(now),
                            hashes,
                        },
                    );
                }
            }
        }
        if let Ok(b) = serde_json::to_vec(&*t) {
            let _ = std::fs::create_dir_all(&self.dir);
            let tmp = self.dir.join("tracker.json.tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, self.dir.join("tracker.json"));
            }
        }
    }

    fn stats(&self, mint: &str, hour: u64) -> RedemptionStats {
        let t = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        let end = hour + HOUR_S;
        let mut s = RedemptionStats::default();
        let mut minutes = Vec::new();
        for r in t.redemptions.values().filter(|r| r.mint == mint) {
            if !r.baseline && hour_start(r.first_seen_s) == hour {
                s.opened += 1;
            }
            match r.finished_s {
                Some(f) if hour_start(f) == hour => {
                    if r.settled {
                        s.settled += 1;
                    } else {
                        s.completed += 1;
                    }
                    s.finished_quoted_tih += r.amount_tih;
                    s.finished_delivered_tih += r.delivered_tih;
                    if !r.baseline {
                        minutes.push(f.saturating_sub(r.first_seen_s) / 60);
                    }
                }
                Some(f) if f < end => {}
                _ if r.first_seen_s < end => {
                    s.open += 1;
                    if r.first_seen_s < hour && r.last_progress_s.is_none_or(|p| p < hour) {
                        s.stalled += 1;
                    }
                }
                _ => {}
            }
        }
        (s.minutes_to_finish_median, s.minutes_to_finish_worst) = median_and_worst(minutes);
        s
    }

    /// Publish every closed hour the daemon ran in, for every mint.
    async fn publish_closed(&self, now: u64) {
        let current = hour_start(now);
        let mut buckets: HashMap<(String, u64), _> = self
            .recorder
            .take_closed(current)
            .into_iter()
            .map(|(m, h, b)| ((m, h), b))
            .collect();
        let hours: Vec<(u64, u64)> = {
            let t = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
            t.uptime
                .iter()
                .filter(|(h, up)| **h < current && **up > 0)
                .map(|(h, up)| (*h, *up))
                .collect()
        };
        for mint in self.mints() {
            let Some(store) = self.store(&mint) else {
                continue;
            };
            for &(hour, up) in &hours {
                if store.has_hour(hour) {
                    continue;
                }
                // A store opened after this hour (a new supplier) starts
                // at its first full hour.
                if mint != OPERATOR_MINT && store.last_hour().is_none() && hour + HOUR_S < current {
                    continue;
                }
                let bucket = buckets.remove(&(mint.clone(), hour)).unwrap_or_default();
                let record = HourRecord::new(
                    store.url().to_string(),
                    hour,
                    bucket.clone(),
                    self.stats(&mint, hour),
                    up,
                    (self.hashrate)(&mint) / 1e12,
                );
                if let Err(e) = store.publish(record, now).await {
                    tracing::error!(mint = %store.url(), hour, error = %e, "could not publish reliability record");
                    self.recorder.restore(mint.clone(), hour, bucket);
                }
            }
        }
        // Work for a mint that no longer exists, or an hour we never ran
        // in (cannot happen: a credit implies the daemon was up).
        for ((m, h), b) in buckets {
            if b.shares > 0 {
                tracing::warn!(mint = %m, hour = h, shares = b.shares, "dropping reliability bucket with no store");
            }
        }
        // Keep the uptime map short.
        let mut t = self.tracker.lock().unwrap_or_else(|e| e.into_inner());
        let keep_from = current.saturating_sub(48 * HOUR_S);
        t.uptime.retain(|h, _| *h >= keep_from);
    }
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Which mint a router serves.
#[derive(Clone)]
struct Ctx {
    rel: Arc<Reliability>,
    mint: String,
}

impl Ctx {
    fn store(&self) -> Result<Arc<RecordStore>, Response> {
        self.rel
            .store(&self.mint)
            .ok_or_else(|| (StatusCode::NOT_FOUND, "no such mint").into_response())
    }
}

/// Routes for `mint` (`""` = the operator's mint).
pub fn router(rel: Arc<Reliability>, mint: &str) -> axum::Router {
    axum::Router::new()
        .route("/v1/hashu/reliability", get(index))
        .route("/v1/hashu/reliability/key", get(key_doc))
        .route("/v1/hashu/reliability/key.ots", get(key_ots))
        .route("/v1/hashu/reliability/{hour}", get(hour))
        .layer(from_fn(cors))
        .route(
            "/reliability",
            get(|| async { axum::response::Redirect::permanent("/reliability/") }),
        )
        .route("/reliability/", get(|| async { Html(PAGE) }))
        .with_state(Ctx {
            rel,
            mint: mint.to_string(),
        })
}

async fn cors(req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res
}

#[derive(Deserialize)]
struct IndexQuery {
    limit: Option<usize>,
}

async fn index(State(ctx): State<Ctx>, Query(q): Query<IndexQuery>) -> Response {
    let store = match ctx.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(24).min(MAX_LIMIT);
    let now = now_s();
    let hours = store.summaries(limit);
    Json(json!({
        "v": 1,
        "mint": store.url(),
        "pubkey": store.pubkey(),
        "npub": store.npub(),
        "kind": RECORD_KIND,
        "tag": RECORD_TAG,
        "relays": store.relays(),
        "key": "/v1/hashu/reliability/key",
        "event": "/v1/hashu/reliability/{hour}",
        "ots": "/v1/hashu/reliability/{hour}.ots",
        "totals": store.totals(now),
        "announcement": {
            "kind": ANNOUNCE_KIND,
            "event": store
                .announcement_json()
                .and_then(|e| serde_json::from_str::<serde_json::Value>(&e).ok()),
            "relays": store.announcement_relays(),
        },
        "hours": hours,
    }))
    .into_response()
}

async fn hour(State(ctx): State<Ctx>, UrlPath(raw): UrlPath<String>) -> Response {
    let store = match ctx.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    let (num, ots) = match raw.strip_suffix(".ots") {
        Some(n) => (n, true),
        None => (raw.strip_suffix(".json").unwrap_or(&raw), false),
    };
    let Ok(hour) = num.parse::<u64>() else {
        return (StatusCode::NOT_FOUND, "no such hour").into_response();
    };
    if ots {
        return match store.ots(hour) {
            Some(b) => ots_response(b, &format!("{hour}.ots")),
            None => (StatusCode::NOT_FOUND, "no timestamp for this hour").into_response(),
        };
    }
    match store.event_json(hour) {
        Some(ev) => ([(header::CONTENT_TYPE, "application/json")], ev).into_response(),
        None => (StatusCode::NOT_FOUND, "no record for this hour").into_response(),
    }
}

async fn key_doc(State(ctx): State<Ctx>) -> Response {
    let store = match ctx.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.key_document() {
        Some((doc, _)) => ([(header::CONTENT_TYPE, "application/json")], doc).into_response(),
        None => (StatusCode::NOT_FOUND, "no key document yet").into_response(),
    }
}

async fn key_ots(State(ctx): State<Ctx>) -> Response {
    let store = match ctx.store() {
        Ok(s) => s,
        Err(r) => return r,
    };
    match store.key_document().and_then(|(_, o)| o) {
        Some(b) => ots_response(b, "key.ots"),
        None => (StatusCode::NOT_FOUND, "no key timestamp yet").into_response(),
    }
}

fn ots_response(bytes: Vec<u8>, name: &str) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                "application/vnd.opentimestamps.v1".to_string(),
            ),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::redemption::{Redemption, RedemptionId};
    use nostr::JsonUtil;
    use tower::ServiceExt;

    fn rel(dir: &Path, ledger: Arc<RedemptionLedger>) -> Arc<Reliability> {
        Reliability::open(
            dir,
            "https://mint.example",
            PublishConfig::default(),
            ledger,
            Arc::new(|_: &str| 2e12),
            None,
        )
        .unwrap()
    }

    const H: u64 = 1_800_000_000; // an hour boundary

    #[tokio::test]
    async fn publishes_each_hour_it_ran_with_work_and_stats() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(RedemptionLedger::new());
        let r = rel(dir.path(), ledger.clone());
        r.step(H + 60, true).await;
        // A redemption opens and finishes inside the hour.
        let id = RedemptionId::from_bytes([1; 16]);
        ledger
            .open(Redemption::new(id, 1, "p".into(), "w".into()))
            .unwrap();
        r.step(H + 120, false).await;
        ledger.credit_share(&id, 256.0).unwrap();
        r.recorder().record("", H + 130, 256.0, &[0; 80]);
        r.step(H + 600, false).await;
        assert!(r.operator.last_hour().is_none(), "hour still open");
        r.step(H + 3600 + 5, false).await;
        assert_eq!(r.operator.last_hour(), Some(H));
        let ev: nostr::Event =
            nostr::JsonUtil::from_json(r.operator.event_json(H).unwrap()).unwrap();
        ev.verify().unwrap();
        let rec: HourRecord = serde_json::from_str(&ev.content).unwrap();
        assert_eq!(rec.shares, 1);
        assert_eq!(rec.work_tih, 1.0);
        assert_eq!(rec.redemptions.opened, 1);
        assert_eq!(rec.redemptions.completed, 1);
        assert_eq!(rec.redemptions.minutes_to_finish_median, Some(8));
        assert_eq!(rec.hashrate_ths, 2.0);
        assert!(rec.uptime_s > 0 && rec.uptime_s <= 3600);

        // Served over HTTP.
        let app = router(r.clone(), "");
        let res = app
            .clone()
            .oneshot(
                axum::http::Request::get("/v1/hashu/reliability")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["hours"][0]["hour"], H);
        assert_eq!(v["hours"][0]["ots"], "none");
        assert_eq!(v["pubkey"], r.operator.pubkey());
        assert!(v.get("current").is_none(), "no live view of the open hour");
        let res = app
            .oneshot(
                axum::http::Request::get(format!("/v1/hashu/reliability/{H}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn downtime_is_a_gap_and_restart_keeps_work() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(RedemptionLedger::new());
        {
            let r = rel(dir.path(), ledger.clone());
            r.step(H + 60, true).await;
            r.step(H + 120, false).await;
            r.recorder().record("", H + 130, 512.0, &[0; 80]);
            r.step(H + 180, false).await;
        }
        // Down for two hours, back up.
        let r = rel(dir.path(), ledger.clone());
        r.step(H + 3 * 3600 + 30, true).await;
        r.step(H + 3 * 3600 + 90, false).await;
        // The hour before the outage is published late, with its work.
        assert!(r.operator.has_hour(H));
        assert!(!r.operator.has_hour(H + 3600));
        assert!(!r.operator.has_hour(H + 2 * 3600));
        let rec: HourRecord = serde_json::from_str(
            &nostr::Event::from_json(r.operator.event_json(H).unwrap())
                .unwrap()
                .content,
        )
        .unwrap();
        assert_eq!(rec.shares, 1);
        assert_eq!(rec.uptime_s, 120);
    }

    fn announcement(url: &str) -> Announcement {
        Announcement {
            mint_pubkey: Some(format!("02{:0>64}", url.len())),
            url: url.into(),
            nuts: vec![1, 2, 3, 4, 5, 6],
            network: "mainnet".into(),
            units: vec!["tih".into()],
            name: None,
            description: None,
            icon_url: None,
        }
    }

    #[tokio::test]
    async fn announces_each_mint_and_recommends_suppliers() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Arc::new(RedemptionLedger::new());
        let r = Reliability::open(
            dir.path(),
            "https://mint.example",
            PublishConfig::default(),
            ledger,
            Arc::new(|_: &str| 0.0),
            Some(SupplierMints {
                base: "mint.example".into(),
                dir: dir.path().join("mints"),
                names: Arc::new(|| vec!["alice".into()]),
            }),
        )
        .unwrap();
        r.set_announcement("", announcement("https://mint.example"));
        let alice_a = announcement("https://alice.mint.example");
        r.set_announcement("alice", alice_a.clone());
        r.step(H + 60, true).await;

        let ev = nostr::Event::from_json(r.operator.announcement_json().unwrap()).unwrap();
        ev.verify().unwrap();
        assert_eq!(ev.kind, nostr::Kind::Custom(ANNOUNCE_KIND));
        assert_eq!(ev.pubkey.to_hex(), r.operator.pubkey());

        let alice = r.store("alice").unwrap();
        let aev = nostr::Event::from_json(alice.announcement_json().unwrap()).unwrap();
        assert_eq!(aev.pubkey.to_hex(), alice.pubkey());
        assert_ne!(alice.pubkey(), r.operator.pubkey(), "one identity per mint");

        // The operator recommends alice's mint, pointing at her announcement.
        let rec: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("reliability/nostr/recommend-alice.json")).unwrap(),
        )
        .unwrap();
        let rev = nostr::Event::from_json(rec["event"].as_str().unwrap()).unwrap();
        assert_eq!(rev.kind, nostr::Kind::Custom(38000));
        assert_eq!(rev.pubkey.to_hex(), r.operator.pubkey());
        let a = format!("38172:{}:{}", alice.pubkey(), alice_a.d());
        assert!(rev
            .tags
            .iter()
            .any(|t| t.clone().to_vec()[..2] == ["a".to_string(), a.clone()]));

        // The index points at the announcement.
        let res = router(r.clone(), "alice")
            .oneshot(
                axum::http::Request::get("/v1/hashu/reliability")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["announcement"]["kind"], 38172);
        assert_eq!(v["announcement"]["event"]["id"], aev.id.to_hex());
    }
}
