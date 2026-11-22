//! Tips and Nostr zaps to the operator (`tips@<mint host>`).
//!
//! Hashu is open source; tips support its development. They are **not**
//! voucher sales: a tip issues no `TiH`, opens no mint quote and is never
//! credited to anyone. The invoice comes from the operator's phoenixd node
//! but is not one of cdk's quotes, so the voucher rail ignores it.
//!
//! - LUD-16: `GET /.well-known/lnurlp/tips` returns the LNURL-pay
//!   parameters ([`TipJar::pay_request`]), with NIP-57 `allowsNostr` and
//!   the mint's receipt key as `nostrPubkey`.
//! - LUD-06: `GET /lnurlp/tips/callback?amount=<msat>[&nostr=<zap request>]`
//!   returns a bolt11 invoice ([`TipJar::invoice`]). Its description hash
//!   commits to the zap request (NIP-57) or, for a plain tip, to the
//!   metadata (LUD-06).
//! - NIP-57: zap invoices are watched until paid or expired; once paid,
//!   the receipt key signs a **kind 9735** zap receipt and posts it to the
//!   relays the zap request names plus the mint's own ([`TipJar::tick`]).
//!   Open zaps are saved to disk so a restart doesn't lose a receipt.
//!
//! Supplier mints don't use this: their profiles point at the supplier's
//! own LNURL-pay endpoint, so tips there go to the miner.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use async_trait::async_trait;
use nostr::{Alphabet, Event, EventBuilder, JsonUtil, Keys, Kind, SingleLetterTag, TagKind};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The user part of the tip address.
pub const TIP_USER: &str = "tips";
/// Where the LNURL-pay parameters are served.
pub const LNURLP_PATH: &str = "/.well-known/lnurlp/tips";
/// Where invoices are requested.
pub const CALLBACK_PATH: &str = "/lnurlp/tips/callback";
/// Smallest tip, msat (phoenixd invoices whole sats).
pub const MIN_MSAT: u64 = 1_000;
/// Largest tip, msat (10M sat).
pub const MAX_MSAT: u64 = 10_000_000_000;
/// Invoice lifetime.
pub const EXPIRY_SECS: u64 = 3600;
/// Open zaps are still checked this long after they expire.
const GRACE_SECS: u64 = 300;

/// A Lightning node that invoices tips (phoenixd in production).
#[async_trait]
pub trait TipNode: Send + Sync {
    /// A bolt11 invoice for `amount_msat` whose description hash is
    /// `description_hash`. Returns the invoice and its payment hash.
    async fn invoice(
        &self,
        amount_msat: u64,
        description_hash: [u8; 32],
        expiry_secs: u64,
    ) -> Result<(String, [u8; 32])>;

    /// `None` while unpaid; `Some(preimage)` once paid (the preimage when
    /// the node gives it).
    async fn paid(&self, payment_hash: &[u8; 32]) -> Result<Option<Option<String>>>;
}

/// A zap invoice waiting to be paid.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct OpenZap {
    request: String,
    bolt11: String,
    expires_s: u64,
}

/// The operator's tip jar. See the module docs.
pub struct TipJar {
    node: Box<dyn TipNode>,
    keys: Keys,
    /// Mint URL, e.g. `https://mint.hashu.io`.
    url: String,
    host: String,
    relays: Vec<String>,
    open: Mutex<HashMap<String, OpenZap>>,
    path: PathBuf,
}

impl TipJar {
    /// `url` is the mint's public `https://` URL; `keys` sign zap receipts
    /// (the mint's receipt key, so they come from its profile's key);
    /// open zaps are kept in `dir/zaps.json`.
    pub fn open(
        node: Box<dyn TipNode>,
        keys: Keys,
        url: &str,
        relays: Vec<String>,
        dir: &std::path::Path,
    ) -> Result<Self> {
        let url = url.trim().trim_end_matches('/').to_string();
        let host = url
            .strip_prefix("https://")
            .and_then(|r| r.split(['/', '?', '#']).next())
            .filter(|h| !h.is_empty() && !h.contains(':'))
            .context("tips need the mint's public https:// URL")?
            .to_ascii_lowercase();
        std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
        let path = dir.join("zaps.json");
        let open = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Ok(Self {
            node,
            keys,
            url,
            host,
            relays,
            open: Mutex::new(open),
            path,
        })
    }

