//! Hosted-mint waitlist for miners (`--waitlist`, off by default).
//!
//! Hosted per-miner mints (`<name>.<mint domain>`) are not open for sign-up
//! yet. Until they are, `GET /waitlist/` on the mint's public HTTP port shows a
//! splash page where a miner leaves one way to reach them, and
//! `POST /waitlist/api` appends it to `<data>/waitlist.jsonl`. The operator reads
//! (and deletes) entries behind the operator password, at
//! `/operator/waitlist`.
//!
//! Minimal data, on purpose: an email, a Nostr npub or another contact
//! handle (at least one), and optionally a
//! wished-for mint name, rough hashrate and a short note. No IP address, user
//! agent or cookie is stored. Rate limiting keys on a salted hash of the
//! client address that lives only in memory and is forgotten within the hour.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const PAGE: &str = include_str!("waitlist.html");
const OPERATOR_PAGE: &str = include_str!("waitlist-operator.html");

/// Field limits, in characters.
pub const MAX_CONTACT: usize = 200;
pub const MAX_HASHRATE: usize = 80;
pub const MAX_NOTE: usize = 500;
/// Largest request body accepted by `POST /waitlist/api`.
const MAX_BODY: usize = 4 * 1024;
/// Entries kept at most; past this the form says the list is full.
pub const MAX_ENTRIES: usize = 5_000;
/// Sign-ups per client address per window.
const PER_CLIENT: usize = 5;
/// Sign-ups from everyone per window (a flood guard, not a quota).
const GLOBAL: usize = 120;
const WINDOW: Duration = Duration::from_secs(3600);

/// One waitlist entry, as stored (one JSON object per line).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: String,
    /// Unix seconds.
    pub at: u64,
    /// Email address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Nostr public key, `npub1...`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npub: Option<String>,
    /// Any other way to reach the miner (Signal, Telegram, X, ...). Before
    /// the email and npub fields this held every contact, emails included.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub contact: String,
    /// Wished-for mint name, as in `<name>.<mint domain>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint_name: Option<String>,
    /// Their rigs, free text ("2 Bitaxes", "1 S21").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashrate: Option<String>,
    /// Hashrate they'd put behind the mint, in TH/s (converted from the
    /// unit they picked).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashrate_ths: Option<f64>,
    /// Unit they typed the hashrate in: a key of [`HASHRATE_UNITS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashrate_unit: Option<String>,
    /// How long they'd commit it: a key of [`HORIZONS`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon: Option<String>,
    /// TiH they could put up for sale: `hashrate_ths` × the horizon's
    /// hours, in TiH, when both are given. Recomputed on load, so entries
    /// from before #90 (`thh_for_sale`) read in TiH too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tih_for_sale: Option<f64>,
    /// How they'd like to be paid for sales: any of [`PAYOUTS`] (select
    /// all that apply). Entries from before stored one string; those load
    /// as a one-item list.
    #[serde(
        default,
        deserialize_with = "one_or_many",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub payout: Vec<String>,
    /// What "other" means, when `payout` includes "other".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payout_other: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Commitment horizons the form offers, with their length in hours. The
/// miner's hashrate times the horizon is the TiH their mint could sell.
pub const HORIZONS: [(&str, u32); 8] = [
    ("hour", 1),
    ("6-hours", 6),
    ("day", 24),
    ("week", 7 * 24),
    ("month", 30 * 24),
    ("quarter", 90 * 24),
    ("half-year", 182 * 24),
    ("year", 365 * 24),
];

/// Whole TiH that `ths` TH/s over `horizon` would produce.
fn tih_for_sale(ths: Option<f64>, horizon: Option<&str>) -> Option<f64> {
    let hours = HORIZONS.iter().find(|(k, _)| Some(*k) == horizon)?.1;
    ths.map(|t| hashu_core::denom::tih_from_ths_hours(t, hours as f64).round())
}

/// Hashrate units the form offers, with their size in TH/s.
pub const HASHRATE_UNITS: [(&str, f64); 3] = [("TH/s", 1.0), ("PH/s", 1e3), ("EH/s", 1e6)];

/// Largest hashrate accepted, in TH/s (1,000 EH/s, about the whole network).
pub const MAX_THS: f64 = 1e9;

