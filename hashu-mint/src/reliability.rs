//! One mint's published reliability record (issue #109): signing, storage,
//! OpenTimestamps and Nostr. ARCHITECTURE.md §4.7.4.
//!
//! Each closed UTC hour becomes one **regular** Nostr event (kind
//! [`RECORD_KIND`], so relays keep every hour rather than only the latest)
//! signed with the mint's receipt key, whose content is a
//! [`hashu_core::reliability::HourRecord`]. The event id, a SHA-256 over
//! the signed event, is timestamped with OpenTimestamps; the `.ots` proof
//! is upgraded once a calendar commits it to Bitcoin.
//!
//! The receipt key also signs a small key document (mint URL, receipt
//! pubkey, keyset ids) that is timestamped once, so the key's age is
//! provable too.
//!
//! On disk, under the store's directory:
//! - `receipt.nsec`: the receipt key (created on first run, never rotated).
//! - `key.json`, `key.ots`: the key document and its proof.
//! - `hours/<hour>.json`: the signed event. `hours/<hour>.ots`: its proof.
//!   `hours/<hour>.meta.json`: relays that took it.
//! - `nostr/<slot>.json`: addressable events the key keeps current (the
//!   NIP-87 mint announcement and recommendations, [`crate::discovery`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use hashu_core::ots::{DetachedTimestamp, Timestamp};
use hashu_core::reliability::{HourRecord, HOUR_S};
use nostr::{Event, EventBuilder, JsonUtil, Keys, Kind, Tag, ToBech32};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Nostr kind of an hourly record: regular (1000-9999), so every hour is
/// kept.
pub const RECORD_KIND: u16 = 8333;
/// Hashtag on every record event.
pub const RECORD_TAG: &str = "hashu-reliability";

const OTS_MEDIA: &str = "application/vnd.opentimestamps.v1";
/// Stop trying to upgrade a proof after this long.
const UPGRADE_FOR_S: u64 = 7 * 24 * 3600;
/// Re-post an unchanged addressable event this often, so relays that
/// prune or joined later still have it.
const REPOST_EVERY_S: u64 = 7 * 24 * 3600;
/// Retry an addressable event no relay took after this long.
const RETRY_AFTER_S: u64 = 30 * 60;

/// Where records go besides the mint's own HTTP API. Empty lists turn that
/// part off.
#[derive(Clone, Debug, Default)]
pub struct PublishConfig {
    /// Nostr relays (`wss://…`).
    pub relays: Vec<String>,
    /// OpenTimestamps calendars (`https://…`).
    pub calendars: Vec<String>,
}

impl PublishConfig {
    /// Parse comma- or whitespace-separated lists.
    pub fn parse(relays: &str, calendars: &str) -> Self {
        let split = |s: &str| {
            s.split(|c: char| c == ',' || c.is_whitespace())
                .map(|x| x.trim().trim_end_matches('/').to_string())
                .filter(|x| !x.is_empty())
                .collect::<Vec<_>>()
        };
        Self {
            relays: split(relays),
            calendars: split(calendars),
        }
    }
}

/// The key document: what the receipt key speaks for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyDocument {
    pub v: u32,
    pub mint: String,
    /// Receipt key, x-only hex (the Nostr pubkey).
    pub pubkey: String,
    pub keysets: Vec<String>,
    pub created_at: u64,
}

impl KeyDocument {
    /// SHA-256 of the document's bytes as served: what `key.ots` stamps.
    pub fn digest(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }
}

/// Proof status of one hour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProofStatus {
    /// No calendar answered (or none configured).
    None,
    /// Calendars promised to commit it; not in a block yet.
    Pending,
    /// In a Bitcoin block.
    Bitcoin,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Meta {
    relays: Vec<String>,
}

/// One addressable event kept current on relays.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Addressable {
    /// SHA-256 over kind, tags and content: a change re-signs.
    fingerprint: String,
    event: String,
    relays: Vec<String>,
    posted_s: u64,
    tried_s: u64,
}

