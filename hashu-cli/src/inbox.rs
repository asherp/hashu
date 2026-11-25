//! Direct messages to a mint (NIP-17), live.
//!
//! The operator's mint can be messaged at its Nostr key, the receipt key
//! behind its profile. The daemon keeps a standing subscription open on its
//! relays (`HASHU_NOSTR_RELAYS`) for gift wraps to that key, so a message
//! lands within a second or two of being published. The subscription asks
//! for the last two days (gift wraps are back-dated), is sent again by the
//! relay pool whenever a relay reconnects, and is renewed every
//! [`RENEW`] as a catch-up; wraps already seen are skipped. Each one is
//! opened with [`hashu_mint::inbox`], and the operator reads and answers
//! them:
//!
//! - operator, on the control port (operator login): `GET /api/inbox`,
//!   `POST /api/inbox/reply` `{ "peer", "text" }`, `POST /api/inbox/read`
//!   `{ "peer" }`, and `GET /api/inbox/events`, a server-sent event stream
//!   that says `changed` whenever the inbox does, so the page refetches
//!   at once instead of polling.
//! - `POST /api/inbox/send-hashes` (in [`crate::operator`]) mints hashes and
//!   sends them to a peer as a `cashuB` token in a direct message.
//!
//! Supplier mints are not read here: their messages go to the supplier's
//! own npub (from sign-up or the dashboard), which the mint lists first in
//! its NUT-06 contact and names in `/v1/hashu/reliability` `messages`. Hashu
//! never reads them.
//!
//! Replies are signed by the mint's key and sent to the mint's relays plus
//! the sender's NIP-17 inbox relays (kind 10050) when they publish some.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hashu_mint::inbox::{Inbox, Message, Received};
use hashu_mint::reliability::RecordStore;
use nostr::{Event, Filter, Kind, PublicKey, SubscriptionId, TagKind, Timestamp};
use nostr_sdk::RelayPoolNotification;
use serde::Deserialize;
use serde_json::json;

use crate::reliability::Reliability;
use hashu_proxy::reliability::OPERATOR_MINT;

/// How often the standing subscription is renewed (with a fresh `since`),
/// which also catches anything a relay dropped while it was down.
const RENEW: Duration = Duration::from_secs(15 * 60);
/// Gift wraps are dated up to two days back (NIP-59): look a bit further.
const LOOKBACK_S: u64 = 2 * 24 * 3600 + 3600;
/// Most relays a reply goes to.
const MAX_REPLY_RELAYS: usize = 8;

/// The inbox of each mint, opened on first use.
pub struct Inboxes {
    boxes: Mutex<HashMap<String, Arc<Inbox>>>,
    /// Bumped whenever any inbox changes: what `/api/inbox/events` waits on.
    changed: tokio::sync::watch::Sender<u64>,
}

impl Default for Inboxes {
    fn default() -> Self {
        Self {
            boxes: Mutex::default(),
            changed: tokio::sync::watch::channel(0).0,
        }
    }
}

impl Inboxes {
    fn bump(&self) {
        self.changed.send_modify(|n| *n = n.wrapping_add(1));
    }

    fn get(&self, mint: &str, store: &RecordStore) -> Arc<Inbox> {
        self.boxes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(mint.to_string())
            .or_insert_with(|| Arc::new(Inbox::open(&store.dir().join("inbox.json"))))
            .clone()
    }
}

impl Reliability {
    /// `mint`'s (`""` = operator) record store and inbox.
    pub fn inbox(&self, mint: &str) -> Option<(Arc<RecordStore>, Arc<Inbox>)> {
        let store = self.store(mint)?;
        let inbox = self.inboxes.get(mint, &store);
        Some((store, inbox))
    }

    /// Tell open operator pages that an inbox changed.
    pub(crate) fn inbox_changed(&self) {
        self.inboxes.bump();
    }