/// A string or a list of strings, as a list (blank strings dropped).
fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(Option<String>),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => s.into_iter().collect(),
        OneOrMany::Many(v) => v,
    }
    .into_iter()
    .filter(|s| !s.trim().is_empty())
    .collect())
}

/// Payout preferences the form offers: stablecoins (USDC/USDT),
/// Lightning, on-chain bitcoin, or something else (spelled out).
pub const PAYOUTS: [&str; 4] = ["stablecoin", "lightning", "onchain", "other"];

/// What the form posts.
#[derive(Debug, Default, Deserialize)]
pub struct Submission {
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub npub: String,
    /// Other contact (Signal, Telegram, X, ...).
    #[serde(default)]
    pub contact: String,
    #[serde(default)]
    pub mint_name: String,
    #[serde(default)]
    pub hashrate: String,
    /// Hashrate, as typed (a number), in `hashrate_unit`.
    #[serde(default)]
    pub hashrate_ths: String,
    /// "TH/s" (default), "PH/s" or "EH/s".
    #[serde(default)]
    pub hashrate_unit: String,
    #[serde(default)]
    pub horizon: String,
    /// One payout preference or a list of them.
    #[serde(default, deserialize_with = "one_or_many")]
    pub payout: Vec<String>,
    #[serde(default)]
    pub payout_other: String,
    #[serde(default)]
    pub note: String,
    /// Honeypot: a field hidden from people. Bots that fill it get a
    /// success reply and nothing is stored.
    #[serde(default)]
    pub website: String,
}

#[derive(Debug, PartialEq)]
pub enum Outcome {
    Added,
    /// Same email, npub or contact already on the list; nothing changed.
    Duplicate,
    /// Honeypot filled; reported as success, nothing stored.
    Ignored,
}

#[derive(Debug, PartialEq)]
pub enum Refusal {
    Invalid(String),
    RateLimited,
    Full,
    Storage,
}

struct Limiter {
    salt: [u8; 16],
    clients: HashMap<[u8; 16], Vec<Instant>>,
    global: Vec<Instant>,
}

impl Limiter {
    fn new() -> Self {
        Self {
            salt: rand::random(),
            clients: HashMap::new(),
            global: Vec::new(),
        }
    }

    fn key(&self, client: &str) -> [u8; 16] {
        let mut h = Sha256::new();
        h.update(self.salt);
        h.update(client.as_bytes());
        let d = h.finalize();
        let mut k = [0u8; 16];
        k.copy_from_slice(&d[..16]);
        k
    }

    /// Records an attempt; false when over a limit.
    fn allow(&mut self, client: &str, now: Instant) -> bool {
        let fresh = |t: &Instant| now.duration_since(*t) < WINDOW;
        self.global.retain(fresh);
        self.clients.retain(|_, v| {
            v.retain(fresh);
            !v.is_empty()
        });
        if self.global.len() >= GLOBAL {
            return false;
        }
        let key = self.key(client);
        let seen = self.clients.entry(key).or_default();
        if seen.len() >= PER_CLIENT {
            return false;
        }
        seen.push(now);
        self.global.push(now);
        true
    }
}

pub struct Waitlist {
    path: PathBuf,
    entries: Mutex<Vec<Entry>>,
    limiter: Mutex<Limiter>,
}

