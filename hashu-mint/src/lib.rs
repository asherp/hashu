//! Cashu mint integration. Wraps `cdk` to expose the THH unit and the
//! custom melt method `hashrate`, plus the payment rails that sell THH
//! (`tether` for USDT).
//! See ARCHITECTURE.md §4.1, §5, §7.

pub mod attestation;
pub mod lightning;
pub mod manifest;
pub mod melt;
pub mod oracle;
pub mod payment;
pub mod pricing;
pub mod tether;
