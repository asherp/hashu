//! Per-connection Stratum forwarder.
//!
//! Each accepted miner gets one of these. We open a connection to the
//! upstream pool (plain TCP, or TLS for a `tls://` upstream; see
//! [`crate::upstream`]) and run one event loop that forwards lines both
//! ways. Every line goes through [`super::observe::observe_line`] for
//! metrics and through the upstream's [`SessionState`] (typed parse +
//! share commitment).
//!
//! On every accepted share, the typed-parse path emits enough info to
//! reconstruct the 80-byte block header; we append the resulting leaf to a
//! per-connection [`ShareCommitter`]. When the connection ends we log the
//! final tree summary (root, leaf count) so an operator can correlate.
//!
//! With a [`CreditContext`], each accepted share is also credited to the
//! redemption the share was sent for (see [`crate::credit`]).
//!
//! ## Seamless switching
//!
//! A routed connection can be moved to another pool without closing the
//! miner's socket ([`Signal::Switch`]). The upstream session is a *leg*:
//!
//! 1. The proxy dials the new pool and replays the miner's handshake on
//!    it (`mining.configure`, `mining.subscribe`, `mining.authorize` with
//!    the route's worker), with proxy-owned request ids. The current leg
//!    keeps mining meanwhile. Once authorized, it asks the new pool for a
//!    starting difficulty sized from the miner's measured hashrate (see
//!    [`super::startdiff`]).
//! 2. Once the new pool has answered and sent a job, the proxy cuts over:
//!    it writes the new pool's difficulty and latest job (forced
//!    `clean_jobs = true`) to the miner. Extranonces are translated (see
//!    [`super::extranonce`]): miners get a small proxy-owned extranonce at
//!    subscribe, which fits into most pools' extranonce2 space. If it
//!    does not fit and the miner sent `mining.extranonce.subscribe`, the
//!    proxy sends `mining.set_extranonce` instead.
//! 3. The old leg drains for [`DRAIN_FOR`]: submits for its jobs still go
//!    to it (and credit its route), and their results reach the miner.
//!
//! If the switch can't work in place (miner not authorized yet, new
//! pool's extranonce2 too small, version-rolling mask not granted), the
//! connection closes and the miner reconnects onto the new route, as in
//! [`crate::router::SwitchMode::Reconnect`]. If the new pool is
//! unreachable the miner stays where it is and the router is told.

use std::collections::HashMap;
use std::future::pending;
use std::io;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, WriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use super::committer::ShareCommitter;
use super::compact::target_bits_from_difficulty;
use super::extranonce::{
    rewrite_subscribe_result, set_extranonce_line, virtual_downstream, Extranonce, ExtranonceMap,
};
use super::header::{build_block_header, SubmitFields};
use super::observe::{observe_line, Direction, PendingSubmits};
use super::replay::{force_clean, JobGate, PoolLine};
use super::session::{SessionEvent, SessionState};
use super::startdiff::{password_with_difficulty, WorkRate};
use crate::credit::{ConnectionId, CreditContext, ShareEvidence};
use crate::router::{Route, Router, Signal, SwitchMode};
use crate::upstream::{dial, PoolStream};
use crate::ProxyMetrics;
use hashu_core::redemption::RedemptionId;

/// How long a seamless switch may take (dial + handshake + first job)
/// before it is abandoned and the miner stays where it is.
pub const SWITCH_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the previous pool stays connected after a switch, so shares
/// the miner found on its jobs still reach it.
pub const DRAIN_FOR: Duration = Duration::from_secs(5);

/// Request ids the proxy uses for its own handshake with a new pool.
/// Far above anything a miner counts up to.
const ID_CONFIGURE: i64 = 7_340_000_001;
const ID_SUBSCRIBE: i64 = 7_340_000_002;
const ID_SUGGEST: i64 = 7_340_000_003;
const ID_AUTHORIZE: i64 = 7_340_000_004;
const PROXY_IDS: [i64; 4] = [ID_CONFIGURE, ID_SUBSCRIBE, ID_SUGGEST, ID_AUTHORIZE];

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
/// `credit` is set, crediting of accepted shares to the ledger (to
/// whatever `credit.binding` says the connection serves).
pub async fn proxy_connection_with(
    miner_socket: TcpStream,
    upstream_addr: &str,
    metrics: Arc<ProxyMetrics>,
    conn: ConnectionId,
    credit: Option<CreditContext>,
) -> Result<()> {
    let pool_socket = dial(upstream_addr)
        .await
        .with_context(|| format!("connect upstream {upstream_addr}"))?;
    let route = Route::Default {
        upstream: upstream_addr.to_string(),
    };
    let (_never, signal) = watch::channel(Signal::Stay);
    let ctx = Ctx {
        conn,
        credit,
        router: None,
        seamless: false,
        metrics,
    };
    relay(miner_socket, Leg::new(route, pool_socket), signal, ctx).await
}