    /// Wait for the next inbox change.
    pub(crate) fn inbox_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.inboxes.changed.subscribe()
    }

    /// The mints that read messages at their own key, by key. Only the
    /// operator's: supplier mints point wallets at the supplier's own npub.
    fn inbox_keys(&self) -> HashMap<PublicKey, String> {
        let mut by_key = HashMap::new();
        if let Some(st) = self.store(OPERATOR_MINT) {
            by_key.insert(st.keys().public_key(), OPERATOR_MINT.to_string());
        }
        by_key
    }

    /// Listen on the relays for messages to every mint, forever. Does
    /// nothing without relays.
    pub async fn run_inbox(self: Arc<Self>) {
        let relays = self.configured_relays().to_vec();
        if relays.is_empty() {
            tracing::info!("no Nostr relays: mint direct messages are off");
            return;
        }
        let client = nostr_sdk::Client::default();
        for r in &relays {
            if let Err(e) = client.add_read_relay(r.as_str()).await {
                tracing::warn!(relay = %r, error = %e, "inbox relay");
            }
        }
        client.connect().await;
        let by_key = self.inbox_keys();
        if by_key.is_empty() {
            return;
        }
        tracing::info!(
            relays = relays.len(),
            "mint direct messages (NIP-17) on, live"
        );
        // Take notifications before subscribing so nothing slips between.
        let mut notes = client.notifications();
        let sub = SubscriptionId::new("hashu-inbox");
        let mut renew = tokio::time::interval(RENEW);
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = renew.tick() => {
                    // Same id: replaces the subscription on every relay with
                    // a fresh `since`, and the relays send what they hold.
                    let filter = Filter::new()
                        .kind(Kind::GiftWrap)
                        .pubkeys(by_key.keys().copied())
                        .since(Timestamp::from(now_s().saturating_sub(LOOKBACK_S)));
                    if let Err(e) = client.subscribe_with_id(sub.clone(), filter, None).await {
                        tracing::warn!(error = %e, "inbox subscription");
                    }
                }
                n = notes.recv() => match n {
                    Ok(RelayPoolNotification::Event { event, .. }) => {
                        if event.kind == Kind::GiftWrap {
                            self.take(&by_key, &event).await;
                        }
                    }
                    Ok(RelayPoolNotification::Shutdown) => return,
                    Ok(_) => {}
                    // Too many at once: the next renewal fetches them again.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(skipped = n, "inbox notifications lagged");
                        renew.reset_immediately();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                },
            }
        }
    }

    /// Open one gift wrap and keep it if it is a new message to a mint.
    async fn take(&self, by_key: &HashMap<PublicKey, String>, wrap: &Event) {
        let to = wrap
            .tags
            .public_keys()
            .find(|p| by_key.contains_key(p))
            .copied();
        let Some(mint) = to.and_then(|p| by_key.get(&p)) else {
            return;
        };
        let Some((store, inbox)) = self.inbox(mint) else {
            return;
        };
        match inbox.receive(store.keys(), wrap, now_s()).await {
            Ok(Received::New(m)) => {
                tracing::info!(mint = %store.url(), from = %m.peer, "new direct message");
                self.inbox_changed();
            }
            Ok(Received::Ignored) => {}
            Err(e) => tracing::debug!(mint = %store.url(), error = %e, "unreadable gift wrap"),
        }
    }

    /// Answer `peer` from `mint`'s key.
    pub async fn send_reply(&self, mint: &str, peer: &str, text: &str) -> Result<Message> {
        let (store, inbox) = self.inbox(mint).context("no such mint")?;
        let now = now_s();
        let (wrap, msg) = hashu_mint::inbox::reply(store.keys(), peer, text, now).await?;
        let to = hashu_mint::inbox::parse_peer(peer)?;
        let mut relays = store.configured_relays().to_vec();
        anyhow::ensure!(
            !relays.is_empty(),
            "no Nostr relays configured (HASHU_NOSTR_RELAYS)"
        );
        for r in inbox_relays(&relays, to).await {
            if !relays.contains(&r) && relays.len() < MAX_REPLY_RELAYS {
                relays.push(r);
            }
        }
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            hashu_mint::manifest::publish(&wrap, &relays),
        )
        .await
        .context("relays timed out")??;
        anyhow::ensure!(
            !out.succeeded.is_empty(),
            "no relay took the reply: {}",
            out.failed
                .iter()
                .map(|(r, e)| format!("{r}: {e}"))
                .collect::<Vec<_>>()
                .join("; ")
        );
        tracing::info!(mint = %store.url(), to = %msg.peer, relays = out.succeeded.len(), "sent direct message");
        inbox.record_sent(msg.clone(), now);
        self.inbox_changed();
        Ok(msg)
    }
}

