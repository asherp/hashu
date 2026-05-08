//! Operator manifest publisher. Publishes the kind 0 nostr event with the
//! namespaced `hashu` object binding the operator's npub to the mint's
//! current keyset. ARCHITECTURE.md §4.7.1.

// TODO: ManifestPublisher { nostr_secret_key, relays }
// TODO: append-only mint_pubkey_ids across keyset rotations.
// TODO: republish on keyset rotation.
