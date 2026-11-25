//! Pool routing controller. ARCHITECTURE.md §4.2, §4.6.
//!
//! Decides which upstream pool each miner connection talks to:
//!
//! - **No active redemption**: the operator's default pool, worker name
//!   passed through untouched.
//! - **Active redemption(s)**: the redeemer's pool, with the miner's
//!   worker name rewritten to the redeemer's `worker_name` in
//!   `mining.authorize` and `mining.submit`. With [`MintHosts`] set, the
//!   worker label becomes the selling mint's host, so the redeemer's pool
//!   shows the rig as e.g. `bc1q….mint.hashu.io` or `acct.alice.hashu.io`
//!   (see [`delivery_worker`]).
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
//! - **First half first.** An order still in its first half (less than
//!   half its hashes delivered) outranks every order past half, so a new
//!   redemption starts right away and reaches half quickly, while orders
//!   nearly done wait a little longer. Within each half: an order with no
//!   rig waiting longer than [`STARVE_AFTER`] first, then orders with no
//!   rig at all, then the rest; oldest first within each.
//! - **Cap.** Each connection's hashrate is an EWMA of its accepted share
//!   work ([`RIG_RATE_TAU`]). An order whose assigned rigs already finish
//!   it within [`TARGET_RUN`] takes no more (more would only overshoot by
//!   about a share per rig plus the switch drain). If every order is
//!   capped, extra rigs stay on (or go back to) the default pool.
//! - **Moving placed rigs.** A rig stays on its order until it fills,
//!   with two exceptions ([`rescue`]): a first-half order with no rig
//!   takes the biggest rig from an order past half at once, even its only
//!   rig; and an order with no rig for [`STARVE_AFTER`] takes one from an
//!   order in the same half that has two or more. One rig per order per
//!   pass.
//! - **Fallback pools.** A redemption lists its pool and up to
//!   [`hashu_core::redemption::MAX_FALLBACK_POOLS`] fallbacks. A pool that
//!   can't be dialed, refuses the authorize, drops the session
//!   [`DROP_LIMIT`] times within [`DROP_WINDOW`] (a single drop is
//!   retried: the rig goes back to the same redemption when it
//!   reconnects) or rejects [`REJECT_LIMIT`] shares in a row is marked
//!   down for that redemption
//!   for [`FALLBACK_HOLD`], and its rigs move to the next pool on the
//!   list. Shares on any of its pools fill the same redemption. Once the
//!   hold ends, rigs on a fallback try the earlier pool again (seamless
//!   mode only). With every pool down the rig idles.
//! - **One miner.** It serves each new order's first half as it arrives,
//!   then stays on that order to the end before going back to the oldest.
//!
//! The proportional EMA-weighted split in §4.6 stays a TODO.
//!
//! The router learns about redemptions by polling
//! [`RedemptionLedger::active`] (see [`Router::reconcile`]). The melt
//! path opens redemptions there and the share-crediting path completes
//! them, so the ledger is the only interface between the three.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hashu_core::redemption::{Redemption, RedemptionId, RedemptionLedger};
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::credit::{ConnectionId, RedemptionBinding};
use crate::home::{HomeConfig, MinerIdentity, REFUSE_NO_HOME};
use crate::sessions::{Session, SessionLog};
use crate::suppliers::{Check, SupplierRegistry};
use hashu_core::availability::DOWN_AFTER;

/// Start of the authorize error for a supplier name with a wrong or
/// missing `pw=`; the name follows.
pub const REFUSE_WRONG_SECRET: &str = "wrong or missing pw= for supplier";
use crate::upstream::normalize_upstream;

/// How long a redeemer's pool that failed (unreachable, refused the
/// authorize, dropped the session or kept rejecting shares) is skipped for
/// that redemption, and how long a connection with no pool left to try
/// stays on its idle pool. Stops a dead pool URL from turning into a
/// reconnect loop on every reconcile tick.
pub const FALLBACK_HOLD: Duration = Duration::from_secs(60);

/// Shares in a row a redeemer's pool may reject, with none accepted,
/// before the proxy treats it as refusing the work and fails over.
pub const REJECT_LIMIT: u32 = 10;
/// A redeemer's pool that closes a session is retried: it is marked down
/// only after it has closed sessions this many times within
/// [`DROP_WINDOW`] (drops within [`DROP_DEBOUNCE`] of each other count
/// once, so a pool restart that closes every rig's session is one drop).
pub const DROP_LIMIT: usize = 3;
/// See [`DROP_LIMIT`].
pub const DROP_WINDOW: Duration = Duration::from_secs(300);
/// See [`DROP_LIMIT`].
pub const DROP_DEBOUNCE: Duration = Duration::from_secs(10);
/// How long a rig whose redemption pool closed its session goes back to
/// that redemption when it reconnects (through hashu-relay, in seconds).
pub const RETURN_WINDOW: Duration = Duration::from_secs(120);

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
        /// Which of the redemption's pools: 0 = its primary, then its
        /// fallbacks in order (see [`Redemption::pools`]).
        pool: usize,
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

    /// The redemption this route serves and which of its pools, if any.
    pub fn redemption_pool(&self) -> Option<(RedemptionId, usize)> {
        match self {
            Route::Redemption { id, pool, .. } => Some((*id, *pool)),
            _ => None,
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
    /// name instead of the miner's. On the default route only the
    /// proxy's own `pool=` / `pw=` options are taken out of the authorize
    /// password, so a secret never reaches a pool. Anything else is
    /// returned as-is (`None`).
    pub fn rewrite_miner_line(&self, line: &str) -> Option<String> {
        match self.credentials() {
            Some((worker_name, worker_password)) => {
                rewrite_worker(line, worker_name, worker_password)
            }
            None => strip_default_authorize(line),
        }
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

/// A `mining.authorize` line with the proxy's own password options
/// removed, or `None` if it isn't one or has none.
fn strip_default_authorize(line: &str) -> Option<String> {
    let mut v: Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("method")?.as_str()? != "mining.authorize" {
        return None;
    }
    let pw = v.get_mut("params")?.get_mut(1)?;
    *pw = Value::String(crate::home::strip_proxy_options(pw.as_str()?)?);
    let mut out = serde_json::to_string(&v).ok()?;
    out.push('\n');
    Some(out)
}

/// Turn a melt request's `pool_url` into an upstream the proxy can dial
/// (`host:port`, `tls://host:port` for `stratum+ssl://`, or
/// `sv2://host:port/<key>` for `stratum2+tcp://`), or `None` if it can't
/// (no port, a path, or an unknown scheme).
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
    /// Which orders this rig may fill.
    serves: Serves,
    /// The miner's authorize user (e.g. `alice.rig1`), once seen: the
    /// rig's label on the operator's hashrate chart.
    rig: Option<String>,
    /// Set when a Stratum relay resumed this miner's session, with the
    /// time the miner connected if the relay said (see
    /// [`crate::sessions`]).
    resumed: Option<Option<u64>>,
}

/// Which orders a rig may fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Serves {
    /// Orders from the operator's own mint (`supplier: None`): Hashu's
    /// rigs, or any unregistered rig when the operator's rigs aren't
    /// named (see [`crate::home::HomeConfig::has_operator`]).
    Operator,
    /// Orders from this registered supplier's mint only.
    Supplier(String),
    /// None: the rig mines to its home pool and is never borrowed.
    Nobody,
}

impl Serves {
    /// The mint whose orders this rig fills: `Some(None)` for the
    /// operator's, `Some(Some(name))` for a supplier's, `None` for none.
    fn mint(&self) -> Option<Option<String>> {
        match self {
            Serves::Operator => Some(None),
            Serves::Supplier(n) => Some(Some(n.clone())),
            Serves::Nobody => None,
        }
    }

    /// Whether a rig with this role may fill `r`.
    pub fn fits(&self, r: &Redemption) -> bool {
        match self {
            Serves::Operator => r.supplier.is_none(),
            Serves::Supplier(n) => r.supplier.as_deref() == Some(n.as_str()),
            Serves::Nobody => false,
        }
    }

    fn filter(&self, active: &[Redemption]) -> Vec<Redemption> {
        active.iter().filter(|r| self.fits(r)).cloned().collect()
    }
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
    /// When each (redemption, pool index) last failed. Skipped while
    /// younger than [`FALLBACK_HOLD`].
    pool_down: HashMap<(RedemptionId, usize), Instant>,
    /// Recent session drops per (redemption, pool index), for
    /// [`DROP_LIMIT`].
    pool_drops: HashMap<(RedemptionId, usize), Vec<Instant>>,
    /// Rigs whose redemption pool closed their session, by rig label: the
    /// redemption they go back to on reconnect (see [`RETURN_WINDOW`]).
    returning: HashMap<String, (RedemptionId, Instant)>,
    /// [`Redemption::pools_rev`] last seen per redemption.
    pools_rev: HashMap<RedemptionId, u32>,
    /// Accepted work per rig label since the proxy started, for the
    /// operator's per-rig hashrate chart (see [`Router::rig_work`]).
    rig_work: HashMap<String, RigWork>,
    /// Hosts of the mints served here, for naming delivery workers.
    hosts: MintHosts,
    /// When a rig last stopped filling each mint's orders (Unix ms), by
    /// mint (`None` = the operator's), for [`Router::down_since`].
    served_until: HashMap<Option<String>, u64>,
}

/// Hosts of the mints whose orders this proxy fills, so a redeemer's pool
/// sees which mint the hashrate came from (see [`delivery_worker`]).
/// Unset (the default) leaves the redeemer's worker name as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MintHosts {
    /// The operator's own mint, e.g. `mint.hashu.io`.
    pub operator: Option<String>,
    /// Domain supplier mints are named under: supplier `alice` is
    /// `alice.<suppliers>`, e.g. `alice.hashu.io`.
    pub suppliers: Option<String>,
}

