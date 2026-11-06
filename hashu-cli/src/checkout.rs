//! Buyer checkout page, served by `hashu mint run` on the mint's public
//! HTTP port at `/checkout/` (issue #3).
//!
//! A buyer picks an amount of THH and a rail, pays the Lightning invoice
//! (or the USDT deposit) shown as a QR code, and gets a Cashu token for the
//! THH. They can then paste the token back with their pool and worker to
//! redeem it as hashrate, and watch the hashrate land on their pool.
//!
//! The daemon runs a throwaway cdk wallet per checkout against its own mint
//! API, so the page needs no browser wallet. That makes the daemon briefly
//! hold the buyer's ecash: the token it hands over is bearer money, and so
//! is the one a redeemer pastes in. Any Cashu wallet that speaks the `thh`
//! unit can buy and redeem without this page.
//!
//! - `GET  /checkout/`: the page.
//! - `GET  /checkout/api/info`: price, rails, limits, whether issuance is on.
//! - `POST /checkout/api/buy` `{ amount_thh, method }`: a quote to pay.
//! - `GET  /checkout/api/buy/{id}`: quote state; once paid, the token.
//! - `POST /checkout/api/redeem` `{ token, pool_url, worker_name }`.
//! - `GET  /checkout/api/redemption/{id}`: progress and delivered work.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Json;
use cdk::amount::SplitTarget;
use cdk::mint_url::MintUrl;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod, Token};
use cdk::wallet::{MeltOutcome, ReceiveOptions, Wallet};
use cdk::Amount;
use hashu_core::redemption::{RedemptionId, RedemptionLedger, RedemptionState, HASHES_PER_THH};
use hashu_mint::melt::{redemption_id_for, METHOD as HASHRATE};
use hashu_mint::tether::METHOD as USDT;
use hashu_proxy::meter::HashrateMeter;
use hashu_proxy::router::upstream_for_pool_url;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::operator::{ControlledPricer, MintControls};

const PAGE: &str = include_str!("checkout.html");
/// Mint the proofs for a paid quote in-process: blind outputs, have the
/// mint sign them, unblind. Used for custom methods (`usdt`), where cdk
/// 0.16's wallet cannot mint (it misreads the custom quote responses).
async fn mint_in_process(
    s: &CheckoutState,
    quote_id: &str,
    amount_thh: u64,
) -> anyhow::Result<cdk::nuts::Proofs> {
    use cdk::nuts::{MintRequest, PreMintSecrets};
    let unit = CurrencyUnit::Custom("thh".into());
    let keyset = s
        .mint
        .keysets()
        .keysets
        .into_iter()
        .find(|k| k.unit == unit && k.active)
        .ok_or_else(|| anyhow::anyhow!("no active THH keyset"))?;
    let keys = s
        .mint
        .keyset_pubkeys(&keyset.id)?
        .keysets
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no keys for {}", keyset.id))?
        .keys;
    let amounts: Vec<u64> = keys.iter().map(|(a, _)| u64::from(*a)).collect();
    let premint = PreMintSecrets::random(
        keyset.id,
        Amount::from(amount_thh),
        &SplitTarget::default(),
        &(0u64, amounts).into(),
    )?;
    let resp = s
        .mint
        .process_mint_request(cdk::mint::MintInput::Single(MintRequest {
            quote: cdk::mint::QuoteId::from_str(quote_id)?,
            outputs: premint.blinded_messages(),
            signature: None,
        }))
        .await?;
    Ok(cdk::dhke::construct_proofs(
        resp.signatures,
        premint.rs(),
        premint.secrets(),
        &keys,
    )?)
}

/// Whether a quote is paid, asked of the mint directly.
async fn quote_paid(s: &CheckoutState, quote_id: &str) -> anyhow::Result<bool> {
    let id = cdk::mint::QuoteId::from_str(quote_id)?;
    let q = s.mint.check_mint_quote(&id).await?;
    let state = match q {
        cdk::mint::MintQuoteResponse::Bolt11(r) => r.state,
        cdk::mint::MintQuoteResponse::Bolt12(_) => return Ok(false),
        cdk::mint::MintQuoteResponse::Custom { response, .. } => response.state,
    };
    Ok(matches!(
        state,
        MintQuoteState::Paid | MintQuoteState::Issued
    ))
}

