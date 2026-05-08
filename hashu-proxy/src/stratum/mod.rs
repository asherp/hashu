//! Stratum V1 listener and upstream relay. ARCHITECTURE.md §4.2.
//!
//! V2 (via SRI) is a fast-follow.

pub mod connection;
pub mod listener;
pub mod observe;

pub use connection::proxy_connection;
pub use listener::{run as run_listener, strip_stratum_prefix, ListenerConfig};
pub use observe::{observe_line, Direction, PendingSubmits};
