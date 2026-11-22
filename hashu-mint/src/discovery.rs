//! NIP-87 mint discovery (issue #116). ARCHITECTURE.md §4.7.5.
//!
//! Each mint announces itself on Nostr with an addressable **kind 38172**
//! Cashu mint event, signed with the same receipt key as its hourly
//! reliability record (kind 8333), so the listing and the record belong to
//! one Nostr identity. The operator's key also publishes a **kind 38000**
//! recommendation for each supplier mint it hosts.
//!
//! Tags follow NIP-87: `d` = the mint's NUT-06 pubkey (or its URL when it
//! has none), `u` = mint URL, `nuts` = supported NUT numbers, `n` =
//! network. Hashu adds one `unit` tag per unit the mint issues (`tih`),
//! `t=hashu` and a NIP-31 `alt`. `content` is kind-0 style metadata.
//!
//! The same key also publishes a **kind 0** profile (NIP-01 metadata,
//! NIP-24 extras) so Nostr clients show the mint by name with a picture,
//! a link and a NIP-05 address (`_@<mint host>`, served by the mint at
//! `/.well-known/nostr.json`). See [`Announcement::profile_content`].

use cdk::nuts::MintInfo;
use nostr::Tag;
use serde::{Deserialize, Serialize};

/// NIP-87 Cashu mint announcement.
pub const ANNOUNCE_KIND: u16 = 38172;
/// NIP-87 mint recommendation.
pub const RECOMMEND_KIND: u16 = 38000;
/// Hashtag on every announcement.
pub const ANNOUNCE_TAG: &str = "hashu";
/// NIP-01 profile metadata.
pub const PROFILE_KIND: u16 = 0;
/// Path of the default profile picture on the mint, used when NUT-06 has
/// no `icon_url`. A PNG, since several clients don't show SVG avatars.
pub const AVATAR_PATH: &str = "/nostr/avatar.png";

/// Where a mint's profile takes tips (`lud16`, or `lud06` for an LNURL
/// that is not a Lightning address), and the line `about` adds for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tip {
    /// `lud16` or `lud06`.
    pub field: &'static str,
    pub value: String,
    pub note: String,
}

impl Tip {
    /// The operator's tip address: tips support Hashu's development.
    pub fn operator(endpoint: &crate::lnaddress::PayEndpoint) -> Self {
        Self::new(
            endpoint,
            "Zaps and tips support Hashu's open-source development. A tip is a gift; it buys no vouchers."
                .into(),
        )
    }

    /// A supplier mint: tips go to the miner's own LNURL-pay endpoint.
    pub fn supplier(name: &str, endpoint: &crate::lnaddress::PayEndpoint) -> Self {
        Self::new(
            endpoint,
            format!("Tips go to {name}, the miner behind this mint. A tip is a gift; it buys no vouchers."),
        )
    }

    fn new(endpoint: &crate::lnaddress::PayEndpoint, note: String) -> Self {
        use crate::lnaddress::PayEndpoint;
        match endpoint {
            PayEndpoint::Address(a) => Self {
                field: "lud16",
                value: a.to_string(),
                note,
            },
            PayEndpoint::Url(u) => Self {
                field: "lud06",
                value: lnurl_bech32(u),
                note,
            },
        }
    }
}

/// LUD-01: a URL as a bech32 `lnurl1…` code.
fn lnurl_bech32(url: &str) -> String {
    let hrp = bech32::Hrp::parse("lnurl").expect("valid hrp");
    bech32::encode::<bech32::Bech32>(hrp, url.as_bytes()).unwrap_or_else(|_| url.to_string())
}

/// What a mint announces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Announcement {
    /// NUT-06 `pubkey`, hex.
    pub mint_pubkey: Option<String>,
    pub url: String,
    /// Supported NUTs, ascending.
    pub nuts: Vec<u16>,
    /// `mainnet`, `testnet`, `signet` or `regtest`.
    pub network: String,
    /// Units the mint issues, lowercase (`tih`).
    pub units: Vec<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub icon_url: Option<String>,
}

