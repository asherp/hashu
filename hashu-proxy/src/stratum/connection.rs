//! Per-connection bidirectional Stratum forwarder.
//!
//! Each accepted miner gets one of these. We open a TCP connection to the
//! upstream pool, then run two forwarding tasks (miner→pool, pool→miner).
//! Each task reads lines, calls into [`super::observe::observe_line`] for
//! metrics and into a shared [`SessionState`] (for typed parse +
//! share-commitment), then writes the line through unmodified. Closing
//! either side aborts the other.
//!
//! On every accepted share, the typed-parse path emits enough info to
//! reconstruct the 80-byte block header; we append the resulting leaf to a
//! per-connection [`ShareCommitter`]. When the connection ends we log the
//! final tree summary (root, leaf count) so an operator can correlate.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use super::committer::ShareCommitter;
use super::compact::target_bits_from_difficulty;
use super::header::{build_block_header, SubmitFields};
use super::observe::{observe_line, Direction, PendingSubmits};
use super::session::{SessionEvent, SessionState};
use crate::ProxyMetrics;

/// State shared between the two forward tasks of a single connection.
#[derive(Default)]
struct ConnectionState {
    session: SessionState,
    committer: ShareCommitter,
}

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
    let state = Arc::new(Mutex::new(ConnectionState::default()));

    let (miner_r, miner_w) = miner_socket.into_split();
    let (pool_r, pool_w) = pool_socket.into_split();

    let m_to_p = tokio::spawn(forward(
        BufReader::new(miner_r),
        pool_w,
        ForwardDirection::Miner2Pool,
        pending.clone(),
        state.clone(),
        metrics.clone(),
    ));
    let p_to_m = tokio::spawn(forward(
        BufReader::new(pool_r),
        miner_w,
        ForwardDirection::Pool2Miner,
        pending.clone(),
        state.clone(),
        metrics.clone(),
    ));

    // First side to finish or error wins; abort the other so we don't leak
    // a half-open connection.
    tokio::select! {
        r = m_to_p => { let _ = r; },
        r = p_to_m => { let _ = r; },
    }

    // Log final commitment-tree state so the operator can correlate later.
    if let Ok(s) = state.lock() {
        if !s.committer.is_empty() {
            let root = s.committer.root().map(hex::encode).unwrap_or_default();
            tracing::info!(
                committed_leaves = s.committer.len(),
                root = %root,
                "share commitment tree finalized for connection",
            );
        }
    }
    Ok(())
}

async fn forward<R, W>(
    mut reader: BufReader<R>,
    mut writer: W,
    direction: ForwardDirection,
    pending: Arc<PendingSubmits>,
    state: Arc<Mutex<ConnectionState>>,
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
            break;
        }
        observe_line(&line, direction.into(), &pending, &metrics);
        update_session(&line, direction, &state, &metrics);
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
    }
    let _ = writer.shutdown().await;
    Ok(())
}

/// Drive the typed-parse session machine; on accepted shares, build the
/// leaf and append to the connection's committer.
fn update_session(
    line: &str,
    direction: ForwardDirection,
    state: &Mutex<ConnectionState>,
    metrics: &ProxyMetrics,
) {
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return, // poisoned; the other task already errored
    };

    // Subscribe responses come from the pool with `id` set and `result`
    // shaped like a 3-tuple. The session module's loose absorber finds
    // these without needing to remember the matching request id.
    if matches!(direction, ForwardDirection::Pool2Miner) {
        guard.session.try_absorb_subscribe_response(line);
    }

    let event = guard.session.ingest(line, direction.into());
    if let SessionEvent::SubmitAccepted {
        submit,
        job,
        extranonce1,
        ..
    } = event
    {
        let fields = SubmitFields {
            extranonce2: submit.extranonce2.clone(),
            ntime: submit.ntime,
            nonce: submit.nonce,
            version_mask: submit.version_mask,
        };
        let header = build_block_header(job.as_ref(), &extranonce1, &fields);
        let target_bits = match submit.difficulty_at_submit {
            Some(d) => target_bits_from_difficulty(d),
            None => 0x1d00_ffff,
        };
        guard
            .committer
            .append(header.to_vec(), target_bits, submit.ntime);
        metrics.shares_committed.fetch_add(1, Ordering::Relaxed);
    }
}

