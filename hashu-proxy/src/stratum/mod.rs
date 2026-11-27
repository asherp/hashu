//! Stratum V1 listener and upstream relay. ARCHITECTURE.md §4.2.
//!
//! Upstream may be plain TCP or TLS (see [`crate::upstream`]). V2 (via
//! SRI) is a fast-follow.

pub mod compact;
pub mod connection;
pub mod extranonce;
pub mod header;
pub mod listener;
pub mod observe;
pub mod replay;
pub mod session;
pub mod startdiff;

pub use connection::{
    proxy_connection, proxy_connection_with, proxy_routed_connection, RESUME_METHOD,
};
pub use listener::{
    run as run_listener, run_routed as run_routed_listener,
    run_routed_on as run_routed_listener_on, run_with as run_listener_with, strip_stratum_prefix,
    ListenerConfig,
};
pub use observe::{observe_line, Direction, PendingSubmits};
