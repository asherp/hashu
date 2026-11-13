//! Supplier mints: one cdk mint per registered supplier, served from the
//! same process as the operator's mint and picked by the request's Host.
//!
//! `alice.mint.hashu.io` is supplier `alice`'s mint when the operator's
//! mint is `https://mint.hashu.io` and `alice` is in the supplier registry
//! (`HASHU_SUPPLIERS_FILE`). Each supplier mint has its own seed, keys and
//! database under `<data-dir>/mints/<name>/`, and writes its redemptions
//! into the one ledger the proxy router reads, tagged with the supplier's
//! name, so only that supplier's rigs fill them.
//!
//! Supplier mints are built when the daemon starts and, for names added to
//! the registry later, on their first request. Names removed from the
//! registry stop being served (their data stays on disk).
//!
//! Selling: buyers pay the supplier's own wallet through their Lightning
//! address (see [`crate::supplier`] and [`hashu_mint::lnaddress`]), at the
//! supplier's ask, capped at their verified hashrate × their horizon − what
//! the mint already owes. The operator's phoenixd wallet is never used for
//! a supplier's mint. With `--lightning fake` (testing) a supplier mint
//! without an address sells over the fake wallet instead.
//!
//! Each supplier mint also serves its page at `/` (status, and settings
//! behind the supplier's worker secret) and `/api/supplier`,
//! `/api/settings`. The operator's mint serves sign-up at `/join`.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use cdk::mint::{Mint, MintBuilder, MintMeltLimits, UnitConfig};
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use futures::future::BoxFuture;
use hashu_core::redemption::RedemptionLedger;
use hashu_mint::lightning::add_lightning_issuance;
use hashu_mint::lnaddress::{LightningAddress, LnAddressRail};
use hashu_mint::melt::{reopen_pending_quotes_in, HashrateMelter, METHOD as HASHRATE};
use hashu_mint::payment::{thh_unit, ThhIssuer};
use hashu_proxy::suppliers::{secret_hash, valid_name, Check, RegisterError, SupplierRegistry};
use serde_json::json;
use tower::ServiceExt;

use crate::supplier::{HashrateFn, SupplierCapacity, SupplierPricer, SupplierSettings};

/// Checks a Lightning address can be invoiced and verified.
pub type ProbeFn =
    Arc<dyn Fn(LightningAddress) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// The real probe: [`hashu_mint::lnaddress::probe`].
pub fn lnurl_probe() -> ProbeFn {
    Arc::new(|addr| Box::pin(async move { hashu_mint::lnaddress::probe(&addr).await.map(|_| ()) }))
}

/// Sign-ups allowed per hour, so nobody can mint thousands of names (each
/// one a database and, on first visit, a TLS certificate).
const JOINS_PER_HOUR: usize = 10;
/// How often a supplier mint recounts what it owes.
const OWED_REFRESH: Duration = Duration::from_secs(5);
/// Header carrying the supplier's worker secret on `/api/settings`.
const SECRET_HEADER: &str = "x-hashu-secret";

const SUPPLIER_PAGE: &str = include_str!("supplier.html");
const JOIN_PAGE: &str = include_str!("join.html");

/// How supplier mints are built.
pub struct TenantConfig {
    /// Host of the operator's mint (`mint.hashu.io`): supplier `alice` is
    /// served at `alice.<base>`.
    pub base: String,
    /// `<data-dir>/mints`.
    pub dir: PathBuf,
    pub ledger: Arc<RedemptionLedger>,
    pub min_redeem_thh: u64,
    pub max_thh: u64,
    /// Sell over cdk's fake wallet when a supplier has no address (testing).
    pub fake_lightning: bool,
    /// Live hashrate of a supplier's verified rigs (the proxy router).
    pub hashrate: HashrateFn,
    /// Port miners connect to, shown on sign-up.
    pub stratum_port: u16,
    pub probe: ProbeFn,
}

struct Tenant {
    mint: Arc<Mint>,
    app: axum::Router,
    settings: Arc<RwLock<SupplierSettings>>,
    pricer: Arc<SupplierPricer>,
    refresher: tokio::task::JoinHandle<()>,
}

impl Tenant {
    async fn stop(&self, name: &str) {
        self.refresher.abort();
        if let Err(e) = self.mint.stop().await {
            tracing::warn!(supplier = %name, error = %e, "supplier mint stop");
        }
    }
}

