//! Per-connection bidirectional Stratum forwarder.
//!
//! Each accepted miner gets one of these. We open a TCP connection to the
//! upstream pool, then run two forwarding tasks (miner→pool, pool→miner).
//! Each task reads lines, calls into [`super::observe::observe_line`] for
//! metrics, and writes the line through unmodified. Closing either side
//! aborts the other.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::observe::{observe_line, Direction, PendingSubmits};
use crate::ProxyMetrics;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForwardDirection {
    Miner2Pool,
    Pool2Miner,
}

impl From<ForwardDirection> for Direction {
    fn from(d: ForwardDirection) -> Self {
        match d {
            ForwardDirection::Miner2Pool => Direction::M2P,
            ForwardDirection::Pool2Miner => Direction::P2M,
        }
    }
}

/// Run a single miner ⇄ pool proxy until either side disconnects.
pub async fn proxy_connection(
    miner_socket: TcpStream,
    upstream_addr: &str,
    metrics: Arc<ProxyMetrics>,
) -> Result<()> {
    let pool_socket = TcpStream::connect(upstream_addr)
        .await
        .with_context(|| format!("connect upstream {upstream_addr}"))?;
    pool_socket.set_nodelay(true).ok();
    miner_socket.set_nodelay(true).ok();

    let pending = Arc::new(PendingSubmits::default());

    let (miner_r, miner_w) = miner_socket.into_split();
    let (pool_r, pool_w) = pool_socket.into_split();

    let m_to_p = tokio::spawn(forward(
        BufReader::new(miner_r),
        pool_w,
        ForwardDirection::Miner2Pool,
        pending.clone(),
        metrics.clone(),
    ));
    let p_to_m = tokio::spawn(forward(
        BufReader::new(pool_r),
        miner_w,
        ForwardDirection::Pool2Miner,
        pending.clone(),
        metrics.clone(),
    ));

    // First side to finish or error wins; abort the other so we don't leak
    // a half-open connection.
    tokio::select! {
        r = m_to_p => { let _ = r; },
        r = p_to_m => { let _ = r; },
    }
    Ok(())
}

async fn forward<R, W>(
    mut reader: BufReader<R>,
    mut writer: W,
    direction: ForwardDirection,
    pending: Arc<PendingSubmits>,
    metrics: Arc<ProxyMetrics>,
) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF
            break;
        }
        observe_line(&line, direction.into(), &pending, &metrics);
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
    }
    let _ = writer.shutdown().await;
    Ok(())
}