/// The relays `peer` reads direct messages on (kind 10050), looked up on
/// `relays`.
async fn inbox_relays(relays: &[String], peer: PublicKey) -> Vec<String> {
    let client = nostr_sdk::Client::default();
    for r in relays {
        let _ = client.add_read_relay(r.as_str()).await;
    }
    client.connect().await;
    let filter = Filter::new().kind(Kind::InboxRelays).author(peer).limit(1);
    let found = client
        .fetch_events(filter, Duration::from_secs(5))
        .await
        .ok()
        .and_then(|ev| ev.first_owned());
    let _ = client.disconnect().await;
    found
        .map(|e| {
            e.tags
                .iter()
                .filter(|t| t.kind() == TagKind::Relay)
                .filter_map(|t| t.content().map(str::to_string))
                .filter(|u| u.starts_with("wss://"))
                .collect()
        })
        .unwrap_or_default()
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// Who may use a mint's inbox routes, beyond reaching them.
pub type AuthFn = Arc<dyn Fn(&HeaderMap) -> bool + Send + Sync>;

#[derive(Clone)]
struct Ctx {
    rel: Arc<Reliability>,
    mint: String,
    auth: Option<AuthFn>,
}

impl Ctx {
    fn check(&self, headers: &HeaderMap) -> Result<(Arc<RecordStore>, Arc<Inbox>), Response> {
        if let Some(a) = &self.auth {
            if !a(headers) {
                return Err((
                    StatusCode::UNAUTHORIZED,
                    Json(json!({ "error": "wrong dashboard key" })),
                )
                    .into_response());
            }
        }
        self.rel.inbox(&self.mint).ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "no such mint" })),
            )
                .into_response()
        })
    }
}

/// Inbox routes for `mint` (`""` = the operator's). `auth` = `None` when a
/// layer in front already checks (the operator login).
pub fn router<S: Clone + Send + Sync + 'static>(
    rel: Arc<Reliability>,
    mint: &str,
    auth: Option<AuthFn>,
) -> Router<S> {
    Router::new()
        .route("/api/inbox", get(list))
        .route("/api/inbox/reply", post(send))
        .route("/api/inbox/read", post(read))
        .route("/api/inbox/events", get(events))
        .with_state(Ctx {
            rel,
            mint: mint.to_string(),
            auth,
        })
}