impl Waitlist {
    /// Opens (or starts) the list at `path`.
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut entries = Vec::new();
        match std::fs::File::open(&path) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Entry>(&line) {
                        Ok(mut e) => {
                            e.tih_for_sale = tih_for_sale(e.hashrate_ths, e.horizon.as_deref());
                            entries.push(e)
                        }
                        Err(e) => tracing::warn!(error = %e, "skipping bad waitlist line"),
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        Ok(Self {
            path,
            entries: Mutex::new(entries),
            limiter: Mutex::new(Limiter::new()),
        })
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.entries.lock().unwrap().clone()
    }

    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Validates and stores a submission from `client` (any stable string
    /// for the sender, e.g. its IP; only a salted hash is kept, in memory).
    pub fn submit(&self, client: &str, s: Submission) -> Result<Outcome, Refusal> {
        if !s.website.trim().is_empty() {
            return Ok(Outcome::Ignored);
        }
        let entry = validate(s)?;
        if !self.limiter.lock().unwrap().allow(client, Instant::now()) {
            return Err(Refusal::RateLimited);
        }
        let mut entries = self.entries.lock().unwrap();
        if entries.iter().any(|e| same_person(e, &entry)) {
            return Ok(Outcome::Duplicate);
        }
        if entries.len() >= MAX_ENTRIES {
            return Err(Refusal::Full);
        }
        let line = serde_json::to_string(&entry).map_err(|_| Refusal::Storage)?;
        let written = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{line}"));
        if let Err(e) = written {
            tracing::error!(error = %e, path = ?self.path, "write waitlist");
            return Err(Refusal::Storage);
        }
        entries.push(entry);
        tracing::info!(
            total = entries.len(),
            "miner joined the hosted-mint waitlist"
        );
        Ok(Outcome::Added)
    }

    /// Removes one entry (e.g. when a miner asks to be forgotten).
    pub fn remove(&self, id: &str) -> anyhow::Result<bool> {
        let mut entries = self.entries.lock().unwrap();
        let before = entries.len();
        let kept: Vec<Entry> = entries.iter().filter(|e| e.id != id).cloned().collect();
        if kept.len() == before {
            return Ok(false);
        }
        let mut body = String::new();
        for e in &kept {
            body.push_str(&serde_json::to_string(e)?);
            body.push('\n');
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &self.path)?;
        *entries = kept;
        Ok(true)
    }
}

fn clean(s: &str, max: usize, field: &str) -> Result<Option<String>, Refusal> {
    // Collapse control characters (newlines included) to spaces.
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let s = s.trim();
    if s.chars().count() > max {
        return Err(Refusal::Invalid(format!(
            "{field} is too long (at most {max} characters)"
        )));
    }
    Ok((!s.is_empty()).then(|| s.to_string()))
}

/// A mint name works as one DNS label: 2 to 32 of a-z, 0-9 and `-`, not
/// starting or ending with `-`.
pub fn valid_mint_name(n: &str) -> bool {
    (2..=32).contains(&n.len())
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !n.starts_with('-')
        && !n.ends_with('-')
}

/// The ways to reach `e`, lowercased, for spotting repeat sign-ups. An
/// older entry's `contact` may hold an email or npub, so every handle is
/// compared with every other.
fn handles(e: &Entry) -> Vec<String> {
    [
        e.email.as_deref(),
        e.npub.as_deref(),
        Some(e.contact.as_str()),
    ]
    .into_iter()
    .flatten()
    .filter(|h| !h.is_empty())
    .map(str::to_lowercase)
    .collect()
}

fn same_person(a: &Entry, b: &Entry) -> bool {
    let b = handles(b);
    handles(a).iter().any(|h| b.contains(h))
}

/// Loose email check: one `@`, something before it, a dot after it.
pub fn valid_email(e: &str) -> bool {
    match e.split_once('@') {
        Some((user, host)) => {
            !user.is_empty()
                && !host.contains('@')
                && host.contains('.')
                && !host.starts_with('.')
                && !host.ends_with('.')
                && !e.contains(char::is_whitespace)
        }
        None => false,
    }
}

/// A bech32 npub: `npub1` then 58 data characters (32-byte key + checksum).
pub fn valid_npub(n: &str) -> bool {
    const CHARSET: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    n.len() == 63 && n.starts_with("npub1") && n[5..].chars().all(|c| CHARSET.contains(c))
}

fn validate(s: Submission) -> Result<Entry, Refusal> {
    let email = clean(&s.email, MAX_CONTACT, "email")?;
    if let Some(e) = &email {
        if !valid_email(e) {
            return Err(Refusal::Invalid(
                "email: that doesn't look like an address".into(),
            ));
        }
    }
    let npub = clean(&s.npub, 63 + 8, "npub")?
        .map(|n| n.strip_prefix("nostr:").unwrap_or(&n).to_lowercase());
    if let Some(n) = &npub {
        if !valid_npub(n) {
            return Err(Refusal::Invalid(
                "npub: a Nostr public key starting npub1 (63 characters)".into(),
            ));
        }
    }
    let contact = clean(&s.contact, MAX_CONTACT, "contact")?.unwrap_or_default();
    if !contact.is_empty() && contact.chars().count() < 3 {
        return Err(Refusal::Invalid("other contact is too short".into()));
    }
    if email.is_none() && npub.is_none() && contact.is_empty() {
        return Err(Refusal::Invalid(
            "leave an email, an npub or another way to reach you".into(),
        ));
    }
    let mint_name = clean(&s.mint_name, 32, "mint name")?.map(|n| n.to_lowercase());
    if let Some(n) = &mint_name {
        if !valid_mint_name(n) {
            return Err(Refusal::Invalid(
                "mint name: 2 to 32 of a-z, 0-9 and -, not starting or ending with -".into(),
            ));
        }
    }
    let horizon = clean(&s.horizon, 16, "horizon")?;
    if let Some(h) = &horizon {
        if !HORIZONS.iter().any(|(k, _)| k == h) {
            let keys: Vec<&str> = HORIZONS.iter().map(|(k, _)| *k).collect();
            return Err(Refusal::Invalid(format!(
                "horizon: one of {}",
                keys.join(", ")
            )));
        }
    }
    let unit = match s.hashrate_unit.trim() {
        "" => HASHRATE_UNITS[0],
        u => *HASHRATE_UNITS
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(u))
            .ok_or_else(|| Refusal::Invalid("hashrate unit: TH/s, PH/s or EH/s".into()))?,
    };
    let hashrate_ths = match s.hashrate_ths.trim() {
        "" => None,
        t => match t.parse::<f64>().map(|v| v * unit.1) {
            Ok(v) if v.is_finite() && v > 0.0 && v <= MAX_THS => Some(v),
            _ => {
                return Err(Refusal::Invalid(
                    "hashrate: a number above 0 and at most 1,000 EH/s".into(),
                ))
            }
        },
    };
    let hashrate_unit = hashrate_ths.map(|_| unit.0.to_string());
    let tih_for_sale = tih_for_sale(hashrate_ths, horizon.as_deref());
    // Known keys only, each once, in the form's order.
    let picked: Vec<String> = s.payout.iter().map(|p| p.trim().to_string()).collect();
    if let Some(bad) = picked.iter().find(|p| !PAYOUTS.contains(&p.as_str())) {
        let bad: String = bad.chars().take(16).collect();
        return Err(Refusal::Invalid(format!(
            "payout {bad:?}: pick from {}",
            PAYOUTS.join(", ")
        )));
    }
    let payout: Vec<String> = PAYOUTS
        .iter()
        .filter(|k| picked.iter().any(|p| p == *k))
        .map(|k| k.to_string())
        .collect();
    let payout_other = if payout.iter().any(|p| p == "other") {
        Some(
            clean(&s.payout_other, MAX_HASHRATE, "payout")?
                .ok_or_else(|| Refusal::Invalid("say how you'd like to be paid".into()))?,
        )
    } else {
        None
    };
    Ok(Entry {
        id: hex::encode(rand::random::<[u8; 8]>()),
        at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        email,
        npub,
        contact,
        mint_name,
        hashrate: clean(&s.hashrate, MAX_HASHRATE, "hashrate")?,
        hashrate_ths,
        hashrate_unit,
        horizon,
        tih_for_sale,
        payout,
        payout_other,
        note: clean(&s.note, MAX_NOTE, "note")?,
    })
}

