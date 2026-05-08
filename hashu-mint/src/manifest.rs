//! Operator manifest publisher. ARCHITECTURE.md §4.7.1.
//!
//! Binds the operator's nostr identity (long-lived BIP-340 Schnorr keypair)
//! to the mint's BDHKE keysets via a kind 0 metadata event. Kind 0 is
//! replaceable, so historical attribution depends on `mint_pubkey_ids` being
//! append-only across keyset rotations — rotated keysets stay listed with
//! `status: "retired"` rather than being removed. This module owns the local
//! persistence of that history, the event construction, and relay
//! publishing.

pub mod event;
pub mod keyset;
pub mod publish;
pub mod state;

pub use event::{build_event, build_metadata, HashuManifest, ManifestError};
pub use keyset::{FixedKeysetSource, KeysetSource};
pub use publish::{publish, PublishError, PublishOutcome};
pub use state::{KeysetEntry, KeysetStatus, ManifestState, Profile, StateError, Transition};