impl Announcement {
    /// From the mint's NUT-06 info as served at `/v1/info`.
    pub fn from_info(info: &MintInfo, url: &str, network: &str) -> Self {
        let nuts = serde_json::to_value(&info.nuts).unwrap_or_default();
        Self {
            mint_pubkey: info.pubkey.map(|p| p.to_hex()),
            url: url.trim_end_matches('/').to_string(),
            nuts: supported_nuts(&nuts),
            network: network.to_string(),
            units: issued_units(&nuts),
            name: info.name.clone().filter(|s| !s.is_empty()),
            description: info.description.clone().filter(|s| !s.is_empty()),
            icon_url: info.icon_url.clone().filter(|s| !s.is_empty()),
        }
    }

    /// The `d` identifier: the mint pubkey, so clients can look the mint
    /// up by the pubkey in `/v1/info`.
    pub fn d(&self) -> String {
        self.mint_pubkey.clone().unwrap_or_else(|| self.url.clone())
    }

    /// The kind 38172 tags.
    pub fn tags(&self) -> anyhow::Result<Vec<Tag>> {
        let nuts = self
            .nuts
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let mut tags = vec![
            Tag::parse(["d", self.d().as_str()])?,
            Tag::parse(["u", self.url.as_str()])?,
            Tag::parse(["nuts", nuts.as_str()])?,
            Tag::parse(["n", self.network.as_str()])?,
        ];
        for u in &self.units {
            tags.push(Tag::parse(["unit", u.as_str()])?);
        }
        tags.push(Tag::hashtag(ANNOUNCE_TAG));
        tags.push(Tag::parse([
            "alt",
            format!("Cashu mint: {} (Hashu hashrate vouchers)", self.url).as_str(),
        ])?);
        Ok(tags)
    }

    /// Kind-0 style metadata for `content`.
    pub fn content(&self) -> String {
        let mut m = serde_json::Map::new();
        if let Some(n) = &self.name {
            m.insert("name".into(), n.clone().into());
        }
        if let Some(d) = &self.description {
            m.insert("about".into(), d.clone().into());
        }
        if let Some(p) = &self.icon_url {
            m.insert("picture".into(), p.clone().into());
        }
        m.insert("website".into(), self.url.clone().into());
        serde_json::Value::Object(m).to_string()
    }

    /// The NIP-05 address `_@<host>` (clients show it as just the host),
    /// or `None` when the URL has no host. The mint answers it at
    /// `https://<host>/.well-known/nostr.json?name=_`.
    pub fn nip05(&self) -> Option<String> {
        let host = self
            .url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.url)
            .split(['/', '?', '#'])
            .next()?
            .to_ascii_lowercase();
        // NIP-05 is fetched over HTTPS on port 443: a host with a port or
        // a plain-HTTP mint can't serve it.
        if host.is_empty() || host.contains(':') || !self.url.starts_with("https://") {
            return None;
        }
        Some(format!("_@{host}"))
    }

    /// Profile picture: the mint's NUT-06 icon, else Hashu's mark served
    /// by the mint itself.
    pub fn picture(&self) -> String {
        self.icon_url
            .clone()
            .unwrap_or_else(|| format!("{}{AVATAR_PATH}", self.url))
    }

    /// Kind 0 `content`: the mint's profile as Nostr clients show it.
    ///
    /// - `name`, `about`, `picture`, `website` from NUT-06 (`about` also
    ///   points at the hourly delivery record).
    /// - `nip05` = `_@<mint host>`, so the profile shows a verified domain.
    /// - `bot: true` (NIP-24): every post from this key is automated.
    /// - `tip`: `lud16`/`lud06` where tips go (the operator's tip jar, or a
    ///   supplier's own LNURL), with a line in `about` saying a tip buys no
    ///   vouchers. Vouchers are bought through the mint's quotes.
    pub fn profile_content(&self, tip: Option<&Tip>) -> String {
        let mut m = serde_json::Map::new();
        if let Some(n) = &self.name {
            m.insert("name".into(), n.clone().into());
            m.insert("display_name".into(), n.clone().into());
        }
        let record = format!("Hourly delivery record: {}/reliability/", self.url);
        let mut about = match &self.description {
            Some(d) => format!("{d}\n\n{record}"),
            None => record,
        };
        if let Some(t) = tip {
            about.push_str("\n\n");
            about.push_str(&t.note);
            m.insert(t.field.into(), t.value.clone().into());
        }
        m.insert("about".into(), about.into());
        m.insert("picture".into(), self.picture().into());
        m.insert("website".into(), self.url.clone().into());
        if let Some(n) = self.nip05() {
            m.insert("nip05".into(), n.into());
        }
        m.insert("bot".into(), true.into());
        serde_json::Value::Object(m).to_string()
    }

    /// Kind 0 tags: a NIP-31 `alt` and `t=hashu`.
    pub fn profile_tags(&self) -> anyhow::Result<Vec<Tag>> {
        Ok(vec![
            Tag::hashtag(ANNOUNCE_TAG),
            Tag::parse([
                "alt",
                format!("Nostr profile of the Cashu mint {}", self.url).as_str(),
            ])?,
        ])
    }

    /// Kind 38000 tags recommending this mint, whose announcement is
    /// signed by `announcer` (hex pubkey). `relay` is a hint for where to
    /// find it.
    pub fn recommendation_tags(
        &self,
        announcer: &str,
        relay: Option<&str>,
    ) -> anyhow::Result<Vec<Tag>> {
        let d = self.d();
        let a = format!("{ANNOUNCE_KIND}:{announcer}:{d}");
        let mut atag = vec!["a".to_string(), a];
        atag.push(relay.unwrap_or("").to_string());
        atag.push("cashu".into());
        Ok(vec![
            Tag::parse(["k", ANNOUNCE_KIND.to_string().as_str()])?,
            Tag::parse(["d", d.as_str()])?,
            Tag::parse(["u", self.url.as_str(), "cashu"])?,
            Tag::parse(atag)?,
        ])
    }
}

