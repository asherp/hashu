//! Supplier mints: one cdk mint per registered supplier, served from the
//! same process as the operator's mint and picked by the request's Host.
//!
//! `alice.hashu.io` is supplier `alice`'s mint when the supplier domain is
//! `hashu.io` (`HASHU_SUPPLIER_DOMAIN`) and `alice` is in the supplier
//! registry (`HASHU_SUPPLIERS_FILE`). Without a supplier domain, mints are
//! named under the operator's mint host (`alice.mint.hashu.io`), and that
//! older name keeps answering for the same mint when one is set. Each supplier mint has its own seed, keys and
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
//! behind the supplier's dashboard key) and `/api/supplier`,
//! `/api/settings`. The operator's mint serves sign-up at `/join`.
//!
//! Epochs (#126, `HASHU_EPOCHS=true`): every supplier mint then dates its
//! vouchers to difficulty epochs like the operator's. Its own TiH keyset
//! rotates each epoch (from its own seed), live issuance sells only the
//! current epoch from its stock, future epochs within the supplier's
//! horizon are sold from its hashrate plan (`/api/plan`, the planner on its
//! page), and expired vouchers convert 1:1 to the current epoch, or the
//! next one with room. One chain-tip reader serves every mint. The routes
//! are the operator's: `/v1/hashu/epochs*` ([`crate::epochs`]) and
//! `/v1/hashu/plan` ([`crate::planner`]).

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
use hashu_mint::capacity::MIN_PURCHASE_TIH;
use hashu_mint::epoch::{EpochConverter, Epochs, PlanInput, METHOD as EPOCH};
use hashu_mint::lightning::add_lightning_issuance;
use hashu_mint::lnaddress::{LnAddressRail, PayEndpoint};
use hashu_mint::melt::{reopen_pending_quotes_in, HashrateMelter, METHOD as HASHRATE};
use hashu_mint::payment::{tih_unit, TihIssuer};
use hashu_proxy::suppliers::{
    reserved_name, secret_hash, valid_name, RegisterError, SupplierRegistry,
};
use serde_json::json;
use tower::ServiceExt;

use crate::supplier::{
    HashrateFn, SupplierBook, SupplierCapacity, SupplierPricer, SupplierSettings,
};

/// Checks an LNURL-pay endpoint can be invoiced and verified.
pub type ProbeFn = Arc<dyn Fn(PayEndpoint) -> BoxFuture<'static, Result<(), String>> + Send + Sync>;

/// The real probe: [`hashu_mint::lnaddress::probe`].
pub fn lnurl_probe() -> ProbeFn {
    Arc::new(|addr| Box::pin(async move { hashu_mint::lnaddress::probe(&addr).await.map(|_| ()) }))
}

fn clean_host(h: &str) -> String {
    h.trim().trim_end_matches('.').to_ascii_lowercase()
}

/// Sign-ups allowed per hour, so nobody can mint thousands of names (each
/// one a database and, on first visit, a TLS certificate).
const JOINS_PER_HOUR: usize = 10;
/// How often a supplier mint recounts what it owes.
const OWED_REFRESH: Duration = Duration::from_secs(5);
/// Header carrying the supplier's dashboard key on `/api/settings` and
/// `/api/restock`. Not the rig secret: that travels in clear over Stratum.
const KEY_HEADER: &str = "x-hashu-key";

const SUPPLIER_PAGE: &str = include_str!("supplier.html");
const JOIN_PAGE: &str = include_str!("join.html");

/// How supplier mints are built.
pub struct TenantConfig {
    /// Host of the operator's mint (`mint.hashu.io`). Supplier `alice` is
    /// also served at `alice.<base>`, the name before supplier domains.
    pub base: String,
    /// Supplier domain (`hashu.io`): supplier `alice`'s mint is
    /// `alice.<domain>`. `None`: `alice.<base>`.
    pub domain: Option<String>,
    /// `<data-dir>/mints`.
    pub dir: PathBuf,
    pub ledger: Arc<RedemptionLedger>,
    pub min_redeem_tih: u64,
    pub max_tih: u64,
    /// Sell over cdk's fake wallet when a supplier has no address (testing).
    pub fake_lightning: bool,
    /// Live hashrate of a supplier's verified rigs (the proxy router).
    pub hashrate: HashrateFn,
    /// Public Stratum address miners connect to, shown on sign-up. Every
    /// supplier's rigs use this shared endpoint (`pw=` tells them apart).
    pub stratum: crate::stratum_addr::StratumAddr,
    pub probe: ProbeFn,
    /// Serves each supplier mint's reliability record (#109).
    pub reliability: Option<Arc<crate::reliability::Reliability>>,
    /// With epochs on (#126): the chain tip every supplier mint dates its
    /// vouchers by.
    pub epochs: Option<crate::epochs::ClockFeed>,
}

