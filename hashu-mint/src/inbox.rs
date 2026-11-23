//! Private messages to a mint (NIP-17).
//!
//! A mint's Nostr identity is its receipt key ([`crate::reliability`]): the
//! key that signs its profile, announcement and hourly record. Wallets DM
//! that key with NIP-17 gift wraps (kind 1059 → seal kind 13 → kind 14).
//! The mint unwraps them with the same key, keeps them in `inbox.json` next
//! to its record, and the operator (or the supplier, on a supplier mint)
//! reads and answers them from the dashboard. Replies are kind 14 messages
//! signed by the mint's key, gift wrapped to the sender.
//!
//! Anyone can write to a public key, so the inbox is capped: messages are
//! cut to [`MAX_TEXT`] characters, each sender keeps its newest
//! [`MAX_PER_PEER`] and the inbox its newest [`MAX_MESSAGES`].

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use nostr::nips::nip59::UnwrappedGift;
use nostr::{Event, EventBuilder, Keys, Kind, PublicKey, ToBech32};
use serde::{Deserialize, Serialize};

/// Longest message kept, in characters.
pub const MAX_TEXT: usize = 4000;
/// Messages kept per sender.
pub const MAX_PER_PEER: usize = 200;
/// Messages kept in all.
pub const MAX_MESSAGES: usize = 2000;
/// Gift wraps are dated up to two days back (NIP-59), so remember the ids
/// of the ones seen a little longer than that.
const SEEN_FOR_S: u64 = 3 * 24 * 3600;

/// One message, in or out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Kind 14 id (incoming) or gift wrap id (outgoing).
    pub id: String,
    /// The other side, hex pubkey.
    pub peer: String,
    /// Sent by the mint.
    pub outgoing: bool,
    pub text: String,
    /// Unix seconds, as the author dated it (never in the future).
    pub created_at: u64,
}

/// Messages with one sender, oldest first.
#[derive(Clone, Debug, Serialize)]
pub struct Conversation {
    pub peer: String,
    pub npub: String,
    pub unread: usize,
    pub last_at: u64,
    pub messages: Vec<Message>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    messages: Vec<Message>,
    /// Peer → newest `created_at` the dashboard has shown as read.
    #[serde(default)]
    read: HashMap<String, u64>,
    /// Gift wrap id → when it was first seen.
    #[serde(default)]
    seen: HashMap<String, u64>,
}

/// One mint's inbox, saved at `path`.
pub struct Inbox {
    path: PathBuf,
    state: Mutex<State>,
}

/// What [`Inbox::receive`] did with a gift wrap.
#[derive(Debug, PartialEq)]
pub enum Received {
    New(Message),
    /// Seen before, our own copy, or not a direct message.
    Ignored,
}

