//! Operator CLI surface — generate keys, init the mint, publish manifest,
//! view active redemptions, run the daemon. ARCHITECTURE.md §1, §4.7.1.
//!
//! Implemented subcommands:
//!   hashu oracle ping    — fetch one hashprice sample from the live oracle
//!
//! Planned subcommands:
//!   hashu init           — generate operator nostr keypair + mint config
//!   hashu manifest push  — (re)publish the kind 0 manifest
//!   hashu mint run       — run mint + proxy in one process (done)
//!   hashu redemptions    — list active redemptions

pub mod checkout;
pub mod config;
pub mod epochs;
pub mod inbox;
pub mod init;
pub mod landing;
pub mod manifest;
pub mod mine;
pub mod mint;
pub mod mrr_sync;
pub mod network;
pub mod operator;
pub mod operator_auth;
pub mod oracle;
pub mod orders;
pub mod paid;
pub mod planner;
pub mod proxy;
pub mod reliability;
pub mod stratum_addr;
pub mod supplier;
pub mod supply;
pub mod telemetry;
pub mod tenants;
pub mod terms;
pub mod tips;
mod ur;
pub mod waitlist;
pub mod wallet;
pub mod words;
