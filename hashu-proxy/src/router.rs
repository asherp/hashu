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
//! A connection's route is fixed for its lifetime. To change it, the
//! router *kicks* the connection: the proxy closes the miner socket and
//! the miner reconnects (every Stratum V1 miner does this on its own),
//! at which point [`Router::assign`] picks the new route. This avoids
//! re-handshaking a live miner onto a new extranonce, which needs
//! `mining.set_extranonce` support that many miners lack.
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
use crate::stratum::strip_stratum_prefix;

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
    /// `host:port` to dial.
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

/// Turn a melt request's `pool_url` into a dialable `host:port`, or
/// `None` if the proxy can't reach it (TLS stratum is not supported yet).
pub fn upstream_for_pool_url(pool_url: &str) -> Option<String> {
    if pool_url.starts_with("stratum+ssl://") || pool_url.starts_with("stratum+tcps://") {
        return None;
    }
    let bare = strip_stratum_prefix(pool_url);
    if bare.contains(':') && !bare.contains('/') {
        Some(bare.to_string())
    } else {
        None
    }
}

/// Bookkeeping for one live connection.
struct Binding {
    route: Route,
    kick: watch::Sender<bool>,
    /// Set when this connection fell back to the default pool.
    fell_back_at: Option<Instant>,
}

#[derive(Default)]
struct Inner {
    conns: HashMap<ConnectionId, Binding>,
}

/// Shared routing state. One per proxy process.
pub struct Router {
    default_upstream: String,
    ledger: Arc<RedemptionLedger>,
    inner: Mutex<Inner>,
}

/// What a new connection gets from [`Router::assign`].
pub struct Assignment {
    pub conn: ConnectionId,
    pub route: Route,
    /// Flips to `true` when the router wants this connection closed so
    /// the miner reconnects onto a new route.
    pub kicked: watch::Receiver<bool>,
}

impl Router {
    pub fn new(default_upstream: impl Into<String>, ledger: Arc<RedemptionLedger>) -> Self {
        Self {
            default_upstream: default_upstream.into(),
            ledger,
            inner: Mutex::new(Inner::default()),
        }
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
        let (tx, rx) = watch::channel(false);
        inner.conns.insert(
            conn,
            Binding {
                route: route.clone(),
                kick: tx,
                fell_back_at: None,
            },
        );
        if let Some(id) = route.redemption_id() {
            tracing::info!(%conn, redemption = %id, upstream = route.upstream(), "routing connection to redeemer's pool");
        }
        Assignment {
            conn,
            route,
            kicked: rx,
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

    /// Compare live connections against the ledger and kick any whose
    /// route is stale:
    ///
    /// - bound to a redemption that has completed (or vanished) → back
    ///   to the default pool on reconnect;
    /// - on the default pool while some redemption is waiting for
    ///   hashrate → onto that redemption on reconnect.
    ///
    /// Returns how many connections were kicked.
    pub fn reconcile(&self) -> usize {
        let active = self.routable_active();
        let inner = self.lock();
        let mut kicked = 0;
        for (conn, b) in inner.conns.iter() {
            let stale = match &b.route {
                Route::Redemption { id, .. } => !active.iter().any(|r| r.id == *id),
                Route::Default { .. } => {
                    let holding = b.fell_back_at.is_some_and(|t| t.elapsed() < FALLBACK_HOLD);
                    !active.is_empty() && !holding
                }
            };
            if stale && !*b.kick.borrow() {
                let _ = b.kick.send(true);
                kicked += 1;
                tracing::info!(%conn, from = ?b.route.redemption_id().map(|i| i.to_string()), "kicking connection to re-route");
            }
        }
        kicked
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
                    tracing::warn!(redemption = %r.id, pool_url = %r.pool_url, "redemption pool URL not routable (TLS stratum unsupported); skipping");
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
            .filter(|b| b.route.redemption_id().as_ref() == Some(id))
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
            upstream_for_pool_url("stratum+ssl://pool.example:443"),
            None
        );
        assert_eq!(upstream_for_pool_url("stratum+tcp://noport"), None);
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
    fn tls_pool_is_skipped() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+ssl://alice:443", "a");
        let router = Router::new("default:3333", ledger);
        assert_eq!(
            router.assign(ConnectionId::next()).route.redemption_id(),
            None
        );
    }

    #[test]
    fn reconcile_kicks_default_when_redemption_opens_and_back_when_done() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());

        // Idle: miner on the default pool, nothing to kick.
        let a = router.assign(ConnectionId::next());
        assert_eq!(a.route.redemption_id(), None);
        assert_eq!(router.reconcile(), 0);

        // Melt opens a redemption: the default-pool connection is kicked
        // (once, not on every reconcile).
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        assert_eq!(router.reconcile(), 1);
        assert!(*a.kicked.borrow());
        assert_eq!(router.reconcile(), 0);

        // Miner reconnects and lands on the redeemer's pool.
        router.release(a.conn);
        let b = router.assign(ConnectionId::next());
        assert_eq!(b.route.redemption_id(), Some(rid(1)));
        assert_eq!(router.reconcile(), 0);

        // Shares fill the redemption: kicked back to default.
        ledger.credit_share(&rid(1), 1e9).unwrap();
        assert_eq!(router.reconcile(), 1);
        assert!(*b.kicked.borrow());
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
}
