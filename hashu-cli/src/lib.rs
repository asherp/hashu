//! Operator CLI surface — generate keys, init the mint, publish manifest,
//! view active redemptions, run the daemon. ARCHITECTURE.md §1, §4.7.1.

// TODO: clap-based subcommands:
//       hashu init           — generate operator nostr keypair + mint config
//       hashu manifest push  — (re)publish the kind 0 manifest
//       hashu daemon         — run mint + proxy in one process
//       hashu redemptions    — list active redemptions
