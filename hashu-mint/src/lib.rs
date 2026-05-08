//! Cashu mint integration. Wraps `cdk` to expose the THH unit and the
//! custom melt methods `hashrate` and `hashrate-sats`.
//! See ARCHITECTURE.md §4.1, §5, §7.

pub mod attestation;
pub mod manifest;
pub mod melt;
pub mod oracle;