/// Like [`proxy_connection_with`], but the [`Router`] picks the upstream:
/// the default pool, or a redeemer's pool while a redemption is active,
/// with the worker name rewritten to the redeemer's. When the router
/// re-routes the connection it is switched in place or closed (see the
/// module docs). ARCHITECTURE.md §4.2. Pass the router itself as the
/// [`CreditContext`] binding.
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
        let pool_socket = match dial(route.upstream()).await {
            Ok(s) => s,
            Err(e) if route.redemption_id().is_some() => {
                tracing::warn!(
                    %conn,
                    upstream = route.upstream(),
                    error = %e,
                    "redeemer's pool unreachable; falling back to default pool",
                );
                route = router.fall_back_to_default(conn);
                dial(route.upstream())
                    .await
                    .with_context(|| format!("connect default upstream {}", route.upstream()))?
            }
            Err(e) => {
                return Err(e).with_context(|| format!("connect upstream {}", route.upstream()))
            }
        };
        let ctx = Ctx {
            conn,
            credit,
            seamless: router.switch_mode() == SwitchMode::Seamless,
            router: Some(router.clone()),
            metrics,
        };
        relay(
            miner_socket,
            Leg::new(route, pool_socket),
            assignment.signal,
            ctx,
        )
        .await
    }
    .await;
    router.release(conn);
    result
}

/// Per-connection identity and hooks.
struct Ctx {
    conn: ConnectionId,
    credit: Option<CreditContext>,
    /// Set for routed connections: shares credit the route of the leg
    /// they were sent on, and switches report back here.
    router: Option<Arc<Router>>,
    /// Hand the miner a proxy-owned extranonce so later switches fit.
    seamless: bool,
    metrics: Arc<ProxyMetrics>,
}

impl Ctx {
    /// Redemption a submit sent on a leg with `route` is for.
    fn bound_for(&self, route: &Route) -> Option<RedemptionId> {
        if self.router.is_some() {
            return route.redemption_id();
        }
        self.credit
            .as_ref()
            .and_then(|c| c.binding.current(self.conn))
    }
}

/// What the miner has told us, kept so the handshake can be replayed on
/// a new pool.
#[derive(Default)]
struct MinerFacts {
    subscribe_id: Option<i64>,
    subscribe_params: Option<Value>,
    configure_id: Option<i64>,
    configure_params: Option<Value>,
    /// Version-rolling mask the miner was granted.
    version_mask: Option<u32>,
    authorize_params: Option<Value>,
    extranonce_subscribed: bool,
    suggest_difficulty: Option<Value>,
    /// Extranonce the miner is working with.
    downstream: Option<Extranonce>,
}

enum Ev {
    Line(String),
    Closed(String),
}

struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Read lines from `r` into a channel. The receiver is cancel-safe, so
/// the event loop can `select!` over several sockets.
fn spawn_reader<R>(r: R) -> (mpsc::Receiver<Ev>, AbortOnDrop<()>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let (tx, rx) = mpsc::channel(256);
    let task = tokio::spawn(async move {
        let mut br = BufReader::new(r);
        loop {
            let mut line = String::new();
            match br.read_line(&mut line).await {
                Ok(0) => {
                    let _ = tx.send(Ev::Closed("closed (EOF)".into())).await;
                    break;
                }
                Ok(_) => {
                    if tx.send(Ev::Line(line)).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Ev::Closed(format!("side error: {e}"))).await;
                    break;
                }
            }
        }
    });
    (rx, AbortOnDrop(task))
}

/// One upstream session.
struct Leg {
    route: Route,
    w: WriteHalf<PoolStream>,
    rx: mpsc::Receiver<Ev>,
    _reader: AbortOnDrop<()>,
    session: SessionState,
    /// Extranonce translation, once the subscribe answer is in.
    map: Option<ExtranonceMap>,
    /// Redemption each in-flight `mining.submit` on this leg was sent
    /// for, keyed by request id.
    submits: HashMap<i64, Option<RedemptionId>>,
}