impl Inbox {
    /// Open `path` (empty if missing or unreadable).
    pub fn open(path: &Path) -> Self {
        let state = std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            path: path.to_path_buf(),
            state: Mutex::new(state),
        }
    }

    /// Unwrap `wrap` with the mint's `keys` and keep it if it is a new
    /// NIP-17 direct message to the mint. Errors are wraps that are not for
    /// this key or do not verify.
    pub async fn receive(&self, keys: &Keys, wrap: &Event, now_s: u64) -> Result<Received> {
        let wrap_id = wrap.id.to_hex();
        if self.lock().seen.contains_key(&wrap_id) {
            return Ok(Received::Ignored);
        }
        let unwrapped = UnwrappedGift::from_gift_wrap(keys, wrap).await;
        // Remember it either way: a wrap that does not open never will.
        self.lock().seen.insert(wrap_id, now_s);
        let UnwrappedGift { sender, mut rumor } = unwrapped.context("unwrap gift wrap")?;
        // NIP-17: the seal's signer must be the message's author.
        if rumor.kind != Kind::PrivateDirectMessage
            || rumor.pubkey != sender
            || sender == keys.public_key()
        {
            self.save();
            return Ok(Received::Ignored);
        }
        let id = rumor.id().to_hex();
        let mut text = rumor.content.clone();
        if text.chars().count() > MAX_TEXT {
            text = text.chars().take(MAX_TEXT).collect();
        }
        let msg = Message {
            id,
            peer: sender.to_hex(),
            outgoing: false,
            text,
            created_at: rumor.created_at.as_secs().min(now_s),
        };
        let added = self.add(msg.clone(), now_s);
        Ok(if added {
            Received::New(msg)
        } else {
            Received::Ignored
        })
    }

    /// Keep a reply the mint sent.
    pub fn record_sent(&self, msg: Message, now_s: u64) {
        let peer = msg.peer.clone();
        let at = msg.created_at;
        if self.add(msg, now_s) {
            // Answering a conversation reads it.
            let mut s = self.lock();
            let r = s.read.entry(peer).or_default();
            *r = (*r).max(at);
            drop(s);
            self.save();
        }
    }

    /// Every conversation, newest activity first.
    pub fn conversations(&self) -> Vec<Conversation> {
        let s = self.lock();
        let mut by_peer: BTreeMap<&str, Vec<&Message>> = BTreeMap::new();
        for m in &s.messages {
            by_peer.entry(m.peer.as_str()).or_default().push(m);
        }
        let mut out: Vec<Conversation> = by_peer
            .into_iter()
            .map(|(peer, mut msgs)| {
                msgs.sort_by_key(|m| (m.created_at, m.outgoing));
                let read = s.read.get(peer).copied().unwrap_or(0);
                Conversation {
                    peer: peer.to_string(),
                    npub: PublicKey::from_hex(peer)
                        .ok()
                        .and_then(|p| p.to_bech32().ok())
                        .unwrap_or_default(),
                    unread: msgs
                        .iter()
                        .filter(|m| !m.outgoing && m.created_at > read)
                        .count(),
                    last_at: msgs.last().map(|m| m.created_at).unwrap_or(0),
                    messages: msgs.into_iter().cloned().collect(),
                }
            })
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.last_at));
        out
    }

    /// Unread messages in all.
    pub fn unread(&self) -> usize {
        self.conversations().iter().map(|c| c.unread).sum()
    }

    /// Mark everything from `peer` read.
    pub fn mark_read(&self, peer: &str) {
        let mut s = self.lock();
        let newest = s
            .messages
            .iter()
            .filter(|m| m.peer == peer)
            .map(|m| m.created_at)
            .max();
        if let Some(n) = newest {
            s.read.insert(peer.to_string(), n);
        }
        drop(s);
        self.save();
    }

    /// Add `msg` unless its id is already there; apply the caps; save.
    fn add(&self, msg: Message, now_s: u64) -> bool {
        let mut s = self.lock();
        if s.messages.iter().any(|m| m.id == msg.id) {
            return false;
        }
        let peer = msg.peer.clone();
        s.messages.push(msg);
        s.messages.sort_by_key(|m| m.created_at);
        let from_peer = s.messages.iter().filter(|m| m.peer == peer).count();
        if from_peer > MAX_PER_PEER {
            let mut drop_n = from_peer - MAX_PER_PEER;
            s.messages.retain(|m| {
                if drop_n > 0 && m.peer == peer {
                    drop_n -= 1;
                    false
                } else {
                    true
                }
            });
        }
        if s.messages.len() > MAX_MESSAGES {
            let extra = s.messages.len() - MAX_MESSAGES;
            s.messages.drain(..extra);
        }
        s.seen.retain(|_, at| *at + SEEN_FOR_S > now_s);
        drop(s);
        self.save();
        true
    }

    fn save(&self) {
        let bytes = match serde_json::to_vec(&*self.lock()) {
            Ok(b) => b,
            Err(_) => return,
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A reply from the mint (`keys`) to `peer`: a kind 14 message, sealed and
/// gift wrapped to `peer`. Returns the wrap to publish and the message to
/// keep.
pub async fn reply(keys: &Keys, peer: &str, text: &str, now_s: u64) -> Result<(Event, Message)> {
    let text = text.trim();
    anyhow::ensure!(!text.is_empty(), "empty message");
    anyhow::ensure!(
        text.chars().count() <= MAX_TEXT,
        "message longer than {MAX_TEXT} characters"
    );
    let to = parse_peer(peer)?;
    let wrap = EventBuilder::private_msg(keys, to, text, [])
        .await
        .context("gift wrap reply")?;
    let msg = Message {
        id: wrap.id.to_hex(),
        peer: to.to_hex(),
        outgoing: true,
        text: text.to_string(),
        created_at: now_s,
    };
    Ok((wrap, msg))
}

/// A peer as hex or npub.
pub fn parse_peer(peer: &str) -> Result<PublicKey> {
    PublicKey::parse(peer.trim()).context("not a Nostr public key")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn dm(from: &Keys, to: &Keys, text: &str) -> Event {
        EventBuilder::private_msg(from, to.public_key(), text, [])
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn keeps_new_dms_once_and_replies_unwrap_for_the_sender() {
        let dir = tempfile::tempdir().unwrap();
        let mint = Keys::generate();
        let wallet = Keys::generate();
        let inbox = Inbox::open(&dir.path().join("inbox.json"));
        let wrap = dm(&wallet, &mint, "Do you sell PHd?").await;

        let got = inbox.receive(&mint, &wrap, 2_000_000_000).await.unwrap();
        let Received::New(m) = got else {
            panic!("expected a new message")
        };
        assert_eq!(m.text, "Do you sell PHd?");
        assert_eq!(m.peer, wallet.public_key().to_hex());
        assert!(!m.outgoing);
        assert_eq!(
            inbox.receive(&mint, &wrap, 2_000_000_000).await.unwrap(),
            Received::Ignored,
            "same wrap twice"
        );
        assert_eq!(inbox.unread(), 1);

        // Reply: the wallet can open it and it is from the mint.
        let (out, sent) = reply(
            &mint,
            &wallet.public_key().to_bech32().unwrap(),
            "Yes",
            2_000_000_100,
        )
        .await
        .unwrap();
        let back = UnwrappedGift::from_gift_wrap(&wallet, &out).await.unwrap();
        assert_eq!(back.sender, mint.public_key());
        assert_eq!(back.rumor.content, "Yes");
        assert_eq!(back.rumor.kind, Kind::PrivateDirectMessage);
        inbox.record_sent(sent, 2_000_000_100);
        assert_eq!(inbox.unread(), 0, "answering reads the conversation");

        // Survives a reopen.
        let again = Inbox::open(&dir.path().join("inbox.json"));
        let c = again.conversations();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].messages.len(), 2);
        assert!(c[0].messages[1].outgoing);
        assert!(c[0].npub.starts_with("npub1"));
    }

    #[tokio::test]
    async fn refuses_wraps_for_other_keys_and_caps_each_sender() {
        let dir = tempfile::tempdir().unwrap();
        let mint = Keys::generate();
        let other = Keys::generate();
        let wallet = Keys::generate();
        let inbox = Inbox::open(&dir.path().join("inbox.json"));
        let wrap = dm(&wallet, &other, "hi").await;
        assert!(inbox.receive(&mint, &wrap, 2_000_000_000).await.is_err());

        for i in 0..(MAX_PER_PEER + 5) {
            inbox.add(
                Message {
                    id: format!("{i}"),
                    peer: "aa".into(),
                    outgoing: false,
                    text: "x".into(),
                    created_at: i as u64,
                },
                2_000_000_000,
            );
        }
        let c = inbox.conversations();
        assert_eq!(c[0].messages.len(), MAX_PER_PEER);
        assert_eq!(c[0].messages[0].created_at, 5, "oldest dropped first");
    }

    #[tokio::test]
    async fn cuts_long_messages() {
        let dir = tempfile::tempdir().unwrap();
        let mint = Keys::generate();
        let wallet = Keys::generate();
        let inbox = Inbox::open(&dir.path().join("inbox.json"));
        let long = "a".repeat(MAX_TEXT + 50);
        let wrap = dm(&wallet, &mint, &long).await;
        let Received::New(m) = inbox.receive(&mint, &wrap, 2_000_000_000).await.unwrap() else {
            panic!()
        };
        assert_eq!(m.text.len(), MAX_TEXT);
        assert!(reply(&mint, &wallet.public_key().to_hex(), &long, 0)
            .await
            .is_err());
    }
}