/// Open a mint quote for `wallet` and return `(quote id, payment request)`.
///
/// cdk 0.16's wallet cannot request a custom-method (`usdt`) mint quote: it
/// posts the `(method, request)` tuple as a JSON array, which the mint
/// rejects. For those, ask the mint in-process and record the quote in the
/// wallet's store so `wallet.mint` can redeem it as usual.
async fn new_quote(
    s: &CheckoutState,
    wallet: &Wallet,
    method: &str,
    amount_thh: u64,
) -> anyhow::Result<(String, String)> {
    if method == "bolt11" {
        let q = wallet
            .mint_quote(
                method_of(method),
                Some(Amount::from(amount_thh)),
                None,
                None,
            )
            .await?;
        return Ok((q.id, q.request));
    }
    let unit = CurrencyUnit::Custom("thh".into());
    let resp = s
        .mint
        .get_mint_quote(cdk::mint::MintQuoteRequest::Custom {
            method: method.to_string(),
            request: cdk::nuts::MintQuoteCustomRequest {
                amount: Amount::from(amount_thh),
                unit: unit.clone(),
                description: None,
                pubkey: None,
                extra: Value::Null,
            },
        })
        .await?;
    let cdk::mint::MintQuoteResponse::Custom { response, .. } = resp else {
        anyhow::bail!("unexpected quote response for {method}");
    };
    let id = response.quote.to_string();
    let quote = cdk::wallet::MintQuote::new(
        id.clone(),
        MintUrl::from_str(&s.internal_url)?,
        method_of(method),
        Some(Amount::from(amount_thh)),
        unit,
        response.request.clone(),
        response.expiry.unwrap_or_default(),
        None,
    );
    wallet.localstore.add_mint_quote(quote).await?;
    Ok((id, response.request))
}
/// Checkouts are forgotten this long after they start.
const CHECKOUT_TTL: Duration = Duration::from_secs(24 * 3600);

struct Checkout {
    wallet: Wallet,
    quote_id: String,
    method: String,
    amount_thh: u64,
    price: Option<u64>,
    request: String,
    token: Option<String>,
    started: Instant,
}

/// What the checkout routes share.
pub struct CheckoutState {
    /// URL the throwaway wallets reach the mint on (loopback).
    pub internal_url: String,
    /// URL written into tokens: the mint's public URL when one is set.
    pub public_url: String,
    pub controls: Arc<MintControls>,
    pub sats: Option<Arc<ControlledPricer>>,
    pub usdt: Option<Arc<ControlledPricer>>,
    /// Payment methods the mint sells THH over (`bolt11`, `usdt`).
    pub methods: Vec<String>,
    pub min_redeem_thh: u64,
    pub max_thh: u64,
    pub ledger: Arc<RedemptionLedger>,
    pub mint: Arc<cdk::mint::Mint>,
    pub meter: Arc<HashrateMeter>,
    checkouts: Mutex<HashMap<String, Arc<Mutex<Checkout>>>>,
}

impl CheckoutState {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        internal_url: String,
        public_url: String,
        controls: Arc<MintControls>,
        sats: Option<Arc<ControlledPricer>>,
        usdt: Option<Arc<ControlledPricer>>,
        methods: Vec<String>,
        min_redeem_thh: u64,
        max_thh: u64,
        ledger: Arc<RedemptionLedger>,
        mint: Arc<cdk::mint::Mint>,
        meter: Arc<HashrateMeter>,
    ) -> Self {
        Self {
            internal_url,
            public_url,
            controls,
            sats,
            usdt,
            methods,
            min_redeem_thh,
            max_thh,
            ledger,
            mint,
            meter,
            checkouts: Mutex::new(HashMap::new()),
        }
    }
}