impl Leg {
    fn new(route: Route, stream: PoolStream) -> Self {
        let (r, w) = tokio::io::split(stream);
        let (rx, reader) = spawn_reader(r);
        Self {
            route,
            w,
            rx,
            _reader: reader,
            session: SessionState::default(),
            map: None,
            submits: HashMap::new(),
        }
    }

    async fn send(&mut self, line: &str) -> io::Result<()> {
        self.w.write_all(line.as_bytes()).await?;
        self.w.flush().await
    }

    /// A miner line as this leg's pool should see it: redeemer's worker
    /// name, upstream extranonce2.
    fn upstream_line(&self, line: &str) -> String {
        let line = self
            .route
            .rewrite_miner_line(line)
            .unwrap_or_else(|| line.to_string());
        match &self.map {
            Some(m) => m.patch_submit(&line).unwrap_or(line),
            None => line,
        }
    }

    /// Track a submit (already in upstream form) sent on this leg.
    fn note_submit(&mut self, line: &str, ctx: &Ctx) {
        if let SessionEvent::SubmitOpened(id) = self.session.ingest(line, Direction::M2P) {
            if ctx.credit.is_some() {
                let bound = ctx.bound_for(&self.route);
                self.submits.insert(id, bound);
            }
        }
    }

    /// Feed a line from this leg's pool to its session; on an accepted
    /// share, build the leaf, commit it, and credit the redemption the
    /// share was sent for. Returns an accepted share's difficulty.
    fn ingest_pool(
        &mut self,
        line: &str,
        ctx: &Ctx,
        committer: &mut ShareCommitter,
    ) -> Option<f64> {
        self.session.try_absorb_subscribe_response(line);
        match self.session.ingest(line, Direction::P2M) {
            SessionEvent::SubmitRejected { id } => {
                self.submits.remove(&id);
                None
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
                committer.append(header.to_vec(), target_bits, submit.ntime);
                let evidence = ShareEvidence {
                    preimage: header.to_vec(),
                    target_bits,
                    ntime: submit.ntime,
                };
                ctx.metrics.shares_committed.fetch_add(1, Ordering::Relaxed);
                let difficulty = submit
                    .difficulty_at_submit
                    .unwrap_or(crate::credit::DEFAULT_SHARE_DIFFICULTY);
                ctx.metrics.add_accepted_work(difficulty);
                // Credit only shares that made it into the commitment
                // tree, so every credited hash has a leaf behind it
                // (§4.7.2).
                let bound = self.submits.remove(&id).flatten();
                if let Some(credit) = &ctx.credit {
                    credit.credit_accepted(
                        ctx.conn,
                        bound,
                        submit.difficulty_at_submit,
                        evidence,
                        &ctx.metrics,
                    );
                }
                Some(difficulty)
            }
            _ => {
                // A response the session could not turn into a leaf (e.g.
                // the proxy missed the job's `mining.notify`) still
                // settles the submit; drop its binding.
                if !self.submits.is_empty() {
                    if let Some(id) = response_id(line) {
                        self.submits.remove(&id);
                    }
                }
                None
            }
        }
    }
}

/// The previous leg after a switch, kept until `until`.
struct Drain {
    leg: Leg,
    until: Instant,
}

/// A new pool being brought up behind the miner.
struct Handshake {
    leg: Leg,
    need_configure: bool,
    /// `Some(mask)` once `mining.configure` is answered.
    configured: Option<Option<u32>>,
    subscribed: bool,
    authorized: bool,
    difficulty: Option<String>,
    notify: Option<String>,
    /// `mining.suggest_difficulty` params to send once authorized.
    suggest: Option<Value>,
}

enum Stage {
    Dialing(AbortOnDrop<io::Result<PoolStream>>),
    Handshake(Box<Handshake>),
}

struct Switch {
    target: Route,
    started: Instant,
    deadline: Instant,
    stage: Stage,
}

enum SwitchEv {
    Dialed(io::Result<PoolStream>),
    Line(Ev),
    TimedOut,
}

enum Progress {
    Pending,
    Ready,
    Failed(String),
}

/// Why a switch didn't happen.
enum SwitchFail {
    /// New pool unreachable, refused us, or too slow: stay put.
    Pool(String),
    /// The miner can't be moved in place: reconnect it instead.
    Incompatible(String),
}

