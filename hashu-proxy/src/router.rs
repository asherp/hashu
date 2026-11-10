//! Pool routing controller. ARCHITECTURE.md §4.2, §4.6.
//!
//! Decides which upstream pool each miner connection talks to:
//!
//! - **No active redemption**: the operator's default pool, worker name
//!   passed through untouched.
//! - **Active redemption(s)**: the redeemer's pool, with the miner's
//!   worker name rewritten to the redeemer's `worker_name` in
//!   `mining.authorize` and `mining.submit`.
//!
//! To change a live connection's route the router *signals* it (see
//! [`Signal`]). In [`SwitchMode::Seamless`] (the default) the connection
//! keeps the miner's socket open, dials and authorizes the new pool
//! behind it, and cuts over with a clean job, translating extranonces so
//! the miner never re-subscribes (see [`crate::stratum::extranonce`]). It
//! reports back with [`Router::switched`] or [`Router::switch_failed`]. A
//! connection that cannot switch in place (not yet authorized, or the
//! new pool's extranonce or version-rolling mask doesn't fit the miner)
//! closes the socket instead, as [`SwitchMode::Reconnect`] always does:
//! the miner reconnects and [`Router::assign`] picks the new route.
//!
//! Strategy is *dedicated assignment* (§4.6 "v1 alternative"): each
//! connection serves exactly one redemption, picked by [`pick_route`]:
//!
//! - **Size matching.** Each connection's hashrate is an EWMA of its
//!   accepted share work ([`RIG_RATE_TAU`]). An order can usefully absorb
//!   `remaining / TARGET_RUN − assigned rate` more hashrate: anything past
//!   that just finishes it sooner than [`TARGET_RUN`] and overshoots (about
//!   one share per rig plus the switch drain). A rig goes to the order with
//!   the *smallest* such room that still fits it (best fit), so large rigs
//!   land on large orders and small rigs on small ones. A rig too big for
//!   every order goes where it overshoots least.
//! - **Cap.** An order whose assigned rigs already finish it within
//!   [`TARGET_RUN`] takes no more. If every order is capped, extra rigs
//!   stay on (or go back to) the default pool.
//! - **Fairness.** An order with no rig waiting longer than
//!   [`STARVE_AFTER`] is served first (oldest first), then orders with no
//!   rig at all, then the rest.
//! - **Fallback.** Until a rig has a hashrate estimate ([`MIN_RATE_SHARES`]
//!   shares over [`MIN_RATE_WINDOW`]) it goes to the order with the fewest
//!   connections, oldest first, as before. With one miner, redemptions are
//!   served in melt order.
//!
//! The proportional EMA-weighted split in §4.6 stays a TODO.
//!
//! The router learns about redemptions by polling
//! [`RedemptionLedger::active`] (see [`Router::reconcile`]). The melt
//! path opens redemptions there and the share-crediting path completes
//! them, so the ledger is the only interface between the three.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hashu_core::redemption::{Redemption, RedemptionId, RedemptionLedger};
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::credit::{ConnectionId, RedemptionBinding};
use crate::home::{HomeConfig, MinerIdentity};
use crate::upstream::normalize_upstream;

/// How long a connection that fell back to the default pool (redeemer's
/// pool unreachable) stays there before it is retried. Stops a dead pool
/// URL from turning into a reconnect loop on every reconcile tick.
pub const FALLBACK_HOLD: Duration = Duration::from_secs(60);

/// How long the rigs on an order should take to finish it. Orders whose
/// assigned hashrate would finish sooner are capped, and a rig fits an
/// order only if it would not finish it sooner on its own share of the
/// order. A minute keeps per-switch costs (a 5 s drain, about one share of
/// overshoot per rig) small against the run.
pub const TARGET_RUN: Duration = Duration::from_secs(60);

/// An order with no rig for this long jumps ahead of size matching, so a
/// small order is not left waiting behind big ones forever.
pub const STARVE_AFTER: Duration = Duration::from_secs(120);

/// Time constant of the per-connection hashrate EWMA. Long enough to
/// smooth share noise, short enough to notice a rig going offline.
pub const RIG_RATE_TAU: Duration = Duration::from_secs(120);

/// Accepted shares a connection needs before its hashrate is trusted.
pub const MIN_RATE_SHARES: u32 = 4;

/// How long a connection must have been observed before its hashrate is
/// trusted.
pub const MIN_RATE_WINDOW: Duration = Duration::from_secs(10);

/// Exponentially weighted hashrate of one connection, from its accepted
/// shares (`difficulty × 2^32` hashes each). Rate is the decayed work
/// divided by the decayed time observed, so it is unbiased from the first
/// share and falls toward zero when shares stop.
#[derive(Debug, Clone)]
struct RigRate {
    since: Instant,
    last: Instant,
    /// Work as of `last`, decayed with [`RIG_RATE_TAU`].
    decayed_hashes: f64,
    shares: u32,
}

impl RigRate {
    fn new(since: Instant) -> Self {
        Self {
            since,
            last: since,
            decayed_hashes: 0.0,
            shares: 0,
        }
    }

    fn observe(&mut self, hashes: f64, at: Instant) {
        let at = at.max(self.last);
        let tau = RIG_RATE_TAU.as_secs_f64();
        let dt = (at - self.last).as_secs_f64();
        self.decayed_hashes = self.decayed_hashes * (-dt / tau).exp() + hashes;
        self.last = at;
        self.shares = self.shares.saturating_add(1);
    }

    /// Hashes per second as of `now`, or `None` until there is enough to
    /// go on.
    fn rate(&self, now: Instant) -> Option<f64> {
        let elapsed = now.saturating_duration_since(self.since);
        if self.shares < MIN_RATE_SHARES || elapsed < MIN_RATE_WINDOW {
            return None;
        }
        let tau = RIG_RATE_TAU.as_secs_f64();
        let idle = now.saturating_duration_since(self.last).as_secs_f64();
        let work = self.decayed_hashes * (-idle / tau).exp();
        let time = tau * (1.0 - (-elapsed.as_secs_f64() / tau).exp());
        (time > 0.0).then(|| work / time)
    }
}

/// Where a connection's shares go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Operator's default pool; lines pass through unmodified.
    Default { upstream: String },
    /// The miner's own home pool (see [`crate::home`]): its own user,
    /// its password without the `pool=` option. Credits nothing.
    Home {
        upstream: String,
        worker_name: String,
        worker_password: String,
    },
    /// A redeemer's pool; worker name is rewritten to `worker_name` and
    /// the authorize password to `worker_password`.
    Redemption {
        id: RedemptionId,
        upstream: String,
        worker_name: String,
        worker_password: String,
    },
}

impl Route {
    /// Upstream to dial: `host:port`, or `tls://host:port` (see
    /// [`crate::upstream::dial`]).
    pub fn upstream(&self) -> &str {
        match self {
            Route::Default { upstream }
            | Route::Home { upstream, .. }
            | Route::Redemption { upstream, .. } => upstream,
        }
    }

