//! Stratum proxy. Transparent passthrough to the operator's default pool;
//! redirects to a redeemer's pool while a redemption is active.
//! See ARCHITECTURE.md §4.2, §4.6.

pub mod stratum;
pub mod router;