/// The supplier mints this process serves.
pub struct Tenants {
    cfg: TenantConfig,
    registry: Arc<SupplierRegistry>,
    mints: tokio::sync::Mutex<HashMap<String, Arc<Tenant>>>,
    joins: Mutex<VecDeque<Instant>>,
    me: Weak<Tenants>,
}

impl Tenants {
    pub fn new(cfg: TenantConfig, registry: Arc<SupplierRegistry>) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            cfg: TenantConfig {
                base: cfg.base.trim().trim_end_matches('.').to_ascii_lowercase(),
                ..cfg
            },
            registry,
            mints: Default::default(),
            joins: Default::default(),
            me: me.clone(),
        })
    }

    /// The host supplier `name` is served on.
    pub fn host_for(&self, name: &str) -> String {
        format!("{name}.{}", self.cfg.base)
    }

    /// The registered supplier whose mint `host` names, if any. `host` may
    /// carry a port.
    pub fn supplier_for_host(&self, host: &str) -> Option<String> {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        let host = match host.rsplit_once(':') {
            Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h.to_string(),
            _ => host,
        };
        let name = host.strip_suffix(&format!(".{}", self.cfg.base))?;
        (valid_name(name) && self.registry.contains(name)).then(|| name.to_string())
    }

    /// Build every registered supplier's mint. A mint that fails to build
    /// is logged and retried on its first request.
    pub async fn start_all(&self) {
        for name in self.registry.names() {
            if let Err(e) = self.get(&name).await {
                tracing::error!(supplier = %name, error = %e, "could not start supplier mint");
            }
        }
    }

    /// Stop every supplier mint.
    pub async fn stop_all(&self) {
        for (name, t) in self.mints.lock().await.drain() {
            t.stop(&name).await;
        }
    }

    async fn get(&self, name: &str) -> Result<Arc<Tenant>> {
        // Held across the build so two first requests build one mint.
        let mut mints = self.mints.lock().await;
        if let Some(t) = mints.get(name) {
            return Ok(t.clone());
        }
        let t = Arc::new(self.build(name).await?);
        mints.insert(name.to_string(), t.clone());
        Ok(t)
    }

    /// Supplier `name`'s capacity, building its mint if needed.
    pub async fn capacity(&self, name: &str) -> Result<SupplierCapacity> {
        Ok(self.get(name).await?.pricer.capacity())
    }

    /// Save new settings for `name`. A changed payment address is probed
    /// first and rebuilds the mint (cdk fixes its payment rails at build).
    async fn update_settings(
        &self,
        name: &str,
        new: SupplierSettings,
    ) -> Result<SupplierSettings, String> {
        let new = new.validated()?;
        let t = self.get(name).await.map_err(|e| e.to_string())?;
        let old = t.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
        let rail_changed = old.lightning_address != new.lightning_address;
        if rail_changed {
            if let Some(a) = &new.lightning_address {
                (self.cfg.probe)(a.parse()?).await?;
            }
        }
        new.save(&self.cfg.dir.join(name))
            .map_err(|e| e.to_string())?;
        *t.settings.write().unwrap_or_else(|e| e.into_inner()) = new.clone();
        if rail_changed {
            if let Some(t) = self.mints.lock().await.remove(name) {
                let name = name.to_string();
                // Stop after the current request has its answer.
                tokio::spawn(async move { t.stop(&name).await });
            }
        }
        Ok(new)
    }

    async fn build(&self, name: &str) -> Result<Tenant> {
        let dir = self.cfg.dir.join(name);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {dir:?}"))?;
        let settings = Arc::new(RwLock::new(SupplierSettings::load(&dir)?));
        let owed = Arc::new(Mutex::new(0.0));
        let pricer = Arc::new(SupplierPricer::new(
            name,
            settings.clone(),
            self.cfg.hashrate.clone(),
            owed.clone(),
        ));
        let seed = crate::mint::load_or_create_seed(&dir)?;
        let db_path = dir.join("mint.sqlite");
        let db = Arc::new(
            cdk_sqlite::MintSqliteDatabase::new(db_path.to_string_lossy().as_ref())
                .await
                .with_context(|| format!("open {db_path:?}"))?,
        );
        {
            use cdk::cdk_database::MintQuotesDatabase;
            let quotes = db
                .get_melt_quotes()
                .await
                .map_err(|e| anyhow::anyhow!("read melt quotes: {e}"))?;
            reopen_pending_quotes_in(Some(name), &self.cfg.ledger, &quotes);
        }

        let url = format!("https://{}", self.host_for(name));
        let mut builder = MintBuilder::new(db.clone())
            .with_name(name.to_string())
            .with_description(format!(
                "THH backed by {name}'s own rigs, redeemed as hashrate through Hashu"
            ))
            .with_urls(vec![url]);
        builder
            .configure_unit(
                thh_unit(),
                UnitConfig {
                    amounts: crate::mint::DENOMS.to_vec(),
                    input_fee_ppk: 0,
                },
            )
            .context("configure THH unit")?;
        builder
            .add_payment_processor(
                thh_unit(),
                PaymentMethod::Custom(HASHRATE.into()),
                MintMeltLimits::new(self.cfg.min_redeem_thh.max(1), self.cfg.max_thh),
                Arc::new(
                    HashrateMelter::new(self.cfg.ledger.clone())
                        .with_supplier(name)
                        .with_min_thh(self.cfg.min_redeem_thh)
                        .with_max_thh(self.cfg.max_thh),
                ),
            )
            .await
            .context("register hashrate melt")?;
        let mut methods = vec![HASHRATE.to_string()];
        let address = settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .lightning_address
            .clone();
        let rail: Option<cdk::cdk_payment::DynMintPayment> = match &address {
            Some(a) => {
                let addr: LightningAddress = a.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                Some(Arc::new(
                    LnAddressRail::new(&addr, Some(dir.join("ln-invoices.json")))
                        .map_err(|e| anyhow::anyhow!("Lightning address rail: {e}"))?,
                ))
            }
            None if self.cfg.fake_lightning => Some(Arc::new(cdk_fake_wallet::FakeWallet::new(
                cdk::types::FeeReserve {
                    min_fee_reserve: 1.into(),
                    percent_fee_reserve: 0.0,
                },
                Default::default(),
                Default::default(),
                2,
                CurrencyUnit::Sat,
            ))),
            None => None,
        };
        if let Some(rail) = rail {
            // Priced in msat: suppliers' asks have Lightning's full resolution.
            let issuer = ThhIssuer::new(pricer.clone(), rail, CurrencyUnit::Msat);
            builder =
                add_lightning_issuance(builder, issuer, MintMeltLimits::new(1, self.cfg.max_thh))
                    .await
                    .context("register Lightning rail")?;
            methods.push("bolt11".into());
        }
        // Same split as the operator's mint: sell over the rails, redeem
        // only as hashrate.
        let mut info = builder.current_mint_info();
        let hashrate = PaymentMethod::Custom(HASHRATE.into());
        info.nuts.nut04.methods.retain(|m| m.method != hashrate);
        info.nuts.nut05.methods.retain(|m| m.method == hashrate);
        info.nuts.nut15.methods.clear();
        builder = builder.with_mint_info(info.clone());

        let mint = Arc::new(
            builder
                .build_with_seed(db, &seed)
                .await
                .context("build mint")?,
        );
        crate::mint::refresh_stored_mint_info(&mint, info).await?;
        mint.start().await.context("start mint")?;
        let refresher = tokio::spawn(refresh_owed(
            mint.clone(),
            self.cfg.ledger.clone(),
            name.to_string(),
            owed,
        ));
        let app = cdk_axum::create_mint_router(mint.clone(), methods)
            .await
            .context("build mint HTTP router")?
            .merge(supplier_router(SupplierCtx {
                name: name.to_string(),
                tenants: self.me.clone(),
            }));
        tracing::info!(
            supplier = %name,
            host = %self.host_for(name),
            pays_to = ?address,
            "supplier mint ready"
        );
        Ok(Tenant {
            mint,
            app,
            settings,
            pricer,
            refresher,
        })
    }

    /// Register `name` with fresh settings and a generated secret.
    async fn join(
        &self,
        name: &str,
        settings: SupplierSettings,
    ) -> Result<String, (StatusCode, String)> {
        let bad = |e: String| (StatusCode::BAD_REQUEST, e);
        let settings = settings.validated().map_err(bad)?;
        let Some(addr) = settings.lightning_address.as_deref() else {
            return Err(bad(
                "a Lightning address is required, so buyers can pay you".into(),
            ));
        };
        if settings.ask_msat_per_thh.is_none() {
            return Err(bad("set your ask per THH".into()));
        }
        if !valid_name(name) {
            return Err(bad(RegisterError::InvalidName.to_string()));
        }
        if self.registry.contains(name) {
            return Err((StatusCode::CONFLICT, RegisterError::Taken.to_string()));
        }
        {
            let mut joins = self.joins.lock().unwrap_or_else(|e| e.into_inner());
            let hour_ago = Instant::now() - Duration::from_secs(3600);
            while joins.front().is_some_and(|t| *t < hour_ago) {
                joins.pop_front();
            }
            if joins.len() >= JOINS_PER_HOUR {
                return Err((
                    StatusCode::TOO_MANY_REQUESTS,
                    "too many sign-ups right now; try again in an hour".into(),
                ));
            }
            joins.push_back(Instant::now());
        }
        (self.cfg.probe)(addr.parse().map_err(bad)?)
            .await
            .map_err(bad)?;
        let secret = {
            let mut b = [0u8; 16];
            rand::Rng::fill(&mut rand::thread_rng(), &mut b[..]);
            hex::encode(b)
        };
        let dir = self.cfg.dir.join(name);
        std::fs::create_dir_all(&dir)
            .and_then(|_| settings.save(&dir).map_err(std::io::Error::other))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        self.registry
            .register(name, &secret_hash(&secret))
            .map_err(|e| match e {
                RegisterError::Taken => (StatusCode::CONFLICT, e.to_string()),
                RegisterError::InvalidName => bad(e.to_string()),
                RegisterError::Closed | RegisterError::Io(_) => {
                    (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
                }
            })?;
        tracing::info!(supplier = %name, "supplier signed up");
        Ok(secret)
    }
}