pub fn router(state: Arc<CheckoutState>) -> axum::Router {
    axum::Router::new()
        .route(
            "/checkout",
            get(|| async { Redirect::permanent("/checkout/") }),
        )
        .route("/checkout/", get(page))
        .route("/checkout/api/info", get(info))
        .route("/checkout/api/buy", post(buy))
        .route("/checkout/api/buy/{id}", get(buy_status))
        .route("/checkout/api/redeem", post(redeem))
        .route("/checkout/api/redemption/{id}", get(redemption))
        .with_state(state)
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

fn err(code: StatusCode, msg: impl ToString) -> Response {
    (code, Json(json!({ "error": msg.to_string() }))).into_response()
}

async fn fresh_wallet(url: &str) -> anyhow::Result<Wallet> {
    let seed: [u8; 64] = std::array::from_fn(|_| rand::random());
    let db = cdk_sqlite::wallet::memory::empty().await?;
    Ok(Wallet::new(
        url,
        CurrencyUnit::Custom("thh".into()),
        Arc::new(db),
        seed,
        None,
    )?)
}

fn qr_svg(data: &str) -> Option<String> {
    use qrcode::render::svg;
    let code =
        qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::L).ok()?;
    Some(
        code.render::<svg::Color>()
            .min_dimensions(240, 240)
            .quiet_zone(true)
            .build(),
    )
}

fn method_of(name: &str) -> PaymentMethod {
    match name {
        "bolt11" => PaymentMethod::Known(KnownMethod::Bolt11),
        other => PaymentMethod::Custom(other.to_string()),
    }
}

async fn info(State(s): State<Arc<CheckoutState>>) -> Json<Value> {
    Json(json!({
        "issuance_paused": s.controls.issuance_paused(),
        "methods": s.methods,
        "sats_per_thh": s.sats.as_ref().and_then(|p| p.quote(1).ok()),
        "usdt_per_thh": s.usdt.as_ref().and_then(|p| p.quote(1_000_000).ok()).map(|m| m as f64 / 1e12),
        "min_redeem_thh": s.min_redeem_thh,
        "max_thh": s.max_thh,
        "mint_url": s.public_url,
        "hashes_per_thh": HASHES_PER_THH as f64,
    }))
}

async fn buy(State(s): State<Arc<CheckoutState>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")),
    };
    let Some(amount) = v["amount_thh"]
        .as_u64()
        .filter(|a| *a >= 1 && *a <= s.max_thh)
    else {
        return err(
            StatusCode::BAD_REQUEST,
            format!("amount_thh must be 1 to {}", s.max_thh),
        );
    };
    let method = v["method"].as_str().unwrap_or("bolt11").to_string();
    if !s.methods.contains(&method) {
        return err(
            StatusCode::BAD_REQUEST,
            format!("this mint does not sell over {method}"),
        );
    }
    if s.controls.issuance_paused() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "the operator has paused sales; try again later",
        );
    }

    let wallet = match fresh_wallet(&s.internal_url).await {
        Ok(w) => w,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let quote = match new_quote(&s, &wallet, &method, amount).await {
        Ok(q) => q,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("mint refused the quote: {e}"),
            )
        }
    };
    let price = match method.as_str() {
        "bolt11" => s.sats.as_ref().and_then(|p| p.quote(amount).ok()),
        USDT => s.usdt.as_ref().and_then(|p| p.quote(amount).ok()),
        _ => None,
    };
    let id = quote.0.clone();
    let c = Checkout {
        wallet,
        quote_id: quote.0.clone(),
        method,
        amount_thh: amount,
        price,
        request: quote.1.clone(),
        token: None,
        started: Instant::now(),
    };
    let body = checkout_json(&id, &c, "unpaid");
    {
        let mut map = s.checkouts.lock().await;
        map.retain(|_, c| {
            c.try_lock()
                .map(|c| c.started.elapsed() < CHECKOUT_TTL)
                .unwrap_or(true)
        });
        map.insert(id.clone(), Arc::new(Mutex::new(c)));
    }
    tracing::info!(quote = %id, amount_thh = amount, "checkout started");
    Json(body).into_response()
}

fn checkout_json(id: &str, c: &Checkout, state: &str) -> Value {
    let qr_data = if c.method == "bolt11" {
        format!("lightning:{}", c.request)
    } else {
        c.request.clone()
    };
    json!({
        "id": id,
        "method": c.method,
        "amount_thh": c.amount_thh,
        // sats for bolt11, micro-USDT for usdt
        "price": c.price,
        "request": c.request,
        "qr_svg": qr_svg(&qr_data),
        "state": state,
        "token": c.token,
    })
}

