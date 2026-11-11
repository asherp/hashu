//! Hosted-mint waitlist for miners (`--waitlist`, off by default).
//!
//! Hosted per-miner mints (`<name>.<mint domain>`) are not open for sign-up
//! yet. Until they are, `GET /join/` on the mint's public HTTP port shows a
//! splash page where a miner leaves one way to reach them, and
//! `POST /join/api` appends it to `<data>/waitlist.jsonl`. The operator reads
//! (and deletes) entries behind the operator password, at
//! `/operator/waitlist`.
//!
//! Minimal data, on purpose: a contact handle (required), and optionally a
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
/// Largest request body accepted by `POST /join/api`.
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
    /// How to reach the miner: email, Nostr npub, Signal, Telegram, X, ...
    pub contact: String,
    /// Wished-for mint name, as in `<name>.<mint domain>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint_name: Option<String>,
    /// Rough hashrate, free text ("2 Bitaxes", "200 TH/s").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hashrate: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What the form posts.
#[derive(Debug, Default, Deserialize)]
pub struct Submission {
    #[serde(default)]
    pub contact: String,
    #[serde(default)]
    pub mint_name: String,
    #[serde(default)]
    pub hashrate: String,
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
    /// Same contact already on the list; nothing changed.
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
                        Ok(e) => entries.push(e),
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
        let key = entry.contact.to_lowercase();
        if entries.iter().any(|e| e.contact.to_lowercase() == key) {
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

fn validate(s: Submission) -> Result<Entry, Refusal> {
    let contact = clean(&s.contact, MAX_CONTACT, "contact")?
        .filter(|c| c.chars().count() >= 3)
        .ok_or_else(|| Refusal::Invalid("leave a way to reach you".into()))?;
    let mint_name = clean(&s.mint_name, 32, "mint name")?.map(|n| n.to_lowercase());
    if let Some(n) = &mint_name {
        if !valid_mint_name(n) {
            return Err(Refusal::Invalid(
                "mint name: 2 to 32 of a-z, 0-9 and -, not starting or ending with -".into(),
            ));
        }
    }
    Ok(Entry {
        id: hex::encode(rand::random::<[u8; 8]>()),
        at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default(),
        contact,
        mint_name,
        hashrate: clean(&s.hashrate, MAX_HASHRATE, "hashrate")?,
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

/// Public routes on the mint's HTTP port: `/join/` and `POST /join/api`.
pub fn router(w: Arc<Waitlist>) -> Router {
    Router::new()
        .route(
            "/join",
            get(|| async { axum::response::Redirect::permanent("/join/") }),
        )
        .route("/join/", get(|| async { Html(PAGE) }))
        .route(
            "/join/api",
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
    let mut out = String::from("id,at,contact,mint_name,hashrate,note\n");
    for e in w.entries() {
        let row = [
            csv_field(&e.id),
            e.at.to_string(),
            csv_field(&e.contact),
            csv_field(e.mint_name.as_deref().unwrap_or("")),
            csv_field(e.hashrate.as_deref().unwrap_or("")),
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