/// NUT numbers a mint supports, from its serialized NUT-06 `nuts` map.
/// NUT-01, 02, 03 and 06 are always on; 04/05 count when they offer a
/// method; the rest when marked `supported` or, for settings objects
/// without that flag (NUT-19, NUT-22, …), when present.
pub fn supported_nuts(nuts: &serde_json::Value) -> Vec<u16> {
    let mut out: Vec<u16> = vec![1, 2, 3, 6];
    if let Some(map) = nuts.as_object() {
        for (k, v) in map {
            let Ok(n) = k.parse::<u16>() else { continue };
            let on = match v {
                serde_json::Value::Null => false,
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::Object(o) => {
                    if let Some(s) = o.get("supported") {
                        match s {
                            serde_json::Value::Bool(b) => *b,
                            serde_json::Value::Array(a) => !a.is_empty(),
                            _ => true,
                        }
                    } else if let Some(m) = o.get("methods") {
                        let disabled = o.get("disabled").and_then(|d| d.as_bool()).unwrap_or(false);
                        !disabled && m.as_array().is_some_and(|a| !a.is_empty())
                    } else {
                        !o.is_empty()
                    }
                }
                serde_json::Value::Array(a) => !a.is_empty(),
                _ => true,
            };
            if on {
                out.push(n);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Units the mint issues (NUT-04 methods), lowercase, sorted.
fn issued_units(nuts: &serde_json::Value) -> Vec<String> {
    let mut units: Vec<String> = nuts["4"]["methods"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(nuts["5"]["methods"].as_array().into_iter().flatten())
        .filter_map(|m| m["unit"].as_str())
        .map(|u| u.to_ascii_lowercase())
        .collect();
    units.sort();
    units.dedup();
    units
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::JsonUtil;
    use serde_json::json;

    fn sample() -> Announcement {
        Announcement {
            mint_pubkey: Some("02ab".into()),
            url: "https://mint.example".into(),
            nuts: vec![1, 2, 3, 4, 5, 6, 7],
            network: "mainnet".into(),
            units: vec!["tih".into()],
            name: Some("Hashu".into()),
            description: None,
            icon_url: None,
        }
    }

    #[test]
    fn nuts_from_info_map() {
        let nuts = json!({
            "4": {"methods": [{"method": "bolt11", "unit": "tih"}], "disabled": false},
            "5": {"methods": [{"method": "hashrate", "unit": "tih"}], "disabled": false},
            "7": {"supported": true},
            "8": {"supported": false},
            "14": {"supported": true},
            "15": {"methods": []},
            "17": {"supported": []},
            "19": {"ttl": 60, "cached_endpoints": []},
            "22": null
        });
        assert_eq!(supported_nuts(&nuts), vec![1, 2, 3, 4, 5, 6, 7, 14, 19]);
        assert_eq!(issued_units(&nuts), vec!["tih"]);
    }

    #[test]
    fn announcement_tags_follow_nip87() {
        let a = sample();
        let tags: Vec<Vec<String>> = a.tags().unwrap().into_iter().map(|t| t.to_vec()).collect();
        assert_eq!(tags[0], vec!["d", "02ab"]);
        assert_eq!(tags[1], vec!["u", "https://mint.example"]);
        assert_eq!(tags[2], vec!["nuts", "1,2,3,4,5,6,7"]);
        assert_eq!(tags[3], vec!["n", "mainnet"]);
        assert!(tags.contains(&vec!["unit".into(), "tih".into()]));
        let c: serde_json::Value = serde_json::from_str(&a.content()).unwrap();
        assert_eq!(c["name"], "Hashu");
        assert_eq!(c["website"], "https://mint.example");
    }

    #[test]
    fn profile_is_kind0_metadata_without_lightning() {
        let a = sample();
        let c: serde_json::Value = serde_json::from_str(&a.profile_content(None)).unwrap();
        assert_eq!(c["name"], "Hashu");
        assert_eq!(c["website"], "https://mint.example");
        assert_eq!(c["nip05"], "_@mint.example");
        assert_eq!(c["picture"], "https://mint.example/nostr/avatar.png");
        assert_eq!(c["bot"], true);
        assert!(c["about"]
            .as_str()
            .unwrap()
            .contains("https://mint.example/reliability/"));
        assert!(c.get("lud16").is_none() && c.get("lud06").is_none());
        let meta = nostr::Metadata::from_json(a.profile_content(None)).unwrap();
        assert_eq!(meta.name.as_deref(), Some("Hashu"));
    }

    #[test]
    fn tips_go_to_lud16_or_lud06() {
        use crate::lnaddress::PayEndpoint;
        let a = sample();
        let op = Tip::operator(&"tips@mint.example".parse::<PayEndpoint>().unwrap());
        let c: serde_json::Value = serde_json::from_str(&a.profile_content(Some(&op))).unwrap();
        assert_eq!(c["lud16"], "tips@mint.example");
        assert!(c["about"]
            .as_str()
            .unwrap()
            .contains("open-source development"));
        let url: PayEndpoint = "https://pay.alice.example/lnurlp/1".parse().unwrap();
        let sup = Tip::supplier("alice", &url);
        assert_eq!(sup.field, "lud06");
        assert!(sup.value.starts_with("lnurl1"));
        // Round-trips through the same parser suppliers sign up with.
        assert_eq!(sup.value.parse::<PayEndpoint>().unwrap(), url);
        let c: serde_json::Value = serde_json::from_str(&a.profile_content(Some(&sup))).unwrap();
        assert!(c["about"].as_str().unwrap().contains("Tips go to alice"));
        assert!(c.get("lud16").is_none());
    }

    #[test]
    fn nip05_only_for_plain_https_hosts() {
        let mut a = sample();
        a.url = "https://Alice.Hashu.io".into();
        assert_eq!(a.nip05().as_deref(), Some("_@alice.hashu.io"));
        a.url = "https://mint.example:8085".into();
        assert_eq!(a.nip05(), None);
        a.url = "http://localhost".into();
        assert_eq!(a.nip05(), None);
    }

    #[test]
    fn picture_prefers_the_mint_icon() {
        let mut a = sample();
        a.icon_url = Some("https://cdn.example/i.png".into());
        assert_eq!(a.picture(), "https://cdn.example/i.png");
    }

    #[test]
    fn d_falls_back_to_url() {
        let mut a = sample();
        a.mint_pubkey = None;
        assert_eq!(a.d(), "https://mint.example");
    }

    #[test]
    fn recommendation_points_at_announcement() {
        let tags: Vec<Vec<String>> = sample()
            .recommendation_tags("ff00", Some("wss://relay.example"))
            .unwrap()
            .into_iter()
            .map(|t| t.to_vec())
            .collect();
        assert_eq!(tags[0], vec!["k", "38172"]);
        assert_eq!(tags[1], vec!["d", "02ab"]);
        assert_eq!(
            tags[3],
            vec!["a", "38172:ff00:02ab", "wss://relay.example", "cashu"]
        );
    }
}