async fn buy_status(State(s): State<Arc<CheckoutState>>, Path(id): Path<String>) -> Response {
    let Some(entry) = s.checkouts.lock().await.get(&id).cloned() else {
        return err(StatusCode::NOT_FOUND, "unknown checkout");
    };
    let mut c = entry.lock().await;
    if c.token.is_some() {
        return Json(checkout_json(&id, &c, "issued")).into_response();
    }
    let custom = c.method != "bolt11";
    let paid = if custom {
        quote_paid(&s, &c.quote_id).await.map_err(|e| e.to_string())
    } else {
        c.wallet
            .check_mint_quote_status(&c.quote_id)
            .await
            .map(|q| matches!(q.state, MintQuoteState::Paid | MintQuoteState::Issued))
            .map_err(|e| e.to_string())
    };
    match paid {
        Ok(true) => {}
        Ok(false) => return Json(checkout_json(&id, &c, "unpaid")).into_response(),
        Err(e) => return err(StatusCode::BAD_GATEWAY, e),
    }
    let minted = if custom {
        mint_in_process(&s, &c.quote_id, c.amount_thh)
            .await
            .map_err(|e| e.to_string())
    } else {
        c.wallet
            .mint(&c.quote_id, SplitTarget::default(), None)
            .await
            .map_err(|e| e.to_string())
    };
    let proofs = match minted {
        Ok(p) => p,
        Err(e) => {
            return err(
                StatusCode::BAD_GATEWAY,
                format!("paid, but minting failed: {e}"),
            )
        }
    };
    let mint_url = match MintUrl::from_str(&s.public_url) {
        Ok(u) => u,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let token = Token::new(
        mint_url,
        proofs,
        Some("Hashu THH".into()),
        CurrencyUnit::Custom("thh".into()),
    );
    c.token = Some(token.to_string());
    tracing::info!(quote = %id, amount_thh = c.amount_thh, "checkout paid; token issued");
    Json(checkout_json(&id, &c, "issued")).into_response()
}

pub(crate) async fn redeem(State(s): State<Arc<CheckoutState>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")),
    };
    let token = v["token"].as_str().unwrap_or_default().trim().to_string();
    let pool_url = v["pool_url"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    let worker = v["worker_name"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if upstream_for_pool_url(&pool_url).is_none() {
        return err(
            StatusCode::BAD_REQUEST,
            "pool must be stratum+tcp://host:port",
        );
    }
    if worker.is_empty() {
        return err(StatusCode::BAD_REQUEST, "worker name is required");
    }
    let token = match Token::from_str(&token) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("not a Cashu token: {e}")),
    };
    if token.unit() != Some(CurrencyUnit::Custom("thh".into())) {
        return err(StatusCode::BAD_REQUEST, "token is not THH");
    }

    let wallet = match fresh_wallet(&s.internal_url).await {
        Ok(w) => w,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    // The token names the mint's public URL; take its proofs and receive
    // them over loopback.
    let keysets = match wallet.load_mint_keysets().await {
        Ok(k) => k,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e),
    };
    let proofs = match token.proofs(&keysets) {
        Ok(p) => p,
        Err(_) => return err(StatusCode::BAD_REQUEST, "token is not from this mint"),
    };
    let amount = match wallet
        .receive_proofs(proofs, ReceiveOptions::default(), None, None)
        .await
    {
        Ok(a) => u64::from(a),
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!("token rejected (already spent?): {e}"),
            )
        }
    };
    if amount < s.min_redeem_thh {
        return err(
            StatusCode::BAD_REQUEST,
            format!("redeem at least {} THH", s.min_redeem_thh),
        );
    }

    let request = json!({
        "pool_url": pool_url,
        "worker_name": worker,
        "amount_thh": amount,
        "nonce": hex::encode(rand::random::<[u8; 8]>()),
    })
    .to_string();
    let rid = redemption_id_for(&request);
    let quote = match wallet
        .melt_quote(PaymentMethod::Custom(HASHRATE.into()), request, None, None)
        .await
    {
        Ok(q) => q,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("redeem quote failed: {e}")),
    };
    let prepared = match wallet.prepare_melt(&quote.id, HashMap::new()).await {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("redeem failed: {e}")),
    };
    match prepared.confirm_prefer_async().await {
        Ok(MeltOutcome::Paid(_)) | Ok(MeltOutcome::Pending(_)) => {}
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("redeem failed: {e}")),
    }
    tracing::info!(redemption = %rid, amount_thh = amount, %pool_url, "checkout redemption opened");
    Json(json!({ "redemption_id": rid.to_string(), "amount_thh": amount })).into_response()
}

