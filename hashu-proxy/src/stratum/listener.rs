//! TCP accept loop. Spawns one [`super::connection::proxy_connection`] task
//! per inbound miner.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use super::connection::proxy_connection;
use crate::ProxyMetrics;

#[derive(Debug, Clone)]
pub struct ListenerConfig {
    /// Address to bind for inbound miners (e.g. `0.0.0.0:3333`).
    pub bind: SocketAddr,
    /// Upstream pool address (`host:port`). Stratum URL prefixes like
    /// `stratum+tcp://` should be stripped before construction.
    pub upstream: String,
}

/// Run the accept loop until cancelled. Each accepted connection becomes its
/// own tokio task; the loop returns only on listener errors (rare) or task
/// cancellation.
pub async fn run(cfg: ListenerConfig, metrics: Arc<ProxyMetrics>) -> Result<()> {
    let listener = TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("bind {}", cfg.bind))?;
    tracing::info!(bind = %cfg.bind, upstream = %cfg.upstream, "stratum proxy listening");

    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        metrics.connections_accepted.fetch_add(1, Ordering::Relaxed);
        let upstream = cfg.upstream.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            tracing::info!(%peer, "miner connected");
            match proxy_connection(sock, &upstream, metrics).await {
                Ok(()) => tracing::info!(%peer, "miner disconnected"),
                Err(e) => tracing::warn!(%peer, error = %e, "connection ended with error"),
            }
        });
    }
}

/// Strip an optional `stratum+tcp://` (or `stratum+tcps://`) URI prefix and
/// return a bare `host:port`. Returns the input unchanged if no prefix matches.
pub fn strip_stratum_prefix(s: &str) -> &str {
    s.strip_prefix("stratum+tcp://")
        .or_else(|| s.strip_prefix("stratum+tcps://"))
        .or_else(|| s.strip_prefix("stratum://"))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_prefix_handles_common_forms() {
        assert_eq!(strip_stratum_prefix("stratum+tcp://pool.com:3333"), "pool.com:3333");
        assert_eq!(strip_stratum_prefix("stratum+tcps://pool.com:3334"), "pool.com:3334");
        assert_eq!(strip_stratum_prefix("stratum://pool.com:3333"), "pool.com:3333");
        assert_eq!(strip_stratum_prefix("pool.com:3333"), "pool.com:3333");
    }
}