async fn switch_event(sw: &mut Option<Switch>) -> SwitchEv {
    let Some(sw) = sw.as_mut() else {
        return pending().await;
    };
    let deadline = tokio::time::sleep_until(sw.deadline.into());
    match &mut sw.stage {
        Stage::Dialing(h) => tokio::select! {
            r = &mut h.0 => SwitchEv::Dialed(r.unwrap_or_else(|e| Err(io::Error::other(e)))),
            _ = deadline => SwitchEv::TimedOut,
        },
        Stage::Handshake(hs) => tokio::select! {
            ev = hs.leg.rx.recv() => SwitchEv::Line(ev.unwrap_or(Ev::Closed("closed".into()))),
            _ = deadline => SwitchEv::TimedOut,
        },
    }
}

enum DrainEv {
    Line(String),
    Done,
}

async fn drain_event(d: &mut Option<Drain>) -> DrainEv {
    let Some(d) = d.as_mut() else {
        return pending().await;
    };
    tokio::select! {
        ev = d.leg.rx.recv() => match ev {
            Some(Ev::Line(l)) => DrainEv::Line(l),
            _ => DrainEv::Done,
        },
        _ = tokio::time::sleep_until(d.until.into()) => DrainEv::Done,
    }
}

struct Conn {
    ctx: Ctx,
    cur: Leg,
    old: Option<Drain>,
    switch: Option<Switch>,
    miner: MinerFacts,
    gate: JobGate,
    committer: ShareCommitter,
    pending: PendingSubmits,
    mw: tokio::net::tcp::OwnedWriteHalf,
    /// The miner's hashrate, from accepted shares on any leg: sizes the
    /// starting difficulty asked of the next pool.
    work: WorkRate,
}

/// Forward lines between the miner and its current pool until either
/// side closes or the router kicks the connection, switching pools in
/// place when told to.
async fn relay(
    miner_socket: TcpStream,
    first: Leg,
    mut signal: watch::Receiver<Signal>,
    ctx: Ctx,
) -> Result<()> {
    // `dial` already set TCP_NODELAY on the pool side.
    miner_socket.set_nodelay(true).ok();
    let (mr, mw) = miner_socket.into_split();
    let (mut miner_rx, _miner_reader) = spawn_reader(mr);
    let mut c = Conn {
        ctx,
        cur: first,
        old: None,
        switch: None,
        miner: MinerFacts::default(),
        gate: JobGate::default(),
        committer: ShareCommitter::default(),
        pending: PendingSubmits::default(),
        mw,
        work: WorkRate::new(Instant::now()),
    };
    let mut signal_live = true;

    let started = Instant::now();
    let closed_by: String = loop {
        tokio::select! {
            ev = miner_rx.recv() => match ev {
                Some(Ev::Line(l)) => {
                    if let Err(e) = c.on_miner_line(&l).await {
                        break format!("pool side error: {e}");
                    }
                }
                Some(Ev::Closed(r)) => break format!("miner {r}"),
                None => break "miner closed".into(),
            },
            ev = c.cur.rx.recv() => match ev {
                Some(Ev::Line(l)) => {
                    if let Err(e) = c.on_pool_line(&l).await {
                        break format!("miner side error: {e}");
                    }
                }
                Some(Ev::Closed(r)) => break format!("pool {r}"),
                None => break "pool closed".into(),
            },
            ev = drain_event(&mut c.old) => match ev {
                DrainEv::Line(l) => {
                    if let Err(e) = c.on_old_line(&l).await {
                        break format!("miner side error: {e}");
                    }
                }
                DrainEv::Done => {
                    if let Some(d) = c.old.take() {
                        tracing::debug!(conn = %c.ctx.conn, upstream = d.leg.route.upstream(), "previous pool released");
                    }
                }
            },
            ev = switch_event(&mut c.switch) => {
                if let Some(reason) = c.on_switch_event(ev).await {
                    break reason;
                }
            },
            r = signal.changed(), if signal_live => {
                if r.is_err() {
                    signal_live = false;
                    continue;
                }
                let sig = signal.borrow_and_update().clone();
                match sig {
                    Signal::Stay => {}
                    Signal::Kick => {
                        tracing::info!(
                            conn = %c.ctx.conn,
                            redemption = ?c.cur.route.redemption_id().map(|i| i.to_string()),
                            "route changed; closing miner connection so it reconnects",
                        );
                        break "router kick".into();
                    }
                    Signal::Switch(target) => {
                        if let Err(why) = c.begin_switch(target) {
                            tracing::info!(conn = %c.ctx.conn, reason = %why, "can't switch in place; closing miner connection so it reconnects");
                            break format!("router kick ({why})");
                        }
                    }
                }
            },
        }
    };
    tracing::info!(
        conn = %c.ctx.conn,
        upstream = c.cur.route.upstream(),
        closed_by = %closed_by,
        after_ms = started.elapsed().as_millis() as u64,
        "connection closing",
    );

    if !c.committer.is_empty() {
        let root = c.committer.root().map(hex::encode).unwrap_or_default();
        tracing::info!(
            committed_leaves = c.committer.len(),
            root = %root,
            "share commitment tree finalized for connection",
        );
    }
    Ok(())
}