/// The sender as Caddy reports it. Caddy overwrites `X-Forwarded-For` with
/// the peer address for untrusted clients, and the mint port is not
/// published outside the compose network, so the first entry is the
/// client. Without the header everyone shares one bucket.
fn client_of(headers: &HeaderMap) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

async fn submit(State(w): State<Arc<Waitlist>>, headers: HeaderMap, body: Bytes) -> Response {
    let form: Submission = if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
    {
        match serde_json::from_slice(&body) {
            Ok(f) => f,
            Err(_) => return err(StatusCode::BAD_REQUEST, "send a JSON object"),
        }
    } else {
        return err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "send application/json");
    };
    match w.submit(&client_of(&headers), form) {
        Ok(Outcome::Added | Outcome::Ignored) => Json(json!({ "ok": true })).into_response(),
        Ok(Outcome::Duplicate) => Json(json!({ "ok": true, "already": true })).into_response(),
        Err(Refusal::Invalid(m)) => err(StatusCode::BAD_REQUEST, &m),
        Err(Refusal::RateLimited) => err(
            StatusCode::TOO_MANY_REQUESTS,
            "too many sign-ups from here, try again in an hour",
        ),
        Err(Refusal::Full) => err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the waitlist is full for now",
        ),
        Err(Refusal::Storage) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not save, try again later",
        ),
    }
}