/// What the index lists per hour.
#[derive(Clone, Debug, Serialize)]
pub struct HourSummary {
    pub hour: u64,
    pub event_id: String,
    pub shares: u64,
    pub work_tih: f64,
    pub best_difficulty: Option<f64>,
    pub blocks: usize,
    pub uptime_s: u64,
    pub completed: u64,
    pub settled: u64,
    pub opened: u64,
    pub stalled: u64,
    pub ots: ProofStatus,
    /// Bitcoin block height the proof reached, once it has.
    pub ots_height: Option<u64>,
    /// Relays that took the event.
    pub relays: usize,
}

/// One mint's record store.
pub struct RecordStore {
    url: String,
    keys: Keys,
    dir: PathBuf,
    cfg: PublishConfig,
    http: reqwest::Client,
    index: Mutex<BTreeMap<u64, HourSummary>>,
}

impl RecordStore {
    /// Open (creating) the store in `dir` for the mint at `url`.
    pub fn open(dir: &Path, url: &str, cfg: PublishConfig) -> Result<Self> {
        std::fs::create_dir_all(dir.join("hours")).with_context(|| format!("create {dir:?}"))?;
        let keys = load_or_create_key(&dir.join("receipt.nsec"))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent("hashu-mint")
            .build()?;
        let store = Self {
            url: url.trim_end_matches('/').to_string(),
            keys,
            dir: dir.to_path_buf(),
            cfg,
            http,
            index: Mutex::new(BTreeMap::new()),
        };
        store.load_index();
        Ok(store)
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn pubkey(&self) -> String {
        self.keys.public_key().to_hex()
    }

    pub fn npub(&self) -> String {
        self.keys.public_key().to_bech32().unwrap_or_default()
    }

    pub fn relays(&self) -> &[String] {
        &self.cfg.relays
    }

    /// Newest published hour.
    pub fn last_hour(&self) -> Option<u64> {
        self.lock().keys().next_back().copied()
    }

    pub fn has_hour(&self, hour: u64) -> bool {
        self.lock().contains_key(&hour)
    }

    /// Summaries, newest first, at most `limit`.
    pub fn summaries(&self, limit: usize) -> Vec<HourSummary> {
        self.lock().values().rev().take(limit).cloned().collect()
    }

    /// Work claimed by the published hours that start in
    /// `[from_s, to_s)`: `(TiH, hours published)`. What a hashrate plan
    /// is checked against.
    pub fn work_between(&self, from_s: u64, to_s: u64) -> (f64, u64) {
        let idx = self.lock();
        idx.range(from_s..to_s)
            .fold((0.0, 0), |(w, n), (_, s)| (w + s.work_tih, n + 1))
    }

    /// Totals over every published hour.
    pub fn totals(&self, now_s: u64) -> serde_json::Value {
        let idx = self.lock();
        let first = idx.keys().next().copied();
        let published = idx.len() as u64;
        let expected = first
            .map(|f| (hashu_core::reliability::hour_start(now_s).saturating_sub(f)) / HOUR_S)
            .unwrap_or(0);
        let mut work = 0.0;
        let mut blocks = 0;
        let mut completed = 0;
        let mut settled = 0;
        let mut uptime = 0;
        for s in idx.values() {
            work += s.work_tih;
            blocks += s.blocks;
            completed += s.completed;
            settled += s.settled;
            uptime += s.uptime_s;
        }
        serde_json::json!({
            "since": first,
            "hours_published": published,
            "hours_since_first": expected,
            "uptime_s": uptime,
            "work_tih": work,
            "blocks": blocks,
            "redemptions_completed": completed,
            "redemptions_settled": settled,
        })
    }

    /// The signed event for `hour`, as JSON.
    pub fn event_json(&self, hour: u64) -> Option<String> {
        std::fs::read_to_string(self.hour_path(hour, "json")).ok()
    }

    /// The `.ots` proof for `hour`.
    pub fn ots(&self, hour: u64) -> Option<Vec<u8>> {
        std::fs::read(self.hour_path(hour, "ots")).ok()
    }

    /// The key document as served, and its proof.
    pub fn key_document(&self) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
        let doc = std::fs::read(self.dir.join("key.json")).ok()?;
        Some((doc, std::fs::read(self.dir.join("key.ots")).ok()))
    }

