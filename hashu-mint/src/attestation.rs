//! Share commitment tree publisher and completion certificates.
//! ARCHITECTURE.md §4.7.2, §4.7.3.

// TODO: SharetreeBuilder — accepts share leaves, emits signed roots at
//       configurable cadence (default: every 1024 leaves OR 5 min).
// TODO: CompletionCert — terminal root, total_difficulty, integrated_value,
//       oracle_samples_root, signed by operator npub.
// TODO: NIP-44 encrypted leaf delivery to redeemer pubkey.