/// Public routes on the mint's HTTP port: `/waitlist/` and `POST /waitlist/api`.
pub fn router(w: Arc<Waitlist>) -> Router {
    Router::new()
        .route(
            "/waitlist",
            get(|| async { axum::response::Redirect::permanent("/waitlist/") }),
        )
        .route("/waitlist/", get(|| async { Html(PAGE) }))
        .route(
            "/waitlist/api",
            post(submit).get(|| async { Json(json!({ "open": true })) }),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(w)
}

// ---- operator side (behind the operator password) -------------------------

async fn list(State(w): State<Arc<Waitlist>>) -> Json<serde_json::Value> {
    let mut e = w.entries();
    e.reverse();
    Json(json!({ "count": e.len(), "max": MAX_ENTRIES, "entries": e }))
}

fn csv_field(s: &str) -> String {
    // Quote everything; neutralise spreadsheet formulas.
    let s = if s.starts_with(['=', '+', '-', '@']) {
        format!("'{s}")
    } else {
        s.to_string()
    };
    format!("\"{}\"", s.replace('"', "\"\""))
}

async fn csv(State(w): State<Arc<Waitlist>>) -> Response {
    let mut out =
        String::from("id,at,email,npub,contact,mint_name,hashrate,hashrate_ths,hashrate_unit,horizon,tih_for_sale,payout,payout_other,note\n");
    for e in w.entries() {
        let row = [
            csv_field(&e.id),
            e.at.to_string(),
            csv_field(e.email.as_deref().unwrap_or("")),
            csv_field(e.npub.as_deref().unwrap_or("")),
            csv_field(&e.contact),
            csv_field(e.mint_name.as_deref().unwrap_or("")),
            csv_field(e.hashrate.as_deref().unwrap_or("")),
            e.hashrate_ths.map(|v| v.to_string()).unwrap_or_default(),
            csv_field(e.hashrate_unit.as_deref().unwrap_or("")),
            csv_field(e.horizon.as_deref().unwrap_or("")),
            e.tih_for_sale.map(|v| v.to_string()).unwrap_or_default(),
            csv_field(&e.payout.join("; ")),
            csv_field(e.payout_other.as_deref().unwrap_or("")),
            csv_field(e.note.as_deref().unwrap_or("")),
        ];
        out.push_str(&row.join(","));
        out.push('\n');
    }
    (
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=\"waitlist.csv\"",
            ),
        ],
        out,
    )
        .into_response()
}

