//! Kind 0 metadata event construction. ARCHITECTURE.md §4.7.1.
//!
//! `content` is a JSON object merging standard nostr profile fields
//! (name/about/nip05/lud16/website) with a namespaced `hashu` object
//! describing the operator's mint capabilities and append-only keyset
//! history. NIP-01 signs the event header tuple, not the `content` string —
//! no canonicalization required.

use std::collections::BTreeMap;

use nostr::{Event, EventBuilder, Keys, Metadata, Timestamp};
use serde::{Deserialize, Serialize};

use super::state::{KeysetEntry, ManifestState};

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest has no active keyset; observe one first")]
    NoActiveKeyset,
    #[error("nostr signing failed: {0}")]
    Sign(#[from] nostr::event::builder::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Wire shape of the namespaced `hashu` object inside the kind 0 `content`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HashuManifest {
    pub instance_url: String,
    pub mint_pubkey_ids: Vec<KeysetEntry>,
    pub supported_units: Vec<String>,
    pub supported_melts: Vec<String>,
    pub hashprice_oracle: String,
    pub manifest_version: u32,
    pub issued_at: u64,
}

impl HashuManifest {
    pub const VERSION: u32 = 1;

    pub fn from_state(state: &ManifestState, issued_at: u64) -> Self {
        Self {
            instance_url: state.instance_url.clone(),
            mint_pubkey_ids: state.keysets.clone(),
            supported_units: vec!["THH".to_string()],
            supported_melts: vec![
                "bolt11".to_string(),
                "hashrate".to_string(),
                "hashrate-sats".to_string(),
            ],
            hashprice_oracle: state.hashprice_oracle.clone(),
            manifest_version: Self::VERSION,
            issued_at,
        }
    }
}

/// Build a `nostr::Metadata` populated with the operator's profile fields
/// plus the namespaced `hashu` object as a custom field.
pub fn build_metadata(state: &ManifestState, issued_at: u64) -> Result<Metadata, ManifestError> {
    let manifest = HashuManifest::from_state(state, issued_at);
    let hashu_value = serde_json::to_value(&manifest)?;

    let p = &state.profile;
    let metadata = Metadata {
        name: p.name.clone(),
        display_name: None,
        about: p.about.clone(),
        website: p.website.clone(),
        picture: None,
        banner: None,
        nip05: p.nip05.clone(),
        lud06: None,
        lud16: p.lud16.clone(),
        custom: {
            let mut m = BTreeMap::new();
            m.insert("hashu".to_string(), hashu_value);
            m
        },
    };
    Ok(metadata)
}

/// Build and sign a kind 0 event committing the manifest at time `now`.
///
/// `created_at` is `max(now, last_published_at + 1)` so that a republish
/// under clock skew still produces a strictly later event — relays that
/// keep only the latest event per (pubkey, kind=0) won't get stuck on a
/// stale one.
pub fn build_event(
    state: &ManifestState,
    keys: &Keys,
    now: u64,
) -> Result<Event, ManifestError> {
    let created_at = match state.last_published_at {
        Some(prev) if prev >= now => prev + 1,
        _ => now,
    };
    let metadata = build_metadata(state, created_at)?;
    let builder = EventBuilder::metadata(&metadata).custom_created_at(Timestamp::from(created_at));
    Ok(builder.sign_with_keys(keys)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::state::Profile;
    use serde_json::Value;

    fn state_with_keysets() -> ManifestState {
        let mut s = ManifestState::new(
            "https://mint.example.com".to_string(),
            "esplora".to_string(),
            vec!["wss://relay.damus.io".to_string()],
        );
        s.profile = Profile {
            name: Some("Hashu Op".to_string()),
            about: Some("Hashu mint at mint.example.com".to_string()),
            nip05: Some("op@example.com".to_string()),
            lud16: Some("op@example.com".to_string()),
            website: Some("https://mint.example.com".to_string()),
        };
        s.apply_observed_keyset("00aa", 1_000).unwrap();
        s.apply_observed_keyset("00bb", 2_000).unwrap();
        s
    }

    fn fixed_keys() -> Keys {
        // Deterministic test key. Hex of 32 bytes.
        Keys::parse("0000000000000000000000000000000000000000000000000000000000000001")
            .unwrap()
    }

    #[test]
    fn manifest_carries_all_required_fields() {
        let s = state_with_keysets();
        let m = HashuManifest::from_state(&s, 9_000);
        assert_eq!(m.manifest_version, 1);
        assert_eq!(m.instance_url, "https://mint.example.com");
        assert_eq!(m.hashprice_oracle, "esplora");
        assert_eq!(m.supported_units, vec!["THH"]);
        assert_eq!(m.supported_melts.len(), 3);
        assert_eq!(m.issued_at, 9_000);
        assert_eq!(m.mint_pubkey_ids.len(), 2);
        assert_eq!(m.mint_pubkey_ids[0].id, "00aa");
        assert_eq!(m.mint_pubkey_ids[1].id, "00bb");
    }

    #[test]
    fn metadata_includes_namespaced_hashu_object() {
        let s = state_with_keysets();
        let meta = build_metadata(&s, 9_000).unwrap();
        let hashu_value = meta.custom.get("hashu").expect("hashu key present");
        let obj = hashu_value.as_object().expect("hashu is object");
        assert_eq!(obj["manifest_version"], Value::from(1));
        assert_eq!(obj["hashprice_oracle"], Value::from("esplora"));
        let ids = obj["mint_pubkey_ids"].as_array().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0]["id"], Value::from("00aa"));
        assert_eq!(ids[0]["status"], Value::from("retired"));
        assert_eq!(ids[1]["id"], Value::from("00bb"));
        assert_eq!(ids[1]["status"], Value::from("active"));
    }

    #[test]
    fn metadata_keeps_standard_profile_fields() {
        let s = state_with_keysets();
        let meta = build_metadata(&s, 9_000).unwrap();
        assert_eq!(meta.name.as_deref(), Some("Hashu Op"));
        assert_eq!(meta.about.as_deref(), Some("Hashu mint at mint.example.com"));
        assert_eq!(meta.nip05.as_deref(), Some("op@example.com"));
        assert_eq!(meta.lud16.as_deref(), Some("op@example.com"));
        assert_eq!(meta.website.as_deref(), Some("https://mint.example.com"));
    }

    #[test]
    fn event_signature_verifies() {
        let s = state_with_keysets();
        let keys = fixed_keys();
        let event = build_event(&s, &keys, 9_000).unwrap();
        assert!(event.verify().is_ok());
        assert_eq!(event.kind, nostr::Kind::Metadata);
        assert_eq!(event.created_at.as_secs(), 9_000);
    }

    #[test]
    fn event_content_round_trips_through_json() {
        let s = state_with_keysets();
        let keys = fixed_keys();
        let event = build_event(&s, &keys, 9_000).unwrap();
        let parsed: Value = serde_json::from_str(&event.content).unwrap();
        assert_eq!(parsed["name"], Value::from("Hashu Op"));
        assert_eq!(parsed["hashu"]["instance_url"], Value::from("https://mint.example.com"));
        let ids = parsed["hashu"]["mint_pubkey_ids"].as_array().unwrap();
        assert_eq!(ids[1]["status"], Value::from("active"));
    }

    #[test]
    fn created_at_stays_monotonic_under_clock_skew() {
        let mut s = state_with_keysets();
        s.last_published_at = Some(10_000);
        let keys = fixed_keys();
        // now is *behind* last_published_at — should still produce 10_001.
        let event = build_event(&s, &keys, 5_000).unwrap();
        assert_eq!(event.created_at.as_secs(), 10_001);
    }
}