struct Tenant {
    mint: Arc<Mint>,
    app: axum::Router,
    settings: Arc<RwLock<SupplierSettings>>,
    pricer: Arc<SupplierPricer>,
    /// With epochs on.
    epochs: Option<Arc<Epochs>>,
    planner: Option<Arc<crate::planner::Planner>>,
    /// Background tasks: what it owes, and with epochs the chain and plan.
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Tenant {
    async fn stop(&self, name: &str) {
        for t in &self.tasks {
            t.abort();
        }
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
                base: clean_host(&cfg.base),
                domain: cfg
                    .domain
                    .as_deref()
                    .map(clean_host)
                    .filter(|d| !d.is_empty()),
                ..cfg
            },
            registry,
            mints: Default::default(),
            joins: Default::default(),
            me: me.clone(),
        })
    }

    /// Domain supplier mints are named under: `<name>.<domain>`.
    pub fn domain(&self) -> &str {
        self.cfg.domain.as_deref().unwrap_or(&self.cfg.base)
    }

    /// The host supplier `name` is served on.
    pub fn host_for(&self, name: &str) -> String {
        format!("{name}.{}", self.domain())
    }

    /// The registered supplier whose mint `host` names, if any. `host` may
    /// carry a port.
    pub fn supplier_for_host(&self, host: &str) -> Option<String> {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        let host = match host.rsplit_once(':') {
            Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h.to_string(),
            _ => host,
        };
        let known = |name: &str| valid_name(name) && self.registry.contains(name);
        // A reserved name never takes a host on the supplier domain, which
        // the operator's own hosts (app, www, mint, ...) share.
        if let Some(domain) = self.cfg.domain.as_deref().filter(|d| *d != self.cfg.base) {
            if let Some(name) = host.strip_suffix(&format!(".{domain}")) {
                if known(name) && !reserved_name(name) {
                    return Some(name.to_string());
                }
            }
        }
        let name = host.strip_suffix(&format!(".{}", self.cfg.base))?;
        known(name).then(|| name.to_string())
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

    /// Save new settings for `name`. A changed LNURL-pay endpoint is probed
    /// first and rebuilds the mint (cdk fixes its payment rails at build).
    async fn update_settings(
        &self,
        name: &str,
        new: SupplierSettings,
    ) -> Result<SupplierSettings, String> {
        let new = new.validated()?;
        let t = self.get(name).await.map_err(|e| e.to_string())?;
        let old = t.settings.read().unwrap_or_else(|e| e.into_inner()).clone();
        let rail_changed = old.lnurl != new.lnurl;
        if rail_changed {
            if let Some(a) = &new.lnurl {
                (self.cfg.probe)(a.parse()?).await?;
            }
        }
        new.save(&self.cfg.dir.join(name))
            .map_err(|e| e.to_string())?;
        *t.settings.write().unwrap_or_else(|e| e.into_inner()) = new.clone();
        if old.horizon_hours != new.horizon_hours {
            t.pricer.horizon_changed(hashu_mint::capacity::now_ms());
        }
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
        let book = Arc::new(Mutex::new(SupplierBook::load(&dir)?));
        let pricer = Arc::new(SupplierPricer::new(
            name,
            settings.clone(),
            self.cfg.hashrate.clone(),
            book,
        ));
        let seed = crate::mint::load_or_create_seed(&dir)?;
        let epochs = match &self.cfg.epochs {
            Some(_) => Some(Arc::new(Epochs::open(
                &seed,
                crate::mint::DENOMS.to_vec(),
                Some(dir.join("epochs.json")),
            )?)),
            None => None,
        };
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
                "TiH backed by {name}'s own rigs, redeemed as hashrate through Hashu"
            ))
            .with_urls(vec![url.clone()]);
        // The supplier mint's Nostr identity (#116): its receipt key.
        {
            let mut contacts = Vec::new();
            crate::mint::add_nostr_contact(
                &mut contacts,
                &hashu_mint::reliability::receipt_keys(&dir.join("reliability"))
                    .context("load reliability receipt key")?,
            )?;
            for c in contacts {
                builder = builder.with_contact_info(c);
            }
        }
        builder
            .configure_unit(
                tih_unit(),
                UnitConfig {
                    amounts: crate::mint::DENOMS.to_vec(),
                    input_fee_ppk: 0,
                },
            )
            .context("configure TiH unit")?;
        builder
            .add_payment_processor(
                tih_unit(),
                PaymentMethod::Custom(HASHRATE.into()),
                MintMeltLimits::new(self.cfg.min_redeem_tih.max(1), self.cfg.max_tih),
                Arc::new(
                    HashrateMelter::new(self.cfg.ledger.clone())
                        .with_supplier(name)
                        .with_min_tih(self.cfg.min_redeem_tih)
                        .with_max_tih(self.cfg.max_tih),
                ),
            )
            .await
            .context("register hashrate melt")?;
        let mut methods = vec![HASHRATE.to_string()];
        if let Some(e) = &epochs {
            builder
                .add_payment_processor(
                    tih_unit(),
                    PaymentMethod::Custom(EPOCH.into()),
                    MintMeltLimits::new(1, self.cfg.max_tih),
                    Arc::new(EpochConverter::new(e.clone())),
                )
                .await
                .context("register epoch conversion")?;
            methods.push(EPOCH.to_string());
        }
        let address = settings
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .lnurl
            .clone();
        let rail: Option<cdk::cdk_payment::DynMintPayment> = match &address {
            Some(a) => {
                let addr: PayEndpoint = a.parse().map_err(|e: String| anyhow::anyhow!(e))?;
                Some(Arc::new(
                    LnAddressRail::new(&addr, Some(dir.join("ln-invoices.json")))
                        .map_err(|e| anyhow::anyhow!("LNURL-pay rail: {e}"))?,
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
            let issuer = TihIssuer::new(pricer.clone(), rail, CurrencyUnit::Msat);
            builder = add_lightning_issuance(
                builder,
                issuer,
                MintMeltLimits::new(MIN_PURCHASE_TIH, self.cfg.max_tih),
            )
            .await
            .context("register Lightning rail")?;
            methods.push("bolt11".into());
        }
        // Same split as the operator's mint: sell over the rails, redeem
        // only as hashrate.
        let mut info = builder.current_mint_info();
        let hashrate = PaymentMethod::Custom(HASHRATE.into());
        let epoch_method = PaymentMethod::Custom(EPOCH.into());
        info.nuts
            .nut04
            .methods
            .retain(|m| m.method != hashrate && m.method != epoch_method);
        info.nuts
            .nut05
            .methods
            .retain(|m| m.method == hashrate || m.method == epoch_method);
        info.nuts.nut15.methods.clear();
        builder = builder.with_mint_info(info.clone());

        let mint = Arc::new(
            builder
                .build_with_seed(db.clone(), &seed)
                .await
                .context("build mint")?,
        );
        crate::mint::refresh_stored_mint_info(&mint, info).await?;
        mint.start().await.context("start mint")?;
        let mut tasks = vec![tokio::spawn(refresh_owed(
            mint.clone(),
            self.cfg.ledger.clone(),
            name.to_string(),
            pricer.clone(),
            epochs.clone(),
        ))];
        let planner = match (&epochs, &self.cfg.epochs) {
            (Some(e), Some(clock)) => {
                let (p, t) =
                    self.start_epochs(name, e, &mint, db.clone(), clock.clone(), &pricer)?;
                tasks.extend(t);
                Some(p)
            }
            _ => None,
        };
        let mut app = cdk_axum::create_mint_router(mint.clone(), methods)
            .await
            .context("build mint HTTP router")?
            .merge(supplier_router(SupplierCtx {
                name: name.to_string(),
                tenants: self.me.clone(),
            }));
        if let Some(rel) = &self.cfg.reliability {
            rel.set_keysets(name, crate::mint::tih_keyset_ids(&mint));
            match mint.mint_info().await {
                Ok(info) => rel.set_announcement(
                    name,
                    hashu_mint::discovery::Announcement::from_info(
                        &info,
                        &url,
                        crate::mint::nostr_network(self.cfg.fake_lightning),
                    ),
                ),
                Err(e) => {
                    tracing::warn!(supplier = %name, error = %e, "no NIP-87 announcement: mint info")
                }
            }
            app = app.merge(crate::reliability::router(rel.clone(), name));
        }
        if let (Some(e), Some(p)) = (&epochs, &planner) {
            app = app
                .merge(crate::epochs::router(e.clone(), mint.clone()))
                .merge(crate::planner::public_router(p.clone()))
                .layer(axum::middleware::from_fn_with_state(
                    e.clone(),
                    crate::epochs::guard,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    e.clone(),
                    crate::epochs::list_future_keysets,
                ));
        }
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
            epochs,
            planner,
            tasks,
        })
    }

    /// Date supplier `name`'s mint to epochs: its plan sells future epochs
    /// from the supplier's stock average or plan, and the chain rotates its
    /// keyset. Returns the planner and the tasks to stop with the mint.
    fn start_epochs(
        &self,
        name: &str,
        epochs: &Arc<Epochs>,
        mint: &Arc<Mint>,
        keys: crate::epochs::KeysDb,
        clock: crate::epochs::ClockFeed,
        pricer: &Arc<SupplierPricer>,
    ) -> Result<(
        Arc<crate::planner::Planner>,
        Vec<tokio::task::JoinHandle<()>>,
    )> {
        let dir = self.cfg.dir.join(name);
        {
            let pricer = pricer.clone();
            epochs.set_plan_inputs(Arc::new(move || {
                let cap = pricer.capacity();
                PlanInput {
                    supply: pricer
                        .average_ths()
                        .map(|ths| hashu_core::epoch::Supply {
                            ths,
                            hours_left: None,
                        })
                        .into_iter()
                        .collect(),
                    carried_tih: cap.owed_tih,
                    horizon_hours: cap.horizon_hours,
                }
            }));
        }
        let measured: crate::planner::MeasuredFn = {
            let pricer = pricer.clone();
            Arc::new(move || pricer.average_ths())
        };
        let planner = Arc::new(crate::planner::Planner {
            plan: Arc::new(hashu_mint::planner::HashratePlan::open(Some(
                dir.join("hashrate-plan.json"),
            ))?),
            epochs: epochs.clone(),
            measured,
            record: self.cfg.reliability.as_ref().and_then(|r| r.store(name)),
        });
        let mut tasks = vec![planner.start()];
        let on_tick: Arc<dyn Fn(hashu_core::epoch::EpochClock) + Send + Sync> = {
            let pricer = pricer.clone();
            Arc::new(move |c| pricer.set_epoch(c.current(), c.hours_left()))
        };
        let on_rotate: Arc<dyn Fn() + Send + Sync> = {
            let (rel, mint, name) = (self.cfg.reliability.clone(), mint.clone(), name.to_string());
            Arc::new(move || {
                if let Some(rel) = &rel {
                    rel.set_keysets(&name, crate::mint::tih_keyset_ids(&mint));
                }
            })
        };
        tasks.push(tokio::spawn(crate::epochs::run(
            epochs.clone(),
            mint.clone(),
            keys,
            clock,
            on_tick,
            on_rotate,
        )));
        Ok((planner, tasks))
    }

    /// Register `name` with fresh settings, a generated rig secret and a
    /// generated dashboard key; returns `(rig secret, dashboard key)`.
    async fn join(
        &self,
        name: &str,
        settings: SupplierSettings,
    ) -> Result<(String, String), (StatusCode, String)> {
        let bad = |e: String| (StatusCode::BAD_REQUEST, e);
        let settings = settings.validated().map_err(bad)?;
        let Some(addr) = settings.lnurl.as_deref() else {
            return Err(bad(
                "a Lightning address or LNURL-pay link is required, so buyers can pay you".into(),
            ));
        };
        if settings.ask_sats_per_eih.is_none() {
            return Err(bad("set your ask in sats per EiH".into()));
        }
        if !valid_name(name) {
            return Err(bad(RegisterError::InvalidName.to_string()));
        }
        if reserved_name(name) {
            return Err(bad(RegisterError::Reserved.to_string()));
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
        let random = || {
            let mut b = [0u8; 16];
            rand::Rng::fill(&mut rand::thread_rng(), &mut b[..]);
            hex::encode(b)
        };
        let (secret, key) = (random(), random());
        let dir = self.cfg.dir.join(name);
        std::fs::create_dir_all(&dir)
            .and_then(|_| settings.save(&dir).map_err(std::io::Error::other))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        self.registry
            .register(name, &secret_hash(&secret), Some(&secret_hash(&key)))
            .map_err(|e| match e {
                RegisterError::Taken => (StatusCode::CONFLICT, e.to_string()),
                RegisterError::InvalidName | RegisterError::Reserved => bad(e.to_string()),
                RegisterError::Closed | RegisterError::Io(_) => {
                    (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
                }
            })?;
        tracing::info!(supplier = %name, "supplier signed up");
        Ok((secret, key))
    }
}

/// Keep the stock's inputs current: TiH in circulation on the mint plus
/// what its open redemptions still have to deliver, the running sold total,
/// and the rigs' hashrate; restock when a horizon has passed.
async fn refresh_owed(
    mint: Arc<Mint>,
    ledger: Arc<RedemptionLedger>,
    name: String,
    pricer: Arc<SupplierPricer>,
    epochs: Option<Arc<Epochs>>,
) {
    let mut tick = tokio::time::interval(OWED_REFRESH);
    loop {
        tick.tick().await;
        match crate::operator::obligations_for(&mint, &ledger, Some(&name), epochs.as_deref()).await
        {
            Ok(o) => {
                pricer.refresh(
                    o.in_circulation as f64 + o.open_remaining_tih,
                    o.stock_sold as f64,
                    hashu_mint::capacity::now_ms(),
                    false,
                );
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
        .route("/api/restock", axum::routing::post(post_restock))
        .route("/api/plan", get(get_plan).post(post_plan))
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
        "selling": s.ask_sats_per_eih.is_some() && s.lnurl.is_some() && cap.available_tih > 0,
        "ask_sats_per_eih": s.ask_sats_per_eih,
        // The same ask in sats per TH/s per day, to compare with markets.
        "ask_sats_per_th_day": s.ask_sats_per_eih.map(|a| {
            hashu_core::denom::sats_per_th_day(hashu_core::denom::msat_per_pih_from_sats_per_eih(a as f64))
        }),
        "pays_by_lightning": s.lnurl.is_some(),
        "capacity": cap,
        "epochs": t.epochs.is_some(),
    }))
    .into_response()
}

/// The supplier's dashboard key, from [`KEY_HEADER`].
fn authorized(tenants: &Tenants, name: &str, headers: &HeaderMap) -> bool {
    let key = headers.get(KEY_HEADER).and_then(|v| v.to_str().ok());
    tenants.registry.check_key(name, key)
}

async fn get_settings(State(ctx): State<SupplierCtx>, headers: HeaderMap) -> Response {
    let Some(tenants) = ctx.tenants() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !authorized(&tenants, &ctx.name, &headers) {
        return (StatusCode::UNAUTHORIZED, "wrong dashboard key").into_response();
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
        return (StatusCode::UNAUTHORIZED, "wrong dashboard key").into_response();
    }
    match tenants.update_settings(&ctx.name, new).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, e).into_response(),
    }
}