/// Keep `owed` current: THH in circulation on the mint plus what its open
/// redemptions still have to deliver.
async fn refresh_owed(
    mint: Arc<Mint>,
    ledger: Arc<RedemptionLedger>,
    name: String,
    owed: Arc<Mutex<f64>>,
) {
    let mut tick = tokio::time::interval(OWED_REFRESH);
    loop {
        tick.tick().await;
        match crate::operator::obligations_for(&mint, &ledger, Some(&name)).await {
            Ok(o) => {
                *owed.lock().unwrap_or_else(|e| e.into_inner()) =
                    o.in_circulation as f64 + o.open_remaining_thh;
            }
            Err(e) => tracing::warn!(supplier = %name, error = %e, "could not count obligations"),
        }
    }
}

#[derive(Clone)]
struct SupplierCtx {
    name: String,
    tenants: Weak<Tenants>,
}

impl SupplierCtx {
    fn tenants(&self) -> Option<Arc<Tenants>> {
        self.tenants.upgrade()
    }
}

/// `/`, `/api/supplier` and `/api/settings` on a supplier's mint.
fn supplier_router(ctx: SupplierCtx) -> axum::Router {
    axum::Router::new()
        .route("/", get(|| async { Html(SUPPLIER_PAGE) }))
        .route("/api/supplier", get(supplier_status))
        .route("/api/settings", get(get_settings).post(post_settings))
        .with_state(ctx)
}