    /// The redemption this route serves, if any. Shares on a `None`
    /// route are the operator's and credit nothing.
    pub fn redemption_id(&self) -> Option<RedemptionId> {
        match self {
            Route::Default { .. } | Route::Home { .. } => None,
            Route::Redemption { id, .. } => Some(*id),
        }
    }

    /// Rewrite a miner→pool line for this route. On a redemption route,
    /// `mining.authorize` and `mining.submit` carry the redeemer's worker
    /// name instead of the miner's. Anything else (and every line on the
    /// default route) is returned as-is.
    pub fn rewrite_miner_line(&self, line: &str) -> Option<String> {
        let (worker_name, worker_password) = self.credentials()?;
        rewrite_worker(line, worker_name, worker_password)
    }

    /// Worker name and password this route authorizes with, or `None`
    /// on the default route (the miner's own are passed through).
    pub fn credentials(&self) -> Option<(&str, &str)> {
        match self {
            Route::Default { .. } => None,
            Route::Home {
                worker_name,
                worker_password,
                ..
            }
            | Route::Redemption {
                worker_name,
                worker_password,
                ..
            } => Some((worker_name, worker_password)),
        }
    }
}

/// Replace `params[0]` of `mining.authorize` / `mining.submit` with
/// `worker`, and the authorize password (`params[1]`) with `password`. Returns `None` if the line is not one of those (or does not
/// parse), meaning "forward unchanged". The rewritten line keeps the
/// trailing newline so it can be written straight to the socket.
pub fn rewrite_worker(line: &str, worker: &str, password: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(line.trim()).ok()?;
    let method = v.get("method")?.as_str()?;
    let is_authorize = method == "mining.authorize";
    if !is_authorize && method != "mining.submit" {
        return None;
    }
    let params = v.get_mut("params")?.as_array_mut()?;
    let first = params.first_mut()?;
    *first = Value::String(worker.to_string());
    if is_authorize {
        // The miner's password was meant for the default pool; send the
        // redeemer's (`x` unless they gave one).
        match params.get_mut(1) {
            Some(pw) => *pw = Value::String(password.to_string()),
            None => params.push(Value::String(password.to_string())),
        }
    }
    let mut out = serde_json::to_string(&v).ok()?;
    out.push('\n');
    Some(out)
}

/// Turn a melt request's `pool_url` into an upstream the proxy can dial
/// (`host:port`, or `tls://host:port` for `stratum+ssl://`), or `None` if
/// it can't (no port, a path, or an unknown scheme such as Stratum V2).
/// See [`crate::upstream::normalize_upstream`].
pub fn upstream_for_pool_url(pool_url: &str) -> Option<String> {
    normalize_upstream(pool_url)
}

/// How the router moves a live connection to a new route.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SwitchMode {
    /// Swap the upstream behind the open miner connection (~one pool
    /// round trip), falling back to [`SwitchMode::Reconnect`] for a
    /// connection that can't be switched in place.
    #[default]
    Seamless,
    /// Close the miner's connection; it reconnects onto the new route.
    Reconnect,
}

impl std::str::FromStr for SwitchMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "seamless" => Ok(Self::Seamless),
            "reconnect" | "kick" => Ok(Self::Reconnect),
            other => Err(format!(
                "unknown switch mode {other:?} (expected seamless or reconnect)"
            )),
        }
    }
}

/// What the router wants a live connection to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Signal {
    /// Keep going.
    #[default]
    Stay,
    /// Close the miner's connection so it reconnects.
    Kick,
    /// Move to this route without closing the miner's connection, then
    /// call [`Router::switched`] or [`Router::switch_failed`].
    Switch(Route),
}

/// Bookkeeping for one live connection.
struct Binding {
    route: Route,
    signal: watch::Sender<Signal>,
    /// Set when this connection fell back to the default pool (or failed
    /// to reach a redeemer's pool in a seamless switch).
    fell_back_at: Option<Instant>,
    /// Target of a seamless switch in progress.
    pending: Option<Route>,
    /// Hashrate estimate from this connection's accepted shares.
    rate: RigRate,
    /// Where this connection goes while no order needs it: its home pool
    /// (see [`crate::home`]) or the operator's default pool.
    idle: Route,
    /// Supplier key from the miner's authorize, once seen.
    supplier: Option<String>,
    /// May be borrowed for redemptions (see
    /// [`crate::home::HomeConfig::operator_worker`]).
    borrowable: bool,
}

impl Binding {
    /// The route this connection counts towards: where it is heading if
    /// a switch is in progress, else where it is.
    fn effective(&self) -> &Route {
        self.pending.as_ref().unwrap_or(&self.route)
    }

    fn holding(&self) -> bool {
        self.fell_back_at
            .is_some_and(|t| t.elapsed() < FALLBACK_HOLD)
    }

    /// Clear the signal without waking the connection.
    fn quiet(&self) {
        self.signal.send_if_modified(|s| {
            *s = Signal::Stay;
            false
        });
    }
}

#[derive(Default)]
struct Inner {
    conns: HashMap<ConnectionId, Binding>,
    /// When each active redemption last had no rig heading to it, or was
    /// first seen. Drives [`STARVE_AFTER`].
    unserved_since: HashMap<RedemptionId, Instant>,
    /// Work each supplier delivered to redemptions, in hashes.
    delivered: HashMap<String, f64>,
}

/// Shared routing state. One per proxy process.
pub struct Router {
    default_upstream: String,
    ledger: Arc<RedemptionLedger>,
    mode: SwitchMode,
    home: HomeConfig,
    inner: Mutex<Inner>,
}

/// What a new connection gets from [`Router::assign`].
pub struct Assignment {
    pub conn: ConnectionId,
    pub route: Route,
    /// Changes when the router wants this connection to move: closed so
    /// the miner reconnects ([`Signal::Kick`]), or switched in place
    /// ([`Signal::Switch`]).
    pub signal: watch::Receiver<Signal>,
}