    /// Write (and stamp) the key document if it is missing or names other
    /// keysets than `keysets`.
    pub async fn ensure_key_document(&self, keysets: Vec<String>, now_s: u64) -> Result<()> {
        let path = self.dir.join("key.json");
        if let Ok(b) = std::fs::read(&path) {
            if let Ok(old) = serde_json::from_slice::<KeyDocument>(&b) {
                if old.mint == self.url && old.pubkey == self.pubkey() && old.keysets == keysets {
                    if !self.dir.join("key.ots").exists() {
                        self.stamp_to(KeyDocument::digest(&b), &self.dir.join("key.ots"))
                            .await;
                    }
                    return Ok(());
                }
                // Keep the old one: it is part of the key's history.
                let _ =
                    std::fs::rename(&path, self.dir.join(format!("key-{}.json", old.created_at)));
                let _ = std::fs::rename(
                    self.dir.join("key.ots"),
                    self.dir.join(format!("key-{}.ots", old.created_at)),
                );
            }
        }
        let doc = KeyDocument {
            v: 1,
            mint: self.url.clone(),
            pubkey: self.pubkey(),
            keysets,
            created_at: now_s,
        };
        let bytes = serde_json::to_vec_pretty(&doc)?;
        write_atomic(&path, &bytes)?;
        self.stamp_to(KeyDocument::digest(&bytes), &self.dir.join("key.ots"))
            .await;
        Ok(())
    }

    /// Sign, store, timestamp and post one hour. Idempotent: an hour
    /// already published is left alone.
    pub async fn publish(&self, record: HourRecord, now_s: u64) -> Result<String> {
        let hour = record.hour;
        if let Some(s) = self.lock().get(&hour) {
            return Ok(s.event_id.clone());
        }
        let event = sign_record(&self.keys, &record, now_s)?;
        let id = event.id.to_hex();
        write_atomic(&self.hour_path(hour, "json"), event.as_json().as_bytes())?;
        self.stamp_to(event.id.to_bytes(), &self.hour_path(hour, "ots"))
            .await;
        let meta = self.post(&event).await;
        let _ = write_atomic(
            &self.hour_path(hour, "meta.json"),
            &serde_json::to_vec(&meta).unwrap_or_default(),
        );
        self.refresh(hour);
        tracing::info!(
            mint = %self.url,
            hour,
            event = %id,
            shares = record.shares,
            work_tih = record.work_tih,
            blocks = record.blocks.len(),
            relays = meta.relays.len(),
            "published reliability record"
        );
        Ok(id)
    }

    /// Upgrade pending proofs and re-post events no relay took yet.
    pub async fn maintain(&self, now_s: u64) {
        let hours: Vec<(u64, ProofStatus, usize)> = self
            .lock()
            .values()
            .filter(|s| s.hour + UPGRADE_FOR_S > now_s)
            .map(|s| (s.hour, s.ots, s.relays))
            .collect();
        for (hour, ots, relays) in hours {
            let mut changed = false;
            let path = self.hour_path(hour, "ots");
            match ots {
                ProofStatus::Pending => changed |= self.upgrade(&path).await,
                ProofStatus::None if !self.cfg.calendars.is_empty() => {
                    if let Some(ev) = self.event(hour) {
                        self.stamp_to(ev.id.to_bytes(), &path).await;
                        changed = true;
                    }
                }
                _ => {}
            }
            if relays == 0 && !self.cfg.relays.is_empty() {
                if let Some(ev) = self.event(hour) {
                    let meta = self.post(&ev).await;
                    if !meta.relays.is_empty() {
                        let _ = write_atomic(
                            &self.hour_path(hour, "meta.json"),
                            &serde_json::to_vec(&meta).unwrap_or_default(),
                        );
                        changed = true;
                    }
                }
            }
            if changed {
                self.refresh(hour);
            }
        }
        let key_ots = self.dir.join("key.ots");
        if let Ok(b) = std::fs::read(&key_ots) {
            if DetachedTimestamp::from_bytes(&b).is_ok_and(|d| !d.timestamp.is_complete()) {
                self.upgrade(&key_ots).await;
            }
        }
    }

