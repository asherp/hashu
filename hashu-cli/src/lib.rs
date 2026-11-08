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
pub mod init;
pub mod landing;
pub mod manifest;
pub mod mine;
pub mod mint;
pub mod network;
pub mod operator;
pub mod oracle;
pub mod paid;
pub mod proxy;
mod ur;
pub mod wallet;
pub mod words;