impl Router {
    pub fn new(default_upstream: impl Into<String>, ledger: Arc<RedemptionLedger>) -> Self {
        Self {
            default_upstream: default_upstream.into(),
            ledger,
            mode: SwitchMode::default(),
            home: HomeConfig::off(),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Give miners home pools (see [`crate::home`]). Off by default.
    pub fn with_home_pools(mut self, cfg: HomeConfig) -> Self {
        self.home = cfg;
        self
    }

    pub fn home_config(&self) -> &HomeConfig {
        &self.home
    }

    /// Use `mode` to move live connections (default
    /// [`SwitchMode::Seamless`]).
    pub fn with_switch_mode(mut self, mode: SwitchMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn switch_mode(&self) -> SwitchMode {
        self.mode
    }

    pub fn ledger(&self) -> &Arc<RedemptionLedger> {
        &self.ledger
    }

    pub fn default_upstream(&self) -> &str {
        &self.default_upstream
    }

    /// Pick a route for a newly accepted miner connection.
    pub fn assign(&self, conn: ConnectionId) -> Assignment {
        self.assign_with(conn, None)
    }

    /// Like [`Router::assign`], with the `params` of the miner's
    /// `mining.authorize` when it has already sent it: a miner with a
    /// home pool idles there instead of on the default pool.
    pub fn assign_with(&self, conn: ConnectionId, authorize: Option<&Value>) -> Assignment {
        let identity = authorize.and_then(|p| MinerIdentity::from_params(p, &self.home));
        if let Some(why) = identity.as_ref().and_then(|i| i.refused_pool.as_ref()) {
            tracing::warn!(%conn, reason = %why, "ignoring the miner's pool= option");
        }
        let idle = self.idle_route(identity.as_ref());
        let borrowable = self.borrowable(identity.as_ref());
        let active = if borrowable {
            self.routable_active()
        } else {
            Vec::new()
        };
        let now = Instant::now();
        let mut inner = self.lock();
        inner.track_unserved(&self.routable_active(), now);
        let route = pick_route(&active, &inner, None, now, &idle);
        let (tx, rx) = watch::channel(Signal::Stay);
        inner.conns.insert(
            conn,
            Binding {
                route: route.clone(),
                signal: tx,
                fell_back_at: None,
                pending: None,
                rate: RigRate::new(now),
                idle,
                supplier: identity.map(|i| i.supplier),
                borrowable,
            },
        );
        match &route {
            Route::Redemption { id, upstream, .. } => {
                tracing::info!(%conn, redemption = %id, %upstream, "routing connection to redeemer's pool");
            }
            Route::Home {
                upstream,
                worker_name,
                ..
            } => {
                tracing::info!(%conn, %upstream, worker = %worker_name, "routing connection to the miner's home pool");
            }
            Route::Default { .. } => {}
        }
        Assignment {
            conn,
            route,
            signal: rx,
        }
    }

    /// The redeemer's pool could not be dialed: put this connection on
    /// the default pool instead so the miner keeps working. The
    /// redemption stays open and the next connection tries it again.
    /// "Default" here is the connection's idle route: its home pool if it
    /// has one.
    pub fn fall_back_to_default(&self, conn: ConnectionId) -> Route {
        let mut inner = self.lock();
        match inner.conns.get_mut(&conn) {
            Some(b) => {
                b.route = b.idle.clone();
                b.fell_back_at = Some(Instant::now());
                b.route.clone()
            }
            None => self.idle_route(None),
        }
    }

    /// The miner's authorize arrived after the connection was placed
    /// (firmware that waits for the subscribe answer): remember who it
    /// is, and its home pool for when it next goes idle.
    pub fn identify(&self, conn: ConnectionId, authorize: &Value) {
        let Some(id) = MinerIdentity::from_params(authorize, &self.home) else {
            return;
        };
        let idle = self.idle_route(Some(&id));
        let borrowable = self.borrowable(Some(&id));
        if let Some(b) = self.lock().conns.get_mut(&conn) {
            b.borrowable = borrowable;
            if b.supplier.is_none() {
                b.supplier = Some(id.supplier);
            }
            if matches!(b.idle, Route::Default { .. }) && matches!(idle, Route::Home { .. }) {
                tracing::info!(%conn, upstream = idle.upstream(), "miner has a home pool; it goes there after its next redemption or reconnect");
                b.idle = idle;
            }
        }
    }

    /// `Some(reason)` if a miner with this authorize must be refused (see
    /// [`MinerIdentity::refusal`]).
    pub fn refusal(&self, authorize: &Value) -> Option<&'static str> {
        MinerIdentity::from_params(authorize, &self.home)?.refusal(&self.home)
    }

    /// Whether a miner may be borrowed for redemptions. With Hashu's own
    /// rigs set, a miner not yet identified is not.
    fn borrowable(&self, identity: Option<&MinerIdentity>) -> bool {
        match identity {
            Some(i) => i.borrowable(&self.home),
            None => self.home.operator_worker.is_none(),
        }
    }

    /// Where a miner with `identity` goes while no order needs it.
    fn idle_route(&self, identity: Option<&MinerIdentity>) -> Route {
        match identity.and_then(|i| i.home.as_ref()) {
            Some(h) => Route::Home {
                upstream: h.upstream.clone(),
                worker_name: h.worker_name.clone(),
                worker_password: h.worker_password.clone(),
            },
            None => Route::Default {
                upstream: self.default_upstream.clone(),
            },
        }
    }

    /// Per-supplier view: live connections, their summed hashrate, and
    /// the work delivered to redemptions since the proxy started.
    pub fn suppliers(&self) -> Vec<SupplierView> {
        let now = Instant::now();
        let inner = self.lock();
        let mut out: HashMap<String, SupplierView> = HashMap::new();
        let view = |k: &str| SupplierView {
            supplier: k.to_string(),
            connections: 0,
            hashrate: 0.0,
            home_upstream: None,
            delivered_hashes: 0.0,
        };
        for b in inner.conns.values() {
            let Some(k) = &b.supplier else { continue };
            let v = out.entry(k.clone()).or_insert_with(|| view(k));
            v.connections += 1;
            v.hashrate += b.rate.rate(now).unwrap_or(0.0);
            if let Route::Home { upstream, .. } = &b.idle {
                v.home_upstream = Some(upstream.clone());
            }
        }
        for (k, h) in &inner.delivered {
            out.entry(k.clone())
                .or_insert_with(|| view(k))
                .delivered_hashes = *h;
        }
        let mut v: Vec<SupplierView> = out.into_values().collect();
        v.sort_by(|a, b| {
            b.delivered_hashes
                .total_cmp(&a.delivered_hashes)
                .then(a.supplier.cmp(&b.supplier))
        });
        v
    }

    /// Forget a connection once it has closed.
    pub fn release(&self, conn: ConnectionId) {
        self.lock().conns.remove(&conn);
    }

    /// Record an accepted share of bdiff `difficulty` from `conn`, on any
    /// route, for its hashrate estimate. A share sent for a redemption
    /// (`bound_to`) also counts as work delivered by the connection's
    /// supplier.
    pub fn observe_share(
        &self,
        conn: ConnectionId,
        difficulty: f64,
        bound_to: Option<RedemptionId>,
    ) {
        self.observe_share_at(conn, difficulty, Instant::now(), bound_to);
    }

    fn observe_share_at(
        &self,
        conn: ConnectionId,
        difficulty: f64,
        at: Instant,
        bound_to: Option<RedemptionId>,
    ) {
        let hashes = hashu_core::redemption::hashes_for_difficulty(difficulty) as f64;
        let mut inner = self.lock();
        let Some(b) = inner.conns.get_mut(&conn) else {
            return;
        };
        b.rate.observe(hashes, at);
        if bound_to.is_some() {
            if let Some(k) = b.supplier.clone() {
                *inner.delivered.entry(k).or_default() += hashes;
            }
        }
    }

    /// Estimated hashrate of a live connection in hashes per second, once
    /// it has enough shares (see [`MIN_RATE_SHARES`]).
    pub fn hashrate_of(&self, conn: ConnectionId) -> Option<f64> {
        self.lock()
            .conns
            .get(&conn)
            .and_then(|b| b.rate.rate(Instant::now()))
    }

    /// Current route of a live connection.
    pub fn route_of(&self, conn: ConnectionId) -> Option<Route> {
        self.lock().conns.get(&conn).map(|b| b.route.clone())
    }

    /// A seamless switch finished: `conn` now sends its shares to
    /// `route`.
    pub fn switched(&self, conn: ConnectionId, route: Route) {
        if let Some(b) = self.lock().conns.get_mut(&conn) {
            if route.redemption_id().is_some() {
                b.fell_back_at = None;
            }
            b.route = route;
            b.pending = None;
            b.quiet();
        }
    }

    /// A seamless switch to `target` failed and `conn` stayed where it
    /// was. A redeemer's pool that failed is not retried on this
    /// connection for [`FALLBACK_HOLD`].
    pub fn switch_failed(&self, conn: ConnectionId, target: &Route) {
        if let Some(b) = self.lock().conns.get_mut(&conn) {
            b.pending = None;
            if target.redemption_id().is_some() {
                b.fell_back_at = Some(Instant::now());
            }
            b.quiet();
        }
    }

    /// Compare live connections against the ledger and signal any whose
    /// route is stale:
    ///
    /// - bound to a redemption that has completed (or vanished) → to the
    ///   next redemption waiting for hashrate, else the default pool;
    /// - on the default pool while some redemption is waiting for
    ///   hashrate → onto that redemption.
    ///
    /// In [`SwitchMode::Seamless`] the connection is told where to go
    /// ([`Signal::Switch`]); in [`SwitchMode::Reconnect`] it is kicked and
    /// [`Router::assign`] places it on reconnect. A connection already
    /// switching or kicked is left alone. Returns how many connections
    /// were signalled.
    pub fn reconcile(&self) -> usize {
        self.reconcile_at(Instant::now())
    }

    fn reconcile_at(&self, now: Instant) -> usize {
        let active = self.routable_active();
        let mut inner = self.lock();
        inner.track_unserved(&active, now);
        // Place the biggest rigs first so they claim the big orders; rigs
        // without an estimate go last, oldest connection first.
        let mut ids: Vec<(ConnectionId, Option<f64>)> = inner
            .conns
            .iter()
            .map(|(c, b)| (*c, b.rate.rate(now)))
            .collect();
        ids.sort_by(|a, b| match (a.1, b.1) {
            (Some(x), Some(y)) => y.total_cmp(&x).then(a.0.cmp(&b.0)),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => a.0.cmp(&b.0),
        });
        let mut signalled = 0;
        for (conn, _) in ids {
            let target = {
                let b = &inner.conns[&conn];
                if b.pending.is_some() || *b.signal.borrow() == Signal::Kick {
                    continue;
                }
                match &b.route {
                    Route::Redemption { id, .. } => {
                        if b.borrowable && active.iter().any(|r| r.id == *id) {
                            continue;
                        }
                        if b.holding() || !b.borrowable {
                            b.idle.clone()
                        } else {
                            pick_route(&active, &inner, Some(conn), now, &b.idle)
                        }
                    }
                    Route::Default { .. } | Route::Home { .. } => {
                        if active.is_empty() || b.holding() || !b.borrowable {
                            continue;
                        }
                        let t = pick_route(&active, &inner, Some(conn), now, &b.idle);
                        // Every order is capped: stay on the default pool.
                        if t.redemption_id().is_none() {
                            continue;
                        }
                        t
                    }
                }
            };
            let b = inner.conns.get_mut(&conn).expect("listed above");
            let from = b.route.redemption_id().map(|i| i.to_string());
            match self.mode {
                SwitchMode::Reconnect => {
                    let _ = b.signal.send(Signal::Kick);
                    tracing::info!(%conn, ?from, "kicking connection to re-route");
                }
                SwitchMode::Seamless => {
                    let to = target.redemption_id().map(|i| i.to_string());
                    tracing::info!(%conn, ?from, ?to, upstream = target.upstream(), "switching connection to new route");
                    b.pending = Some(target.clone());
                    let _ = b.signal.send(Signal::Switch(target));
                }
            }
            signalled += 1;
        }
        signalled
    }

    /// Call [`Router::reconcile`] every `every`, and right away whenever
    /// `completions` (the share crediter's completion signal) fires, until
    /// the task is dropped. Polling catches newly opened redemptions; the
    /// signal releases a filled redemption's miners without waiting a tick.
    pub async fn run_reconciler(
        self: Arc<Self>,
        every: Duration,
        mut completions: Option<broadcast::Receiver<RedemptionId>>,
    ) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            match completions.as_mut() {
                Some(rx) => tokio::select! {
                    _ = tick.tick() => {},
                    r = rx.recv() => {
                        if let Err(broadcast::error::RecvError::Closed) = r {
                            completions = None;
                        }
                    }
                },
                None => {
                    tick.tick().await;
                }
            }
            self.reconcile();
        }
    }

