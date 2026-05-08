//! Stratum V1 listener and upstream relay. ARCHITECTURE.md §4.2.
//!
//! V2 (via SRI) is a fast-follow.

// TODO: V1 line-based JSON-RPC listener (mining.subscribe / authorize / submit).
// TODO: per-connection upstream client; bidirectional relay.
// TODO: redirect signal handler — drop+reopen upstream to redeemer's pool.
// TODO: share metering — feed accepted shares into hashu-mint::attestation.