    /// Keep the NIP-87 announcement of this mint current.
    pub async fn ensure_announcement(
        &self,
        a: &crate::discovery::Announcement,
        now_s: u64,
    ) -> Result<()> {
        self.ensure_addressable(
            "announce",
            crate::discovery::ANNOUNCE_KIND,
            a.tags()?,
            a.content(),
            now_s,
        )
        .await
    }

    /// Keep a NIP-87 recommendation of another mint current, under `slot`.
    /// `announcer` signs that mint's announcement.
    pub async fn ensure_recommendation(
        &self,
        slot: &str,
        a: &crate::discovery::Announcement,
        announcer: &str,
        content: String,
        now_s: u64,
    ) -> Result<()> {
        let relay = self.cfg.relays.first().map(String::as_str);
        self.ensure_addressable(
            &format!("recommend-{slot}"),
            crate::discovery::RECOMMEND_KIND,
            a.recommendation_tags(announcer, relay)?,
            content,
            now_s,
        )
        .await
    }

    /// The signed announcement, once made.
    pub fn announcement_json(&self) -> Option<String> {
        self.load_addressable("announce").map(|a| a.event)
    }

    /// Relays that hold the announcement.
    pub fn announcement_relays(&self) -> Vec<String> {
        self.load_addressable("announce")
            .map(|a| a.relays)
            .unwrap_or_default()
    }

    fn addressable_path(&self, slot: &str) -> PathBuf {
        self.dir.join("nostr").join(format!("{slot}.json"))
    }

    fn load_addressable(&self, slot: &str) -> Option<Addressable> {
        let b = std::fs::read(self.addressable_path(slot)).ok()?;
        serde_json::from_slice(&b).ok()
    }

    /// Sign (when it changed) and post an addressable event. Cheap when
    /// nothing changed and relays have it: call it every tick.
    async fn ensure_addressable(
        &self,
        slot: &str,
        kind: u16,
        tags: Vec<Tag>,
        content: String,
        now_s: u64,
    ) -> Result<()> {
        let fingerprint = {
            let tv: Vec<Vec<String>> = tags.iter().map(|t| t.clone().to_vec()).collect();
            let bytes = serde_json::to_vec(&(kind, &tv, &content))?;
            hex::encode(Sha256::digest(bytes))
        };
        let path = self.addressable_path(slot);
        let old = self.load_addressable(slot);
        let mut cur = match old {
            Some(a) if a.fingerprint == fingerprint => {
                let stale = a.posted_s + REPOST_EVERY_S <= now_s;
                let unposted = a.relays.is_empty()
                    && !self.cfg.relays.is_empty()
                    && a.tried_s + RETRY_AFTER_S <= now_s;
                if !stale && !unposted {
                    return Ok(());
                }
                a
            }
            _ => {
                let event = EventBuilder::new(Kind::Custom(kind), content)
                    .tags(tags)
                    .custom_created_at(nostr::Timestamp::from(now_s))
                    .sign_with_keys(&self.keys)?;
                tracing::info!(mint = %self.url, kind, slot, event = %event.id.to_hex(), "signed nostr event");
                Addressable {
                    fingerprint,
                    event: event.as_json(),
                    relays: vec![],
                    posted_s: 0,
                    tried_s: 0,
                }
            }
        };
        if !self.cfg.relays.is_empty() {
            let event = Event::from_json(&cur.event)?;
            let meta = self.post(&event).await;
            cur.tried_s = now_s;
            if !meta.relays.is_empty() {
                cur.relays = meta.relays;
                cur.posted_s = now_s;
                tracing::info!(mint = %self.url, kind, slot, relays = cur.relays.len(), "posted nostr event");
            }
        } else {
            // Nothing to post to: stored and served only.
            cur.posted_s = now_s;
        }
        std::fs::create_dir_all(self.dir.join("nostr"))?;
        write_atomic(&path, &serde_json::to_vec_pretty(&cur)?)?;
        Ok(())
    }