/// Restock now from the current hashrate average (the supplier asked),
/// instead of waiting for the horizon to pass.
async fn post_restock(State(ctx): State<SupplierCtx>, headers: HeaderMap) -> Response {
    let Some(tenants) = ctx.tenants() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !authorized(&tenants, &ctx.name, &headers) {
        return (StatusCode::UNAUTHORIZED, "wrong dashboard key").into_response();
    }
    match tenants.get(&ctx.name).await {
        Ok(t) => match t.pricer.restock_now(hashu_mint::capacity::now_ms()) {
            Ok(cap) => Json(cap).into_response(),
            Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e).into_response(),
        },
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

fn no_planner() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "the hashrate planner needs epochs on (HASHU_EPOCHS=true)" })),
    )
        .into_response()
}

/// `GET /api/plan`: the supplier's hashrate plan (also public at
/// `/v1/hashu/plan`).
async fn get_plan(State(ctx): State<SupplierCtx>) -> Response {
    let Some(tenants) = ctx.tenants() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    match tenants.get(&ctx.name).await {
        Ok(t) => match &t.planner {
            Some(p) => crate::planner::get_plan(State(p.clone())).await,
            None => no_planner(),
        },
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

/// `POST /api/plan` `{"epochs": {"<epoch>": <TH/s> | null}}`, behind the
/// supplier's dashboard key.
async fn post_plan(State(ctx): State<SupplierCtx>, headers: HeaderMap, body: String) -> Response {
    let Some(tenants) = ctx.tenants() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !authorized(&tenants, &ctx.name, &headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "wrong dashboard key" })),
        )
            .into_response();
    }
    match tenants.get(&ctx.name).await {
        Ok(t) => match &t.planner {
            Some(p) => crate::planner::post_plan(State(p.clone()), body).await,
            None => no_planner(),
        },
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
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
        Ok((secret, key)) => Json(json!({
            "name": name,
            "secret": secret,
            "dashboard_key": key,
            "mint_url": format!("https://{}", tenants.host_for(&name)),
            "stratum": {
                "url": tenants.cfg.stratum.url,
                "host": tenants.cfg.stratum.host,
                "port": tenants.cfg.stratum.port,
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

/// The sign-up page, with the supplier domain filled in.
async fn join_page(State(tenants): State<Arc<Tenants>>) -> Html<String> {
    Html(JOIN_PAGE.replace("{{SUPPLIER_DOMAIN}}", tenants.domain()))
}

/// Routes the operator's mint serves for suppliers: `/tls-ask` and sign-up
/// (`/join/`, `POST /api/join`).
pub fn operator_router(tenants: Arc<Tenants>) -> axum::Router {
    axum::Router::new()
        .route("/tls-ask", get(tls_ask))
        .route(
            "/join",
            get(|| async { axum::response::Redirect::permanent("/join/") }),
        )
        .route("/join/", get(join_page))
        .route("/api/join", axum::routing::post(post_join))
        .with_state(tenants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use hashu_proxy::suppliers::Check;

    fn tenants_with(dir: &std::path::Path, ths: f64, fake: bool) -> Arc<Tenants> {
        tenants_on(dir, ths, fake, None)
    }

    fn tenants_on(
        dir: &std::path::Path,
        ths: f64,
        fake: bool,
        domain: Option<&str>,
    ) -> Arc<Tenants> {
        let registry = SupplierRegistry::from_file(dir.join("suppliers.json"));
        registry
            .register("alice", &secret_hash("s3cret"), Some(&secret_hash("k3y")))
            .unwrap();
        Tenants::new(
            TenantConfig {
                domain: domain.map(str::to_string),
                ..test_config(dir, ths, fake)
            },
            Arc::new(registry),
        )
    }

    fn test_config(dir: &std::path::Path, ths: f64, fake: bool) -> TenantConfig {
        {
            TenantConfig {
                base: "Mint.Example.com.".into(),
                domain: None,
                dir: dir.join("mints"),
                ledger: Arc::new(RedemptionLedger::new()),
                min_redeem_tih: 1,
                max_tih: 1 << 20,
                fake_lightning: fake,
                hashrate: Arc::new(move |n: &str| if n == "alice" { ths * 1e12 } else { 0.0 }),
                stratum: crate::stratum_addr::StratumAddr::parse(
                    "stratum+tcp://mint.example.com:3335",
                )
                .unwrap(),
                reliability: None,
                epochs: None,
                probe: Arc::new(|a: PayEndpoint| {
                    Box::pin(async move {
                        if a.lnurlp_url().starts_with("https://wallet.example/") {
                            Ok(())
                        } else {
                            Err("no LUD-21".to_string())
                        }
                    })
                }),
            }
        }
    }

    #[tokio::test]
    async fn supplier_mint_dates_vouchers_to_epochs_and_sells_its_plan() {
        let dir = tempfile::tempdir().unwrap();
        // Epoch 10 with 12 blocks (2 h) left.
        let (_tx, clock) =
            tokio::sync::watch::channel(Some(hashu_core::epoch::EpochClock::new(11 * 2016 - 12)));
        let registry = {
            let t = tenants_with(dir.path(), 1.0, true);
            t.registry.clone()
        };
        let mut cfg = TenantConfig {
            epochs: Some(clock),
            ..test_config(dir.path(), 1.0, true)
        };
        cfg.reliability = None;
        let t = Tenants::new(cfg, registry);
        let app = app(&t);
        let alice = "alice.mint.example.com";
        let set = json!({"ask_sats_per_eih": 2_560, "horizon_hours": 400});
        let (s, body) = send(&app, "POST", alice, "/api/settings", Some("k3y"), Some(set)).await;
        assert_eq!(s, StatusCode::OK, "{body}");

        // The chain clock dates the mint: epoch 10's keyset is active.
        let mut idx = serde_json::Value::Null;
        for _ in 0..200 {
            let (s, body) = get(&app, alice, "/v1/hashu/epochs").await;
            if s == StatusCode::OK {
                idx = serde_json::from_str(&body).unwrap();
                let dated = idx["keysets"]
                    .as_object()
                    .unwrap()
                    .values()
                    .any(|k| k["epoch"] == 10 && k["active"] == true);
                if dated {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(idx["epoch"], 10, "{idx}");
        let ks = idx["keysets"].as_object().unwrap();
        assert!(
            ks.values().any(|k| k["epoch"] == 10 && k["active"] == true),
            "{idx}"
        );
        // The keyset from before epochs stays perpetual.
        assert!(ks.values().any(|k| k["perpetual"] == true), "{idx}");
        let e11 = idx["epochs"][1]["keyset_id"].as_str().unwrap().to_string();

        // Live issuance sells only the current epoch: 1 TH/s × the hours
        // left in it, not the 400 h horizon.
        let mut cap = t.capacity("alice").await.unwrap();
        for _ in 0..100 {
            if cap.available_tih > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            cap = t.capacity("alice").await.unwrap();
        }
        assert_eq!(cap.epoch, Some(10));
        let left = hashu_core::epoch::EpochClock::new(11 * 2016 - 12).hours_left();
        assert!(left < 2.1, "{left}");
        assert_eq!(
            cap.available_tih,
            hashu_core::denom::tih_from_ths_hours(1.0, left).floor() as u64
        );

        // The planner is the supplier's: setting it needs the dashboard key.
        let plan = json!({"epochs": {"11": 5.0}});
        let (s, _) = send(
            &app,
            "POST",
            alice,
            "/api/plan",
            Some("nope"),
            Some(plan.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, body) = send(&app, "POST", alice, "/api/plan", Some("k3y"), Some(plan)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let (s, body) = get(&app, alice, "/v1/hashu/plan").await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["upcoming"][1]["epoch"], 11);
        assert_eq!(v["upcoming"][1]["planned_ths"], 5.0);
        // 5 TH/s over the whole of epoch 11 ≈ 5 × 336 THh.
        let cap11 = v["upcoming"][1]["capacity_tih"].as_u64().unwrap() as f64;
        let want = hashu_core::denom::tih_from_ths_hours(5.0, 336.0);
        assert!((cap11 - want).abs() < 2.0, "{cap11} vs {want}");
        // The operator's mint has no plan of its own here.
        assert!(!dir.path().join("hashrate-plan.json").exists());
        assert!(dir.path().join("mints/alice/hashrate-plan.json").exists());

        // Epoch 11 sells from the plan, beyond what the live stock holds.
        let (s, body) = send(
            &app,
            "POST",
            alice,
            "/v1/hashu/epochs/11/mint/quote/bolt11",
            None,
            Some(json!({"amount": 100_000, "unit": "tih"})),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let q: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(q["epoch"], 11);
        assert_eq!(q["keyset_id"], e11.as_str());
        // ...but not over the standard route.
        let (s, _) = send(
            &app,
            "POST",
            alice,
            "/v1/mint/quote/bolt11",
            None,
            Some(json!({"amount": 100_000, "unit": "tih"})),
        )
        .await;
        assert_ne!(s, StatusCode::OK);
        // Stock wallets see epoch 11's keyset, inactive, before it starts.
        let (s, body) = get(&app, alice, "/v1/keysets").await;
        assert_eq!(s, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["keysets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["id"] == e11.as_str() && k["active"] == false));
        // The supplier page says epochs are on.
        let (_, body) = get(&app, alice, "/api/supplier").await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["epochs"], true);
        assert_eq!(v["capacity"]["epoch"], 10);
        t.stop_all().await;
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

    #[test]
    fn supplier_domain_names_mints_and_keeps_the_old_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let t = tenants_on(dir.path(), 0.0, false, Some("Example.com."));
        assert_eq!(t.host_for("alice"), "alice.example.com");
        for ok in [
            "alice.example.com",
            "ALICE.example.com.",
            "alice.example.com:443",
            "alice.mint.example.com",
        ] {
            assert_eq!(t.supplier_for_host(ok).as_deref(), Some("alice"), "{ok}");
        }
        for no in [
            "example.com",
            "mint.example.com",
            "app.example.com",
            "bob.example.com",
            "x.alice.example.com",
            "alice.example.org",
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
            req = req.header(KEY_HEADER, s);
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
    async fn join_on_the_supplier_domain() {
        let dir = tempfile::tempdir().unwrap();
        let t = tenants_on(dir.path(), 0.0, false, Some("example.com"));
        let app = app(&t);
        let op = "mint.example.com";
        let join = |name: &str| json!({"name": name, "lightning_address": "x@wallet.example", "ask_sats_per_eih": 600, "horizon_hours": 24});
        for reserved in ["app", "www", "mint", "wallet", "app-hashu"] {
            let (s, body) = send(&app, "POST", op, "/api/join", None, Some(join(reserved))).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{reserved}: {body}");
            assert!(body.contains("reserved"), "{body}");
        }
        let (s, body) = send(&app, "POST", op, "/api/join", None, Some(join("bob"))).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let j: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(j["mint_url"], "https://bob.example.com");
        for host in ["bob.example.com", "bob.mint.example.com"] {
            let (s, body) = send(&app, "GET", host, "/api/supplier", None, None).await;
            assert_eq!(s, StatusCode::OK, "{host}: {body}");
        }
        let (_, page) = send(&app, "GET", op, "/join/", None, None).await;
        assert!(page.contains(">.example.com<"), "{page}");
        let ask = |d: &str| format!("/tls-ask?domain={d}");
        let (s, _) = get(&app, "hashu", &ask("bob.example.com")).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = get(&app, "hashu", &ask("app.example.com")).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn join_creates_a_mint_whose_settings_need_the_secret() {
        let dir = tempfile::tempdir().unwrap();
        let t = tenants(dir.path());
        let app = app(&t);
        let op = "mint.example.com";
        let join = |name: &str, ln: &str| json!({"name": name, "lnurl": ln, "ask_sats_per_eih": 600, "horizon_hours": 24});
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
        // The public address (HASHU_STRATUM_URL), not the bind port.
        assert_eq!(j["stratum"]["url"], "stratum+tcp://mint.example.com:3335");
        assert_eq!(j["stratum"]["host"], "mint.example.com");
        assert_eq!(j["stratum"]["port"], 3335);
        let secret = j["secret"].as_str().unwrap().to_string();
        assert_eq!(j["stratum"]["password"], format!("pw={secret}"));
        let key = j["dashboard_key"].as_str().unwrap().to_string();
        assert_ne!(key, secret);
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
        // A miner's own LNURL-pay endpoint works as well as an address,
        // and one on an IP address is refused before anything is fetched.
        let (s, body) = send(
            &app,
            "POST",
            op,
            "/api/join",
            None,
            Some(join("dave", "lightning:https://wallet.example/lnurlp/dave")),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{body}");
        assert_eq!(
            SupplierSettings::load(&dir.path().join("mints/dave"))
                .unwrap()
                .lnurl
                .as_deref(),
            Some("https://wallet.example/lnurlp/dave")
        );
        let (s, _) = send(
            &app,
            "POST",
            op,
            "/api/join",
            None,
            Some(join("erin", "https://10.0.0.1/lnurlp/erin")),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(!t.registry.contains("erin"));

        let bob = "bob.mint.example.com";
        let (s, body) = send(&app, "GET", bob, "/api/supplier", None, None).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let st: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            (st["name"].as_str(), st["ask_sats_per_eih"].as_u64()),
            (Some("bob"), Some(600))
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
        // The rig secret travels in clear over Stratum: it must not open
        // the dashboard.
        let (s, _) = send(&app, "GET", bob, "/api/settings", Some(&secret), None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, body) = send(&app, "GET", bob, "/api/settings", Some(&key), None).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let short = json!({"ask_sats_per_eih": 620, "horizon_hours": 0.5, "lightning_address": "bob@wallet.example"});
        let (s, _) = send(&app, "POST", bob, "/api/settings", Some(&key), Some(short)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let ok = json!({"ask_sats_per_eih": 620, "horizon_hours": 1, "lnurl": "lnurlp://wallet.example/lnurlp/bob"});
        let (s, body) = send(&app, "POST", bob, "/api/settings", Some(&key), Some(ok)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        let saved = SupplierSettings::load(&dir.path().join("mints/bob")).unwrap();
        assert_eq!(saved.ask_sats_per_eih, Some(620));
        assert_eq!(
            saved.lnurl.as_deref(),
            Some("https://wallet.example/lnurlp/bob")
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
        let quote = |tih: u64| json!({"amount": tih, "unit": "tih"});
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

        let set = json!({"ask_sats_per_eih": 2_560, "horizon_hours": 2});
        let (s, body) = send(&app, "POST", alice, "/api/settings", Some("k3y"), Some(set)).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        // 1 TH/s × 2 h = 2 THH ≈ 6,548 TiH, once the mint has counted
        // what it owes and stocked its first batch.
        let mut available = 0;
        for _ in 0..100 {
            available = t.capacity("alice").await.unwrap().available_tih;
            if available > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(available, 6_548);
        let (s, body) = send(
            &app,
            "POST",
            alice,
            "/v1/mint/quote/bolt11",
            None,
            Some(quote(6_548)),
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
            Some(quote(6_549)),
        )
        .await;
        assert_ne!(s, StatusCode::OK, "{body}");
        t.stop_all().await;
    }
}
