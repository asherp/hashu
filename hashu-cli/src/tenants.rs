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
//! Selling: supplier payment backends (the buyer paying the supplier's own
//! wallet) come next. Until then a supplier mint sells only when the
//! operator runs `--lightning fake` (testing); the operator's phoenixd
//! wallet is never used for a supplier's mint.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Query, Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use cdk::mint::{Mint, MintBuilder, MintMeltLimits, UnitConfig};
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use hashu_core::redemption::RedemptionLedger;
use hashu_mint::lightning::add_lightning_issuance;
use hashu_mint::melt::{reopen_pending_quotes_in, HashrateMelter, METHOD as HASHRATE};
use hashu_mint::payment::{thh_unit, ThhIssuer};
use hashu_mint::pricing::ThhPricer;
use hashu_proxy::suppliers::{valid_name, SupplierRegistry};
use tower::ServiceExt;

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
    /// Price for the fake Lightning rail, when the operator runs one.
    pub fake_lightning: Option<Arc<dyn ThhPricer>>,
}

struct Tenant {
    mint: Arc<Mint>,
    app: axum::Router,
}

/// The supplier mints this process serves.
pub struct Tenants {
    cfg: TenantConfig,
    registry: Arc<SupplierRegistry>,
    mints: tokio::sync::Mutex<HashMap<String, Arc<Tenant>>>,
}

impl Tenants {
    pub fn new(cfg: TenantConfig, registry: Arc<SupplierRegistry>) -> Self {
        Self {
            cfg: TenantConfig {
                base: cfg.base.trim().trim_end_matches('.').to_ascii_lowercase(),
                ..cfg
            },
            registry,
            mints: Default::default(),
        }
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
            if let Err(e) = t.mint.stop().await {
                tracing::warn!(supplier = %name, error = %e, "supplier mint stop");
            }
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

    async fn build(&self, name: &str) -> Result<Tenant> {
        let dir = self.cfg.dir.join(name);
        std::fs::create_dir_all(&dir).with_context(|| format!("create {dir:?}"))?;
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
        if let Some(pricer) = &self.cfg.fake_lightning {
            let node = cdk_fake_wallet::FakeWallet::new(
                cdk::types::FeeReserve {
                    min_fee_reserve: 1.into(),
                    percent_fee_reserve: 0.0,
                },
                Default::default(),
                Default::default(),
                2,
                CurrencyUnit::Sat,
            );
            let issuer = ThhIssuer::new(pricer.clone(), Arc::new(node), CurrencyUnit::Sat);
            builder =
                add_lightning_issuance(builder, issuer, MintMeltLimits::new(1, self.cfg.max_thh))
                    .await
                    .context("register fake Lightning rail")?;
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
        let app = cdk_axum::create_mint_router(mint.clone(), methods)
            .await
            .context("build mint HTTP router")?;
        tracing::info!(supplier = %name, host = %self.host_for(name), "supplier mint ready");
        Ok(Tenant { mint, app })
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

/// `/tls-ask` for the mint HTTP app.
pub fn ask_router(tenants: Arc<Tenants>) -> axum::Router {
    axum::Router::new()
        .route("/tls-ask", axum::routing::get(tls_ask))
        .with_state(tenants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn tenants(dir: &std::path::Path) -> Arc<Tenants> {
        Arc::new(Tenants::new(
            TenantConfig {
                base: "Mint.Example.com.".into(),
                dir: dir.join("mints"),
                ledger: Arc::new(RedemptionLedger::new()),
                min_redeem_thh: 1,
                max_thh: 1_000,
                fake_lightning: None,
            },
            Arc::new(SupplierRegistry::from_secrets([("alice", "s3cret")])),
        ))
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
            .merge(ask_router(t.clone()))
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
}