impl Conn {
    async fn write_miner(&mut self, lines: &[String]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        for l in lines {
            self.mw.write_all(l.as_bytes()).await?;
        }
        self.mw.flush().await
    }

    async fn on_miner_line(&mut self, line: &str) -> io::Result<()> {
        observe_line(line, Direction::M2P, &self.pending, &self.ctx.metrics);
        let v = parse(line);
        let method = v
            .as_ref()
            .and_then(|v| v.get("method"))
            .and_then(Value::as_str);
        let id = v.as_ref().and_then(|v| v.get("id")).and_then(Value::as_i64);
        let params = v.as_ref().and_then(|v| v.get("params")).cloned();
        let m = &mut self.miner;
        match method {
            Some("mining.subscribe") if m.subscribe_id.is_none() => {
                m.subscribe_id = id;
                m.subscribe_params = params;
            }
            Some("mining.configure") if m.configure_id.is_none() => {
                m.configure_id = id;
                m.configure_params = params;
            }
            Some("mining.authorize") if m.authorize_params.is_none() => {
                m.authorize_params = params;
            }
            Some("mining.extranonce.subscribe") => m.extranonce_subscribed = true,
            Some("mining.suggest_difficulty") => m.suggest_difficulty = params,
            _ => {}
        }
        self.gate.on_miner_line(line);

        if method == Some("mining.submit") {
            // A share for one of the previous pool's jobs goes there.
            let job = v
                .as_ref()
                .and_then(|v| v.get("params"))
                .and_then(|p| p.get(1))
                .and_then(Value::as_str);
            let to_old = match (&self.old, job) {
                (Some(d), Some(j)) => {
                    !self.cur.session.jobs.contains_key(j) && d.leg.session.jobs.contains_key(j)
                }
                _ => false,
            };
            if to_old {
                let d = self.old.as_mut().expect("checked above");
                let out = d.leg.upstream_line(line);
                d.leg.note_submit(&out, &self.ctx);
                if d.leg.send(&out).await.is_err() {
                    self.old = None;
                }
                return Ok(());
            }
            let out = self.cur.upstream_line(line);
            self.cur.note_submit(&out, &self.ctx);
            return self.cur.send(&out).await;
        }
        let out = self.cur.upstream_line(line);
        self.cur.send(&out).await
    }

    async fn on_pool_line(&mut self, line: &str) -> io::Result<()> {
        observe_line(line, Direction::P2M, &self.pending, &self.ctx.metrics);
        if let Some(d) = self.cur.ingest_pool(line, &self.ctx, &mut self.committer) {
            self.work.accepted(Instant::now(), d);
        }
        let v = parse(line);
        let method = v
            .as_ref()
            .and_then(|v| v.get("method"))
            .and_then(Value::as_str);
        let id = v.as_ref().and_then(|v| v.get("id")).and_then(Value::as_i64);
        let mut out = line.to_string();

        if method.is_none() {
            if id.is_some_and(|i| PROXY_IDS.contains(&i)) {
                return Ok(()); // late answer to our own handshake
            }
            if id.is_some() && id == self.miner.subscribe_id && self.miner.downstream.is_none() {
                if let Some(l) = self.on_first_subscribe_result(line) {
                    out = l;
                }
            }
            if id.is_some() && id == self.miner.configure_id {
                self.miner.version_mask = v.as_ref().and_then(granted_version_mask);
            }
        }
        match method {
            Some("mining.set_extranonce") => {
                if self.on_pool_set_extranonce() {
                    return Ok(()); // translated; the miner keeps its own
                }
            }
            Some("mining.notify") => {
                if let Some(l) = self.cur.map.as_ref().and_then(|m| m.patch_notify(line)) {
                    out = l;
                }
            }
            _ => {}
        }
        let lines = match self.gate.on_pool_line(&out) {
            PoolLine::Write(lines) => lines,
            PoolLine::Held => {
                tracing::debug!(conn = %self.ctx.conn, "holding pool message until authorize result");
                Vec::new()
            }
        };
        self.write_miner(&lines).await
    }