async fn list(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    let (store, inbox) = match ctx.check(&headers) {
        Ok(x) => x,
        Err(r) => return r,
    };
    let conversations = inbox.conversations();
    Json(json!({
        "npub": store.npub(),
        "relays": store.configured_relays(),
        "on": !store.configured_relays().is_empty(),
        "unread": conversations.iter().map(|c| c.unread).sum::<usize>(),
        "conversations": conversations,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ReplyBody {
    peer: String,
    text: String,
}

async fn send(State(ctx): State<Ctx>, headers: HeaderMap, Json(b): Json<ReplyBody>) -> Response {
    if let Err(r) = ctx.check(&headers) {
        return r;
    }
    match ctx.rel.send_reply(&ctx.mint, &b.peer, &b.text).await {
        Ok(m) => Json(m).into_response(),
        Err(e) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ReadBody {
    peer: String,
}

async fn read(State(ctx): State<Ctx>, headers: HeaderMap, Json(b): Json<ReadBody>) -> Response {
    let (_, inbox) = match ctx.check(&headers) {
        Ok(x) => x,
        Err(r) => return r,
    };
    inbox.mark_read(&b.peer);
    ctx.rel.inbox_changed();
    Json(json!({ "ok": true })).into_response()
}

/// `GET /api/inbox/events`: `changed` each time the inbox does (a message
/// in or out, or one read), and once on connect so a page that reconnects
/// catches up. Comments keep proxies from closing the stream.
async fn events(State(ctx): State<Ctx>, headers: HeaderMap) -> Response {
    if let Err(r) = ctx.check(&headers) {
        return r;
    }
    let rx = ctx.rel.inbox_changes();
    let stream = futures::stream::unfold((rx, true), |(mut rx, first)| async move {
        if !first && rx.changed().await.is_err() {
            return None;
        }
        let n = *rx.borrow_and_update();
        let ev = SseEvent::default().event("changed").data(n.to_string());
        Some((Ok::<_, Infallible>(ev), (rx, false)))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use hashu_core::redemption::RedemptionLedger;
    use hashu_mint::reliability::PublishConfig;
    use nostr::{EventBuilder, Keys, ToBech32};
    use tower::ServiceExt;

    #[tokio::test]
    async fn inbox_routes_list_dms_and_check_the_dashboard_key() {
        let dir = tempfile::tempdir().unwrap();
        let rel = Reliability::open(
            dir.path(),
            "https://mint.example",
            PublishConfig::default(),
            Arc::new(RedemptionLedger::new()),
            Arc::new(|_: &str| 0.0),
            None,
        )
        .unwrap();
        let (store, inbox) = rel.inbox("").unwrap();
        let wallet = Keys::generate();
        let wrap = EventBuilder::private_msg(&wallet, store.keys().public_key(), "hello mint", [])
            .await
            .unwrap();
        inbox.receive(store.keys(), &wrap, now_s()).await.unwrap();

        let auth: AuthFn = Arc::new(|h: &HeaderMap| {
            h.get("x-hashu-key").and_then(|v| v.to_str().ok()) == Some("k")
        });
        let app: Router = router(rel.clone(), "", Some(auth));
        let get = |key: Option<&str>| {
            let mut b = Request::get("/api/inbox");
            if let Some(k) = key {
                b = b.header("x-hashu-key", k);
            }
            b.body(Body::empty()).unwrap()
        };
        let res = app.clone().oneshot(get(Some("nope"))).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = app.clone().oneshot(get(Some("k"))).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["unread"], 1);
        assert_eq!(v["on"], false);
        assert_eq!(v["conversations"][0]["messages"][0]["text"], "hello mint");
        assert_eq!(v["conversations"][0]["peer"], wallet.public_key().to_hex());

        // No relays: a reply is refused, not lost silently.
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/inbox/reply")
                    .header("x-hashu-key", "k")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "peer": wallet.public_key().to_hex(), "text": "hi" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        let res = app
            .clone()
            .oneshot(
                Request::post("/api/inbox/read")
                    .header("x-hashu-key", "k")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({ "peer": wallet.public_key().to_hex() }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(inbox.unread(), 0);

        // The live stream says "changed" on connect, then on each change.
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/inbox/events")
                    .header("x-hashu-key", "k")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
        let mut body = res.into_body().into_data_stream();
        use futures::StreamExt;
        let first = body.next().await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&first).contains("event: changed"));
        rel.inbox_changed();
        let next = tokio::time::timeout(Duration::from_secs(2), body.next())
            .await
            .expect("a change is pushed")
            .unwrap()
            .unwrap();
        assert!(String::from_utf8_lossy(&next).contains("event: changed"));
        let res = app
            .clone()
            .oneshot(
                Request::get("/api/inbox/events")
                    .header("x-hashu-key", "nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // The index names whom to message: the operator mint's own key; a
        // supplier mint, the supplier's npub or nobody.
        assert_eq!(rel.message_target(""), Some(store.npub()));
        let npub = Keys::generate().public_key().to_bech32().unwrap();
        rel.set_message_target("alice", Some(npub.clone()));
        assert_eq!(rel.message_target("alice"), Some(npub));
        rel.set_message_target("bob", None);
        assert_eq!(rel.message_target("bob"), None);
        let idx: Router = crate::reliability::router(rel.clone(), "");
        let res = idx
            .oneshot(
                Request::get("/v1/hashu/reliability")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["messages"]["npub"], store.npub());
        assert_eq!(v["messages"]["pubkey"], store.pubkey());
    }
}
