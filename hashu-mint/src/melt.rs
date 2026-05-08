//! Custom melt methods: `hashrate` and `hashrate-sats`.
//! ARCHITECTURE.md §5, §7.

// TODO: HashrateMelter — Th-hour redemption. Settles when ∫ hashrate dt ≥ N·1h.
// TODO: HashrateSatsMelter — sats-equivalent redemption. Settles when
//       ∫ hashprice·hashrate dt ≥ target_sats.
// TODO: standard NUT-05 BOLT11 melt is provided by cdk; re-export.
