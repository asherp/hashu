//! Operator CLI surface — generate keys, init the mint, publish manifest,
//! view active redemptions, run the daemon. ARCHITECTURE.md §1, §4.7.1.
//!
//! Implemented subcommands:
//!   hashu oracle ping    — fetch one hashprice sample from the live oracle
//!
//! Planned subcommands:
//!   hashu init           — generate operator nostr keypair + mint config
//!   hashu manifest push  — (re)publish the kind 0 manifest
//!   hashu daemon         — run mint + proxy in one process
//!   hashu redemptions    — list active redemptions

pub mod oracle;