    /// Active redemptions the proxy can actually dial, oldest first.
    fn routable_active(&self) -> Vec<Redemption> {
        self.ledger
            .active()
            .into_iter()
            .filter(|r| {
                let ok = upstream_for_pool_url(&r.pool_url).is_some();
                if !ok {
                    tracing::warn!(redemption = %r.id, pool_url = %r.pool_url, "redemption pool URL not routable (needs stratum+tcp:// or stratum+ssl:// with a port); skipping");
                }
                ok
            })
            .collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Which redemption a connection's shares belong to: the one it was
/// routed to (fixed for the connection's life), or none on the default
/// pool, including after a fallback.
impl RedemptionBinding for Router {
    fn current(&self, conn: ConnectionId) -> Option<RedemptionId> {
        self.lock()
            .conns
            .get(&conn)
            .and_then(|b| b.route.redemption_id())
    }

    fn observe_share(&self, conn: ConnectionId, difficulty: f64, bound_to: Option<RedemptionId>) {
        Router::observe_share(self, conn, difficulty, bound_to);
    }
}

impl Inner {
    /// Keep [`Inner::unserved_since`] in step with `active`: start the
    /// clock for an order that has no rig heading to it, stop it once one
    /// is, forget orders that are gone.
    fn track_unserved(&mut self, active: &[Redemption], now: Instant) {
        let served: std::collections::HashSet<RedemptionId> = self
            .conns
            .values()
            .filter_map(|b| b.effective().redemption_id())
            .collect();
        self.unserved_since
            .retain(|id, _| active.iter().any(|r| r.id == *id) && !served.contains(id));
        for r in active {
            if !served.contains(&r.id) {
                self.unserved_since.entry(r.id).or_insert(now);
            }
        }
    }
}

/// One active order as [`pick_route`] sees it.
struct Order<'a> {
    r: &'a Redemption,
    /// Index in melt order (oldest first).
    age: usize,
    /// Connections heading to it, other than the one being placed.
    load: usize,
    /// Hashrate it can still absorb before it finishes within
    /// [`TARGET_RUN`]; `≤ 0` means capped.
    room: f64,
    /// Waited past [`STARVE_AFTER`] with no rig.
    starving: bool,
}

/// Choose a route for connection `placing` (or a brand new one when
/// `None`) over the active redemptions, oldest first. See the module docs
/// for the rules; `idle` (the connection's home pool or the default pool)
/// when nothing is active or every order is capped.
fn pick_route(
    active: &[Redemption],
    inner: &Inner,
    placing: Option<ConnectionId>,
    now: Instant,
    idle: &Route,
) -> Route {
    let rig = placing
        .and_then(|c| inner.conns.get(&c))
        .and_then(|b| b.rate.rate(now));
    let target_run = TARGET_RUN.as_secs_f64();
    let orders: Vec<Order> = active
        .iter()
        .enumerate()
        .map(|(age, r)| {
            let mut load = 0;
            let mut assigned = 0.0;
            for (c, b) in &inner.conns {
                if Some(*c) == placing || b.effective().redemption_id() != Some(r.id) {
                    continue;
                }
                load += 1;
                assigned += b.rate.rate(now).unwrap_or(0.0);
            }
            let remaining = r.target_hashes.saturating_sub(r.accumulated_hashes) as f64;
            let starving = load == 0
                && inner
                    .unserved_since
                    .get(&r.id)
                    .is_some_and(|t| now.saturating_duration_since(*t) >= STARVE_AFTER);
            Order {
                r,
                age,
                load,
                room: remaining / target_run - assigned,
                starving,
            }
        })
        .filter(|o| o.room > 0.0)
        .collect();

    // Tier: starving, then unserved, then served.
    let tier = |o: &Order| match (o.starving, o.load) {
        (true, _) => 0u8,
        (false, 0) => 1,
        _ => 2,
    };
    let top = orders.iter().map(tier).min();
    let best = orders
        .iter()
        .filter(|o| Some(tier(o)) == top)
        .min_by(|a, b| {
            let fit = match rig {
                // Starving orders go oldest first whatever the rig.
                _ if a.starving => std::cmp::Ordering::Equal,
                Some(h) => {
                    // Fits → smallest room first; too big for both → most
                    // room first; a fit beats a misfit.
                    let (fa, fb) = (a.room >= h, b.room >= h);
                    match (fa, fb) {
                        (true, true) => a.room.total_cmp(&b.room),
                        (false, false) => b.room.total_cmp(&a.room),
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                    }
                }
                None => a.load.cmp(&b.load),
            };
            fit.then(a.age.cmp(&b.age))
        });

    match best.and_then(|o| upstream_for_pool_url(&o.r.pool_url).map(|u| (o.r, u))) {
        Some((r, upstream)) => Route::Redemption {
            id: r.id,
            upstream,
            worker_name: r.worker_name.clone(),
            worker_password: r.authorize_password().to_string(),
        },
        None => idle.clone(),
    }
}

/// One supplier as [`Router::suppliers`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct SupplierView {
    /// Supplier key (see [`crate::home::MinerIdentity::supplier`]).
    pub supplier: String,
    /// Live connections.
    pub connections: usize,
    /// Summed hashrate estimate of those connections, hashes per second.
    pub hashrate: f64,
    /// Home pool, if a live connection has one.
    pub home_upstream: Option<String>,
    /// Work delivered to redemptions, in hashes.
    pub delivered_hashes: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(n: u8) -> RedemptionId {
        RedemptionId::from_bytes([n; 16])
    }