    fn event(&self, hour: u64) -> Option<Event> {
        Event::from_json(self.event_json(hour)?).ok()
    }

    async fn post(&self, event: &Event) -> Meta {
        if self.cfg.relays.is_empty() {
            return Meta::default();
        }
        match tokio::time::timeout(
            Duration::from_secs(30),
            crate::manifest::publish(event, &self.cfg.relays),
        )
        .await
        {
            Ok(Ok(out)) => {
                for (relay, e) in &out.failed {
                    tracing::warn!(%relay, error = %e, "relay refused reliability record");
                }
                Meta {
                    relays: out.succeeded,
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not post reliability record");
                Meta::default()
            }
            Err(_) => {
                tracing::warn!("posting reliability record timed out");
                Meta::default()
            }
        }
    }

    /// Submit `digest` to every calendar and write the merged proof.
    async fn stamp_to(&self, digest: [u8; 32], path: &Path) {
        if self.cfg.calendars.is_empty() {
            return;
        }
        let mut d = DetachedTimestamp::for_digest(digest);
        for cal in &self.cfg.calendars {
            match self.submit(cal, &digest).await {
                Ok(t) => d.timestamp.merge(t),
                Err(e) => tracing::warn!(calendar = %cal, error = %e, "timestamp submit failed"),
            }
        }
        if d.timestamp.attestations.is_empty() && d.timestamp.ops.is_empty() {
            return;
        }
        match d.to_bytes() {
            Ok(b) => {
                if let Err(e) = write_atomic(path, &b) {
                    tracing::warn!(?path, error = %e, "could not save timestamp");
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not encode timestamp"),
        }
    }

    async fn submit(&self, calendar: &str, digest: &[u8; 32]) -> Result<Timestamp> {
        let res = self
            .http
            .post(format!("{calendar}/digest"))
            .header("Accept", OTS_MEDIA)
            .body(digest.to_vec())
            .send()
            .await?
            .error_for_status()?;
        let body = res.bytes().await?;
        Ok(Timestamp::parse(digest, &body)?)
    }

    /// Ask each pending calendar for its Bitcoin commitment. True if the
    /// proof changed.
    async fn upgrade(&self, path: &Path) -> bool {
        let Ok(bytes) = std::fs::read(path) else {
            return false;
        };
        let Ok(mut d) = DetachedTimestamp::from_bytes(&bytes) else {
            return false;
        };
        let mut changed = false;
        for (msg, uri) in d.timestamp.pending() {
            // Only follow https calendars.
            if !uri.starts_with("https://") || uri.contains(['?', '#', '@']) {
                continue;
            }
            let url = format!(
                "{}/timestamp/{}",
                uri.trim_end_matches('/'),
                hex::encode(&msg)
            );
            let res = match self.http.get(&url).header("Accept", OTS_MEDIA).send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!(%url, error = %e, "timestamp upgrade request failed");
                    continue;
                }
            };
            if !res.status().is_success() {
                continue; // 404: not committed yet
            }
            let Ok(body) = res.bytes().await else {
                continue;
            };
            match Timestamp::parse(&msg, &body) {
                Ok(up) if up.is_complete() => {
                    changed |= d.timestamp.upgrade_at(&msg, &uri, up);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(%url, error = %e, "unreadable timestamp upgrade"),
            }
        }
        if changed {
            if let Ok(b) = d.to_bytes() {
                let _ = write_atomic(path, &b);
            }
        }
        changed
    }

    fn hour_path(&self, hour: u64, ext: &str) -> PathBuf {
        self.dir.join("hours").join(format!("{hour}.{ext}"))
    }

    fn load_index(&self) {
        let Ok(rd) = std::fs::read_dir(self.dir.join("hours")) else {
            return;
        };
        let hours: Vec<u64> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                name.strip_suffix(".json")
                    .filter(|s| !s.ends_with(".meta"))?
                    .parse()
                    .ok()
            })
            .collect();
        for h in hours {
            self.refresh(h);
        }
    }

