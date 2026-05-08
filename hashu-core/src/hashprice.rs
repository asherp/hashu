//! Hashprice oracle trait. ARCHITECTURE.md §4.4.
//!
//! Pluggable so v0 can ship with Luxor and later add Hashrate Index,
//! Braiins, or self-derived sources without changing call sites.

// TODO: trait HashpriceOracle { fn sample_at(&self, t: SystemTime) -> SatsPerThSecond }
// TODO: Luxor adapter (5-min pull cadence, interpolate between samples).
