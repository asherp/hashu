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
//! connection serves exactly one redemption. A new connection goes to the
//! active redemption with the fewest connections, oldest first on ties,
//! so with one miner redemptions are served in melt order and with many
//! miners concurrent redemptions are spread across them. The
//! proportional EMA-weighted split in §4.6 stays a TODO.
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
use crate::upstream::normalize_upstream;

/// How long a connection that fell back to the default pool (redeemer's
/// pool unreachable) stays there before it is retried. Stops a dead pool
/// URL from turning into a reconnect loop on every reconcile tick.
pub const FALLBACK_HOLD: Duration = Duration::from_secs(60);

/// Where a connection's shares go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Operator's default pool; lines pass through unmodified.
    Default { upstream: String },
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
            Route::Default { upstream } | Route::Redemption { upstream, .. } => upstream,
        }
    }

    /// The redemption this route serves, if any. Shares on a `None`
    /// route are the operator's and credit nothing.
    pub fn redemption_id(&self) -> Option<RedemptionId> {
        match self {
            Route::Default { .. } => None,
            Route::Redemption { id, .. } => Some(*id),
        }
    }

    /// Rewrite a miner→pool line for this route. On a redemption route,
    /// `mining.authorize` and `mining.submit` carry the redeemer's worker
    /// name instead of the miner's. Anything else (and every line on the
    /// default route) is returned as-is.
    pub fn rewrite_miner_line(&self, line: &str) -> Option<String> {
        let Route::Redemption {
            worker_name,
            worker_password,
            ..
        } = self
        else {
            return None;
        };
        rewrite_worker(line, worker_name, worker_password)
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
}

/// Shared routing state. One per proxy process.
pub struct Router {
    default_upstream: String,
    ledger: Arc<RedemptionLedger>,
    mode: SwitchMode,
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
            inner: Mutex::new(Inner::default()),
        }
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
        let active = self.routable_active();
        let mut inner = self.lock();
        let route = pick_route(&active, &inner.conns, &self.default_upstream);
        let (tx, rx) = watch::channel(Signal::Stay);
        inner.conns.insert(
            conn,
            Binding {
                route: route.clone(),
                signal: tx,
                fell_back_at: None,
                pending: None,
            },
        );
        if let Some(id) = route.redemption_id() {
            tracing::info!(%conn, redemption = %id, upstream = route.upstream(), "routing connection to redeemer's pool");
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
    pub fn fall_back_to_default(&self, conn: ConnectionId) -> Route {
        let route = Route::Default {
            upstream: self.default_upstream.clone(),
        };
        if let Some(b) = self.lock().conns.get_mut(&conn) {
            b.route = route.clone();
            b.fell_back_at = Some(Instant::now());
        }
        route
    }

    /// Forget a connection once it has closed.
    pub fn release(&self, conn: ConnectionId) {
        self.lock().conns.remove(&conn);
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
        let active = self.routable_active();
        let mut inner = self.lock();
        let ids: Vec<ConnectionId> = inner.conns.keys().copied().collect();
        let mut signalled = 0;
        for conn in ids {
            let target = {
                let b = &inner.conns[&conn];
                if b.pending.is_some() || *b.signal.borrow() == Signal::Kick {
                    continue;
                }
                let stale = match &b.route {
                    Route::Redemption { id, .. } => !active.iter().any(|r| r.id == *id),
                    Route::Default { .. } => !active.is_empty() && !b.holding(),
                };
                if !stale {
                    continue;
                }
                if b.holding() {
                    Route::Default {
                        upstream: self.default_upstream.clone(),
                    }
                } else {
                    pick_route(&active, &inner.conns, &self.default_upstream)
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
}

/// Fewest-connections-first over active redemptions (oldest wins ties),
/// else the default pool.
fn pick_route(
    active: &[Redemption],
    conns: &HashMap<ConnectionId, Binding>,
    default_upstream: &str,
) -> Route {
    let load = |id: &RedemptionId| {
        conns
            .values()
            .filter(|b| b.effective().redemption_id().as_ref() == Some(id))
            .count()
    };
    // `min_by_key` keeps the first minimum, i.e. the oldest redemption.
    let chosen = active.iter().min_by_key(|r| load(&r.id));
    match chosen.and_then(|r| upstream_for_pool_url(&r.pool_url).map(|u| (r, u))) {
        Some((r, upstream)) => Route::Redemption {
            id: r.id,
            upstream,
            worker_name: r.worker_name.clone(),
            worker_password: r.authorize_password().to_string(),
        },
        None => Route::Default {
            upstream: default_upstream.to_string(),
        },
    }
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

    #[test]
    fn switch_mode_parses() {
        assert_eq!("seamless".parse::<SwitchMode>(), Ok(SwitchMode::Seamless));
        assert_eq!("Reconnect".parse::<SwitchMode>(), Ok(SwitchMode::Reconnect));
        assert!("bogus".parse::<SwitchMode>().is_err());
    }
}
