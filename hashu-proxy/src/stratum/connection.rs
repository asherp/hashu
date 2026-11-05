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
//!
//! With a [`CreditContext`], each accepted share is also credited to the
//! redemption the connection was bound to when the share was submitted
//! (see [`crate::credit`]).

use std::collections::HashMap;
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
use crate::credit::{ConnectionId, CreditContext};
use crate::ProxyMetrics;
use hashu_core::redemption::RedemptionId;

/// State shared between the two forward tasks of a single connection.
#[derive(Default)]
struct ConnectionState {
    session: SessionState,
    committer: ShareCommitter,
    /// Redemption each in-flight `mining.submit` was sent for, keyed by
    /// request id. Sampled from the binding at submit time.
    submit_bindings: HashMap<i64, Option<RedemptionId>>,
}

/// Per-connection identity and crediting hooks shared by both forward
/// tasks.
struct ConnectionCtx {
    conn: ConnectionId,
    credit: Option<CreditContext>,
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

/// Run a single miner ⇄ pool proxy until either side disconnects. Shares
/// are counted and committed but not credited to any redemption.
pub async fn proxy_connection(
    miner_socket: TcpStream,
    upstream_addr: &str,
    metrics: Arc<ProxyMetrics>,
) -> Result<()> {
    proxy_connection_with(
        miner_socket,
        upstream_addr,
        metrics,
        ConnectionId::next(),
        None,
    )
    .await
}

/// Like [`proxy_connection`], but with a listener-assigned id and, when
/// `credit` is set, crediting of accepted shares to the ledger.
pub async fn proxy_connection_with(
    miner_socket: TcpStream,
    upstream_addr: &str,
    metrics: Arc<ProxyMetrics>,
    conn: ConnectionId,
    credit: Option<CreditContext>,
) -> Result<()> {
    let pool_socket = TcpStream::connect(upstream_addr)
        .await
        .with_context(|| format!("connect upstream {upstream_addr}"))?;
    pool_socket.set_nodelay(true).ok();
    miner_socket.set_nodelay(true).ok();

    let pending = Arc::new(PendingSubmits::default());
    let state = Arc::new(Mutex::new(ConnectionState::default()));
    let ctx = Arc::new(ConnectionCtx { conn, credit });

    let (miner_r, miner_w) = miner_socket.into_split();
    let (pool_r, pool_w) = pool_socket.into_split();

    let m_to_p = tokio::spawn(forward(
        BufReader::new(miner_r),
        pool_w,
        ForwardDirection::Miner2Pool,
        pending.clone(),
        state.clone(),
        ctx.clone(),
        metrics.clone(),
    ));
    let p_to_m = tokio::spawn(forward(
        BufReader::new(pool_r),
        miner_w,
        ForwardDirection::Pool2Miner,
        pending.clone(),
        state.clone(),
        ctx.clone(),
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
    ctx: Arc<ConnectionCtx>,
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
        update_session(&line, direction, &state, &ctx, &metrics);
        writer.write_all(line.as_bytes()).await?;
        writer.flush().await?;
    }
    let _ = writer.shutdown().await;
    Ok(())
}

/// Drive the typed-parse session machine; on accepted shares, build the
/// leaf, append it to the connection's committer, and credit the
/// redemption the share was submitted for.
fn update_session(
    line: &str,
    direction: ForwardDirection,
    state: &Mutex<ConnectionState>,
    ctx: &ConnectionCtx,
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
    match event {
        SessionEvent::SubmitOpened(id) => {
            if let Some(credit) = &ctx.credit {
                let bound = credit.binding.current(ctx.conn);
                guard.submit_bindings.insert(id, bound);
            }
        }
        SessionEvent::SubmitRejected { id } => {
            guard.submit_bindings.remove(&id);
        }
        SessionEvent::SubmitAccepted {
            id,
            submit,
            job,
            extranonce1,
        } => {
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

            // Credit only shares that made it into the commitment tree, so
            // every credited hash has a leaf behind it (§4.7.2).
            let bound = guard.submit_bindings.remove(&id).flatten();
            if let Some(credit) = &ctx.credit {
                credit.credit_accepted(ctx.conn, bound, submit.difficulty_at_submit, metrics);
            }
        }
        _ => {
            // A response the session could not turn into a leaf (e.g. the
            // proxy missed the job's `mining.notify`) still settles the
            // submit; drop its binding so the map does not grow.
            if matches!(direction, ForwardDirection::Pool2Miner)
                && !guard.submit_bindings.is_empty()
            {
                if let Some(id) = response_id(line) {
                    guard.submit_bindings.remove(&id);
                }
            }
        }
    }
}

/// `id` of a Stratum response line (one with no `method`).
fn response_id(line: &str) -> Option<i64> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("method").is_some_and(|m| !m.is_null()) {
        return None;
    }
    v.get("id").and_then(|x| x.as_i64())
}