/// Mint `amount_thh` of test THH without a payment, for the operator to
/// rehearse a redemption on a live mint. It opens a mint quote on the first
/// rail and marks it paid from inside the mint, so cdk's books stay
/// consistent: the THH counts as issued ecash like any other.
pub(crate) async fn operator_test_mint(
    s: &CheckoutState,
    mint: &cdk::mint::Mint,
    amount_thh: u64,
) -> Result<(String, String), (StatusCode, String)> {
    let bad_gw = |e: String| (StatusCode::BAD_GATEWAY, e);
    let Some(method) = s.methods.first().cloned() else {
        return Err((
            StatusCode::CONFLICT,
            "no payment rail is enabled, so there is no quote to mark paid".into(),
        ));
    };
    let wallet = fresh_wallet(&s.internal_url)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    // A pause makes the pricers refuse quotes; a test mint should work
    // even while public sales are paused.
    let paused = s.controls.issuance_paused();
    if paused {
        s.controls.set_issuance_paused(false);
    }
    let quote = new_quote(s, &wallet, &method, amount_thh).await;
    if paused {
        s.controls.set_issuance_paused(true);
    }
    let (quote_id, _) = quote.map_err(|e| bad_gw(format!("mint refused the quote: {e}")))?;

    let lookup = mint
        .mint_quotes()
        .await
        .map_err(|e| bad_gw(e.to_string()))?
        .into_iter()
        .find(|q| q.id.to_string() == quote_id)
        .map(|q| q.request_lookup_id)
        .ok_or_else(|| bad_gw("quote not found in the mint".into()))?;
    let payment_id = format!("operator-test-{}", hex::encode(rand::random::<[u8; 8]>()));
    mint.pay_mint_quote_for_request_id(cdk::cdk_payment::WaitPaymentResponse {
        payment_identifier: lookup,
        payment_amount: cdk::Amount::new(amount_thh, hashu_mint::payment::thh_unit()),
        payment_id: payment_id.clone(),
    })
    .await
    .map_err(|e| bad_gw(format!("marking the quote paid failed: {e}")))?;

    let proofs = if method == "bolt11" {
        wallet
            .mint(&quote_id, SplitTarget::default(), None)
            .await
            .map_err(|e| e.to_string())
    } else {
        mint_in_process(s, &quote_id, amount_thh)
            .await
            .map_err(|e| e.to_string())
    }
    .map_err(|e| bad_gw(format!("minting failed: {e}")))?;
    let minted: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();
    if minted != amount_thh {
        return Err(bad_gw(format!(
            "minted {minted} THH, expected {amount_thh}"
        )));
    }
    let mint_url = MintUrl::from_str(&s.public_url)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let token = Token::new(
        mint_url,
        proofs,
        Some("Hashu test THH (operator)".into()),
        CurrencyUnit::Custom("thh".into()),
    );
    tracing::warn!(
        amount_thh,
        quote = %quote_id,
        %payment_id,
        "operator minted test THH without payment"
    );
    Ok((token.to_string(), quote_id))
}

async fn redemption(State(s): State<Arc<CheckoutState>>, Path(id): Path<String>) -> Response {
    let Some(rid) = parse_id(&id) else {
        return err(StatusCode::BAD_REQUEST, "bad redemption id");
    };
    let Some(r) = s.ledger.get(&rid) else {
        return err(StatusCode::NOT_FOUND, "unknown redemption");
    };
    let track = s.meter.tracks().into_iter().find(|t| t.id == id);
    let filled = r.accumulated_hashes as f64 / HASHES_PER_THH as f64;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "id": id,
            "pool_url": r.pool_url,
            "worker_name": r.worker_name,
            "amount_thh": r.amount_thh,
            "filled_thh": filled,
            "progress": if r.target_hashes == 0 { 1.0 } else { (r.accumulated_hashes as f64 / r.target_hashes as f64).min(1.0) },
            "state": match r.state {
                RedemptionState::Pending => "pending",
                RedemptionState::Active => "active",
                RedemptionState::Completed => "completed",
            },
            "track": track,
            "now_ms": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default(),
        })),
    )
        .into_response()
}

fn parse_id(s: &str) -> Option<RedemptionId> {
    let bytes = hex::decode(s).ok()?;
    let arr: [u8; 16] = bytes.try_into().ok()?;
    Some(RedemptionId::from_bytes(arr))
}
