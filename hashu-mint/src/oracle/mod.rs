//! Hashprice oracle adapters. ARCHITECTURE.md §4.4.
//!
//! Each adapter pushes [`hashu_core::hashprice::HashpriceSample`] values into
//! a shared in-memory buffer and exposes the [`hashu_core::hashprice::HashpriceOracle`]
//! trait so the redemption controller can query at arbitrary `t`.

pub mod esplora;
pub mod luxor;