impl MintHosts {
    /// Host of the mint that sold an order: the operator's when
    /// `supplier` is `None`.
    pub fn host_for(&self, supplier: Option<&str>) -> Option<String> {
        match supplier {
            None => self.operator.clone(),
            Some(name) => self.suppliers.as_ref().map(|d| format!("{name}.{d}")),
        }
    }
}

/// Worker name to authorize with on a redeemer's pool: the redeemer's
/// account (everything before the first `.`) with the mint's host as the
/// worker label, so `bc1q…` or `bc1q….hashu` becomes `bc1q….mint.hashu.io`
/// and `acct.rig` becomes `acct.alice.hashu.io`. The redeemer's own label is
/// replaced. Left as typed when there is no host, or when the name looks
/// like an email or Lightning address (`@`), whose dots are not a label.
pub fn delivery_worker(redeemer: &str, host: Option<&str>) -> String {
    let redeemer = redeemer.trim();
    let Some(host) = host.map(str::trim).filter(|h| !h.is_empty()) else {
        return redeemer.to_string();
    };
    if redeemer.contains('@') {
        return redeemer.to_string();
    }
    let account = redeemer.split('.').next().unwrap_or_default();
    if account.is_empty() {
        return redeemer.to_string();
    }
    format!("{account}.{host}")
}

/// One rig's accepted work since the proxy started.
#[derive(Debug, Clone, Copy)]
struct RigWork {
    hashes: f64,
    last: Instant,
}

/// A rig that has sent no share for this long drops off the per-rig
/// chart. Matches the meter's hour of history.
pub const RIG_WORK_IDLE: Duration = Duration::from_secs(3600);

/// Longest rig label kept; longer authorize users are cut.
const RIG_LABEL_MAX: usize = 64;

/// The rig label for an authorize: its user, trimmed and cut to
/// [`RIG_LABEL_MAX`] characters.
fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn rig_label(authorize: &Value) -> Option<String> {
    let user = authorize.get(0)?.as_str()?.trim();
    (!user.is_empty()).then(|| user.chars().take(RIG_LABEL_MAX).collect())
}