    /// The first pool answered the miner's subscribe: decide what
    /// extranonce the miner sees. Returns the rewritten answer, if any.
    fn on_first_subscribe_result(&mut self, line: &str) -> Option<String> {
        let up = Extranonce {
            en1: self.cur.session.extranonce1.clone()?,
            en2_size: self.cur.session.extranonce2_size?,
        };
        let virt = if self.ctx.seamless {
            virtual_downstream(&up, self.ctx.conn.0 as u16).and_then(|d| {
                let l = rewrite_subscribe_result(line, &d)?;
                Some((ExtranonceMap::new(up.clone(), d)?, l))
            })
        } else {
            None
        };
        let (map, out) = match virt {
            Some((m, l)) => (m, Some(l)),
            None => (ExtranonceMap::identity(up), None),
        };
        if self.ctx.seamless {
            tracing::info!(
                conn = %self.ctx.conn,
                pool_en1_len = map.upstream().en1.len(),
                pool_en2_size = map.upstream().en2_size,
                miner_en2_size = map.downstream().en2_size,
                translated = !map.is_identity(),
                "miner subscribed",
            );
        }
        self.miner.downstream = Some(map.downstream().clone());
        self.cur.map = Some(map);
        out
    }

    /// The current pool changed its extranonce. Returns true if the
    /// change was absorbed by translation (don't forward it).
    fn on_pool_set_extranonce(&mut self) -> bool {
        let (Some(down), Some(en1), Some(en2_size)) = (
            self.miner.downstream.clone(),
            self.cur.session.extranonce1.clone(),
            self.cur.session.extranonce2_size,
        ) else {
            return false;
        };
        let up = Extranonce { en1, en2_size };
        match ExtranonceMap::new(up.clone(), down) {
            Some(m) if !m.is_identity() => {
                self.cur.map = Some(m);
                true
            }
            _ => {
                self.cur.map = Some(ExtranonceMap::identity(up.clone()));
                self.miner.downstream = Some(up);
                false
            }
        }
    }

    async fn on_old_line(&mut self, line: &str) -> io::Result<()> {
        observe_line(line, Direction::P2M, &self.pending, &self.ctx.metrics);
        let Some(d) = self.old.as_mut() else {
            return Ok(());
        };
        if let Some(diff) = d.leg.ingest_pool(line, &self.ctx, &mut self.committer) {
            self.work.accepted(Instant::now(), diff);
        }
        // Only answers to submits get through; its work is stale.
        let is_response = parse(line).is_some_and(|v| v.get("method").is_none_or(Value::is_null));
        let ours = response_id(line).is_some_and(|i| PROXY_IDS.contains(&i));
        if is_response && !ours {
            self.write_miner(&[terminated(line)]).await?;
        }
        Ok(())
    }

    /// Start moving to `target`. `Err` means it can't be done in place.
    fn begin_switch(&mut self, target: Route) -> Result<(), String> {
        if !self.gate.is_open() || self.miner.downstream.is_none() || self.cur.map.is_none() {
            return Err("miner not authorized yet".into());
        }
        if target == self.cur.route {
            self.switch = None;
            if let Some(r) = &self.ctx.router {
                r.switched(self.ctx.conn, target);
            }
            return Ok(());
        }
        let upstream = target.upstream().to_string();
        let task = tokio::spawn(async move { dial(&upstream).await });
        let now = Instant::now();
        // Replaces (and aborts) any switch already in progress.
        self.switch = Some(Switch {
            target,
            started: now,
            deadline: now + SWITCH_TIMEOUT,
            stage: Stage::Dialing(AbortOnDrop(task)),
        });
        Ok(())
    }