    /// `tips@<mint host>`.
    pub fn address(&self) -> String {
        format!("{TIP_USER}@{}", self.host)
    }

    /// LUD-06 metadata (the string whose hash a plain tip's invoice
    /// commits to).
    pub fn metadata(&self) -> String {
        json!([
            [
                "text/plain",
                format!(
                    "Tip Hashu ({}): supports open-source development. A tip is a gift; it buys no vouchers.",
                    self.host
                )
            ],
            ["text/identifier", self.address()],
        ])
        .to_string()
    }

    /// LUD-06 pay request, with NIP-57 zap support.
    pub fn pay_request(&self) -> Value {
        json!({
            "tag": "payRequest",
            "callback": format!("{}{CALLBACK_PATH}", self.url),
            "minSendable": MIN_MSAT,
            "maxSendable": MAX_MSAT,
            "metadata": self.metadata(),
            "allowsNostr": true,
            "nostrPubkey": self.keys.public_key().to_hex(),
        })
    }

    /// LUD-06 callback: an invoice for `amount_msat`, a zap when `nostr`
    /// holds a NIP-57 zap request. `Err` is the LNURL `reason`.
    pub async fn invoice(
        &self,
        amount_msat: u64,
        nostr: Option<&str>,
        now_s: u64,
    ) -> Result<Value, String> {
        if !(MIN_MSAT..=MAX_MSAT).contains(&amount_msat) {
            return Err(format!(
                "amount must be between {} and {} sats",
                MIN_MSAT / 1000,
                MAX_MSAT / 1000
            ));
        }
        if !amount_msat.is_multiple_of(1000) {
            return Err("amount must be whole sats".into());
        }
        let zap = match nostr.filter(|s| !s.trim().is_empty()) {
            Some(raw) => Some(validate_zap_request(raw, amount_msat)?),
            None => None,
        };
        // The invoice commits to exactly the string a receipt carries.
        let committed = match &zap {
            Some(ev) => ev.as_json(),
            None => self.metadata(),
        };
        let hash: [u8; 32] = Sha256::digest(committed.as_bytes()).into();
        let (bolt11, payment_hash) = self
            .node
            .invoice(amount_msat, hash, EXPIRY_SECS)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "tip invoice");
                "could not create an invoice right now".to_string()
            })?;
        if zap.is_some() {
            self.lock().insert(
                hex::encode(payment_hash),
                OpenZap {
                    request: committed,
                    bolt11: bolt11.clone(),
                    expires_s: now_s + EXPIRY_SECS,
                },
            );
            self.save();
        }
        tracing::info!(
            sats = amount_msat / 1000,
            zap = zap.is_some(),
            "tip invoice"
        );
        Ok(json!({ "pr": bolt11, "routes": [] }))
    }

    /// Check open zaps: post a receipt for each one paid, drop expired
    /// ones. Returns the receipts made.
    pub async fn tick(&self, now_s: u64) -> Vec<Event> {
        let open: Vec<(String, OpenZap)> = self
            .lock()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let mut made = vec![];
        let mut changed = false;
        for (hash_hex, z) in open {
            let Ok(hash) = <[u8; 32]>::try_from(hex::decode(&hash_hex).unwrap_or_default()) else {
                self.lock().remove(&hash_hex);
                changed = true;
                continue;
            };
            match self.node.paid(&hash).await {
                Ok(Some(preimage)) => {
                    match self.receipt(&z, preimage, now_s).await {
                        Ok(ev) => made.push(ev),
                        Err(e) => tracing::warn!(error = %e, "zap receipt"),
                    }
                    self.lock().remove(&hash_hex);
                    changed = true;
                }
                Ok(None) if z.expires_s + GRACE_SECS <= now_s => {
                    self.lock().remove(&hash_hex);
                    changed = true;
                }
                Ok(None) => {}
                Err(e) => tracing::debug!(error = %e, "tip invoice status"),
            }
        }
        if changed {
            self.save();
        }
        made
    }

    async fn receipt(&self, z: &OpenZap, preimage: Option<String>, now_s: u64) -> Result<Event> {
        let request = Event::from_json(&z.request)?;
        let event = EventBuilder::zap_receipt(z.bolt11.clone(), preimage, &request)
            .custom_created_at(nostr::Timestamp::from(now_s))
            .sign_with_keys(&self.keys)?;
        let mut relays = request_relays(&request);
        for r in &self.relays {
            if !relays.contains(r) {
                relays.push(r.clone());
            }
        }
        relays.truncate(12);
        if !relays.is_empty() {
            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                crate::manifest::publish(&event, &relays),
            )
            .await
            {
                Ok(Ok(out)) => {
                    tracing::info!(relays = out.succeeded.len(), event = %event.id.to_hex(), "posted zap receipt")
                }
                Ok(Err(e)) => tracing::warn!(error = %e, "could not post zap receipt"),
                Err(_) => tracing::warn!("posting zap receipt timed out"),
            }
        }
        Ok(event)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, OpenZap>> {
        self.open.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self) {
        let b = serde_json::to_vec_pretty(&*self.lock()).unwrap_or_default();
        let tmp = self.path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, &self.path)) {
            tracing::warn!(error = %e, "could not save open zaps");
        }
    }
}

