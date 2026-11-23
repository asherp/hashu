//! TCP accept loop. Spawns one [`super::connection::proxy_connection`] task
//! per inbound miner.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

use super::connection::{proxy_connection_with, proxy_routed_connection};
use crate::credit::{ConnectionId, CreditContext};
use crate::router::Router;
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
    run_with(cfg, metrics, None).await
}

/// Like [`run`], but credits accepted shares to the redemption ledger
/// through `credit` (see [`crate::credit`]). `None` behaves like [`run`].
pub async fn run_with(
    cfg: ListenerConfig,
    metrics: Arc<ProxyMetrics>,
    credit: Option<CreditContext>,
) -> Result<()> {
    let listener = TcpListener::bind(cfg.bind)
        .await
        .with_context(|| format!("bind {}", cfg.bind))?;
    tracing::info!(bind = %cfg.bind, upstream = %cfg.upstream, "stratum proxy listening");

    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        metrics.connections_accepted.fetch_add(1, Ordering::Relaxed);
        let upstream = cfg.upstream.clone();
        let metrics = metrics.clone();
        let credit = credit.clone();
        let conn = ConnectionId::next();
        tokio::spawn(async move {
            tracing::info!(%peer, %conn, "miner connected");
            match proxy_connection_with(sock, &upstream, metrics, conn, credit).await {
                Ok(()) => tracing::info!(%peer, %conn, "miner disconnected"),
                Err(e) => tracing::warn!(%peer, %conn, error = %e, "connection ended with error"),
            }
        });
    }
}

/// Accept loop where each miner's upstream is chosen by `router` (default
/// pool, or a redeemer's pool during a redemption). Also runs the
/// router's reconciler, which re-routes live connections when a
/// redemption opens or completes. With `credit`, the reconciler also
/// wakes on the crediter's completion signal so a filled redemption
/// releases its miners immediately.
pub async fn run_routed(
    bind: SocketAddr,
    router: Arc<Router>,
    metrics: Arc<ProxyMetrics>,
    credit: Option<CreditContext>,
    reconcile_every: std::time::Duration,
) -> Result<()> {
    let listener = TcpListener::bind(bind)
        .await
        .with_context(|| format!("bind {bind}"))?;
    run_routed_on(listener, router, metrics, credit, reconcile_every).await
}

/// [`run_routed`] on a listener the caller already bound. A daemon binds
/// its Stratum port first thing at startup, so miners reconnecting after
/// a deploy are queued in the kernel's accept backlog while the rest of
/// the daemon starts, instead of being refused and backing off (or
/// failing over to a backup pool they may never leave).
pub async fn run_routed_on(
    listener: TcpListener,
    router: Arc<Router>,
    metrics: Arc<ProxyMetrics>,
    credit: Option<CreditContext>,
    reconcile_every: std::time::Duration,
) -> Result<()> {
    let bind = listener.local_addr().context("listener address")?;
    tracing::info!(%bind, default_upstream = router.default_upstream(), "routed stratum proxy listening");

    let completions = credit.as_ref().map(|c| c.crediter.subscribe_completions());
    let reconciler = tokio::spawn(router.clone().run_reconciler(reconcile_every, completions));
    let _abort_on_exit = AbortOnDrop(reconciler);

    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        metrics.connections_accepted.fetch_add(1, Ordering::Relaxed);
        let router = router.clone();
        let metrics = metrics.clone();
        let credit = credit.clone();
        let conn = ConnectionId::next();
        tokio::spawn(async move {
            tracing::info!(%peer, %conn, "miner connected");
            match proxy_routed_connection(sock, router, metrics, conn, credit).await {
                Ok(()) => tracing::info!(%peer, %conn, "miner disconnected"),
                Err(e) => tracing::warn!(%peer, %conn, error = %e, "connection ended with error"),
            }
        });
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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
        assert_eq!(
            strip_stratum_prefix("stratum+tcp://pool.com:3333"),
            "pool.com:3333"
        );
        assert_eq!(
            strip_stratum_prefix("stratum+tcps://pool.com:3334"),
            "pool.com:3334"
        );
        assert_eq!(
            strip_stratum_prefix("stratum://pool.com:3333"),
            "pool.com:3333"
        );
        assert_eq!(strip_stratum_prefix("pool.com:3333"), "pool.com:3333");
    }
}