    /// Re-read one hour's files into the index.
    fn refresh(&self, hour: u64) {
        let Some(ev) = self.event(hour) else {
            return;
        };
        let Ok(rec) = serde_json::from_str::<HourRecord>(&ev.content) else {
            return;
        };
        let (ots, ots_height) = match self
            .ots(hour)
            .and_then(|b| DetachedTimestamp::from_bytes(&b).ok())
        {
            Some(d) => match d.timestamp.bitcoin().iter().map(|(h, _)| *h).min() {
                Some(h) => (ProofStatus::Bitcoin, Some(h)),
                None => (ProofStatus::Pending, None),
            },
            None => (ProofStatus::None, None),
        };
        let relays = std::fs::read(self.hour_path(hour, "meta.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
            .map(|m| m.relays.len())
            .unwrap_or(0);
        let summary = HourSummary {
            hour,
            event_id: ev.id.to_hex(),
            shares: rec.shares,
            work_tih: rec.work_tih,
            best_difficulty: rec.top.first().map(|s| s.difficulty),
            blocks: rec.blocks.len(),
            uptime_s: rec.uptime_s,
            completed: rec.redemptions.completed,
            settled: rec.redemptions.settled,
            opened: rec.redemptions.opened,
            stalled: rec.redemptions.stalled,
            ots,
            ots_height,
            relays,
        };
        self.lock().insert(hour, summary);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, HourSummary>> {
        self.index.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Sign `record` as a [`RECORD_KIND`] event.
pub fn sign_record(keys: &Keys, record: &HourRecord, now_s: u64) -> Result<Event> {
    let content = serde_json::to_string(record)?;
    let tags = [
        Tag::hashtag(RECORD_TAG),
        Tag::parse(["u", record.mint.as_str()])?,
        Tag::parse(["hour", record.hour.to_string().as_str()])?,
        Tag::parse([
            "alt",
            format!(
                "Hashu mint reliability record: {} for the hour starting {} (unix)",
                record.mint, record.hour
            )
            .as_str(),
        ])?,
    ];
    let created = now_s.max(record.hour + HOUR_S);
    Ok(EventBuilder::new(Kind::Custom(RECORD_KIND), content)
        .tags(tags)
        .custom_created_at(nostr::Timestamp::from(created))
        .sign_with_keys(keys)?)
}

/// The receipt key of the store in `dir` (created on first use), for
/// callers that need the mint's npub before the store opens (NUT-06
/// contact).
pub fn receipt_keys(dir: &Path) -> Result<Keys> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
    load_or_create_key(&dir.join("receipt.nsec"))
}

fn load_or_create_key(path: &Path) -> Result<Keys> {
    match std::fs::read_to_string(path) {
        Ok(s) => Keys::parse(s.trim()).with_context(|| format!("parse {path:?}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let keys = Keys::generate();
            let nsec = keys.secret_key().to_bech32()?;
            write_secret(path, nsec.as_bytes())?;
            tracing::info!(?path, npub = %keys.public_key().to_bech32()?, "created reliability receipt key");
            Ok(keys)
        }
        Err(e) => Err(e).with_context(|| format!("read {path:?}")),
    }
}

fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("create {path:?}"))?;
    f.write_all(bytes)?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_core::reliability::{HourBucket, RedemptionStats};

    fn record(hour: u64) -> HourRecord {
        let mut b = HourBucket::default();
        b.record(256.0, &[0u8; 80]);
        HourRecord::new(
            "https://mint.example".into(),
            hour,
            b,
            RedemptionStats::default(),
            3600,
            1.2,
        )
    }

    #[test]
    fn signed_record_verifies_and_round_trips() {
        let keys = Keys::generate();
        let ev = sign_record(&keys, &record(7200), 10_900).unwrap();
        ev.verify().unwrap();
        assert_eq!(ev.kind, Kind::Custom(RECORD_KIND));
        let back: HourRecord = serde_json::from_str(&ev.content).unwrap();
        assert_eq!(back, record(7200));
        assert_eq!(ev.created_at.as_secs(), 10_900);
    }

    #[tokio::test]
    async fn store_publishes_once_and_reloads() {
        let dir = tempfile::tempdir().unwrap();
        let store = RecordStore::open(
            dir.path(),
            "https://mint.example/",
            PublishConfig::default(),
        )
        .unwrap();
        let id = store.publish(record(3600), 7300).await.unwrap();
        assert_eq!(store.publish(record(3600), 7400).await.unwrap(), id);
        assert_eq!(store.last_hour(), Some(3600));
        let s = &store.summaries(10)[0];
        assert_eq!(s.ots, ProofStatus::None);
        assert_eq!(s.shares, 1);
        let pubkey = store.pubkey();
        drop(store);
        let again = RecordStore::open(dir.path(), "https://mint.example", PublishConfig::default())
            .unwrap();
        assert_eq!(again.pubkey(), pubkey, "receipt key persists");
        assert!(again.has_hour(3600));
        assert!(again.event_json(3600).is_some());
        assert_eq!(again.totals(3 * 3600)["hours_published"], 1);
    }

    #[tokio::test]
    async fn key_document_is_rewritten_only_on_change() {
        let dir = tempfile::tempdir().unwrap();
        let store =
            RecordStore::open(dir.path(), "https://m.example", PublishConfig::default()).unwrap();
        store
            .ensure_key_document(vec!["00aa".into()], 100)
            .await
            .unwrap();
        store
            .ensure_key_document(vec!["00aa".into()], 200)
            .await
            .unwrap();
        let (doc, _) = store.key_document().unwrap();
        let d: KeyDocument = serde_json::from_slice(&doc).unwrap();
        assert_eq!(d.created_at, 100);
        store
            .ensure_key_document(vec!["00aa".into(), "00bb".into()], 300)
            .await
            .unwrap();
        let (doc, _) = store.key_document().unwrap();
        let d: KeyDocument = serde_json::from_slice(&doc).unwrap();
        assert_eq!(d.created_at, 300);
        assert!(dir.path().join("key-100.json").exists());
    }

    #[tokio::test]
    async fn announcement_is_signed_by_the_receipt_key_and_resigned_on_change() {
        use crate::discovery::{Announcement, ANNOUNCE_KIND};
        let dir = tempfile::tempdir().unwrap();
        let store =
            RecordStore::open(dir.path(), "https://m.example", PublishConfig::default()).unwrap();
        let mut a = Announcement {
            mint_pubkey: Some("02ab".into()),
            url: "https://m.example".into(),
            nuts: vec![1, 2, 3, 4, 5, 6],
            network: "mainnet".into(),
            units: vec!["tih".into()],
            name: Some("m".into()),
            description: None,
            icon_url: None,
        };
        store.ensure_announcement(&a, 100).await.unwrap();
        let first = Event::from_json(store.announcement_json().unwrap()).unwrap();
        first.verify().unwrap();
        assert_eq!(first.kind, Kind::Custom(ANNOUNCE_KIND));
        assert_eq!(first.pubkey.to_hex(), store.pubkey());
        store.ensure_announcement(&a, 200).await.unwrap();
        let same = Event::from_json(store.announcement_json().unwrap()).unwrap();
        assert_eq!(same.id, first.id, "unchanged: not re-signed");
        a.nuts.push(7);
        store.ensure_announcement(&a, 300).await.unwrap();
        let changed = Event::from_json(store.announcement_json().unwrap()).unwrap();
        assert_ne!(changed.id, first.id);
        assert_eq!(changed.created_at.as_secs(), 300);
        assert_eq!(
            receipt_keys(dir.path()).unwrap().public_key().to_hex(),
            store.pubkey(),
            "receipt_keys reads the same key"
        );
    }

    #[test]
    fn parses_lists() {
        let c = PublishConfig::parse("wss://a, wss://b/", " https://c.example ");
        assert_eq!(c.relays, vec!["wss://a", "wss://b"]);
        assert_eq!(c.calendars, vec!["https://c.example"]);
    }
}