fn single(c: Alphabet) -> TagKind<'static> {
    TagKind::SingleLetter(SingleLetterTag {
        character: c,
        uppercase: false,
    })
}

fn request_relays(ev: &Event) -> Vec<String> {
    ev.tags
        .iter()
        .find(|t| t.kind() == TagKind::Relays)
        .map(|t| {
            t.as_slice()
                .iter()
                .skip(1)
                .filter(|r| r.starts_with("wss://") || r.starts_with("ws://"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// NIP-57 appendix D checks on a zap request.
pub fn validate_zap_request(raw: &str, amount_msat: u64) -> Result<Event, String> {
    let ev = Event::from_json(raw).map_err(|_| "nostr: not a Nostr event".to_string())?;
    if ev.kind != Kind::ZapRequest {
        return Err("nostr: not a zap request (kind 9734)".into());
    }
    ev.verify()
        .map_err(|_| "nostr: bad signature".to_string())?;
    if ev.tags.is_empty() {
        return Err("nostr: zap request has no tags".into());
    }
    let count = |c| ev.tags.iter().filter(|t| t.kind() == single(c)).count();
    if count(Alphabet::P) != 1 {
        return Err("nostr: zap request needs exactly one p tag".into());
    }
    if count(Alphabet::E) > 1 {
        return Err("nostr: zap request has more than one e tag".into());
    }
    if let Some(a) = ev.tags.iter().find(|t| t.kind() == TagKind::Amount) {
        let asked = a.content().and_then(|s| s.parse::<u64>().ok());
        if asked != Some(amount_msat) {
            return Err("nostr: amount does not match the zap request".into());
        }
    }
    Ok(ev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::Tag;
    use std::sync::Arc;

    #[derive(Default)]
    struct FakeNode {
        hashes: Mutex<Vec<[u8; 32]>>,
        paid: Mutex<bool>,
    }

    #[async_trait]
    impl TipNode for Arc<FakeNode> {
        async fn invoice(&self, msat: u64, dh: [u8; 32], _: u64) -> Result<(String, [u8; 32])> {
            self.hashes.lock().unwrap().push(dh);
            Ok((
                format!("lnbc{msat}n1fake{}", hex::encode(&dh[..4])),
                [7; 32],
            ))
        }
        async fn paid(&self, _: &[u8; 32]) -> Result<Option<Option<String>>> {
            Ok(self.paid.lock().unwrap().then(|| Some("00".repeat(32))))
        }
    }

    fn jar(dir: &std::path::Path) -> (TipJar, Arc<FakeNode>) {
        let node = Arc::new(FakeNode::default());
        let jar = TipJar::open(
            Box::new(node.clone()),
            Keys::generate(),
            "https://mint.example/",
            vec![],
            dir,
        )
        .unwrap();
        (jar, node)
    }

    fn zap_request(amount: Option<u64>) -> String {
        let keys = Keys::generate();
        let mut tags = vec![
            Tag::parse(["p", &Keys::generate().public_key().to_hex()]).unwrap(),
            Tag::parse(["relays", "wss://relay.example"]).unwrap(),
        ];
        if let Some(a) = amount {
            tags.push(Tag::parse(["amount", &a.to_string()]).unwrap());
        }
        EventBuilder::new(Kind::ZapRequest, "thanks!")
            .tags(tags)
            .sign_with_keys(&keys)
            .unwrap()
            .as_json()
    }

    #[test]
    fn pay_request_allows_nostr() {
        let dir = tempfile::tempdir().unwrap();
        let (jar, _) = jar(dir.path());
        assert_eq!(jar.address(), "tips@mint.example");
        let p = jar.pay_request();
        assert_eq!(p["tag"], "payRequest");
        assert_eq!(p["callback"], "https://mint.example/lnurlp/tips/callback");
        assert_eq!(p["allowsNostr"], true);
        assert_eq!(p["nostrPubkey"], jar.keys.public_key().to_hex());
        let meta: Value = serde_json::from_str(p["metadata"].as_str().unwrap()).unwrap();
        assert_eq!(meta[1][1], "tips@mint.example");
    }

    #[tokio::test]
    async fn plain_tip_commits_to_metadata_and_is_not_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let (jar, node) = jar(dir.path());
        let r = jar.invoice(21_000, None, 100).await.unwrap();
        assert!(r["pr"].as_str().unwrap().starts_with("lnbc21000n1"));
        let want: [u8; 32] = Sha256::digest(jar.metadata().as_bytes()).into();
        assert_eq!(node.hashes.lock().unwrap()[0], want);
        assert!(jar.lock().is_empty());
        assert!(jar.invoice(1_500, None, 100).await.is_err(), "whole sats");
        assert!(jar.invoice(0, None, 100).await.is_err());
    }

    #[tokio::test]
    async fn zap_is_receipted_once_paid() {
        let dir = tempfile::tempdir().unwrap();
        let (jar, node) = jar(dir.path());
        let req = zap_request(Some(21_000));
        jar.invoice(21_000, Some(&req), 100).await.unwrap();
        let committed = Event::from_json(&req).unwrap().as_json();
        let want: [u8; 32] = Sha256::digest(committed.as_bytes()).into();
        assert_eq!(node.hashes.lock().unwrap()[0], want);
        assert!(jar.tick(200).await.is_empty(), "unpaid");
        // Survives a restart.
        let again = TipJar::open(
            Box::new(node.clone()),
            jar.keys.clone(),
            "https://mint.example",
            vec![],
            dir.path(),
        )
        .unwrap();
        *node.paid.lock().unwrap() = true;
        let made = again.tick(300).await;
        assert_eq!(made.len(), 1);
        let r = &made[0];
        r.verify().unwrap();
        assert_eq!(r.kind, Kind::ZapReceipt);
        assert_eq!(r.pubkey, jar.keys.public_key());
        let desc = r
            .tags
            .iter()
            .find(|t| t.kind() == TagKind::Description)
            .and_then(|t| t.content())
            .unwrap();
        let got: [u8; 32] = Sha256::digest(desc.as_bytes()).into();
        assert_eq!(got, want, "description matches the invoice's hash");
        assert!(r.tags.iter().any(|t| t.kind() == single(Alphabet::P)));
        assert!(again.tick(400).await.is_empty(), "receipted once");
    }

    #[tokio::test]
    async fn expired_zaps_are_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let (jar, _) = jar(dir.path());
        jar.invoice(21_000, Some(&zap_request(None)), 100)
            .await
            .unwrap();
        jar.tick(100 + EXPIRY_SECS + GRACE_SECS).await;
        assert!(jar.lock().is_empty());
    }

    #[test]
    fn bad_zap_requests_are_refused() {
        assert!(validate_zap_request("{}", 1000).is_err());
        assert!(validate_zap_request(&zap_request(Some(5_000)), 21_000)
            .unwrap_err()
            .contains("amount"));
        let note = EventBuilder::text_note("hi")
            .sign_with_keys(&Keys::generate())
            .unwrap()
            .as_json();
        assert!(validate_zap_request(&note, 1000).is_err());
        assert!(validate_zap_request(&zap_request(Some(21_000)), 21_000).is_ok());
    }

    #[test]
    fn needs_an_https_url() {
        let dir = tempfile::tempdir().unwrap();
        let node = Arc::new(FakeNode::default());
        assert!(TipJar::open(
            Box::new(node),
            Keys::generate(),
            "http://x",
            vec![],
            dir.path()
        )
        .is_err());
    }
}