    /// Advance the switch in progress. `Some(reason)` closes the
    /// connection.
    async fn on_switch_event(&mut self, ev: SwitchEv) -> Option<String> {
        let progress = match ev {
            SwitchEv::TimedOut => Progress::Failed(format!("no job within {SWITCH_TIMEOUT:?}")),
            SwitchEv::Dialed(Err(e)) => Progress::Failed(format!("dial: {e}")),
            SwitchEv::Dialed(Ok(stream)) => self.start_handshake(stream).await,
            SwitchEv::Line(Ev::Closed(r)) => Progress::Failed(format!("pool {r}")),
            SwitchEv::Line(Ev::Line(l)) => self.on_handshake_line(&l).await,
        };
        let fail = match progress {
            Progress::Pending => return None,
            Progress::Failed(why) => SwitchFail::Pool(why),
            Progress::Ready => match self.cut_over().await {
                Ok(()) => return None,
                Err(f) => f,
            },
        };
        let sw = self.switch.take()?;
        match fail {
            SwitchFail::Pool(why) => {
                tracing::warn!(
                    conn = %self.ctx.conn,
                    upstream = sw.target.upstream(),
                    reason = %why,
                    "switch to new pool failed; miner stays on current pool",
                );
                if let Some(r) = &self.ctx.router {
                    r.switch_failed(self.ctx.conn, &sw.target);
                }
                // Leaving the default pool can wait; leaving a finished
                // redemption can't, so if even the default pool failed,
                // reconnect.
                if sw.target.redemption_id().is_none() {
                    return Some(format!("router kick (default pool switch failed: {why})"));
                }
                None
            }
            SwitchFail::Incompatible(why) => {
                tracing::info!(conn = %self.ctx.conn, reason = %why, "can't switch in place; closing miner connection so it reconnects");
                Some(format!("router kick ({why})"))
            }
        }
    }

    async fn start_handshake(&mut self, stream: PoolStream) -> Progress {
        let Some(sw) = self.switch.as_mut() else {
            return Progress::Pending;
        };
        let mut leg = Leg::new(sw.target.clone(), stream);
        let m = &self.miner;
        let mut lines = Vec::new();
        if let Some(p) = &m.configure_params {
            lines.push(json!({"id": ID_CONFIGURE, "method": "mining.configure", "params": p}));
        }
        // Only the user agent: a session id would be the old pool's.
        let ua: Vec<Value> = m
            .subscribe_params
            .as_ref()
            .and_then(|p| p.get(0))
            .cloned()
            .into_iter()
            .collect();
        lines.push(json!({"id": ID_SUBSCRIBE, "method": "mining.subscribe", "params": ua}));
        // Start the new pool near the miner's measured difficulty (see
        // [`super::startdiff`]); without an estimate yet, pass on the
        // miner's own suggestion.
        let start = self.work.start_difficulty(Instant::now());
        let suggest = start
            .map(|d| json!([d]))
            .or_else(|| m.suggest_difficulty.clone());
        let auth = match &sw.target {
            Route::Redemption {
                worker_name,
                worker_password,
                ..
            } => match start {
                Some(d) => json!([worker_name, password_with_difficulty(worker_password, d)]),
                None => json!([worker_name, worker_password]),
            },
            Route::Default { .. } => m.authorize_params.clone().unwrap_or_else(|| json!([])),
        };
        lines.push(json!({"id": ID_AUTHORIZE, "method": "mining.authorize", "params": auth}));
        for l in lines {
            if let Err(e) = leg.send(&format!("{l}\n")).await {
                return Progress::Failed(format!("write: {e}"));
            }
        }
        sw.stage = Stage::Handshake(Box::new(Handshake {
            leg,
            need_configure: m.configure_params.is_some(),
            configured: None,
            subscribed: false,
            authorized: false,
            difficulty: None,
            notify: None,
            suggest,
        }));
        Progress::Pending
    }

    async fn on_handshake_line(&mut self, line: &str) -> Progress {
        let Some(Switch {
            stage: Stage::Handshake(hs),
            ..
        }) = self.switch.as_mut()
        else {
            return Progress::Pending;
        };
        if let Some(d) = hs.leg.ingest_pool(line, &self.ctx, &mut self.committer) {
            self.work.accepted(Instant::now(), d);
        }
        let Some(v) = parse(line) else {
            return Progress::Pending;
        };
        match v.get("method").and_then(Value::as_str) {
            Some("mining.set_difficulty") => hs.difficulty = Some(terminated(line)),
            Some("mining.notify") => hs.notify = Some(terminated(line)),
            Some(_) => {}
            None => match v.get("id").and_then(Value::as_i64) {
                Some(ID_CONFIGURE) => hs.configured = Some(granted_version_mask(&v)),
                Some(ID_SUBSCRIBE) => {
                    if !v.get("result").is_some_and(Value::is_array) {
                        return Progress::Failed("subscribe refused".into());
                    }
                    hs.subscribed = true;
                }
                Some(ID_AUTHORIZE) => {
                    if v.get("result").and_then(Value::as_bool) != Some(true) {
                        let err = v.get("error").cloned().unwrap_or(Value::Null);
                        return Progress::Failed(format!("authorize refused: {err}"));
                    }
                    hs.authorized = true;
                    // After authorize: ckpool ignores a suggestion from a
                    // client that isn't authorized yet.
                    if let Some(p) = hs.suggest.take() {
                        let l = json!({"id": ID_SUGGEST, "method": "mining.suggest_difficulty", "params": p});
                        if let Err(e) = hs.leg.send(&format!("{l}\n")).await {
                            return Progress::Failed(format!("write: {e}"));
                        }
                    }
                }
                _ => {}
            },
        }
        let ready = hs.subscribed
            && hs.authorized
            && hs.notify.is_some()
            && (!hs.need_configure || hs.configured.is_some());
        if ready {
            Progress::Ready
        } else {
            Progress::Pending
        }
    }