    fn open(ledger: &RedemptionLedger, n: u8, pool: &str, worker: &str) {
        ledger
            .open(Redemption::new(rid(n), 1, pool.into(), worker.into()))
            .unwrap();
    }

    #[test]
    fn rewrites_authorize_and_submit_only() {
        let auth = r#"{"id":2,"method":"mining.authorize","params":["miner.1","secret"]}"#;
        let out: Value =
            serde_json::from_str(&rewrite_worker(auth, "bob.rig", "x").unwrap()).unwrap();
        assert_eq!(out["params"][0], "bob.rig");
        assert_eq!(out["params"][1], "x");
        assert_eq!(out["id"], 2);

        let submit = r#"{"id":5,"method":"mining.submit","params":["miner.1","job1","00000000","60000000","deadbeef"]}"#;
        let out = rewrite_worker(submit, "bob.rig", "x").unwrap();
        assert!(out.ends_with('\n'));
        let out: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(out["params"][0], "bob.rig");
        assert_eq!(out["params"][1], "job1");

        let sub = r#"{"id":1,"method":"mining.subscribe","params":["cpuminer/1.0"]}"#;
        assert_eq!(rewrite_worker(sub, "bob.rig", "x"), None);
        assert_eq!(rewrite_worker("not json", "bob.rig", "x"), None);
    }

    #[test]
    fn default_route_never_rewrites() {
        let r = Route::Default {
            upstream: "pool:3333".into(),
        };
        let auth = r#"{"id":2,"method":"mining.authorize","params":["miner.1","x"]}"#;
        assert_eq!(r.rewrite_miner_line(auth), None);
        assert_eq!(r.redemption_id(), None);
    }

    #[test]
    fn pool_url_to_upstream() {
        assert_eq!(
            upstream_for_pool_url("stratum+tcp://pool.example:3333").as_deref(),
            Some("pool.example:3333")
        );
        assert_eq!(
            upstream_for_pool_url("stratum+ssl://pool.example:443").as_deref(),
            Some("tls://pool.example:443")
        );
        assert_eq!(upstream_for_pool_url("stratum+tcp://noport"), None);
        assert_eq!(upstream_for_pool_url("stratum+tcp://p:3333/key"), None);
    }

    #[test]
    fn idle_proxy_routes_to_default() {
        let router = Router::new("default:3333", Arc::new(RedemptionLedger::new()));
        let a = router.assign(ConnectionId::next());
        assert_eq!(
            a.route,
            Route::Default {
                upstream: "default:3333".into()
            }
        );
    }

    #[test]
    fn authorize_carries_the_redeemers_password() {
        let auth = r#"{"id":2,"method":"mining.authorize","params":["miner.1","secret"]}"#;
        let out: Value =
            serde_json::from_str(&rewrite_worker(auth, "bob.rig", "d=1024").unwrap()).unwrap();
        assert_eq!(out["params"], serde_json::json!(["bob.rig", "d=1024"]));
        // A miner that sent no password still authorizes with one.
        let bare = r#"{"id":3,"method":"mining.authorize","params":["miner.1"]}"#;
        let out: Value =
            serde_json::from_str(&rewrite_worker(bare, "bob.rig", "pw").unwrap()).unwrap();
        assert_eq!(out["params"], serde_json::json!(["bob.rig", "pw"]));
    }

