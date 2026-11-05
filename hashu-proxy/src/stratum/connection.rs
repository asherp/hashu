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
use tokio::sync::watch;

use super::committer::ShareCommitter;
use super::compact::target_bits_from_difficulty;
use super::header::{build_block_header, SubmitFields};
use super::observe::{observe_line, Direction, PendingSubmits};
use super::session::{SessionEvent, SessionState};
use crate::credit::{ConnectionId, CreditContext};
use crate::router::{Route, Router};
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
    /// Where this connection's shares go; also rewrites the worker name
    /// on miner→pool lines when routed to a redemption.
    route: Route,
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
    let route = Route::Default {
        upstream: upstream_addr.to_string(),
    };
    let (_never, kicked) = watch::channel(false);
    let ctx = ConnectionCtx {
        conn,
        credit,
        route,
    };
    relay(miner_socket, pool_socket, kicked, ctx, metrics).await
}

/// Like [`proxy_connection_with`], but the [`Router`] picks the upstream:
/// the default pool, or a redeemer's pool while a redemption is active,
/// with the worker name rewritten to the redeemer's. The connection is
/// closed when the router kicks it, so the miner reconnects onto whatever
/// route is current. ARCHITECTURE.md §4.2. Pass the router itself as the
/// [`CreditContext`] binding so shares credit the redemption they were
/// routed for.
pub async fn proxy_routed_connection(
    miner_socket: TcpStream,
    router: Arc<Router>,
    metrics: Arc<ProxyMetrics>,
    conn: ConnectionId,
    credit: Option<CreditContext>,
) -> Result<()> {
    let assignment = router.assign(conn);
    let result = async {
        let mut route = assignment.route;
        let pool_socket = match TcpStream::connect(route.upstream()).await {
            Ok(s) => s,
            Err(e) if route.redemption_id().is_some() => {
                tracing::warn!(
                    %conn,
                    upstream = route.upstream(),
                    error = %e,
                    "redeemer's pool unreachable; falling back to default pool",
                );
                route = router.fall_back_to_default(conn);
                TcpStream::connect(route.upstream())
                    .await
                    .with_context(|| format!("connect default upstream {}", route.upstream()))?
            }
            Err(e) => {
                return Err(e).with_context(|| format!("connect upstream {}", route.upstream()))
            }
        };
        let ctx = ConnectionCtx {
            conn,
            credit,
            route,
        };
        relay(miner_socket, pool_socket, assignment.kicked, ctx, metrics).await
    }
    .await;
    router.release(conn);
    result
}

/// Forward lines both ways between an already-connected miner and pool
/// until either side closes or `kicked` turns true.
async fn relay(
    miner_socket: TcpStream,
    pool_socket: TcpStream,
    mut kicked: watch::Receiver<bool>,
    ctx: ConnectionCtx,
    metrics: Arc<ProxyMetrics>,
) -> Result<()> {
    pool_socket.set_nodelay(true).ok();
    miner_socket.set_nodelay(true).ok();

    let pending = Arc::new(PendingSubmits::default());
    let state = Arc::new(Mutex::new(ConnectionState::default()));
    let ctx = Arc::new(ctx);

    let (miner_r, miner_w) = miner_socket.into_split();
    let (pool_r, pool_w) = pool_socket.into_split();

    let mut m_to_p = tokio::spawn(forward(
        BufReader::new(miner_r),
        pool_w,
        ForwardDirection::Miner2Pool,
        pending.clone(),
        state.clone(),
        ctx.clone(),
        metrics.clone(),
    ));
    let mut p_to_m = tokio::spawn(forward(
        BufReader::new(pool_r),
        miner_w,
        ForwardDirection::Pool2Miner,
        pending.clone(),
        state.clone(),
        ctx.clone(),
        metrics.clone(),
    ));

    // First side to finish (or a kick from the router) wins; abort both so
    // we don't leak a half-open connection. Aborting drops the socket
    // halves, which closes the miner's TCP connection.
    tokio::select! {
        _ = &mut m_to_p => {},
        _ = &mut p_to_m => {},
        _ = kicked.wait_for(|k| *k) => {
            tracing::info!(
                conn = %ctx.conn,
                redemption = ?ctx.route.redemption_id().map(|i| i.to_string()),
                "route changed; closing miner connection so it reconnects",
            );
        },
    }
    m_to_p.abort();
    p_to_m.abort();

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
        let rewritten = match direction {
            ForwardDirection::Miner2Pool => ctx.route.rewrite_miner_line(&line),
            ForwardDirection::Pool2Miner => None,
        };
        let out = rewritten.as_deref().unwrap_or(&line);
        writer.write_all(out.as_bytes()).await?;
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