async fn supplier_status(State(ctx): State<SupplierCtx>) -> Response {
    let Some(tenants) = ctx.tenants() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let t = match tenants.get(&ctx.name).await {
        Ok(t) => t,
        Err(e) => return (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    };
    let s = t.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
    let cap = t.pricer.capacity();
    Json(json!({
        "name": ctx.name,
        "mint_url": format!("https://{}", tenants.host_for(&ctx.name)),
        "selling": s.ask_msat_per_thh.is_some() && s.lightning_address.is_some() && cap.available_thh > 0,
        "ask_msat_per_thh": s.ask_msat_per_thh,
        "pays_by_lightning": s.lightning_address.is_some(),
        "capacity": cap,
    }))
    .into_response()
}

/// The supplier's own secret, from [`SECRET_HEADER`].
fn authorized(tenants: &Tenants, name: &str, headers: &HeaderMap) -> bool {
    let secret = headers.get(SECRET_HEADER).and_then(|v| v.to_str().ok());
    matches!(tenants.registry.check(name, secret), Check::Verified(n) if n == name)
}

async fn get_settings(State(ctx): State<SupplierCtx>, headers: HeaderMap) -> Response {
    let Some(tenants) = ctx.tenants() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !authorized(&tenants, &ctx.name, &headers) {
        return (StatusCode::UNAUTHORIZED, "wrong worker secret").into_response();
    }
    match tenants.get(&ctx.name).await {
        Ok(t) => Json(t.settings.read().unwrap_or_else(|e| e.into_inner()).clone()).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

async fn post_settings(
    State(ctx): State<SupplierCtx>,
    headers: HeaderMap,
    Json(new): Json<SupplierSettings>,
) -> Response {
    let Some(tenants) = ctx.tenants() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !authorized(&tenants, &ctx.name, &headers) {
        return (StatusCode::UNAUTHORIZED, "wrong worker secret").into_response();
    }
    match tenants.update_settings(&ctx.name, new).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct JoinRequest {
    name: String,
    #[serde(flatten)]
    settings: SupplierSettings,
}

async fn post_join(State(tenants): State<Arc<Tenants>>, Json(req): Json<JoinRequest>) -> Response {
    let name = req.name.trim().to_ascii_lowercase();
    match tenants.join(&name, req.settings).await {
        Ok(secret) => Json(json!({
            "name": name,
            "secret": secret,
            "mint_url": format!("https://{}", tenants.host_for(&name)),
            "stratum": {
                "url": tenants.cfg.base,
                "port": tenants.cfg.stratum_port,
                "user": format!("{name}.<rig>"),
                "password": format!("pw={secret}"),
            },
        }))
        .into_response(),
        Err((status, e)) => (status, e).into_response(),
    }
}

/// Middleware for the mint HTTP app: a request whose Host names a
/// registered supplier goes to that supplier's mint; everything else to
/// the operator's.
pub async fn dispatch(State(tenants): State<Arc<Tenants>>, req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().host().map(str::to_string));
    let Some(name) = host.as_deref().and_then(|h| tenants.supplier_for_host(h)) else {
        return next.run(req).await;
    };
    match tenants.get(&name).await {
        Ok(t) => match t.app.clone().oneshot(req).await {
            Ok(r) => r,
            Err(e) => match e {},
        },
        Err(e) => {
            tracing::error!(supplier = %name, error = %e, "supplier mint unavailable");
            (StatusCode::SERVICE_UNAVAILABLE, "mint unavailable").into_response()
        }
    }
}

#[derive(serde::Deserialize)]
pub struct AskQuery {
    domain: String,
}

/// Caddy's on-demand TLS `ask`: 200 only for a registered supplier's
/// host, so nobody can make Caddy request certificates for made-up names.
pub async fn tls_ask(State(tenants): State<Arc<Tenants>>, Query(q): Query<AskQuery>) -> StatusCode {
    match tenants.supplier_for_host(&q.domain) {
        Some(_) if !q.domain.contains(':') => StatusCode::OK,
        _ => StatusCode::NOT_FOUND,
    }
}

/// Routes the operator's mint serves for suppliers: `/tls-ask` and sign-up
/// (`/join/`, `POST /api/join`). It replaces the waitlist splash at
/// `/join/` (`--waitlist`) when both are on.
pub fn operator_router(tenants: Arc<Tenants>) -> axum::Router {
    axum::Router::new()
        .route("/tls-ask", get(tls_ask))
        .route(
            "/join",
            get(|| async { axum::response::Redirect::permanent("/join/") }),
        )
        .route("/join/", get(|| async { Html(JOIN_PAGE) }))
        // The landing page shows its "Hosted mints" link when this answers.
        .route(
            "/join/api",
            get(|| async { Json(json!({ "open": true, "signup": true })) }),
        )
        .route("/api/join", axum::routing::post(post_join))
        .with_state(tenants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn tenants_with(dir: &std::path::Path, ths: f64, fake: bool) -> Arc<Tenants> {
        let registry = SupplierRegistry::from_file(dir.join("suppliers.json"));
        registry.register("alice", &secret_hash("s3cret")).unwrap();
        Tenants::new(
            TenantConfig {
                base: "Mint.Example.com.".into(),
                dir: dir.join("mints"),
                ledger: Arc::new(RedemptionLedger::new()),
                min_redeem_thh: 1,
                max_thh: 1_000,
                fake_lightning: fake,
                hashrate: Arc::new(move |n: &str| if n == "alice" { ths * 1e12 } else { 0.0 }),
                stratum_port: 3335,
                probe: Arc::new(|a: LightningAddress| {
                    Box::pin(async move {
                        if a.domain == "wallet.example" {
                            Ok(())
                        } else {
                            Err("no LUD-21".to_string())
                        }
                    })
                }),
            },
            Arc::new(registry),
        )
    }

    fn tenants(dir: &std::path::Path) -> Arc<Tenants> {
        tenants_with(dir, 0.0, false)
    }

    #[test]
    fn hosts_map_to_registered_suppliers_only() {
        let dir = tempfile::tempdir().unwrap();
        let t = tenants(dir.path());
        assert_eq!(t.host_for("alice"), "alice.mint.example.com");
        for ok in [
            "alice.mint.example.com",
            "ALICE.mint.example.com.",
            "alice.mint.example.com:443",
        ] {
            assert_eq!(t.supplier_for_host(ok).as_deref(), Some("alice"), "{ok}");
        }
        for no in [
            "mint.example.com",
            "bob.mint.example.com",
            "x.alice.mint.example.com",
            "alice.mint.example.org",
            "alicemint.example.com",
        ] {
            assert_eq!(t.supplier_for_host(no), None, "{no}");
        }
    }

    async fn get(app: &axum::Router, host: &str, path: &str) -> (StatusCode, String) {
        let req = Request::builder()
            .uri(path)
            .header(header::HOST, host)
            .body(Body::empty())
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn supplier_host_gets_its_own_mint() {
        let dir = tempfile::tempdir().unwrap();
        let t = tenants(dir.path());
        let operator = axum::Router::new()
            .route("/v1/info", axum::routing::get(|| async { "operator" }))
            .route("/checkout/", axum::routing::get(|| async { "operator" }))
            .merge(operator_router(t.clone()))
            .layer(axum::middleware::from_fn_with_state(t.clone(), dispatch));

        let (s, body) = get(&operator, "mint.example.com", "/v1/info").await;
        assert_eq!((s, body.as_str()), (StatusCode::OK, "operator"));

        let (s, body) = get(&operator, "alice.mint.example.com", "/v1/info").await;
        assert_eq!(s, StatusCode::OK);
        let info: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(info["name"], "alice");
        assert_eq!(info["urls"][0], "https://alice.mint.example.com");
        assert!(dir.path().join("mints/alice/seed.hex").exists());
        // Routes only the supplier mint has reach it too.
        let (s, body) = get(&operator, "alice.mint.example.com", "/v1/keys").await;
        assert_eq!(s, StatusCode::OK, "{body}");
        // Paths the supplier mint doesn't have don't fall through to the
        // operator's routes.
        let (s, _) = get(&operator, "alice.mint.example.com", "/checkout/").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        // An unregistered name is the operator's (Caddy never gets it a
        // certificate anyway).
        let (_, body) = get(&operator, "bob.mint.example.com", "/v1/info").await;
        assert_eq!(body, "operator");

        let ask = |d: &str| format!("/tls-ask?domain={d}");
        let (s, _) = get(&operator, "hashu", &ask("alice.mint.example.com")).await;
        assert_eq!(s, StatusCode::OK);
        for d in ["bob.mint.example.com", "mint.example.com", "evil.com"] {
            let (s, _) = get(&operator, "hashu", &ask(d)).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{d}");
        }
        t.stop_all().await;
    }

    async fn send(
        app: &axum::Router,
        method: &str,
        host: &str,
        path: &str,
        secret: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, host)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(s) = secret {
            req = req.header(SECRET_HEADER, s);
        }
        let req = req
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    fn app(t: &Arc<Tenants>) -> axum::Router {
        axum::Router::new()
            .merge(operator_router(t.clone()))
            .layer(axum::middleware::from_fn_with_state(t.clone(), dispatch))
    }

    #[tokio::test]
    async fn join_creates_a_mint_whose_settings_need_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let t = tenants(dir.path());
        let app = app(&t);
        let op = "mint.example.com";
        let join = |name: &str, ln: &str| json!({"name": name, "lightning_address": ln, "ask_msat_per_thh": 40_000, "horizon_hours": 24});
        let (s, body) = send(
            &app,
            "POST",
            op,
            "/api/join",
            None,
            Some(join("Bob", "bob@wallet.example")),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let j: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(j["mint_url"], "https://bob.mint.example.com");
        assert_eq!(j["stratum"]["user"], "bob.<rig>");
        let secret = j["secret"].as_str().unwrap().to_string();
        assert_eq!(j["stratum"]["password"], format!("pw={secret}"));
        // The registry file now holds bob, so his rigs verify.
        assert_eq!(
            SupplierRegistry::from_file(dir.path().join("suppliers.json"))
                .check("bob.rig1", Some(&secret)),
            Check::Verified("bob".into())
        );

        let (s, _) = send(
            &app,
            "POST",
            op,
            "/api/join",
            None,
            Some(join("bob", "bob@wallet.example")),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT);
        let (s, body) = send(
            &app,
            "POST",
            op,
            "/api/join",
            None,
            Some(join("carol", "carol@nolud21.example")),
        )
        .await;
        assert_eq!((s, body.as_str()), (StatusCode::BAD_REQUEST, "no LUD-21"));
        assert!(!t.registry.contains("carol"));

        let bob = "bob.mint.example.com";
        let (s, body) = send(&app, "GET", bob, "/api/supplier", None, None).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (st["name"].as_str(), st["ask_msat_per_thh"].as_u64()),
            (Some("bob"), Some(40_000))
        );
        assert_eq!(st["pays_by_lightning"], true);
        // No rigs online: not selling.
        assert_eq!(st["selling"], false);
        let (s, _) = send(&app, "GET", bob, "/", None, None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, body) = send(&app, "GET", op, "/join/", None, None).await;
        assert!(s == StatusCode::OK && body.contains("Sell your hashrate"));

        let (s, _) = send(&app, "GET", bob, "/api/settings", None, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _) = send(&app, "GET", bob, "/api/settings", Some("wrong"), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, body) = send(&app, "GET", bob, "/api/settings", Some(&secret), None).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let short = json!({"ask_msat_per_thh": 45_500, "horizon_hours": 0.5, "lightning_address": "bob@wallet.example"});
        let (s, _) = send(
            &app,
            "POST",
            bob,
            "/api/settings",
            Some(&secret),
            Some(short),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let ok = json!({"ask_msat_per_thh": 45_500, "horizon_hours": 1, "lightning_address": "bob@wallet.example"});
        let (s, body) = send(&app, "POST", bob, "/api/settings", Some(&secret), Some(ok)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(
            SupplierSettings::load(&dir.path().join("mints/bob"))
                .unwrap()
                .ask_msat_per_thh,
            Some(45_500)
        );
        t.stop_all().await;
    }

    #[tokio::test]
    async fn supplier_mint_sells_only_what_its_rigs_cover() {
        let dir = tempfile::tempdir().unwrap();
        // Alice has 1 TH/s online; sells over the fake wallet (no address).
        let t = tenants_with(dir.path(), 1.0, true);
        let app = app(&t);
        let alice = "alice.mint.example.com";
        let quote = |thh: u64| json!({"amount": thh, "unit": "thh"});
        // No ask yet: nothing to sell.
        let (s, _) = send(
            &app,
            "POST",
            alice,
            "/v1/mint/quote/bolt11",
            None,
            Some(quote(1)),
        )
        .await;
        assert_ne!(s, StatusCode::OK);

        let set = json!({"ask_msat_per_thh": 2_500, "horizon_hours": 2});
        let (s, body) = send(
            &app,
            "POST",
            alice,
            "/api/settings",
            Some("s3cret"),
            Some(set),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(t.capacity("alice").await.unwrap().available_thh, 2);
        let (s, body) = send(
            &app,
            "POST",
            alice,
            "/v1/mint/quote/bolt11",
            None,
            Some(quote(2)),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        // cdk reports the pricer's "sold out" as an invalid request.
        let (s, body) = send(
            &app,
            "POST",
            alice,
            "/v1/mint/quote/bolt11",
            None,
            Some(quote(3)),
        )
        .await;
        assert_ne!(s, StatusCode::OK, "{body}");
        t.stop_all().await;
    }
}