/// Shared routing state. One per proxy process.
pub struct Router {
    default_upstream: String,
    ledger: Arc<RedemptionLedger>,
    mode: SwitchMode,
    home: HomeConfig,
    registry: Arc<SupplierRegistry>,
    inner: Mutex<Inner>,
    /// When each rig was connected (see [`crate::sessions`]). Locked
    /// after `inner` when both are needed.
    sessions: Mutex<SessionLog>,
    /// When this router started (Unix ms): before any rig, every mint has
    /// been without rigs since then.
    started_ms: u64,
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
            registry: Arc::new(SupplierRegistry::empty()),
            inner: Mutex::new(Inner::default()),
            sessions: Mutex::new(SessionLog::new(unix_ms())),
            started_ms: unix_ms(),
        }
    }

    /// Give miners home pools (see [`crate::home`]). Off by default.
    pub fn with_home_pools(mut self, cfg: HomeConfig) -> Self {
        self.home = cfg;
        self
    }

    /// Registered suppliers whose rigs fill only their own mint's orders
    /// (see [`crate::suppliers`]). Empty by default.
    pub fn with_suppliers(mut self, registry: Arc<SupplierRegistry>) -> Self {
        self.registry = registry;
        self
    }

    /// Whether the proxy should read each miner's authorize before placing
    /// it: home pools, Hashu's own rigs or suppliers are configured.
    pub fn wants_authorize(&self) -> bool {
        self.home != HomeConfig::off() || self.registry.path_configured()
    }

    pub fn home_config(&self) -> &HomeConfig {
        &self.home
    }

    /// Name the worker on a redeemer's pool after the mint that sold the
    /// order (see [`delivery_worker`]). Off by default.
    pub fn with_mint_hosts(self, hosts: MintHosts) -> Self {
        self.lock().hosts = hosts;
        self
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
        let serves = self.serves(identity.as_ref());
        let all = self.routable_active();
        let active = serves.filter(&all);
        let now = Instant::now();
        let mut inner = self.lock();
        inner.track_unserved(&all, now);
        let label = authorize.and_then(rig_label);
        // A rig whose redemption pool just closed its session goes back to
        // that redemption: an ordinary disconnect is retried.
        let back = label.as_ref().and_then(|l| {
            let (id, at) = inner.returning.remove(l)?;
            if now.saturating_duration_since(at) >= RETURN_WINDOW {
                return None;
            }
            let r = active.iter().find(|r| r.id == id)?;
            redemption_route(r, &inner, now)
        });
        let route = match back {
            Some(r) => {
                tracing::info!(%conn, rig = ?label, "rig back from a dropped pool session: retrying the same redemption");
                r
            }
            None => pick_route(&active, &inner, None, now, &idle),
        };
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
                supplier: identity.map(|i| match &serves {
                    Serves::Supplier(n) => n.clone(),
                    _ => i.supplier,
                }),
                serves,
                rig: label,
                resumed: None,
            },
        );
        let rig = inner.conns.get(&conn).and_then(|b| b.rig.clone());
        drop(inner);
        if let Some(rig) = rig {
            self.sessions().open(conn, &rig, unix_ms(), None);
        }
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

    /// The redeemer's pool this connection is on could not be dialed:
    /// mark it down for the redemption and move the connection to the
    /// redemption's next healthy fallback pool, or, with none left, to
    /// its idle route (its home pool if it has one, else the default
    /// pool) so the miner keeps working. The redemption stays open.
    pub fn fail_over(&self, conn: ConnectionId) -> Route {
        let now = Instant::now();
        let current = self.lock().conns.get(&conn).map(|b| b.route.clone());
        let Some(current) = current else {
            return self.idle_route(None);
        };
        let next = current.redemption_pool().and_then(|(id, pool)| {
            let r = self.ledger.get(&id)?;
            let mut inner = self.lock();
            inner.mark_down(id, pool, now);
            redemption_route(&r, &inner, now)
        });
        let mut inner = self.lock();
        match inner.conns.get_mut(&conn) {
            Some(b) => {
                match next {
                    Some(route) => {
                        tracing::warn!(%conn, redemption = ?route.redemption_id().map(|i| i.to_string()), upstream = route.upstream(), "failing over to the redeemer's next pool");
                        b.route = route;
                    }
                    None => {
                        b.route = b.idle.clone();
                        b.fell_back_at = Some(now);
                    }
                }
                b.route.clone()
            }
            None => self.idle_route(None),
        }
    }

    /// The redeemer's pool `route` points at failed for this connection
    /// while it was in use: it closed the session, refused the authorize
    /// or kept rejecting shares. Mark it down for the redemption so this
    /// and other connections move to the next pool on the list (on the
    /// next reconcile, or when the miner reconnects).
    pub fn pool_failed(&self, conn: ConnectionId, route: &Route, why: &str) {
        let Some((id, pool)) = route.redemption_pool() else {
            return;
        };
        tracing::warn!(%conn, redemption = %id, pool, upstream = route.upstream(), reason = %why, "redeemer's pool failed; marking it down");
        self.lock().mark_down(id, pool, Instant::now());
    }

    /// The redeemer's pool `route` points at closed this connection's
    /// session. An ordinary disconnect: the rig goes back to the same
    /// redemption when it reconnects, and the pool is marked down (like
    /// [`Router::pool_failed`]) only once it has closed sessions
    /// [`DROP_LIMIT`] times within [`DROP_WINDOW`].
    pub fn pool_dropped(&self, conn: ConnectionId, route: &Route, why: &str) {
        self.pool_dropped_at(conn, route, why, Instant::now());
    }

    fn pool_dropped_at(&self, conn: ConnectionId, route: &Route, why: &str, now: Instant) {
        let Some((id, pool)) = route.redemption_pool() else {
            return;
        };
        let mut inner = self.lock();
        let drops = inner.pool_drops.entry((id, pool)).or_default();
        drops.retain(|t| now.saturating_duration_since(*t) < DROP_WINDOW);
        if drops
            .last()
            .is_none_or(|t| now.saturating_duration_since(*t) >= DROP_DEBOUNCE)
        {
            drops.push(now);
        }
        let n = drops.len();
        if n >= DROP_LIMIT {
            inner.pool_drops.remove(&(id, pool));
            tracing::warn!(%conn, redemption = %id, pool, upstream = route.upstream(), reason = %why, drops = n, "redeemer's pool keeps closing sessions; marking it down");
            inner.mark_down(id, pool, now);
            return;
        }
        tracing::info!(%conn, redemption = %id, pool, upstream = route.upstream(), reason = %why, drops = n, limit = DROP_LIMIT, "redeemer's pool closed the session; the rig will retry it");
        inner
            .returning
            .retain(|_, (_, t)| now.saturating_duration_since(*t) < RETURN_WINDOW);
        if let Some(rig) = inner.conns.get(&conn).and_then(|b| b.rig.clone()) {
            inner.returning.insert(rig, (id, now));
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
        let serves = self.serves(Some(&id));
        let mut inner = self.lock();
        if let Some(b) = inner.conns.get_mut(&conn) {
            if b.rig.is_none() {
                b.rig = rig_label(authorize);
                if let Some(rig) = &b.rig {
                    self.sessions().open(conn, rig, unix_ms(), b.resumed);
                }
            }
            if b.supplier.is_none() {
                b.supplier = Some(match &serves {
                    Serves::Supplier(n) => n.clone(),
                    _ => id.supplier,
                });
            }
            // A rig that changes role stops filling its old mint's orders.
            let left = (b.serves != serves).then(|| b.serves.mint()).flatten();
            b.serves = serves;
            if matches!(b.idle, Route::Default { .. }) && matches!(idle, Route::Home { .. }) {
                tracing::info!(%conn, upstream = idle.upstream(), "miner has a home pool; it goes there after its next redemption or reconnect");
                b.idle = idle;
            }
            if let Some(mint) = left {
                inner.served_until.insert(mint, unix_ms());
            }
        }
    }

    /// `Some(reason)` if a miner with this authorize must be refused: a
    /// registered supplier's name with a wrong or missing `pw=`, or a
    /// miner with no home pool when there is no default pool to idle on.
    pub fn refusal(&self, authorize: &Value) -> Option<String> {
        let id = MinerIdentity::from_params(authorize, &self.home)?;
        if id.operator_by_worker {
            tracing::warn!(user = ?authorize.get(0), "rig is Hashu's only through HASHU_OPERATOR_WORKER, which is deprecated: add pw=<operator secret> to its password and set HASHU_OPERATOR_SECRET_SHA256");
        }
        // Hashu's own rigs never need a supplier secret, even if the
        // registry lists a supplier under the same name.
        if id.operator {
            if id.operator_by_worker {
                self.warn_if_operator_registered(&id.supplier);
            }
        } else if let Check::WrongSecret(n) =
            self.registry.check(&id.supplier, id.secret.as_deref())
        {
            return Some(format!("{REFUSE_WRONG_SECRET} {n}"));
        }
        (id.home.is_none() && self.default_upstream.trim().is_empty())
            .then(|| REFUSE_NO_HOME.to_string())
    }

    /// Which orders a miner may fill. With Hashu's own rigs named, a
    /// miner not yet identified fills none.
    fn serves(&self, identity: Option<&MinerIdentity>) -> Serves {
        let open = !self.home.has_operator();
        let Some(i) = identity else {
            return if open {
                Serves::Operator
            } else {
                Serves::Nobody
            };
        };
        if i.operator {
            return Serves::Operator;
        }
        if let Check::Verified(n) = self.registry.check(&i.supplier, i.secret.as_deref()) {
            return Serves::Supplier(n);
        }
        if open {
            Serves::Operator
        } else {
            Serves::Nobody
        }
    }

    /// A supplier registered under the operator's own name can never be
    /// verified (Hashu's rigs win), so its mint could not fill orders.
    fn warn_if_operator_registered(&self, name: &str) {
        if self.registry.contains(name) {
            tracing::warn!(supplier = %name, "supplier registry lists HASHU_OPERATOR_WORKER's name; Hashu's own rigs take it and that supplier is ignored");
        }
    }

    /// Live hashrate (hashes per second) of a registered supplier's
    /// verified rigs: what its mint can sell against.
    pub fn supplier_hashrate(&self, name: &str) -> f64 {
        let now = Instant::now();
        self.lock()
            .conns
            .values()
            .filter(|b| b.serves == Serves::Supplier(name.to_string()))
            .filter_map(|b| b.rate.rate(now))
            .sum()
    }

    /// Summed measured hashrate (H/s) of rigs that fill the operator's own
    /// mint's orders: Hashu's rigs, or any unregistered rig when no operator
    /// worker is set. Home-pool-only and supplier rigs are not counted.
    pub fn operator_hashrate(&self) -> f64 {
        let now = Instant::now();
        self.lock()
            .conns
            .values()
            .filter(|b| b.serves == Serves::Operator)
            .filter_map(|b| b.rate.rate(now))
            .sum()
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
            verified: false,
        };
        for b in inner.conns.values() {
            let Some(k) = &b.supplier else { continue };
            let v = out.entry(k.clone()).or_insert_with(|| view(k));
            v.connections += 1;
            v.verified |= matches!(b.serves, Serves::Supplier(_));
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
        let mut inner = self.lock();
        if let Some(b) = inner.conns.remove(&conn) {
            if let Some(mint) = b.serves.mint() {
                inner.served_until.insert(mint, unix_ms());
            }
        }
        self.sessions().close(conn, unix_ms());
    }

    /// Since when (Unix ms) no rig has been connected to fill `mint`'s
    /// orders (`None` = the operator's mint), once that has lasted
    /// [`DOWN_AFTER`]; `None` while one is connected or for less than
    /// that. See [`hashu_core::availability`].
    pub fn down_since(&self, mint: Option<&str>) -> Option<u64> {
        self.down_since_at(mint, unix_ms())
    }

    fn down_since_at(&self, mint: Option<&str>, now: u64) -> Option<u64> {
        let inner = self.lock();
        let key = mint.map(str::to_string);
        if inner
            .conns
            .values()
            .any(|b| b.serves.mint().as_ref() == Some(&key))
        {
            return None;
        }
        let since = inner
            .served_until
            .get(&key)
            .copied()
            .unwrap_or(self.started_ms);
        (now.saturating_sub(since) >= DOWN_AFTER.as_millis() as u64).then_some(since)
    }

    /// A Stratum relay resumed this connection's miner, which first
    /// connected at `since` (Unix ms) if the relay said: it continues that
    /// miner's session rather than starting one.
    pub fn resumed(&self, conn: ConnectionId, since: Option<u64>) {
        let mut inner = self.lock();
        let Some(b) = inner.conns.get_mut(&conn) else {
            return;
        };
        b.resumed = Some(since);
        if b.rig.is_some() {
            self.sessions().resumed(conn, unix_ms(), since);
        }
    }

    /// The daemon is stopping: connections closing from now on leave their
    /// rigs' sessions open, to be resumed through the relay.
    pub fn begin_shutdown(&self) {
        self.sessions().begin_shutdown();
    }

    /// Rig sessions open at or after `from_ms`, and since when sessions
    /// are recorded (Unix ms).
    pub fn rig_sessions(&self, from_ms: u64) -> (Vec<Session>, u64) {
        let mut log = self.sessions();
        (log.list(unix_ms(), from_ms), log.since_ms())
    }

    /// Carry on sessions saved by an earlier run (see
    /// [`SessionLog::restore`]).
    pub fn restore_sessions(&self, saved: Vec<Session>, alive_ms: u64, since_ms: u64) {
        self.sessions()
            .restore(saved, alive_ms, since_ms, unix_ms());
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, SessionLog> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
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
        let rig = b.rig.clone().unwrap_or_else(|| conn.to_string());
        let supplier = bound_to.and(b.supplier.clone());
        if let Some(k) = supplier {
            *inner.delivered.entry(k).or_default() += hashes;
        }
        let w = inner.rig_work.entry(rig).or_insert(RigWork {
            hashes: 0.0,
            last: at,
        });
        w.hashes += hashes;
        w.last = w.last.max(at);
    }

    /// Accepted work (hashes) of each rig since the proxy started, keyed
    /// by its authorize user (`conn-<n>` before one is seen), sorted by
    /// label. Reconnects under the same user add to the same rig. Rigs
    /// with no share for [`RIG_WORK_IDLE`] are forgotten.
    pub fn rig_work(&self) -> Vec<(String, f64)> {
        let now = Instant::now();
        let mut inner = self.lock();
        inner
            .rig_work
            .retain(|_, w| now.saturating_duration_since(w.last) < RIG_WORK_IDLE);
        let mut v: Vec<(String, f64)> = inner
            .rig_work
            .iter()
            .map(|(k, w)| (k.clone(), w.hashes))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Each rig's work with how long ago its last share came, for saving
    /// across a restart ([`Self::restore_rig_work`]).
    pub fn rig_work_saved(&self) -> Vec<(String, f64, Duration)> {
        let now = Instant::now();
        let inner = self.lock();
        let mut v: Vec<(String, f64, Duration)> = inner
            .rig_work
            .iter()
            .filter(|(_, w)| now.saturating_duration_since(w.last) < RIG_WORK_IDLE)
            .map(|(k, w)| (k.clone(), w.hashes, now.saturating_duration_since(w.last)))
            .collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Carry on rig totals saved by an earlier run: `(label, hashes, time
    /// since its last share)`. Rigs idle for [`RIG_WORK_IDLE`] are skipped.
    /// Adds to work already counted under the same label.
    pub fn restore_rig_work(&self, saved: Vec<(String, f64, Duration)>) {
        let now = Instant::now();
        let mut inner = self.lock();
        for (rig, hashes, idle) in saved {
            if idle >= RIG_WORK_IDLE || !hashes.is_finite() || hashes <= 0.0 {
                continue;
            }
            let last = now.checked_sub(idle).unwrap_or(now);
            let w = inner
                .rig_work
                .entry(rig)
                .or_insert(RigWork { hashes: 0.0, last });
            w.hashes += hashes;
            w.last = w.last.max(last);
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

    /// The rigs each redemption's hashrate comes from right now: live
    /// connections routed to it, as `(rig label, hashrate estimate)`.
    /// The label is the authorize user (`conn-<n>` before one is seen);
    /// the estimate is `None` until the rig has enough shares. Sorted by
    /// label within each redemption.
    pub fn rigs_by_redemption(&self) -> HashMap<RedemptionId, Vec<(String, Option<f64>)>> {
        let now = Instant::now();
        let inner = self.lock();
        let mut out: HashMap<RedemptionId, Vec<(String, Option<f64>)>> = HashMap::new();
        for (conn, b) in &inner.conns {
            if let Some(id) = b.route.redemption_id() {
                let label = b.rig.clone().unwrap_or_else(|| conn.to_string());
                out.entry(id).or_default().push((label, b.rate.rate(now)));
            }
        }
        for v in out.values_mut() {
            v.sort_by(|a, b| a.0.cmp(&b.0));
        }
        out
    }

    /// When each open order of a mint (`None` = the operator's) should
    /// start getting hashrate, and when a new one would. See
    /// [`RigQueue`].
    pub fn rig_queue(&self, mint: Option<&str>) -> RigQueue {
        self.rig_queue_at(mint, Instant::now())
    }

    fn rig_queue_at(&self, mint: Option<&str>, now: Instant) -> RigQueue {
        let want = Some(mint.map(str::to_string));
        let serves = |b: &Binding| b.serves.mint() == want;
        let active: Vec<Redemption> = self
            .routable_active()
            .into_iter()
            .filter(|r| r.supplier.as_deref() == mint)
            .collect();
        let inner = self.lock();
        let rigs: Vec<(Option<RedemptionId>, Option<f64>)> = inner
            .conns
            .values()
            .filter(|b| serves(b))
            .map(|b| (b.effective().redemption_id(), b.rate.rate(now)))
            .collect();
        let orders: Vec<QueuedOrder> = active
            .iter()
            .map(|r| QueuedOrder {
                id: r.id,
                remaining: r.target_hashes.saturating_sub(r.accumulated_hashes) as f64,
                target: r.target_hashes as f64,
                waited: inner
                    .unserved_since
                    .get(&r.id)
                    .map(|t| now.saturating_duration_since(*t))
                    .unwrap_or_default(),
                routable: redemption_route(r, &inner, now).is_some(),
            })
            .collect();
        drop(inner);
        estimate_starts(&orders, &rigs)
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
    /// was. A redeemer's pool that failed is marked down for that
    /// redemption for [`FALLBACK_HOLD`], so the next reconcile tries its
    /// next fallback pool; with none left, this connection is not moved
    /// to a redemption for that long.
    pub fn switch_failed(&self, conn: ConnectionId, target: &Route) {
        let now = Instant::now();
        let left = target.redemption_pool().and_then(|(id, pool)| {
            let r = self.ledger.get(&id)?;
            let mut inner = self.lock();
            inner.mark_down(id, pool, now);
            redemption_route(&r, &inner, now)
        });
        if let Some(b) = self.lock().conns.get_mut(&conn) {
            b.pending = None;
            if target.redemption_id().is_some() && left.is_none() {
                b.fell_back_at = Some(now);
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
        // A redeemer who changed pools starts fresh: forget which of the
        // old ones were down.
        for r in &active {
            if inner.pools_rev.get(&r.id).copied().unwrap_or(0) != r.pools_rev {
                inner.pools_rev.insert(r.id, r.pools_rev);
                inner.pool_down.retain(|(id, _), _| *id != r.id);
            }
        }
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
        // Starving orders a rig was taken for this pass (see
        // [`rescue`]), so one pass sends each just one rig.
        let mut rescued: std::collections::HashSet<RedemptionId> = Default::default();
        for (conn, _) in ids {
            let target = {
                let b = &inner.conns[&conn];
                if b.pending.is_some() || *b.signal.borrow() == Signal::Kick {
                    continue;
                }
                let mine = b.serves.filter(&active);
                match &b.route {
                    Route::Redemption { id, pool, .. } => {
                        if let Some(r) = mine.iter().find(|r| r.id == *id) {
                            // An order starved of rigs takes this one if
                            // its own order keeps another.
                            if let Some(to) = rescue(&mine, &inner, r, now, &rescued) {
                                if let Some(sid) = to.redemption_id() {
                                    rescued.insert(sid);
                                }
                                to
                            } else {
                                // Still open: stay unless its pool went down
                                // (move to the next one) or an earlier pool is
                                // back (try it again, in place only).
                                match redemption_route(r, &inner, now) {
                                    Some(best) => {
                                        let to = best.redemption_pool().map_or(0, |(_, p)| p);
                                        if best == b.route {
                                            continue;
                                        }
                                        // Same pool slot but a new pool behind
                                        // it (the redeemer changed pools): go.
                                        if to != *pool {
                                            if to < *pool && self.mode != SwitchMode::Seamless {
                                                continue;
                                            }
                                            if to > *pool && !inner.is_down(*id, *pool, now) {
                                                continue;
                                            }
                                        }
                                        best
                                    }
                                    // Every pool of it is down.
                                    None => pick_route(&mine, &inner, Some(conn), now, &b.idle),
                                }
                            }
                        } else if b.holding() {
                            b.idle.clone()
                        } else {
                            pick_route(&mine, &inner, Some(conn), now, &b.idle)
                        }
                    }
                    Route::Default { .. } | Route::Home { .. } => {
                        if mine.is_empty() || b.holding() {
                            continue;
                        }
                        let t = pick_route(&mine, &inner, Some(conn), now, &b.idle);
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
        // Tell the ledger which pool each redemption's rigs are on (the
        // earliest one if they are split), for status pages.
        let mut serving: HashMap<RedemptionId, usize> = HashMap::new();
        for b in inner.conns.values() {
            if let Some((id, pool)) = b.route.redemption_pool() {
                let e = serving.entry(id).or_insert(pool);
                *e = (*e).min(pool);
            }
        }
        inner.pool_down.retain(|(id, _), t| {
            active.iter().any(|r| r.id == *id) && now.saturating_duration_since(*t) < FALLBACK_HOLD
        });
        drop(inner);
        self.ledger.set_serving_pools(serving);
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

    /// Active redemptions with at least one pool the proxy can dial,
    /// oldest first. Paused ones get no hashrate until resumed.
    fn routable_active(&self) -> Vec<Redemption> {
        self.ledger
            .active()
            .into_iter()
            .filter(|r| !r.is_paused())
            .filter(|r| {
                let ok = r
                    .pools()
                    .iter()
                    .any(|p| upstream_for_pool_url(&p.pool_url).is_some());
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
    fn mark_down(&mut self, id: RedemptionId, pool: usize, now: Instant) {
        self.pool_down.insert((id, pool), now);
    }

    fn is_down(&self, id: RedemptionId, pool: usize, now: Instant) -> bool {
        self.pool_down
            .get(&(id, pool))
            .is_some_and(|t| now.saturating_duration_since(*t) < FALLBACK_HOLD)
    }

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

/// Where a rig already filling order `current` should go instead, if
/// anywhere: the oldest first-half order with no rig when `current` is
/// past half (taking even its only rig), else the oldest order in the same
/// half that has had no rig for [`STARVE_AFTER`] when `current` keeps
/// another rig. Without this a rig never left an open order, so a new or
/// starved order waited for the others to finish. `skip` lists orders
/// already sent a rig this pass.
fn rescue(
    active: &[Redemption],
    inner: &Inner,
    current: &Redemption,
    now: Instant,
    skip: &std::collections::HashSet<RedemptionId>,
) -> Option<Route> {
    let heading_to = |id: RedemptionId| {
        inner
            .conns
            .values()
            .filter(|b| b.effective().redemption_id() == Some(id))
            .count()
    };
    let empty =
        |r: &&Redemption| r.id != current.id && !skip.contains(&r.id) && heading_to(r.id) == 0;
    if !in_first_half(current) {
        let new = active
            .iter()
            .filter(empty)
            .filter(|r| in_first_half(r))
            .find_map(|r| redemption_route(r, inner, now));
        if new.is_some() {
            return new;
        }
    }
    if heading_to(current.id) < 2 {
        return None;
    }
    active
        .iter()
        .filter(empty)
        .filter(|r| in_first_half(r) == in_first_half(current))
        .filter(|r| {
            inner
                .unserved_since
                .get(&r.id)
                .is_some_and(|t| now.saturating_duration_since(*t) >= STARVE_AFTER)
        })
        .find_map(|r| redemption_route(r, inner, now))
}

/// When a mint's open orders should start getting hashrate: an estimate
/// from the rigs' measured rates and the dedicated-assignment rules (see
/// the module docs): a rig stays on its order until it is filled, except
/// that a first-half order with no rig takes a rig as soon as that rig's
/// order is past half (even its only rig), and an order waiting
/// [`STARVE_AFTER`] takes a spare rig from an order in the same half that
/// has two or more. Waiting first-half orders go before waiting orders
/// past half, oldest first within each. It ignores the slowdown of an
/// order that gives up a rig.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RigQueue {
    /// Rigs that fill this mint's orders.
    pub rigs: usize,
    /// Per open (unpaused) order.
    pub orders: HashMap<RedemptionId, OrderStart>,
    /// Seconds until a redemption opened now would get its first rig:
    /// 0 when a rig would take it right away; `None` when there is no
    /// estimate (no rigs, or none with a measured rate).
    pub new_order_start_s: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrderStart {
    /// Rigs on (or switching to) the order now.
    pub rigs: usize,
    /// Seconds until its first rig: 0 while it has one; `None` when
    /// there is no estimate (no rigs, none rated, or every pool of it is
    /// down).
    pub start_s: Option<f64>,
}

/// An open order as [`estimate_starts`] sees it, oldest first.
struct QueuedOrder {
    id: RedemptionId,
    /// Hashes still owed.
    remaining: f64,
    /// Hashes redeemed in all.
    target: f64,
    /// How long it has had no rig (zero while it has one).
    waited: Duration,
    /// Has a pool the proxy can dial and that is not marked down.
    routable: bool,
}

/// Simulate rigs freeing up and taking waiting orders. `rigs` lists each
/// rig of the mint as (order it is on, hashrate estimate in H/s).
fn estimate_starts(
    orders: &[QueuedOrder],
    rigs: &[(Option<RedemptionId>, Option<f64>)],
) -> RigQueue {
    let mut out = RigQueue {
        rigs: rigs.len(),
        ..Default::default()
    };
    // A rig with no estimate yet counts at the others' average.
    let rated: Vec<f64> = rigs
        .iter()
        .filter_map(|r| r.1)
        .filter(|h| *h > 0.0)
        .collect();
    let avg = (!rated.is_empty()).then(|| rated.iter().sum::<f64>() / rated.len() as f64);
    let rate_of = |h: Option<f64>| h.filter(|h| *h > 0.0).or(avg);

    #[derive(Clone)]
    struct Slot {
        /// Seconds from now until its order is filled.
        free_at: f64,
        /// Seconds from now until its order is past half (0 if it is).
        half_at: f64,
        rate: f64,
        /// Shares its order with another rig, so a starving order in the
        /// same half may take it before that order is filled.
        spare: bool,
    }
    let half_in =
        |remaining: f64, target: f64, rate: f64| ((remaining - target / 2.0) / rate).max(0.0);
    let mut slots: Vec<Slot> = Vec::new();
    for o in orders {
        let on: Vec<f64> = rigs
            .iter()
            .filter(|r| r.0 == Some(o.id))
            .filter_map(|r| rate_of(r.1))
            .collect();
        let count = rigs.iter().filter(|r| r.0 == Some(o.id)).count();
        out.orders.insert(
            o.id,
            OrderStart {
                rigs: count,
                start_s: (count > 0).then_some(0.0),
            },
        );
        let total: f64 = on.iter().sum();
        if total <= 0.0 {
            continue;
        }
        let done = o.remaining / total;
        let half_at = half_in(o.remaining, o.target, total);
        for (i, rate) in on.into_iter().enumerate() {
            slots.push(Slot {
                free_at: done,
                half_at,
                rate,
                spare: i > 0,
            });
        }
    }
    // Rigs not on an open order are free now.
    for r in rigs {
        if r.0.is_none_or(|id| !orders.iter().any(|o| o.id == id)) {
            if let Some(rate) = rate_of(r.1) {
                slots.push(Slot {
                    free_at: 0.0,
                    half_at: 0.0,
                    rate,
                    spare: false,
                });
            }
        }
    }
    // A waiting order takes the slot it can have first.
    let starve = STARVE_AFTER.as_secs_f64();
    let take = |slots: &mut Vec<Slot>, remaining: f64, target: f64, waited: f64| -> Option<f64> {
        let first_half = remaining * 2.0 > target;
        let rescue_at = (starve - waited).max(0.0);
        let at = |s: &Slot| {
            let mut t = s.free_at;
            if first_half {
                // Its order past half: a first-half order takes the rig.
                t = t.min(s.half_at);
                // A spare from an order also in its first half.
                if s.spare && rescue_at < s.half_at {
                    t = t.min(rescue_at);
                }
            } else if s.spare {
                // A spare once both are past half and it has starved.
                t = t.min(rescue_at.max(s.half_at));
            }
            t
        };
        let i = (0..slots.len()).min_by(|a, b| at(&slots[*a]).total_cmp(&at(&slots[*b])))?;
        let start = at(&slots[i]);
        let s = &mut slots[i];
        s.free_at = start + remaining / s.rate;
        s.half_at = start + half_in(remaining, target, s.rate);
        s.spare = false;
        Some(start)
    };
    let waiting = |first: bool| {
        orders
            .iter()
            .filter(move |o| (o.remaining * 2.0 > o.target) == first)
            .filter(|o| out.orders[&o.id].rigs == 0)
            .map(|o| o.id)
            .collect::<Vec<_>>()
    };
    let (first, second) = (waiting(true), waiting(false));
    let mut starts: Vec<(RedemptionId, Option<f64>)> = Vec::new();
    let mut run = |slots: &mut Vec<Slot>, ids: &[RedemptionId]| {
        for id in ids {
            let o = orders.iter().find(|o| o.id == *id).expect("listed");
            let start = if o.routable {
                take(slots, o.remaining, o.target, o.waited.as_secs_f64())
            } else {
                None
            };
            starts.push((*id, start));
        }
    };
    run(&mut slots, &first);
    // A new order is in its first half and newest: after the waiting
    // first-half orders, ahead of those past half.
    out.new_order_start_s = take(&mut slots.clone(), 1.0, 1.0, 0.0);
    run(&mut slots, &second);
    for (id, start) in starts {
        if let Some(e) = out.orders.get_mut(&id) {
            e.start_s = start;
        }
    }
    out
}

/// The route to `r`'s first pool that the proxy can dial and that is not
/// marked down, or `None` if every one is.
fn redemption_route(r: &Redemption, inner: &Inner, now: Instant) -> Option<Route> {
    r.pools().into_iter().enumerate().find_map(|(pool, p)| {
        if inner.is_down(r.id, pool, now) {
            return None;
        }
        let upstream = upstream_for_pool_url(&p.pool_url)?;
        Some(Route::Redemption {
            id: r.id,
            pool,
            upstream,
            worker_password: p.authorize_password().to_string(),
            worker_name: delivery_worker(
                &p.worker_name,
                inner.hosts.host_for(r.supplier.as_deref()).as_deref(),
            ),
        })
    })
}

/// One active order as [`pick_route`] sees it.
struct Order {
    /// Where its rigs go: its first healthy pool.
    route: Route,
    /// Index in melt order (oldest first).
    age: usize,
    /// Connections heading to it, other than the one being placed.
    load: usize,
    /// Hashrate it can still absorb before it finishes within
    /// [`TARGET_RUN`]; `≤ 0` means capped.
    room: f64,
    /// Waited past [`STARVE_AFTER`] with no rig.
    starving: bool,
    /// Less than half its hashes delivered.
    first_half: bool,
}

/// Whether `r` has had less than half its hashes delivered.
fn in_first_half(r: &Redemption) -> bool {
    r.accumulated_hashes.saturating_mul(2) < r.target_hashes
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
        .filter_map(|(age, r)| Some((age, r, redemption_route(r, inner, now)?)))
        .map(|(age, r, route)| {
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
                route,
                age,
                load,
                room: remaining / target_run - assigned,
                starving,
                first_half: in_first_half(r),
            }
        })
        .filter(|o| o.room > 0.0)
        .collect();

    // First half before second half; within each, starving, then
    // unserved, then served; then orders the rig fits; fewest rigs, then
    // oldest.
    let rank = |o: &Order| {
        let tier = match (o.starving, o.load) {
            (true, _) => 0u8,
            (false, 0) => 1,
            _ => 2,
        };
        // A rig that would finish an order within TARGET_RUN on its own
        // overshoots it: prefer orders it fits.
        let misfit = rig.is_some_and(|h| o.room < h);
        (!o.first_half, tier, misfit, o.load, o.age)
    };
    let best = orders.iter().min_by_key(|o| rank(o));

    match best {
        Some(o) => o.route.clone(),
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
    /// A live connection proved it is this registered supplier (`pw=`).
    pub verified: bool,
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
                pool: 0,
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
    fn rigs_by_redemption_lists_each_orders_rigs() {
        let ledger = Arc::new(RedemptionLedger::new());
        open(&ledger, 1, "stratum+tcp://alice:3333", "a");
        open(&ledger, 2, "stratum+tcp://bob:3333", "b");
        let router = Router::new("default:3333", ledger);
        let one = serde_json::json!(["acct.rig1", "x"]);
        let two = serde_json::json!(["acct.rig2", "x"]);
        router.assign_with(ConnectionId::next(), Some(&one));
        router.assign_with(ConnectionId::next(), Some(&two));
        let by = router.rigs_by_redemption();
        assert_eq!(by[&rid(1)], vec![("acct.rig1".to_string(), None)]);
        assert_eq!(by[&rid(2)], vec![("acct.rig2".to_string(), None)]);
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
        let r = router.fail_over(a.conn);
        assert_eq!(router.current(a.conn), None);
        assert_eq!(r.redemption_id(), None);
        assert_eq!(router.route_of(a.conn), Some(r));
        // A fresh fallback is not kicked straight back onto the dead pool.
        assert_eq!(router.reconcile(), 0);
    }

    fn open_with_fallbacks(ledger: &RedemptionLedger, n: u8, pools: &[&str]) {
        let fallbacks = pools[1..]
            .iter()
            .enumerate()
            .map(|(i, p)| hashu_core::redemption::PoolTarget {
                pool_url: (*p).into(),
                worker_name: format!("w{}", i + 1),
                worker_password: None,
            })
            .collect();
        ledger
            .open(
                Redemption::new(rid(n), 1, pools[0].into(), "w0".into()).with_fallbacks(fallbacks),
            )
            .unwrap();
    }

    #[test]
    fn unreachable_primary_fails_over_to_the_next_pool() {
        let ledger = Arc::new(RedemptionLedger::new());
        open_with_fallbacks(
            &ledger,
            1,
            &[
                "stratum+tcp://main:3333",
                "stratum+tcp://backup:3333",
                "stratum+tcp://third:3333",
            ],
        );
        let router = Router::new("default:3333", ledger.clone());
        let a = router.assign(ConnectionId::next());
        assert_eq!(a.route.redemption_pool(), Some((rid(1), 0)));
        // Dial fails: next pool, with its own worker, same redemption.
        let r = router.fail_over(a.conn);
        assert_eq!(r.redemption_pool(), Some((rid(1), 1)));
        assert_eq!(r.upstream(), "backup:3333");
        assert_eq!(r.credentials(), Some(("w1", "x")));
        assert_eq!(router.current(a.conn), Some(rid(1)));
        // A new rig skips the dead primary too.
        let b = router.assign(ConnectionId::next());
        assert_eq!(b.route.redemption_pool(), Some((rid(1), 1)));
        // Then the third pool, then the default pool.
        assert_eq!(
            router.fail_over(a.conn).redemption_pool(),
            Some((rid(1), 2))
        );
        assert_eq!(router.fail_over(a.conn).redemption_id(), None);
        assert_eq!(router.reconcile(), 1, "b moves off the dead backup");
    }

    #[test]
    fn a_failing_pool_moves_live_rigs_to_the_fallback_and_back() {
        let ledger = Arc::new(RedemptionLedger::new());
        open_with_fallbacks(
            &ledger,
            1,
            &["stratum+tcp://main:3333", "stratum+tcp://backup:3333"],
        );
        let router = Router::new("default:3333", ledger.clone());
        let a = router.assign(ConnectionId::next());
        let b = router.assign(ConnectionId::next());
        assert_eq!(router.reconcile(), 0);
        assert_eq!(ledger.serving_pool(&rid(1)), Some(0));

        // The primary starts rejecting a's work: both rigs move over.
        router.pool_failed(a.conn, &a.route, "rejects");
        assert_eq!(router.reconcile(), 2);
        for x in [&a, &b] {
            let t = switch_target(x);
            assert_eq!(t.redemption_pool(), Some((rid(1), 1)));
            router.switched(x.conn, t);
        }
        assert_eq!(router.reconcile(), 0);
        assert_eq!(ledger.serving_pool(&rid(1)), Some(1));
        // Shares on the fallback still fill the same redemption.
        assert_eq!(router.current(a.conn), Some(rid(1)));

        // Once the hold is over the primary is tried again.
        let later = Instant::now() + FALLBACK_HOLD + Duration::from_secs(1);
        assert_eq!(router.reconcile_at(later), 2);
        assert_eq!(switch_target(&a).redemption_pool(), Some((rid(1), 0)));
        // That probe fails: the rig stays on the fallback.
        router.switch_failed(a.conn, &switch_target(&a));
        assert_eq!(
            router.route_of(a.conn).unwrap().redemption_pool(),
            Some((rid(1), 1))
        );
        assert_eq!(router.current(a.conn), Some(rid(1)));
    }

    #[test]
    fn failed_switch_tries_the_fallback_next() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let a = router.assign(ConnectionId::next());
        open_with_fallbacks(
            &ledger,
            1,
            &["stratum+tcp://dead:3333", "stratum+tcp://backup:3333"],
        );
        assert_eq!(router.reconcile(), 1);
        let t = switch_target(&a);
        assert_eq!(t.redemption_pool(), Some((rid(1), 0)));
        router.switch_failed(a.conn, &t);
        // Not held on the default pool: the backup is tried right away.
        assert_eq!(router.reconcile(), 1);
        assert_eq!(switch_target(&a).redemption_pool(), Some((rid(1), 1)));
    }

    #[test]
    fn reconnect_mode_stays_on_the_fallback() {
        let ledger = Arc::new(RedemptionLedger::new());
        open_with_fallbacks(
            &ledger,
            1,
            &["stratum+tcp://main:3333", "stratum+tcp://backup:3333"],
        );
        let router =
            Router::new("default:3333", ledger.clone()).with_switch_mode(SwitchMode::Reconnect);
        let a = router.assign(ConnectionId::next());
        assert_eq!(
            router.fail_over(a.conn).redemption_pool(),
            Some((rid(1), 1))
        );
        let later = Instant::now() + FALLBACK_HOLD + Duration::from_secs(1);
        // No kick just to probe the primary again.
        assert_eq!(router.reconcile_at(later), 0);
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
    fn changing_pools_moves_the_rigs_in_place() {
        use hashu_core::redemption::PoolTarget;
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let a = router.assign(ConnectionId::next());
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        assert_eq!(router.reconcile(), 1);
        let target = switch_target(&a);
        router.switched(a.conn, target);
        assert_eq!(router.reconcile(), 0);

        ledger
            .change_pools(
                &rid(1),
                PoolTarget {
                    pool_url: "stratum+tcp://bob:4444".into(),
                    worker_name: "bob.w".into(),
                    worker_password: None,
                },
                vec![],
            )
            .unwrap();
        assert_eq!(router.reconcile(), 1);
        let to = switch_target(&a);
        assert_eq!(to.redemption_id(), Some(rid(1)));
        assert_eq!(to.upstream(), "bob:4444");
        router.switched(a.conn, to);
        assert_eq!(router.current(a.conn), Some(rid(1)));
        assert_eq!(router.reconcile(), 0);
    }

    #[test]
    fn paused_redemption_gets_no_hashrate_until_resumed() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let a = router.assign(ConnectionId::next());
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        assert_eq!(router.reconcile(), 1);
        let target = switch_target(&a);
        router.switched(a.conn, target);
        assert_eq!(router.current(a.conn), Some(rid(1)));

        // Paused: the rig goes back to the default pool.
        ledger
            .pause(&rid(1), hashu_core::redemption::Pauser::Operator)
            .unwrap();
        assert_eq!(router.reconcile(), 1);
        let back = switch_target(&a);
        assert_eq!(back.redemption_id(), None);
        router.switched(a.conn, back);
        assert_eq!(router.reconcile(), 0);
        // A new rig isn't placed on it either.
        let b = router.assign(ConnectionId::next());
        assert_eq!(b.route.redemption_id(), None);

        // Resumed: the rigs come back.
        ledger
            .resume(&rid(1), hashu_core::redemption::Pauser::Operator)
            .unwrap();
        assert!(router.reconcile() >= 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(1)));
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

    /// Open an order of `thh` terahash-hours' worth of TiH, so the rig
    /// sizes below (TH/s) read naturally.
    fn open_thh(ledger: &RedemptionLedger, n: u8, thh: u64) {
        let tih = (thh as f64 * hashu_core::denom::TIH_PER_THH).round() as u64;
        ledger
            .open(Redemption::new(
                rid(n),
                tih,
                format!("stratum+tcp://pool{n}:3333"),
                format!("w{n}"),
            ))
            .unwrap();
    }

    use hashu_core::redemption::HASHES_PER_DIFF_ONE;

    #[test]
    fn a_mint_is_down_once_its_rigs_are_gone_for_two_minutes() {
        let router = Router::new("pool:3333", Arc::new(RedemptionLedger::new()));
        let t0 = router.started_ms;
        let two = DOWN_AFTER.as_millis() as u64;
        // No rig since start.
        assert_eq!(router.down_since_at(None, t0 + two - 1), None);
        assert_eq!(router.down_since_at(None, t0 + two), Some(t0));
        // A rig connects: up.
        let conn = ConnectionId(1);
        router.assign_with(conn, Some(&serde_json::json!(["bc1qxyz.rig1", "x"])));
        assert_eq!(router.down_since_at(None, t0 + 10 * two), None);
        // It leaves: down again two minutes later, since it left.
        router.release(conn);
        let left = router.lock().served_until[&None];
        assert_eq!(router.down_since_at(None, left + two - 1), None);
        assert_eq!(router.down_since_at(None, left + two), Some(left));
        // A supplier's mint is separate.
        assert_eq!(router.down_since_at(Some("alice"), t0 + two), Some(t0));
    }

    #[test]
    fn rig_work_is_kept_per_authorize_user_across_reconnects() {
        let router = Router::new("pool:3333", Arc::new(RedemptionLedger::new()));
        let auth = serde_json::json!(["alice.rig1", "x"]);
        let a = router.assign_with(ConnectionId::next(), Some(&auth));
        router.observe_share(a.conn, 1.0, None);
        router.release(a.conn);
        let b = router.assign_with(ConnectionId::next(), Some(&auth));
        router.observe_share(b.conn, 1.0, None);
        // No authorize yet: labelled by connection until it sends one.
        let c = router.assign(ConnectionId::next());
        router.observe_share(c.conn, 2.0, None);
        let one = hashu_core::redemption::hashes_for_difficulty(1.0) as f64;
        assert_eq!(
            router.rig_work(),
            vec![
                ("alice.rig1".to_string(), 2.0 * one),
                (c.conn.to_string(), 2.0 * one),
            ]
        );
        router.identify(c.conn, &serde_json::json!(["bob.rig2", "x"]));
        router.observe_share(c.conn, 1.0, None);
        assert!(router.rig_work().contains(&("bob.rig2".to_string(), one)));
    }

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
        // Small order first (older), large order second. 1 TiH over a
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
        // Four 50 TH/s rigs; 1 TiH needs 60 TH/s to finish in a minute.
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
        // The 100 TH/s rig alone finishes 1 TiH in 36 s: the order is
        // capped, so a new rig goes to the default pool.
        let b = router.assign(ConnectionId::next());
        assert_eq!(b.route.redemption_id(), None);
    }

    #[test]
    fn big_rig_with_only_small_orders_takes_the_oldest() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 500.0, now);
        open_thh(&ledger, 1, 2);
        open_thh(&ledger, 2, 5);
        open_thh(&ledger, 3, 3);
        assert_eq!(router.reconcile_at(now), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(1)));
    }

    #[test]
    fn rigs_go_oldest_first_whatever_the_order_size() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 1.0, now);
        open_thh(&ledger, 1, 100);
        open_thh(&ledger, 2, 3);
        open_thh(&ledger, 3, 10);
        assert_eq!(router.reconcile_at(now), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(1)));
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
        // The 100 TH/s rig fits the 1000 TiH order, but the new 10 TiH
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
        // Both starved: oldest first.
        let later = t0 + STARVE_AFTER;
        assert_eq!(router.reconcile_at(later), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(1)));
    }

    /// Deliver `frac` of order `n`'s hashes.
    fn fill(ledger: &RedemptionLedger, n: u8, frac: f64) {
        let r = ledger
            .active()
            .into_iter()
            .find(|r| r.id == rid(n))
            .unwrap();
        let diff = frac * r.target_hashes as f64 / HASHES_PER_DIFF_ONE as f64;
        ledger.credit_share(&rid(n), diff).unwrap();
    }

    #[test]
    fn a_dropped_pool_session_is_retried_before_marking_it_down() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 1000);
        let auth = serde_json::json!(["acct.rig1", "x"]);
        let a = router.assign_with(ConnectionId::next(), Some(&auth));
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        // A newer first-half order a fresh rig would take instead.
        open_thh(&ledger, 2, 1000);
        fill(&ledger, 1, 0.6);
        router.pool_dropped_at(a.conn, &a.route, "pool closed", t0);
        router.release(a.conn);
        assert!(!router.lock().is_down(rid(1), 0, t0));
        // It reconnects (through the relay): back to order 1.
        let b = router.assign_with(ConnectionId::next(), Some(&auth));
        assert_eq!(b.route.redemption_id(), Some(rid(1)));
        // Drops close together count once; DROP_LIMIT spaced drops mark
        // the pool down.
        router.pool_dropped_at(b.conn, &b.route, "pool closed", t0 + Duration::from_secs(1));
        assert!(!router.lock().is_down(rid(1), 0, t0));
        let t1 = t0 + DROP_DEBOUNCE;
        router.pool_dropped_at(b.conn, &b.route, "pool closed", t1);
        assert!(!router.lock().is_down(rid(1), 0, t1));
        let t2 = t1 + DROP_DEBOUNCE;
        router.pool_dropped_at(b.conn, &b.route, "pool closed", t2);
        assert!(router.lock().is_down(rid(1), 0, t2));
    }

    #[test]
    fn first_half_orders_come_before_orders_past_half() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let now = Instant::now();
        open_thh(&ledger, 1, 1000);
        open_thh(&ledger, 2, 1000);
        fill(&ledger, 1, 0.6);
        // Both have no rig; the newer one is still in its first half.
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 10.0, now);
        assert_eq!(a.route.redemption_id(), Some(rid(2)));
    }

    #[test]
    fn a_new_order_takes_the_only_rig_from_one_past_half() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 1000);
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 10.0, t0);
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        // Still in its first half: a new order waits.
        open_thh(&ledger, 2, 1000);
        assert_eq!(router.reconcile_at(t0), 0);
        // Past half: the new order takes the rig at once.
        fill(&ledger, 1, 0.6);
        assert_eq!(router.reconcile_at(t0), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(2)));
    }

    #[test]
    fn a_lone_rig_on_an_order_past_half_is_not_taken_by_another_past_half() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 1000);
        open_thh(&ledger, 2, 1000);
        fill(&ledger, 1, 0.8);
        fill(&ledger, 2, 0.6);
        // Both past half: the rig takes the oldest and stays there, even
        // once order 2 has waited past STARVE_AFTER (its only rig).
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 10.0, t0);
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        assert_eq!(router.reconcile_at(t0), 0);
        assert_eq!(router.reconcile_at(t0 + STARVE_AFTER), 0);
    }

    #[test]
    fn starving_order_takes_a_rig_from_a_shared_one() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 1000);
        // Both rigs on order 1.
        let a = router.assign(ConnectionId::next());
        let b = router.assign(ConnectionId::next());
        warm(&router, a.conn, 8.0, t0);
        warm(&router, b.conn, 6.0, t0);
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        assert_eq!(b.route.redemption_id(), Some(rid(1)));
        open_thh(&ledger, 2, 1000);
        // Not starving yet: both stay.
        assert_eq!(router.reconcile_at(t0), 0);
        // Starved: one rig (the bigger, placed first) moves over, only one.
        assert_eq!(router.reconcile_at(t0 + STARVE_AFTER), 1);
        assert_eq!(switch_target(&a).redemption_id(), Some(rid(2)));
        // Served again: nothing more moves.
        assert_eq!(router.reconcile_at(t0 + STARVE_AFTER * 2), 0);
    }

    #[test]
    fn a_lone_rig_stays_on_its_order() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 1000);
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 8.0, t0);
        open_thh(&ledger, 2, 1000);
        assert_eq!(router.reconcile_at(t0), 0);
        // One miner: melt order, no flip-flopping.
        assert_eq!(router.reconcile_at(t0 + STARVE_AFTER), 0);
    }

    #[test]
    fn a_lone_rig_moves_to_a_new_order_when_its_own_is_half_done() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 10);
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 10.0, t0);
        open_thh(&ledger, 2, 5);
        router.reconcile_at(t0);
        let q = router.rig_queue_at(None, t0);
        assert_eq!(q.rigs, 1);
        assert_eq!(q.orders[&rid(1)].rigs, 1);
        assert_eq!(q.orders[&rid(1)].start_s, Some(0.0));
        // 10 THh at 10 TH/s: half done in about half an hour, then the
        // rig moves to order 2 (5 THh, half done 15 min later).
        let s2 = q.orders[&rid(2)].start_s.unwrap();
        assert!((s2 - 1800.0).abs() < 120.0, "{s2}");
        let new = q.new_order_start_s.unwrap();
        assert!((new - 2700.0).abs() < 180.0, "{new}");
    }

    #[test]
    fn a_rig_on_an_order_past_half_starts_a_new_order_now() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 10);
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 10.0, t0);
        fill(&ledger, 1, 0.6);
        let q = router.rig_queue_at(None, t0);
        assert_eq!(q.new_order_start_s, Some(0.0));
    }

    #[test]
    fn a_waiting_order_expects_a_spare_rig_once_it_starves() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        open_thh(&ledger, 1, 1000);
        let a = router.assign(ConnectionId::next());
        let b = router.assign(ConnectionId::next());
        warm(&router, a.conn, 8.0, t0);
        warm(&router, b.conn, 6.0, t0);
        open_thh(&ledger, 2, 1000);
        router.reconcile_at(t0);
        let q = router.rig_queue_at(None, t0);
        assert_eq!(q.orders[&rid(1)].rigs, 2);
        assert_eq!(q.orders[&rid(2)].rigs, 0);
        assert_eq!(q.orders[&rid(2)].start_s, Some(STARVE_AFTER.as_secs_f64()));
    }

    #[test]
    fn a_free_rig_starts_a_new_order_now_and_no_rigs_means_no_estimate() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = Router::new("default:3333", ledger.clone());
        let t0 = Instant::now();
        assert_eq!(router.rig_queue_at(None, t0).new_order_start_s, None);
        let a = router.assign(ConnectionId::next());
        warm(&router, a.conn, 10.0, t0);
        let q = router.rig_queue_at(None, t0);
        assert_eq!(q.rigs, 1);
        assert_eq!(q.new_order_start_s, Some(0.0));
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
        let r = router.fail_over(a.conn);
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
    fn account_users_without_a_home_idle_on_the_default_pool() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = operator_router(ledger.clone());
        assert_eq!(router.refusal(&serde_json::json!(["bob.rig1", "x"])), None);
        open(&ledger, 1, "stratum+tcp://alice:3333", "alice.w");
        let bob = rig(&router, "bob.rig1", "x");
        assert_eq!(
            bob.route,
            Route::Default {
                upstream: "default:3333".into()
            },
            "a stranger idles on the default pool and is never borrowed"
        );
        // Only with no default pool at all is it refused.
        let nowhere = Router::new("", ledger).with_home_pools(HomeConfig::default());
        assert_eq!(
            nowhere.refusal(&serde_json::json!(["bob.rig1", "x"])),
            Some(crate::home::REFUSE_NO_HOME.to_string())
        );
        assert_eq!(nowhere.refusal(&serde_json::json!([ADDR, "x"])), None);
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

    fn supplier_router(ledger: Arc<RedemptionLedger>) -> Router {
        Router::new("default:3333", ledger)
            .with_home_pools(HomeConfig {
                operator_worker: Some("hashu".into()),
                ..HomeConfig::default()
            })
            .with_suppliers(Arc::new(SupplierRegistry::from_secrets([
                ("alice", "a-secret"),
                ("bob", "b-secret"),
            ])))
    }

    fn open_for(ledger: &RedemptionLedger, n: u8, supplier: Option<&str>) {
        ledger
            .open(
                Redemption::new(
                    rid(n),
                    1,
                    format!("stratum+tcp://buyer{n}:3333"),
                    format!("buyer{n}.w"),
                )
                .with_supplier(supplier.map(str::to_string)),
            )
            .unwrap();
    }

    fn rig(router: &Router, user: &str, pw: &str) -> Assignment {
        router.assign_with(ConnectionId::next(), Some(&serde_json::json!([user, pw])))
    }

    #[test]
    fn orders_only_go_to_their_own_suppliers_rigs() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = supplier_router(ledger.clone());
        open_for(&ledger, 1, Some("alice"));
        open_for(&ledger, 2, None);

        let home = "pw=a-secret,pool=public-pool.io:21496";
        let alice = rig(&router, "alice.rig1", home);
        assert_eq!(alice.route.redemption_id(), Some(rid(1)));
        let bob = rig(&router, "bob.rig1", "pw=b-secret,pool=public-pool.io:21496");
        assert!(matches!(bob.route, Route::Home { .. }), "{:?}", bob.route);
        let op = rig(&router, "hashu.mrr1", "x");
        assert_eq!(op.route.redemption_id(), Some(rid(2)));
        // A second order from Alice goes to her next rig, in parallel.
        open_for(&ledger, 3, Some("alice"));
        let alice2 = rig(&router, "alice.rig2", home);
        assert_eq!(alice2.route.redemption_id(), Some(rid(3)));
        // Bob's home pool password never carries pw=.
        if let Route::Home {
            worker_password, ..
        } = &bob.route
        {
            assert!(!worker_password.contains("pw="));
        }
    }

    #[test]
    fn delivery_worker_takes_the_mint_host_as_its_label() {
        let h = Some("mint.hashu.io");
        assert_eq!(delivery_worker("bc1qabc", h), "bc1qabc.mint.hashu.io");
        assert_eq!(delivery_worker("bc1qabc.hashu", h), "bc1qabc.mint.hashu.io");
        assert_eq!(
            delivery_worker(" acct.rig.7 ", Some("alice.hashu.io")),
            "acct.alice.hashu.io"
        );
        // Already named: unchanged.
        assert_eq!(
            delivery_worker("bc1qabc.mint.hashu.io", h),
            "bc1qabc.mint.hashu.io"
        );
        // No host, or an address whose dots are not a label: as typed.
        assert_eq!(delivery_worker("bc1qabc.hashu", None), "bc1qabc.hashu");
        assert_eq!(delivery_worker("me@getalby.com", h), "me@getalby.com");
        assert_eq!(delivery_worker(".x", h), ".x");
    }

    #[test]
    fn redeemers_see_the_mint_host_and_idle_rigs_keep_their_name() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = supplier_router(ledger.clone()).with_mint_hosts(MintHosts {
            operator: Some("mint.hashu.io".into()),
            suppliers: Some("hashu.io".into()),
        });
        open_for(&ledger, 1, Some("alice"));
        open_for(&ledger, 2, None);
        let worker = |a: &Assignment| a.route.credentials().map(|(w, _)| w.to_string());

        // Alice's rig fills Alice's order: Bob's pool sees alice.hashu.io.
        let alice = rig(&router, "alice.rig1", "pw=a-secret");
        assert_eq!(alice.route.redemption_id(), Some(rid(1)));
        assert_eq!(worker(&alice).as_deref(), Some("buyer1.alice.hashu.io"));
        let line =
            r#"{"id":4,"method":"mining.submit","params":["alice.rig1","j","00","00","00"]}"#;
        let out: Value =
            serde_json::from_str(&alice.route.rewrite_miner_line(line).unwrap()).unwrap();
        assert_eq!(out["params"][0], "buyer1.alice.hashu.io");

        // Hashu's own rig fills the operator mint's order.
        let op = rig(&router, "hashu.mrr1", "x");
        assert_eq!(op.route.redemption_id(), Some(rid(2)));
        assert_eq!(worker(&op).as_deref(), Some("buyer2.mint.hashu.io"));

        // Bob has no order: his rig idles on the default pool under its
        // own name, its pw= taken out.
        let idle = rig(&router, "bob.rig1", "pw=b-secret");
        assert!(
            matches!(idle.route, Route::Default { .. }),
            "{:?}",
            idle.route
        );
        let auth = r#"{"id":2,"method":"mining.authorize","params":["bob.rig1","pw=b-secret"]}"#;
        let out: Value =
            serde_json::from_str(&idle.route.rewrite_miner_line(auth).unwrap()).unwrap();
        assert_eq!(out["params"][0], "bob.rig1");
        assert!(!out["params"][1].as_str().unwrap_or("").contains("pw="));
    }

    #[test]
    fn spare_supplier_rigs_join_an_open_order() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = supplier_router(ledger.clone());
        open_for(&ledger, 1, Some("alice"));
        let home = "pw=a-secret,pool=public-pool.io:21496";
        let a = rig(&router, "alice.rig1", home);
        let b = rig(&router, "alice.rig2", home);
        assert_eq!(a.route.redemption_id(), Some(rid(1)));
        assert_eq!(
            b.route.redemption_id(),
            Some(rid(1)),
            "one invoice, several rigs"
        );
    }

    #[test]
    fn supplier_rigs_go_home_when_their_order_fills() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = supplier_router(ledger.clone());
        let a = rig(
            &router,
            "alice.rig1",
            "pw=a-secret,pool=public-pool.io:21496",
        );
        assert!(matches!(a.route, Route::Home { .. }));
        // Another supplier's or the operator's order never takes her rig.
        open_for(&ledger, 1, Some("bob"));
        open_for(&ledger, 2, None);
        assert_eq!(router.reconcile(), 0);
        open_for(&ledger, 3, Some("alice"));
        assert_eq!(router.reconcile(), 1);
        let t = switch_target(&a);
        assert_eq!(t.redemption_id(), Some(rid(3)));
        router.switched(a.conn, t);
        ledger.credit_share(&rid(3), 1e9).unwrap();
        assert_eq!(router.reconcile(), 1);
        assert!(matches!(switch_target(&a), Route::Home { .. }));
    }

    #[test]
    fn supplier_names_need_their_secret_and_a_home() {
        let router = supplier_router(Arc::new(RedemptionLedger::new()));
        let refuse = |u: &str, p: &str| router.refusal(&serde_json::json!([u, p]));
        assert_eq!(
            refuse("alice.rig1", "pw=a-secret,pool=public-pool.io:21496"),
            None
        );
        assert_eq!(
            refuse("alice.rig1", "pw=wrong,pool=public-pool.io:21496"),
            Some(format!("{REFUSE_WRONG_SECRET} alice"))
        );
        assert_eq!(
            refuse("alice.rig1", "pool=public-pool.io:21496"),
            Some(format!("{REFUSE_WRONG_SECRET} alice"))
        );
        // No home pool: it idles on the default pool like any rig.
        assert_eq!(refuse("alice.rig1", "pw=a-secret"), None);
    }

    fn secret_router(ledger: Arc<RedemptionLedger>) -> Router {
        Router::new("default:3333", ledger)
            .with_home_pools(HomeConfig {
                operator_secret_sha256: Some(crate::suppliers::secret_hash("op-secret")),
                ..HomeConfig::default()
            })
            .with_suppliers(Arc::new(SupplierRegistry::from_secrets([(
                "alice", "a-secret",
            )])))
    }

    #[test]
    fn operator_secret_makes_any_user_one_of_hashus_rigs() {
        let tb = "tb1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq.bitaxe1";
        // (user, password, borrowed for the operator's order?)
        let cases = [
            (tb, "pw=op-secret,pool=public-pool.io:21496", true),
            ("acct.hashu.bitaxe1", "pw=op-secret", true),
            // The operator secret wins even on a supplier's name.
            ("alice.rig9", "pw=op-secret", true),
            ("acct.hashu.bitaxe2", "pw=nope", false),
            ("acct.hashu.bitaxe3", "x", false),
            (tb, "pool=public-pool.io:21496", false),
        ];
        for (user, pw, borrowed) in cases {
            let ledger = Arc::new(RedemptionLedger::new());
            let router = secret_router(ledger.clone());
            open_for(&ledger, 1, None);
            assert_eq!(
                router.refusal(&serde_json::json!([user, pw])),
                None,
                "{user} {pw}"
            );
            let r = rig(&router, user, pw);
            assert_eq!(
                r.route.redemption_id().is_some(),
                borrowed,
                "{user} {pw}: {:?}",
                r.route
            );
        }
    }

    #[test]
    fn operator_rigs_ignore_a_supplier_with_their_name() {
        let ledger = Arc::new(RedemptionLedger::new());
        let router = supplier_router(ledger.clone()).with_suppliers(Arc::new(
            SupplierRegistry::from_secrets([("hashu", "squatter")]),
        ));
        assert_eq!(
            router.refusal(&serde_json::json!(["hashu.mrr1", "x"])),
            None
        );
        open_for(&ledger, 1, None);
        open_for(&ledger, 2, Some("hashu"));
        let op = rig(&router, "hashu.mrr1", "x");
        assert_eq!(op.route.redemption_id(), Some(rid(1)));
        // Even with the registered secret, it stays one of Hashu's rigs.
        let op2 = rig(&router, "hashu.mrr2", "pw=squatter");
        assert_eq!(op2.route.redemption_id(), Some(rid(1)));
    }

    #[test]
    fn supplier_hashrate_counts_only_verified_rigs() {
        let router = supplier_router(Arc::new(RedemptionLedger::new()));
        let a = rig(
            &router,
            "alice.rig1",
            "pw=a-secret,pool=public-pool.io:21496",
        );
        let fake = rig(&router, "alicex.rig1", "pool=public-pool.io:21496");
        warm(&router, a.conn, 1.0, Instant::now());
        warm(&router, fake.conn, 1.0, Instant::now());
        assert!(router.supplier_hashrate("alice") > 0.0);
        assert_eq!(router.supplier_hashrate("alicex"), 0.0);
        let v = router.suppliers();
        assert!(v.iter().any(|s| s.supplier == "alice" && s.verified));
        assert!(v.iter().any(|s| s.supplier == "alicex" && !s.verified));
    }

    #[test]
    fn operator_hashrate_skips_supplier_rigs() {
        let router = supplier_router(Arc::new(RedemptionLedger::new()));
        let a = rig(
            &router,
            "alice.rig1",
            "pw=a-secret,pool=public-pool.io:21496",
        );
        warm(&router, a.conn, 1.0, Instant::now());
        assert_eq!(router.operator_hashrate(), 0.0);
        let op = rig(&router, "hashu.bitaxe", "x");
        warm(&router, op.conn, 1.0, Instant::now());
        assert!(router.operator_hashrate() > 0.0);
    }

    #[test]
    fn switch_mode_parses() {
        assert_eq!("seamless".parse::<SwitchMode>(), Ok(SwitchMode::Seamless));
        assert_eq!("Reconnect".parse::<SwitchMode>(), Ok(SwitchMode::Reconnect));
        assert!("bogus".parse::<SwitchMode>().is_err());
    }
}