    #[test]
    fn route_uses_the_redemption_password() {
        let ledger = Arc::new(RedemptionLedger::new());
        ledger
            .open(
                Redemption::new(rid(9), 1, "stratum+tcp://p:3333".into(), "w".into())
                    .with_password(Some("hunter2".into())),
            )
            .unwrap();
        let router = Router::new("default:3333", ledger);
        match router.assign(ConnectionId::next()).route {
            Route::Redemption {
                worker_password, ..
            } => assert_eq!(worker_password, "hunter2"),
            r => panic!("unexpected {r:?}"),
        }
    }

    #[test]
    fn active_redemption_takes_new_connections() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        let router = Router::new("default:3333", ledger);
        let a = router.assign(ConnectionId::next());
        assert_eq!(
            a.route,
            Route::Redemption {
                id: rid(1),
                upstream: "alice:3333".into(),
                worker_name: "alice.w".into(),
                worker_password: "x".into(),
            }
        );
    }

    #[test]
    fn concurrent_redemptions_spread_across_connections() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+tcp://alice:3333", "a");
        open(&ledger, 2, "stratum+tcp://bob:3333", "b");
        let router = Router::new("default:3333", ledger);
        let ids: Vec<_> = (0..4)
            .map(|_| {
                router
                    .assign(ConnectionId::next())
                    .route
                    .redemption_id()
                    .unwrap()
            })
            .collect();
        assert_eq!(ids, vec![rid(1), rid(2), rid(1), rid(2)]);
    }

    #[test]
    fn tls_pool_is_routed_over_tls() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+ssl://alice:443", "a");
        let router = Router::new("default:3333", ledger);
        let route = router.assign(ConnectionId::next()).route;
        assert_eq!(route.redemption_id(), Some(rid(1)));
        assert_eq!(route.upstream(), "tls://alice:443");
    }

    #[test]
    fn unroutable_pool_is_skipped() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+tcp://alice:3333/key", "a");
        let router = Router::new("default:3333", ledger);
        assert_eq!(
            router.assign(ConnectionId::next()).route.redemption_id(),
            None
        );
    }

    #[test]
    fn reconcile_kicks_default_when_redemption_opens_and_back_when_done() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router =
            Router::new("default:3333", ledger.clone()).with_switch_mode(SwitchMode::Reconnect);

        // Idle: miner on the default pool, nothing to kick.
        let a = router.assign(ConnectionId::next());
        assert_eq!(a.route.redemption_id(), None);
        assert_eq!(router.reconcile(), 0);

        // Melt opens a redemption: the default-pool connection is kicked
        // (once, not on every reconcile).
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        assert_eq!(router.reconcile(), 1);
        assert_eq!(*a.signal.borrow(), Signal::Kick);
        assert_eq!(router.reconcile(), 0);

        // Miner reconnects and lands on the redeemer's pool.
        router.release(a.conn);
        let b = router.assign(ConnectionId::next());
        assert_eq!(b.route.redemption_id(), Some(rid(1)));
        assert_eq!(router.reconcile(), 0);

        // Shares fill the redemption: kicked back to default.
        ledger.credit_share(&rid(1), 1e9).unwrap();
        assert_eq!(router.reconcile(), 1);
        assert_eq!(*b.signal.borrow(), Signal::Kick);
        router.release(b.conn);
        assert_eq!(
            router.assign(ConnectionId::next()).route.redemption_id(),
            None
        );
    }

    #[test]
    fn fallback_moves_connection_to_default() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+tcp://alice:3333", "a");
        let router = Router::new("default:3333", ledger);
        let a = router.assign(ConnectionId::next());
        assert_eq!(router.current(a.conn), Some(rid(1)));
        let r = router.fall_back_to_default(a.conn);
        assert_eq!(router.current(a.conn), None);
        assert_eq!(r.redemption_id(), None);
        assert_eq!(router.route_of(a.conn), Some(r));
        // A fresh fallback is not kicked straight back onto the dead pool.
        assert_eq!(router.reconcile(), 0);
    }

    fn switch_target(a: &Assignment) -> Route {
        match &*a.signal.borrow() {
            Signal::Switch(r) => r.clone(),
            s => panic!("expected a switch, got {s:?}"),
        }
    }

    #[test]
    fn seamless_reconcile_switches_in_place() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        assert_eq!(router.switch_mode(), SwitchMode::Seamless);
        let a = router.assign(ConnectionId::next());

        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        assert_eq!(router.reconcile(), 1);
        let target = switch_target(&a);
        assert_eq!(target.redemption_id(), Some(rid(1)));
        // Mid-switch: not signalled again, and shares still go to default.
        assert_eq!(router.reconcile(), 0);
        assert_eq!(router.current(a.conn), None);

        router.switched(a.conn, target);
        assert_eq!(router.current(a.conn), Some(rid(1)));
        assert_eq!(*a.signal.borrow(), Signal::Stay);
        assert_eq!(router.reconcile(), 0);

        // Filled: switched back to the default pool, same connection.
        ledger.credit_share(&rid(1), 1e9).unwrap();
        assert_eq!(router.reconcile(), 1);
        let back = switch_target(&a);
        assert_eq!(back.redemption_id(), None);
        router.switched(a.conn, back);
        assert_eq!(router.current(a.conn), None);
        assert_eq!(router.reconcile(), 0);
    }

    #[test]
    fn seamless_switches_spread_over_redemptions() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let conns: Vec<_> = (0..4)
            .map(|_| router.assign(ConnectionId::next()))
            .collect();
        open(&ledger, 1, "stratum+tcp://alice:3333", "a");
        open(&ledger, 2, "stratum+tcp://bob:3333", "b");
        assert_eq!(router.reconcile(), 4);
        let mut ids: Vec<_> = conns
            .iter()
            .map(|a| switch_target(a).redemption_id().unwrap())
            .collect();
        ids.sort_by_key(|i| *i.as_bytes());
        assert_eq!(ids, vec![rid(1), rid(1), rid(2), rid(2)]);
    }

    #[test]
    fn failed_seamless_switch_holds_on_default() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let a = router.assign(ConnectionId::next());
        open(&ledger, 1, "stratum+tcp://dead:3333", "a");
        assert_eq!(router.reconcile(), 1);
        let target = switch_target(&a);
        router.switch_failed(a.conn, &target);
        // The dead pool is not retried straight away.
        assert_eq!(router.reconcile(), 0);
        assert_eq!(router.current(a.conn), None);
    }

    #[test]
    fn stale_redemption_with_failed_next_goes_default() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+tcp://alice:3333", "a");
        let router = Router::new("default:3333", ledger.clone());
        let a = router.assign(ConnectionId::next());
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        open(&ledger, 2, "stratum+tcp://dead:3333", "b");
        ledger.credit_share(&rid(1), 1e9).unwrap();
        assert_eq!(router.reconcile(), 1);
        let t = switch_target(&a);
        assert_eq!(t.redemption_id(), Some(rid(2)));
        router.switch_failed(a.conn, &t);
        // Still bound to the finished redemption: next try is the default pool.
        assert_eq!(router.reconcile(), 1);
        assert_eq!(switch_target(&a).redemption_id(), None);
    }

    /// 1 TH/s in hashes per second.
    const TH: f64 = 1e12;

    /// Give `conn` a steady `th_s` TH/s over the last 60 s: one share a
    /// second at the matching difficulty, ending at `now`.
    fn warm(router: &Router, conn: ConnectionId, th_s: f64, now: Instant) {
        let diff = th_s * TH / HASHES_PER_DIFF_ONE as f64;
        {
            let mut inner = router.lock();
            let b = inner.conns.get_mut(&conn).unwrap();
            b.rate = RigRate::new(now - Duration::from_secs(60));
        }
        for s in (0..60).rev() {
            router.observe_share_at(conn, diff, now - Duration::from_secs(s), None);
        }
    }

    fn open_thh(ledger: &RedemptionLedger, n: u8, thh: u64) {
        ledger
            .open(Redemption::new(
                rid(n),
                thh,
                format!("stratum+tcp://pool{n}:3333"),
                format!("w{n}"),
            ))
            .unwrap();
    }

    use hashu_core::redemption::HASHES_PER_DIFF_ONE;

    #[test]
    fn rig_rate_tracks_shares_and_decays() {
        let t0 = Instant::now();
        let mut r = RigRate::new(t0);
        assert_eq!(r.rate(t0), None);
        // 10 TH/s: 1e13 hashes a second.
        for s in 1..=30 {
            r.observe(10.0 * TH, t0 + Duration::from_secs(s));
        }
        let at = t0 + Duration::from_secs(30);
        let est = r.rate(at).unwrap();
        assert!((est / (10.0 * TH) - 1.0).abs() < 0.05, "{est}");
        // Rig goes quiet: the estimate falls.
        let later = r.rate(at + RIG_RATE_TAU).unwrap();
        assert!(later < est * 0.6, "{later} vs {est}");
    }

    #[test]
    fn rig_rate_needs_a_few_shares() {
        let t0 = Instant::now();
        let mut r = RigRate::new(t0);
        r.observe(1e15, t0 + Duration::from_secs(20));
        assert_eq!(r.rate(t0 + Duration::from_secs(20)), None);
    }

    #[test]
    fn observed_shares_give_a_hashrate() {
        let router = Router::new("default:3333", Arc::new(RedemptionLedger::new()));
        let a = router.assign(ConnectionId::next());
        assert_eq!(router.hashrate_of(a.conn), None);
        warm(&router, a.conn, 100.0, Instant::now());
        let h = router.hashrate_of(a.conn).unwrap();
        assert!((h / (100.0 * TH) - 1.0).abs() < 0.05, "{h}");
    }

    #[test]
    fn large_rigs_take_large_orders_small_rigs_small_ones() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        let big = router.assign(ConnectionId::next());
        let small = router.assign(ConnectionId::next());
        warm(&router, big.conn, 200.0, now);
        warm(&router, small.conn, 0.5, now);
        // Small order first (older), large order second. 1 THH over a
        // minute absorbs 60 TH/s, too little for the 200 TH/s rig.
        open_thh(&ledger, 1, 1);
        open_thh(&ledger, 2, 1000);
        assert_eq!(router.reconcile_at(now), 2);
        assert_eq!(switch_target(&big).redemption_id(), Some(rid(2)));
        assert_eq!(switch_target(&small).redemption_id(), Some(rid(1)));
    }

    #[test]
    fn small_order_is_capped_and_extra_rigs_stay_on_default() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        // Four 50 TH/s rigs; 1 THH needs 60 TH/s to finish in a minute.
        let rigs: Vec<_> = (0..4)
            .map(|_| router.assign(ConnectionId::next()))
            .collect();
        for a in &rigs {
            warm(&router, a.conn, 50.0, now);
        }
        open_thh(&ledger, 1, 1);
        // Two rigs cover it (50 + 50 ≥ 60); the other two stay put.
        assert_eq!(router.reconcile_at(now), 2);
        let on_order = rigs
            .iter()
            .filter(|a| matches!(&*a.signal.borrow(), Signal::Switch(r) if r.redemption_id() == Some(rid(1))))
            .count();
        assert_eq!(on_order, 2);
        // Not re-signalled on later ticks.
        assert_eq!(router.reconcile_at(now), 0);
    }

    #[test]
    fn capped_orders_send_new_connections_nowhere_once_rates_are_known() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        open_thh(&ledger, 1, 1);
        let a = router.assign(ConnectionId::next());
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        warm(&router, a.conn, 100.0, now);
        // The 100 TH/s rig alone finishes 1 THH in 36 s: the order is
        // capped, so a new rig goes to the default pool.
        let b = router.assign(ConnectionId::next());
        assert_eq!(b.route.redemption_id(), None);
    }

    #[test]
    fn big_rig_with_only_small_orders_takes_the_biggest() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 500.0, now);
        open_thh(&ledger, 1, 2);
        open_thh(&ledger, 2, 5);
        open_thh(&ledger, 3, 3);
        assert_eq!(router.reconcile_at(now), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(2)));
    }

    #[test]
    fn small_rig_picks_the_smallest_order_it_fits() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 1.0, now);
        open_thh(&ledger, 1, 100);
        open_thh(&ledger, 2, 3);
        open_thh(&ledger, 3, 10);
        assert_eq!(router.reconcile_at(now), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(2)));
    }

    #[test]
    fn unserved_orders_come_before_best_fit() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        let a = router.assign(ConnectionId::next());
        open_thh(&ledger, 1, 1000);
        let on_big = router.assign(ConnectionId::next());
        assert_eq!(on_big.route.redemption_id(), Some(rid(1)));
        warm(&router, on_big.conn, 1.0, now);
        warm(&router, a.conn, 100.0, now);
        // The 100 TH/s rig fits the 1000 THH order, but the new 10 THH
        // order has no rig, so it is served first.
        open_thh(&ledger, 2, 10);
        assert_eq!(router.reconcile_at(now), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(2)));
    }

    #[test]
    fn starving_order_jumps_ahead_oldest_first() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 1.0, t0);
        open_thh(&ledger, 1, 1000);
        open_thh(&ledger, 2, 3);
        // Start the waiting clocks with no rig to place: the rig is
        // still holding on the default pool after a failed switch.
        router.lock().conns.get_mut(&a.conn).unwrap().fell_back_at = Some(t0);
        assert_eq!(router.reconcile_at(t0), 0);
        router.lock().conns.get_mut(&a.conn).unwrap().fell_back_at = None;
        // Fresh, the 1 TH/s rig would pick the 3 THH order (best fit).
        let idle = Route::Default {
            upstream: "d:1".into(),
        };
        let fresh = pick_route(&ledger.active(), &router.lock(), Some(a.conn), t0, &idle);
        assert_eq!(fresh.redemption_id(), Some(rid(2)));
        // Both starved: oldest first.
        let later = t0 + STARVE_AFTER;
        assert_eq!(router.reconcile_at(later), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(1)));
    }

    const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";

    fn home_router(ledger: Arc<RedemptionLedger>) -> Router {
        Router::new("default:3333", ledger).with_home_pools(HomeConfig::default())
    }

    #[test]
    fn address_miner_idles_on_its_home_pool() {
        let router = home_router(Arc::new(RedemptionLedger::new()));
        let auth = serde_json::json!([format!("{ADDR}.bitaxe"), "x"]);
        let a = router.assign_with(ConnectionId::next(), Some(&auth));
        assert_eq!(
            a.route,
            Route::Home {
                upstream: crate::home::DEFAULT_SOLO_UPSTREAM.into(),
                worker_name: format!("{ADDR}.bitaxe"),
                worker_password: "x".into(),
            }
        );
        assert_eq!(router.current(a.conn), None);
        // No authorize yet (or a plain account): the default pool.
        let b = router.assign_with(ConnectionId::next(), None);
        assert_eq!(b.route.upstream(), "default:3333");
        let c = router.assign_with(
            ConnectionId::next(),
            Some(&serde_json::json!(["asherp.rig", "x"])),
        );
        assert_eq!(c.route.upstream(), "default:3333");
    }

    #[test]
    fn home_miner_is_borrowed_then_sent_home() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = home_router(ledger.clone());
        let auth = serde_json::json!([ADDR, "x"]);
        let a = router.assign_with(ConnectionId::next(), Some(&auth));

        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        assert_eq!(router.reconcile(), 1);
        let target = switch_target(&a);
        assert_eq!(target.redemption_id(), Some(rid(1)));
        router.switched(a.conn, target);

        router.observe_share(a.conn, 4.0, Some(rid(1)));
        router.observe_share(a.conn, 4.0, None);
        ledger.credit_share(&rid(1), 1e9).unwrap();
        assert_eq!(router.reconcile(), 1);
        let back = switch_target(&a);
        assert!(matches!(back, Route::Home { .. }), "{back:?}");

        let s = router.suppliers();
        assert_eq!(s[0].supplier, ADDR);
        assert_eq!(s[0].delivered_hashes, 4.0 * 4_294_967_296.0);
    }

    #[test]
    fn unreachable_redeemer_falls_back_home() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = home_router(ledger.clone());
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        let a = router.assign_with(ConnectionId::next(), Some(&serde_json::json!([ADDR, "x"])));
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        let r = router.fall_back_to_default(a.conn);
        assert!(matches!(r, Route::Home { .. }), "{r:?}");
    }

    #[test]
    fn late_authorize_sets_home_for_next_idle() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = home_router(ledger.clone());
        let a = router.assign_with(ConnectionId::next(), None);
        router.identify(a.conn, &serde_json::json!([ADDR, "x"]));
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        assert_eq!(router.reconcile(), 1);
        router.switched(a.conn, switch_target(&a));
        ledger.credit_share(&rid(1), 1e9).unwrap();
        assert_eq!(router.reconcile(), 1);
        assert!(matches!(switch_target(&a), Route::Home { .. }));
        assert_eq!(router.suppliers()[0].supplier, ADDR);
    }

    fn operator_router(ledger: Arc<RedemptionLedger>) -> Router {
        Router::new("default:3333", ledger).with_home_pools(HomeConfig {
            operator_worker: Some("hashu".into()),
            ..HomeConfig::default()
        })
    }

    #[test]
    fn only_hashus_rigs_are_borrowed_with_an_operator_worker() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = operator_router(ledger.clone());
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");

        let op = router.assign_with(
            ConnectionId::next(),
            Some(&serde_json::json!(["hashu.mrr1", "x"])),
        );
        assert_eq!(op.route.redemption_id(), Some(rid(1)));

        let home = router.assign_with(ConnectionId::next(), Some(&serde_json::json!([ADDR, "x"])));
        assert!(matches!(home.route, Route::Home { .. }), "{:?}", home.route);
        // Not yet identified: not borrowed either.
        let unknown = router.assign_with(ConnectionId::next(), None);
        assert_eq!(unknown.route.upstream(), "default:3333");
        assert_eq!(unknown.route.redemption_id(), None);

        open(&ledger, 2, "stratum+tcp://bob:3333", "bob.w");
        // The new order goes to nobody else: home and unknown stay put.
        router.reconcile();
        assert_eq!(*home.signal.borrow(), Signal::Stay);
        assert_eq!(*unknown.signal.borrow(), Signal::Stay);
    }

    #[test]
    fn late_identified_home_miner_is_not_kept_on_an_order() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router =
            Router::new("default:3333", ledger.clone()).with_home_pools(HomeConfig::default());
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        let a = router.assign_with(ConnectionId::next(), None);
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        // Turning the operator rule on is a restart, so model it on a
        // fresh router: an unidentified rig there isn't borrowed.
        let r2 = operator_router(ledger.clone());
        let b = r2.assign_with(ConnectionId::next(), None);
        assert_eq!(b.route.redemption_id(), None);
        r2.identify(b.conn, &serde_json::json!(["hashu.mrr2", "x"]));
        assert_eq!(r2.reconcile(), 1);
        assert_eq!(switch_target(&b).redemption_id(), Some(rid(1)));
    }

    #[test]
    fn refuses_account_users_without_a_home() {
        let router = operator_router(Arc::new(RedemptionLedger::new()));
        assert_eq!(
            router.refusal(&serde_json::json!(["bob.rig1", "x"])),
            Some(crate::home::REFUSE_NO_HOME)
        );
        assert_eq!(
            router.refusal(&serde_json::json!(["hashu.mrr1", "x"])),
            None
        );
        assert_eq!(router.refusal(&serde_json::json!([ADDR, "x"])), None);
        assert_eq!(
            router.refusal(&serde_json::json!([
                "bob.rig1",
                "pool=public-pool.io:21496"
            ])),
            None
        );
        let open_router = home_router(Arc::new(RedemptionLedger::new()));
        assert_eq!(
            open_router.refusal(&serde_json::json!(["bob.rig1", "x"])),
            None
        );
    }

    #[test]
    fn switch_mode_parses() {
        assert_eq!("seamless".parse::<SwitchMode>(), Ok(SwitchMode::Seamless));
        assert_eq!("Reconnect".parse::<SwitchMode>(), Ok(SwitchMode::Reconnect));
        assert!("bogus".parse::<SwitchMode>().is_err());
    }
}
