//! Cashu mint integration. Wraps `cdk` to expose the TiH unit and the
//! custom melt method `hashrate`, plus the payment rails that sell TiH
//! (`tether` for USDT).
//! See ARCHITECTURE.md §4.1, §5, §7.

pub mod attestation;
pub mod capacity;
pub mod heat;
pub mod lightning;
pub mod lnaddress;
pub mod manifest;
pub mod melt;
pub mod oracle;
pub mod payment;
pub mod phoenixd;
pub mod pricing;
pub mod tether;
