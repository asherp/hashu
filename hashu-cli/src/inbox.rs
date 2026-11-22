//! Direct messages to a mint (NIP-17), live.
//!
//! Every mint this daemon serves (the operator's and each supplier's) can be
//! messaged at its Nostr key, the receipt key behind its profile. The
//! daemon polls its relays (`HASHU_NOSTR_RELAYS`) for gift wraps to those
//! keys, opens them with [`hashu_mint::inbox`], and the dashboards read and
//! answer them:
//!
//! - operator, on the control port (operator login): `GET /api/inbox`,
//!   `POST /api/inbox/reply` `{ "peer", "text" }`, `POST /api/inbox/read`
//!   `{ "peer" }`.
//! - supplier, on `<name>.<supplier domain>` with the dashboard key in
//!   `x-hashu-key`: the same three routes, for that supplier's mint only.
//!   Messages to a supplier's mint go to the supplier, not the operator.
//!
//! Replies are signed by the mint's key and sent to the mint's relays plus
//! the sender's NIP-17 inbox relays (kind 10050) when they publish some.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use hashu_mint::inbox::{Inbox, Message, Received};
use hashu_mint::reliability::RecordStore;
use nostr::{Filter, Kind, PublicKey, TagKind, Timestamp};
use serde::Deserialize;
use serde_json::json;

use crate::reliability::Reliability;

/// How often relays are asked for new gift wraps.
const POLL: Duration = Duration::from_secs(90);
/// Gift wraps are dated up to two days back (NIP-59): look a bit further.
const LOOKBACK_S: u64 = 2 * 24 * 3600 + 3600;
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// Most relays a reply goes to.
const MAX_REPLY_RELAYS: usize = 8;

/// The inbox of each mint, opened on first use.
#[derive(Default)]
pub struct Inboxes {
    boxes: Mutex<HashMap<String, Arc<Inbox>>>,
}

impl Inboxes {
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

    /// Poll the relays for messages to every mint, forever. Does nothing
    /// without relays.
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
        tracing::info!(relays = relays.len(), "mint direct messages (NIP-17) on");
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.poll(&client).await;
        }
    }

    async fn poll(&self, client: &nostr_sdk::Client) {
        let now = now_s();
        let mut by_key: HashMap<PublicKey, String> = HashMap::new();
        for m in self.mints() {
            if let Some(st) = self.store(&m) {
                by_key.insert(st.keys().public_key(), m);
            }
        }
        if by_key.is_empty() {
            return;
        }
        let filter = Filter::new()
            .kind(Kind::GiftWrap)
            .pubkeys(by_key.keys().copied())
            .since(Timestamp::from(now.saturating_sub(LOOKBACK_S)));
        let events = match client.fetch_events(filter, FETCH_TIMEOUT).await {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "inbox fetch failed");
                return;
            }
        };
        for wrap in events.iter() {
            let to = wrap
                .tags
                .public_keys()
                .find(|p| by_key.contains_key(p))
                .copied();
            let Some(mint) = to.and_then(|p| by_key.get(&p)) else {
                continue;
            };
            let Some((store, inbox)) = self.inbox(mint) else {
                continue;
            };
            match inbox.receive(store.keys(), wrap, now).await {
                Ok(Received::New(m)) => {
                    tracing::info!(mint = %store.url(), from = %m.peer, "new direct message")
                }
                Ok(Received::Ignored) => {}
                Err(e) => tracing::debug!(mint = %store.url(), error = %e, "unreadable gift wrap"),
            }
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
    Json(json!({ "ok": true })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use hashu_core::redemption::RedemptionLedger;
    use hashu_mint::reliability::PublishConfig;
    use nostr::{EventBuilder, Keys};
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
    }
}