    /// The new pool is ready: hand the miner its work and make it the
    /// current leg.
    async fn cut_over(&mut self) -> Result<(), SwitchFail> {
        let incompatible = SwitchFail::Incompatible;
        let Some(Switch {
            stage: Stage::Handshake(hs),
            ..
        }) = self.switch.as_ref()
        else {
            return Ok(());
        };
        let down = self
            .miner
            .downstream
            .clone()
            .ok_or_else(|| incompatible("miner not subscribed".into()))?;
        let (Some(en1), Some(en2_size)) = (
            hs.leg.session.extranonce1.clone(),
            hs.leg.session.extranonce2_size,
        ) else {
            return Err(SwitchFail::Pool("no extranonce in subscribe answer".into()));
        };
        let up = Extranonce { en1, en2_size };
        if let Some(mask) = self.miner.version_mask.filter(|m| *m != 0) {
            let granted = hs.configured.flatten().unwrap_or(0);
            if mask & !granted != 0 {
                return Err(incompatible(format!(
                    "new pool grants version-rolling mask {granted:08x}, miner uses {mask:08x}"
                )));
            }
        }
        let mut lines = Vec::new();
        let map = match ExtranonceMap::new(up.clone(), down.clone()) {
            Some(m) => m,
            None if self.miner.extranonce_subscribed => {
                lines.push(set_extranonce_line(&up));
                ExtranonceMap::identity(up.clone())
            }
            None => {
                return Err(incompatible(format!(
                    "new pool's extranonce2 is {en2_size} bytes, miner needs {}",
                    down.width()
                )))
            }
        };

        let Some(Switch {
            target,
            started,
            stage: Stage::Handshake(hs),
            ..
        }) = self.switch.take()
        else {
            unreachable!("checked above");
        };
        let mut hs = *hs;
        if let Some(d) = hs.difficulty.take() {
            lines.push(d);
        }
        let notify = hs.notify.take().expect("ready implies a job");
        let notify = map.patch_notify(&notify).unwrap_or(notify);
        lines.push(force_clean(&notify).unwrap_or(notify));

        self.miner.downstream = Some(map.downstream().clone());
        hs.leg.map = Some(map);
        let prev = std::mem::replace(&mut self.cur, hs.leg);
        self.old = Some(Drain {
            leg: prev,
            until: Instant::now() + DRAIN_FOR,
        });
        if let Some(r) = &self.ctx.router {
            r.switched(self.ctx.conn, target.clone());
        }
        tracing::info!(
            conn = %self.ctx.conn,
            redemption = ?target.redemption_id().map(|i| i.to_string()),
            upstream = target.upstream(),
            from = self.old.as_ref().map(|d| d.leg.route.upstream()).unwrap_or_default(),
            switch_ms = started.elapsed().as_millis() as u64,
            "switched pool without reconnecting the miner",
        );
        // A failed write here surfaces on the next miner write; the leg
        // swap has happened either way.
        if let Err(e) = self.write_miner(&lines).await {
            tracing::warn!(conn = %self.ctx.conn, error = %e, "writing new pool's job to miner");
        }
        Ok(())
    }
}

/// `version-rolling.mask` a `mining.configure` answer grants, if any.
fn granted_version_mask(v: &Value) -> Option<u32> {
    let r = v.get("result")?;
    if r.get("version-rolling").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    u32::from_str_radix(r.get("version-rolling.mask")?.as_str()?, 16).ok()
}

fn parse(line: &str) -> Option<Value> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str(t).ok()
}

fn terminated(line: &str) -> String {
    let mut l = line.trim_end_matches(['\r', '\n']).to_string();
    l.push('\n');
    l
}

/// `id` of a Stratum response line (one with no `method`).
fn response_id(line: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("method").is_some_and(|m| !m.is_null()) {
        return None;
    }
    v.get("id").and_then(|x| x.as_i64())
}