async fn remove(State(w): State<Arc<Waitlist>>, Path(id): Path<String>) -> Response {
    match w.remove(&id) {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "no such entry"),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Operator routes, merged into the control API (served at
/// `/operator/waitlist` through Caddy's basic auth).
pub fn operator_router<S: Clone + Send + Sync + 'static>(w: Arc<Waitlist>) -> Router<S> {
    Router::new()
        .route("/waitlist", get(|| async { Html(OPERATOR_PAGE) }))
        .route("/api/waitlist", get(list))
        .route("/api/waitlist.csv", get(csv))
        .route("/api/waitlist/{id}", delete(remove))
        .with_state(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(contact: &str) -> Submission {
        Submission {
            contact: contact.into(),
            ..Default::default()
        }
    }

    #[test]
    fn stores_reloads_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("waitlist.jsonl");
        let w = Waitlist::open(path.clone()).unwrap();
        let s = Submission {
            contact: "  alice@example.com\n".into(),
            mint_name: "Alice".into(),
            hashrate: "2 Bitaxes".into(),
            ..Default::default()
        };
        assert_eq!(w.submit("1.2.3.4", s), Ok(Outcome::Added));
        assert_eq!(
            w.submit("1.2.3.4", sub("ALICE@example.com")),
            Ok(Outcome::Duplicate)
        );
        let w = Waitlist::open(path.clone()).unwrap();
        let e = w.entries();
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].contact, "alice@example.com");
        assert_eq!(e[0].mint_name.as_deref(), Some("alice"));
        assert!(w.remove(&e[0].id).unwrap());
        assert!(!w.remove(&e[0].id).unwrap());
        assert!(Waitlist::open(path).unwrap().is_empty());
    }

    #[test]
    fn validates_fields() {
        let w = Waitlist::open(tempfile::tempdir().unwrap().path().join("w")).unwrap();
        assert!(matches!(w.submit("c", sub("  ")), Err(Refusal::Invalid(_))));
        assert!(matches!(
            w.submit("c", sub(&"x".repeat(MAX_CONTACT + 1))),
            Err(Refusal::Invalid(_))
        ));
        let bad_name = Submission {
            contact: "npub1abc".into(),
            mint_name: "-bad".into(),
            ..Default::default()
        };
        assert!(matches!(w.submit("c", bad_name), Err(Refusal::Invalid(_))));
        assert!(valid_mint_name("alice-2"));
        assert!(!valid_mint_name("a"));
        assert!(!valid_mint_name("al.ice"));
    }

    #[test]
    fn payout_preference() {
        let dir = tempfile::tempdir().unwrap();
        let w = Waitlist::open(dir.path().join("w")).unwrap();
        let with = |payout: &str, other: &str| Submission {
            contact: format!("{payout}{other}@x.io"),
            payout: vec![payout.into()],
            payout_other: other.into(),
            ..Default::default()
        };
        assert_eq!(
            w.submit("a", with("lightning", "ignored")),
            Ok(Outcome::Added)
        );
        assert!(matches!(
            w.submit("a", with("paypal", "")),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            w.submit("a", with("other", " ")),
            Err(Refusal::Invalid(_))
        ));
        assert_eq!(w.submit("a", with("other", "Liquid")), Ok(Outcome::Added));
        let e = w.entries();
        assert_eq!(e[0].payout, vec!["lightning"]);
        assert_eq!(e[0].payout_other, None);
        assert_eq!(e[1].payout_other.as_deref(), Some("Liquid"));
    }

    #[test]
    fn payout_select_all_that_apply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w");
        // An entry from before multi-select stored one string.
        std::fs::write(
            &path,
            "{\"id\":\"old\",\"at\":1,\"contact\":\"old@x.io\",\"payout\":\"onchain\"}\n",
        )
        .unwrap();
        let w = Waitlist::open(path.clone()).unwrap();
        assert_eq!(w.entries()[0].payout, vec!["onchain"]);
        let s: Submission = serde_json::from_str(
            r#"{"contact":"m@x.io","payout":["other","lightning","stablecoin","lightning"],"payout_other":"Liquid"}"#,
        )
        .unwrap();
        assert_eq!(w.submit("a", s), Ok(Outcome::Added));
        let bad: Submission =
            serde_json::from_str(r#"{"contact":"n@x.io","payout":["lightning","paypal"]}"#)
                .unwrap();
        assert!(matches!(w.submit("a", bad), Err(Refusal::Invalid(_))));
        let e = Waitlist::open(path).unwrap().entries();
        // Form order, duplicates dropped.
        assert_eq!(e[1].payout, vec!["stablecoin", "lightning", "other"]);
        assert_eq!(e[1].payout_other.as_deref(), Some("Liquid"));
    }

    #[test]
    fn hashrate_units() {
        let dir = tempfile::tempdir().unwrap();
        let w = Waitlist::open(dir.path().join("w")).unwrap();
        let with = |v: &str, u: &str| Submission {
            contact: format!("{v}{u}@x.io"),
            hashrate_ths: v.into(),
            hashrate_unit: u.into(),
            ..Default::default()
        };
        assert_eq!(w.submit("a", with("1.5", "PH/s")), Ok(Outcome::Added));
        assert_eq!(w.submit("a", with("2", "eh/s")), Ok(Outcome::Added));
        assert_eq!(w.submit("a", with("90", "")), Ok(Outcome::Added));
        assert!(matches!(
            w.submit("b", with("1", "GH/s")),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            w.submit("b", with("1001", "EH/s")),
            Err(Refusal::Invalid(_))
        ));
        let e = w.entries();
        assert_eq!(e[0].hashrate_ths, Some(1_500.0));
        assert_eq!(e[0].hashrate_unit.as_deref(), Some("PH/s"));
        assert_eq!(e[1].hashrate_ths, Some(2_000_000.0));
        assert_eq!(e[1].hashrate_unit.as_deref(), Some("EH/s"));
        assert_eq!(e[2].hashrate_ths, Some(90.0));
        assert_eq!(e[2].hashrate_unit.as_deref(), Some("TH/s"));
    }

    #[test]
    fn horizon_sets_tih_for_sale() {
        let dir = tempfile::tempdir().unwrap();
        let w = Waitlist::open(dir.path().join("w")).unwrap();
        let with = |ths: &str, h: &str| Submission {
            contact: format!("{ths}{h}@x.io"),
            hashrate_ths: ths.into(),
            horizon: h.into(),
            ..Default::default()
        };
        // A Bitaxe (1.2 TH/s) for a month: 1.2 × 720 = 864 THH ≈ 2,828,892 TiH.
        assert_eq!(w.submit("a", with("1.2", "month")), Ok(Outcome::Added));
        // Horizon alone: nothing to multiply.
        assert_eq!(w.submit("a", with("", "year")), Ok(Outcome::Added));
        assert!(matches!(
            w.submit("a", with("1", "forever")),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            w.submit("a", with("-3", "week")),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            w.submit("a", with("lots", "week")),
            Err(Refusal::Invalid(_))
        ));
        // As short as one hour: 100 TH/s × 1 h = 100 THH ≈ 327,418 TiH.
        assert_eq!(w.submit("b", with("100", "hour")), Ok(Outcome::Added));
        let e = w.entries();
        assert_eq!(e[2].tih_for_sale, Some(327_418.0));
        assert_eq!(e[0].hashrate_ths, Some(1.2));
        assert_eq!(e[0].tih_for_sale, Some(2_828_892.0));
        assert_eq!(e[1].horizon.as_deref(), Some("year"));
        assert_eq!(e[1].tih_for_sale, None);
    }

    #[test]
    fn email_and_npub_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w");
        let w = Waitlist::open(path.clone()).unwrap();
        let npub = "npub1sg6plzptd64u62a878hep2kev88swjh3tw00gjsfl8f237lmu63q0uf63m";
        let s: Submission = serde_json::from_str(&format!(
            r#"{{"email":" Miner@Example.com ","npub":"nostr:{}"}}"#,
            npub.to_uppercase()
        ))
        .unwrap();
        assert_eq!(w.submit("a", s), Ok(Outcome::Added));
        let e = &w.entries()[0];
        assert_eq!(e.email.as_deref(), Some("Miner@Example.com"));
        assert_eq!(e.npub.as_deref(), Some(npub));
        assert_eq!(e.contact, "");
        // Same npub again, or the email typed in the old free-text field.
        let again = Submission {
            npub: npub.into(),
            ..Default::default()
        };
        assert_eq!(w.submit("a", again), Ok(Outcome::Duplicate));
        assert_eq!(
            w.submit("a", sub("miner@example.com")),
            Ok(Outcome::Duplicate)
        );
        let bad_email = Submission {
            email: "not-an-email".into(),
            ..Default::default()
        };
        assert!(matches!(w.submit("b", bad_email), Err(Refusal::Invalid(_))));
        let bad_npub = Submission {
            npub: "npub1short".into(),
            ..Default::default()
        };
        assert!(matches!(w.submit("b", bad_npub), Err(Refusal::Invalid(_))));
        assert!(matches!(
            w.submit("b", Submission::default()),
            Err(Refusal::Invalid(_))
        ));
        // Stored lines have no empty contact; old lines with only contact load.
        let line = std::fs::read_to_string(&path).unwrap();
        assert!(!line.contains("\"contact\""));
        assert!(valid_email("a@b.co") && !valid_email("a@b") && !valid_email("a b@c.io"));
    }

    #[test]
    fn honeypot_stores_nothing() {
        let w = Waitlist::open(tempfile::tempdir().unwrap().path().join("w")).unwrap();
        let s = Submission {
            contact: "bot@example.com".into(),
            website: "http://spam".into(),
            ..Default::default()
        };
        assert_eq!(w.submit("c", s), Ok(Outcome::Ignored));
        assert!(w.is_empty());
    }

    #[test]
    fn rate_limits_per_client() {
        let dir = tempfile::tempdir().unwrap();
        let w = Waitlist::open(dir.path().join("w")).unwrap();
        for i in 0..PER_CLIENT {
            assert_eq!(
                w.submit("9.9.9.9", sub(&format!("m{i}@x.io"))),
                Ok(Outcome::Added)
            );
        }
        assert_eq!(
            w.submit("9.9.9.9", sub("late@x.io")),
            Err(Refusal::RateLimited)
        );
        assert_eq!(w.submit("8.8.8.8", sub("other@x.io")), Ok(Outcome::Added));
    }

    #[test]
    fn csv_neutralises_formulas() {
        assert_eq!(csv_field("=cmd()"), "\"'=cmd()\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
    }
}
