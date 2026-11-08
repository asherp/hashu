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
//! - `POST /api/redeem` `{ pool_url, worker_name, amount_thh? }`: the same
//!   redemption paid over HTTP 402 (Cashu NUT-24): no `X-Cashu` header
//!   gets a `402` with a payment request, a `cashuB` THH token in
//!   `X-Cashu` opens the redemption. See [`crate::paid`].

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Json;
use cdk::amount::SplitTarget;
use cdk::mint_url::MintUrl;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod, State as ProofSpentState, Token};
use cdk::wallet::{KeysetFilter, MeltOutcome, ReceiveOptions, Wallet};
use cdk::Amount;
use hashu_core::redemption::{RedemptionId, RedemptionLedger, RedemptionState, HASHES_PER_THH};
use hashu_mint::melt::{redemption_id_for, METHOD as HASHRATE};
use hashu_mint::tether::METHOD as USDT;
use hashu_proxy::meter::HashrateMeter;
use hashu_proxy::router::upstream_for_pool_url;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::operator::{ControlledPricer, MintControls};
use crate::paid::{self, CashuScheme, Credential, Price, Rejection, Scheme};

const PAGE: &str = include_str!("checkout.html");
/// QR reader plus UR decoder, bundled so pages load nothing from a CDN.
/// Built from `assets/scan.src.js`; the pages load it only when someone
/// presses "Scan token".
pub(crate) const SCAN_JS: &str = include_str!("assets/scan.js");

pub(crate) async fn scan_js() -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        SCAN_JS,
    )
        .into_response()
}
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

impl CheckoutState {
    /// NUT-24 payments this mint takes: its own THH, named by the URL its
    /// tokens carry.
    fn cashu_scheme(&self) -> anyhow::Result<CashuScheme> {
        Ok(CashuScheme::thh(MintUrl::from_str(&self.public_url)?))
    }
}

pub fn router(state: Arc<CheckoutState>) -> axum::Router {
    axum::Router::new()
        .route(
            "/checkout",
            get(|| async { Redirect::permanent("/checkout/") }),
        )
        .route("/checkout/", get(page))
        .route("/checkout/scan.js", get(scan_js))
        .route("/checkout/api/info", get(info))
        .route("/checkout/api/buy", post(buy))
        .route("/checkout/api/buy/{id}", get(buy_status))
        .route("/checkout/api/token", post(check_token))
        .route("/checkout/api/redeem", post(redeem))
        .route("/checkout/api/redemption/{id}", get(redemption))
        .route("/api/redeem", post(paid_redeem))
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

/// A QR code as a compact SVG: one path, each row's dark runs as
/// rectangles, so an animated token QR stays a few KB per frame.
fn qr_svg(data: &str) -> Option<String> {
    let code =
        qrcode::QrCode::with_error_correction_level(data.as_bytes(), qrcode::EcLevel::L).ok()?;
    let w = code.width();
    let colors = code.to_colors();
    let quiet = 4;
    let size = w + 2 * quiet;
    let mut d = String::new();
    for y in 0..w {
        let mut x = 0;
        while x < w {
            if colors[y * w + x] == qrcode::Color::Dark {
                let start = x;
                while x < w && colors[y * w + x] == qrcode::Color::Dark {
                    x += 1;
                }
                d.push_str(&format!(
                    "M{} {}h{}v1h-{}z",
                    start + quiet,
                    y + quiet,
                    x - start,
                    x - start
                ));
            } else {
                x += 1;
            }
        }
    }
    Some(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" shape-rendering="crispEdges"><rect width="{size}" height="{size}" fill="#fff"/><path fill="#000" d="{d}"/></svg>"##
    ))
}

/// One proof per set bit of the amount. The cdk wallet's default split
/// keeps several proofs of each size for making change, which only makes
/// a token we hand over longer (and its QR code harder to scan).
fn fewest_proofs(amount_thh: u64) -> SplitTarget {
    SplitTarget::Values(
        (0..64)
            .filter(|b| amount_thh >> b & 1 == 1)
            .map(|b| Amount::from(1u64 << b))
            .collect(),
    )
}

/// QR frames for a Cashu token. A short token is one plain QR code; a
/// longer one becomes an animated `ur:bytes` QR (one SVG per frame), which
/// Cashu wallets such as Cashu.me scan frame by frame.
pub(crate) fn token_qr_frames(token: &str) -> Vec<String> {
    const STATIC_MAX: usize = 600;
    const FRAGMENT: usize = 200;
    if token.len() <= STATIC_MAX {
        return qr_svg(token).into_iter().collect();
    }
    crate::ur::ur_bytes_parts(token.as_bytes(), FRAGMENT)
        .iter()
        .filter_map(|part| qr_svg(part))
        .collect()
}

fn method_of(name: &str) -> PaymentMethod {
    match name {
        "bolt11" => PaymentMethod::Known(KnownMethod::Bolt11),
        other => PaymentMethod::Custom(other.to_string()),
    }
}

async fn info(State(s): State<Arc<CheckoutState>>) -> Json<Value> {
    let cap = s.controls.capacity.snapshot(hashu_mint::capacity::now_ms());
    Json(json!({
        // THH the rented hashrate can still deliver beyond what is owed;
        // null when the mint does not cap sales.
        "available_thh": cap.available_thh,
        // allocated + available = total capacity (rented TH/s × hours
        // left); allocated is THH already sold or still being delivered.
        "capacity_thh": cap.capacity_thh,
        "allocated_thh": cap.owed_thh,
        "rented_ths": cap.rented_ths,
        "capacity_ends_ms": cap.ends_ms,
        "issuance_paused": s.controls.issuance_paused(),
        "methods": s.methods,
        "sats_per_thh": s.sats.as_ref().and_then(|p| p.quote(1).ok()),
        "usdt_per_thh": s.usdt.as_ref().and_then(|p| p.quote(1_000_000).ok()).map(|m| m as f64 / 1e12),
        "min_redeem_thh": s.min_redeem_thh,
        "max_thh": s.max_thh,
        "mint_url": s.public_url,
        "hashes_per_thh": HASHES_PER_THH as f64,
        "delivery": delivery_json(&s),
    }))
}

/// Window the checkout's delivery estimate averages hashrate over.
const ETA_WINDOW: Duration = Duration::from_secs(300);

/// What a delivery estimate needs: the proxy's current hashrate and the
/// THH still owed to open redemptions, which are served before a new one.
fn delivery_json(s: &CheckoutState) -> Value {
    let queue: Vec<_> = s.ledger.active();
    let queue_thh: f64 = queue
        .iter()
        .map(|r| r.target_hashes.saturating_sub(r.accumulated_hashes) as f64)
        .sum::<f64>()
        / HASHES_PER_THH as f64;
    json!({
        "hashrate_hs": s.meter.rate_over(ETA_WINDOW),
        "window_s": ETA_WINDOW.as_secs(),
        "queue_thh": queue_thh,
        "open_redemptions": queue.len(),
    })
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
    if let Err(e) = s
        .controls
        .capacity
        .check(amount, hashu_mint::capacity::now_ms())
    {
        return err(StatusCode::CONFLICT, e);
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
        "token_qr": c.token.as_deref().map(token_qr_frames),
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
            .mint(&c.quote_id, fewest_proofs(c.amount_thh), None)
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
    let target = match RedeemTarget::from_json(&v) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let raw_token = v["token"].as_str().unwrap_or_default();
    let wallet = match fresh_wallet(&s.internal_url).await {
        Ok(w) => w,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    // Refuse a token we can't redeem before taking it: once its proofs are
    // received they belong to a throwaway wallet, so a later refusal would
    // lose the buyer's THH.
    let checked = match inspect_token(&wallet, raw_token).await {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    let amount = checked.unspent_thh;
    if amount == 0 {
        return TokenProblem::new(TokenIssue::Spent).into_response();
    }
    if amount < s.min_redeem_thh {
        return err(
            StatusCode::BAD_REQUEST,
            format!("redeem at least {} THH", s.min_redeem_thh),
        );
    }
    spend_for_hashrate(&s, &wallet, checked, &target).await
}

/// `POST /api/redeem`, paid per NUT-24: redeem a THH voucher sent in the
/// `X-Cashu` header, as hashrate to the pool in the JSON body
/// `{ pool_url, worker_name, worker_password?, amount_thh? }`.
///
/// Without `X-Cashu` it answers `402` with a NUT-18 request for
/// `amount_thh` (default: the smallest redemption) in unit `thh` from this
/// mint. With one, the token must be a `cashuB` token from this mint, in
/// `thh`, worth at least `amount_thh`, or the answer is `400`. Every
/// unspent THH in the token is redeemed, so sending more than asked buys
/// more hashrate rather than tipping the mint.
pub(crate) async fn paid_redeem(
    State(s): State<Arc<CheckoutState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let v: Value = if body.iter().all(u8::is_ascii_whitespace) {
        json!({})
    } else {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("invalid JSON: {e}")),
        }
    };
    let target = match RedeemTarget::from_json(&v) {
        Ok(t) => t,
        Err(r) => return r,
    };
    let amount_thh = match &v["amount_thh"] {
        Value::Null => s.min_redeem_thh,
        n => match n.as_u64().filter(|a| *a > 0) {
            Some(a) => a,
            None => {
                return err(
                    StatusCode::BAD_REQUEST,
                    "amount_thh must be a positive integer",
                )
            }
        },
    };
    if amount_thh < s.min_redeem_thh {
        return err(
            StatusCode::BAD_REQUEST,
            format!("redeem at least {} THH", s.min_redeem_thh),
        );
    }
    let price = Price {
        amount_thh,
        description: format!(
            "{amount_thh} THH of hashrate to {} as {}",
            target.pool_url, target.worker
        ),
    };
    let cashu = match s.cashu_scheme() {
        Ok(c) => c,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let schemes: [&dyn Scheme; 1] = [&cashu];
    let raw = match paid::credential(&headers, &schemes) {
        None => return paid::payment_required(&price, &schemes),
        Some(Credential::Cashu(raw)) => raw,
    };
    if let Err(r) = cashu.accept(&raw, &price) {
        return r.into_response();
    }
    let wallet = match fresh_wallet(&s.internal_url).await {
        Ok(w) => w,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    // Same checks as the page before anything is spent: the face value
    // passed, now count only what the mint says is unspent.
    let checked = match inspect_token(&wallet, &raw).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    if checked.unspent_thh < amount_thh {
        return Rejection::Insufficient {
            have: checked.unspent_thh,
            need: amount_thh,
        }
        .into_response();
    }
    spend_for_hashrate(&s, &wallet, checked, &target).await
}

/// Where a redemption sends its hashrate.
pub(crate) struct RedeemTarget {
    pub pool_url: String,
    pub worker: String,
    pub password: Option<String>,
}

impl RedeemTarget {
    /// `{ pool_url, worker_name, worker_password? }` from a request body.
    #[allow(clippy::result_large_err)]
    fn from_json(v: &Value) -> Result<Self, Response> {
        let field = |k: &str| v[k].as_str().unwrap_or_default().trim().to_string();
        let pool_url = field("pool_url");
        let worker = field("worker_name");
        if upstream_for_pool_url(&pool_url).is_none() {
            return Err(err(
                StatusCode::BAD_REQUEST,
                "pool must be stratum+tcp://host:port or stratum+ssl://host:port",
            ));
        }
        if worker.is_empty() {
            return Err(err(StatusCode::BAD_REQUEST, "worker name is required"));
        }
        // Optional; the proxy sends `x` without one.
        let password = Some(field("worker_password")).filter(|p| !p.is_empty());
        Ok(Self {
            pool_url,
            worker,
            password,
        })
    }
}

/// Melt every unspent proof of `checked` as hashrate to `target`. The
/// melt quote comes first, so a refusal there leaves the token untouched;
/// a failure after the proofs are received hands them back as a new token.
async fn spend_for_hashrate(
    s: &CheckoutState,
    wallet: &Wallet,
    checked: CheckedToken,
    target: &RedeemTarget,
) -> Response {
    let amount = checked.unspent_thh;
    let pool_url = &target.pool_url;
    let mut request = json!({
        "pool_url": pool_url,
        "worker_name": target.worker,
        "amount_thh": amount,
        "nonce": hex::encode(rand::random::<[u8; 8]>()),
    });
    if let Some(pw) = &target.password {
        request["worker_password"] = json!(pw);
    }
    let request = request.to_string();
    let rid = redemption_id_for(&request);
    // Quote first: it needs no proofs, and a refusal here leaves the token
    // untouched.
    let quote = match wallet
        .melt_quote(PaymentMethod::Custom(HASHRATE.into()), request, None, None)
        .await
    {
        Ok(q) => q,
        Err(e) => return err(StatusCode::BAD_GATEWAY, format!("redeem quote failed: {e}")),
    };

    // Receive only the proofs the mint still counts as unspent, so a
    // partly spent token redeems what is left of it.
    let proofs = checked.unspent;
    let received = match wallet
        .receive_proofs(proofs, ReceiveOptions::default(), None, None)
        .await
    {
        Ok(a) => u64::from(a),
        Err(e) => {
            return err(
                StatusCode::BAD_REQUEST,
                format!(
                    "token rejected: {e}. If your wallet already claimed it, send a fresh token from the wallet"
                ),
            )
        }
    };
    if received != amount {
        let msg = format!("token held {received} THH, expected {amount}");
        return refund(s, wallet, StatusCode::BAD_REQUEST, msg).await;
    }

    let prepared = match wallet.prepare_melt(&quote.id, HashMap::new()).await {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("redeem failed: {e}");
            return refund(s, wallet, StatusCode::BAD_GATEWAY, msg).await;
        }
    };
    match prepared.confirm_prefer_async().await {
        Ok(MeltOutcome::Paid(_)) | Ok(MeltOutcome::Pending(_)) => {}
        Err(e) => {
            let msg = format!("redeem failed: {e}");
            return refund(s, wallet, StatusCode::BAD_GATEWAY, msg).await;
        }
    }
    tracing::info!(redemption = %rid, amount_thh = amount, %pool_url, "checkout redemption opened");
    Json(json!({
        "redemption_id": rid.to_string(),
        "amount_thh": amount,
        "status_url": format!("/checkout/api/redemption/{rid}"),
    }))
    .into_response()
}

/// Why a pasted token can't be redeemed. `code` is stable for the page;
/// `message` is shown to the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TokenIssue {
    Empty,
    NotToken,
    NotThh,
    WrongMint,
    Spent,
    Pending,
}

impl TokenIssue {
    fn code(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::NotToken => "not_token",
            Self::NotThh => "not_thh",
            Self::WrongMint => "wrong_mint",
            Self::Spent => "spent",
            Self::Pending => "pending",
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Empty => "Paste or scan a Cashu token.",
            Self::NotToken => "That isn't a Cashu token. Copy the cashuB… text from your wallet's send screen.",
            Self::NotThh => "This token isn't THH, so it can't be redeemed for hashrate here.",
            Self::WrongMint => "This token is from a different mint.",
            Self::Spent => "This token is already spent. If your wallet claimed it, send a fresh token from the wallet.",
            Self::Pending => "This token is in the middle of another redemption. Try again in a moment.",
        }
    }
}

#[derive(Debug)]
pub(crate) struct TokenProblem {
    issue: TokenIssue,
    detail: Option<String>,
}

impl TokenProblem {
    fn new(issue: TokenIssue) -> Self {
        Self { issue, detail: None }
    }
}

impl IntoResponse for TokenProblem {
    fn into_response(self) -> Response {
        let error = match &self.detail {
            Some(d) => format!("{} ({d})", self.issue.message()),
            None => self.issue.message().to_string(),
        };
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error, "code": self.issue.code() })),
        )
            .into_response()
    }
}

/// A token checked against the mint without spending it.
pub(crate) struct CheckedToken {
    pub total_thh: u64,
    pub unspent_thh: u64,
    pub unspent: cdk::nuts::Proofs,
    pub memo: Option<String>,
}

/// Parse a pasted token and ask the mint (NUT-07) which of its proofs are
/// still unspent. Nothing is spent. Errors other than a bad token (the mint
/// unreachable) come back as `Err` with a 502.
pub(crate) async fn inspect_token(wallet: &Wallet, raw: &str) -> Result<CheckedToken, Response> {
    let problem = |i| Err(TokenProblem::new(i).into_response());
    if raw.trim().is_empty() {
        return problem(TokenIssue::Empty);
    }
    let Some(text) = extract_token(raw) else {
        return problem(TokenIssue::NotToken);
    };
    let token = match Token::from_str(&text) {
        Ok(t) => t,
        Err(_) => return problem(TokenIssue::NotToken),
    };
    if !token.unit().as_ref().is_some_and(is_thh) {
        return problem(TokenIssue::NotThh);
    }
    // The token names the mint's public URL; resolve its proofs against our
    // keysets over loopback. Every keyset, not just the active ones, so a
    // token from before a rotation works.
    let keysets = wallet
        .get_mint_keysets(KeysetFilter::All)
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, e))?;
    let Ok(proofs) = token.proofs(&keysets) else {
        return problem(TokenIssue::WrongMint);
    };
    let states = wallet
        .check_proofs_spent(proofs.clone())
        .await
        .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("mint check failed: {e}")))?;
    let total_thh: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();
    let mut pending = false;
    let unspent: cdk::nuts::Proofs = proofs
        .into_iter()
        .zip(states)
        .filter(|(_, st)| {
            pending |= matches!(st.state, ProofSpentState::Pending);
            matches!(st.state, ProofSpentState::Unspent)
        })
        .map(|(p, _)| p)
        .collect();
    let unspent_thh = unspent.iter().map(|p| u64::from(p.amount)).sum();
    if unspent_thh == 0 {
        return problem(if pending { TokenIssue::Pending } else { TokenIssue::Spent });
    }
    Ok(CheckedToken {
        total_thh,
        unspent_thh,
        unspent,
        memo: token.memo().clone(),
    })
}

/// `POST /checkout/api/token` `{ token }`: what a pasted token is worth
/// here, checked with the mint but not spent.
pub(crate) async fn check_token(State(s): State<Arc<CheckoutState>>, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or_default();
    let wallet = match fresh_wallet(&s.internal_url).await {
        Ok(w) => w,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let c = match inspect_token(&wallet, v["token"].as_str().unwrap_or_default()).await {
        Ok(c) => c,
        Err(r) => return r,
    };
    let below_min = c.unspent_thh < s.min_redeem_thh;
    Json(json!({
        "amount_thh": c.unspent_thh,
        "total_thh": c.total_thh,
        "spent_thh": c.total_thh - c.unspent_thh,
        "memo": c.memo,
        "min_redeem_thh": s.min_redeem_thh,
        "redeemable": !below_min,
    }))
    .into_response()
}

/// Pull a Cashu token out of whatever was pasted or scanned: a bare
/// `cashuB…`, a `cashu:` URI, a wallet link such as Cashu.me's
/// `…#token=cashuB…`, or a token broken over several lines.
pub(crate) fn extract_token(input: &str) -> Option<String> {
    let s: String = input.chars().filter(|c| !c.is_whitespace()).collect();
    let start = ["cashuB", "cashuA"]
        .iter()
        .filter_map(|p| s.find(p))
        .min()?;
    // Tokens are base64url (V4) or base64 (V3): stop at the first other
    // character, such as a link's `&` or a trailing quote.
    let token: String = s[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+' | '/' | '='))
        .collect();
    (token.len() > 6).then_some(token)
}

fn is_thh(unit: &CurrencyUnit) -> bool {
    matches!(unit, CurrencyUnit::Custom(u) if u.eq_ignore_ascii_case("thh"))
}

/// A redeem failed after its token was received: hand the THH back as a
/// fresh token so it isn't stranded in the throwaway wallet.
async fn refund(s: &CheckoutState, wallet: &Wallet, status: StatusCode, msg: String) -> Response {
    let proofs = match wallet.get_unspent_proofs().await {
        Ok(p) if !p.is_empty() => p,
        _ => return err(status, msg),
    };
    let Ok(mint_url) = MintUrl::from_str(&s.public_url) else {
        return err(status, msg);
    };
    let token = Token::new(
        mint_url,
        proofs,
        Some("Hashu THH".into()),
        CurrencyUnit::Custom("thh".into()),
    )
    .to_string();
    tracing::warn!(error = %msg, "checkout redeem failed after receiving the token; returned a new one");
    (
        status,
        Json(json!({
            "error": format!("{msg}. Your THH is safe: the token box now holds a fresh token for it (the old one is spent)."),
            "refund_token": token,
        })),
    )
        .into_response()
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
            .mint(&quote_id, fewest_proofs(amount_thh), None)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_pasted_tokens() {
        let t = "cashuBo2FteBlodHRwczovL21pbnQ_aGFzaHUuaW8-YXVjdGho";
        assert_eq!(extract_token(t).as_deref(), Some(t));
        assert_eq!(extract_token(&format!("  {t}\n")).as_deref(), Some(t));
        assert_eq!(extract_token(&format!("cashu:{t}")).as_deref(), Some(t));
        assert_eq!(
            extract_token(&format!("web+cashu://{t}")).as_deref(),
            Some(t)
        );
        assert_eq!(
            extract_token(&format!("https://wallet.cashu.me/#token={t}&x=1")).as_deref(),
            Some(t)
        );
        let wrapped = format!("{}\n{}", &t[..20], &t[20..]);
        assert_eq!(extract_token(&wrapped).as_deref(), Some(t));
        assert_eq!(extract_token("hello"), None);
    }

    #[test]
    fn thh_unit_any_case() {
        assert!(is_thh(&CurrencyUnit::Custom("thh".into())));
        assert!(is_thh(&CurrencyUnit::Custom("THH".into())));
        assert!(!is_thh(&CurrencyUnit::Sat));
    }

    /// A whole mint in-process: THH unit, the `hashrate` melt and a fake
    /// Lightning rail, its API on a loopback port, and the checkout state
    /// the handlers run on.
    mod live {
        use super::super::*;
        use axum::http::HeaderValue;
        use cdk::mint::{MintBuilder, MintMeltLimits, UnitConfig};
        use hashu_mint::lightning::add_lightning_issuance;
        use hashu_mint::melt::HashrateMelter;
        use hashu_mint::payment::{thh_unit, ThhIssuer};
        use hashu_mint::pricing::{PricingError, ThhPricer};
        use hashu_proxy::ProxyMetrics;

        #[derive(Debug)]
        struct TenSatsPerThh;
        impl ThhPricer for TenSatsPerThh {
            fn price(&self, amount_thh: u64) -> Result<u64, PricingError> {
                Ok(amount_thh * 10)
            }
        }

        async fn start() -> Arc<CheckoutState> {
            let ledger = Arc::new(RedemptionLedger::new());
            let db = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
            let mut builder = MintBuilder::new(db.clone());
            builder
                .configure_unit(
                    thh_unit(),
                    UnitConfig {
                        amounts: crate::mint::DENOMS.to_vec(),
                        input_fee_ppk: 0,
                    },
                )
                .unwrap();
            builder
                .add_payment_processor(
                    thh_unit(),
                    PaymentMethod::Custom(HASHRATE.into()),
                    MintMeltLimits::new(1, 1000),
                    Arc::new(HashrateMelter::new(ledger.clone())),
                )
                .await
                .unwrap();
            // Sell uncapped: there are no rentals in a test.
            let capacity = Arc::new(hashu_mint::capacity::CapacityBook::new());
            capacity.set_limit(Some(false));
            let controls = Arc::new(MintControls::with_capacity(0, capacity));
            let pricer = Arc::new(ControlledPricer::new(
                Arc::new(TenSatsPerThh),
                controls.clone(),
            ));
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
            builder = add_lightning_issuance(builder, issuer, MintMeltLimits::new(1, 1000))
                .await
                .unwrap();
            let mut info = builder.current_mint_info();
            let hashrate = PaymentMethod::Custom(HASHRATE.into());
            info.nuts.nut04.methods.retain(|m| m.method != hashrate);
            info.nuts.nut05.methods.retain(|m| m.method == hashrate);
            builder = builder.with_mint_info(info);
            let seed: [u8; 64] = std::array::from_fn(|_| rand::random());
            let mint = Arc::new(builder.build_with_seed(db, &seed).await.unwrap());
            mint.start().await.unwrap();

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = cdk_axum::create_mint_router(
                mint.clone(),
                vec![HASHRATE.to_string(), "bolt11".into()],
            )
            .await
            .unwrap();
            tokio::spawn(async move { axum::serve(listener, app).await });

            let meter = Arc::new(HashrateMeter::new(
                Arc::new(ProxyMetrics::default()),
                ledger.clone(),
                Duration::from_secs(60),
            ));
            Arc::new(CheckoutState::new(
                url.clone(),
                url,
                controls,
                Some(pricer),
                None,
                vec!["bolt11".into()],
                1,
                1000,
                ledger,
                mint,
                meter,
            ))
        }

        async fn token(s: &CheckoutState, thh: u64) -> String {
            operator_test_mint(s, &s.mint, thh).await.unwrap().0
        }

        fn body(amount_thh: Option<u64>) -> Bytes {
            let mut v = json!({
                "pool_url": "stratum+tcp://pool.example:3333",
                "worker_name": "alice.rig1",
            });
            if let Some(a) = amount_thh {
                v["amount_thh"] = json!(a);
            }
            Bytes::from(v.to_string())
        }

        fn paying(token: &str) -> HeaderMap {
            let mut h = HeaderMap::new();
            h.insert(paid::X_CASHU, HeaderValue::from_str(token).unwrap());
            h
        }

        async fn call(
            s: &Arc<CheckoutState>,
            h: HeaderMap,
            b: Bytes,
        ) -> (StatusCode, HeaderMap, Value) {
            let r = paid_redeem(State(s.clone()), h, b).await;
            let (parts, body) = r.into_parts();
            let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
            (
                parts.status,
                parts.headers,
                serde_json::from_slice(&bytes).unwrap(),
            )
        }

        async fn unspent(s: &CheckoutState, t: &str) -> u64 {
            let w = fresh_wallet(&s.internal_url).await.unwrap();
            match inspect_token(&w, t).await {
                Ok(c) => c.unspent_thh,
                Err(_) => 0,
            }
        }

        #[tokio::test]
        async fn unpaid_request_gets_a_402_for_this_mint() {
            let s = start().await;
            let (code, h, v) = call(&s, HeaderMap::new(), body(Some(3))).await;
            assert_eq!(code, StatusCode::PAYMENT_REQUIRED, "{v}");
            let req =
                cdk::nuts::nut18::PaymentRequest::from_str(h["x-cashu"].to_str().unwrap()).unwrap();
            assert_eq!(req.amount, Some(Amount::from(3)));
            assert_eq!(req.unit, Some(thh_unit()));
            assert_eq!(req.mints, vec![MintUrl::from_str(&s.public_url).unwrap()]);
            assert!(req.transports.is_empty());
            // No amount: the smallest redemption.
            let (_, h, _) = call(&s, HeaderMap::new(), body(None)).await;
            let req =
                cdk::nuts::nut18::PaymentRequest::from_str(h["x-cashu"].to_str().unwrap()).unwrap();
            assert_eq!(req.amount, Some(Amount::from(1)));
            // A bad target is a 400 before any payment is asked for.
            let bad = Bytes::from(json!({ "pool_url": "nope", "worker_name": "w" }).to_string());
            assert_eq!(
                call(&s, HeaderMap::new(), bad).await.0,
                StatusCode::BAD_REQUEST
            );
        }

        #[tokio::test]
        async fn x_cashu_token_opens_the_redemption() {
            let s = start().await;
            let t = token(&s, 4).await;
            let (code, _, v) = call(&s, paying(&t), body(Some(4))).await;
            assert_eq!(code, StatusCode::OK, "{v}");
            assert_eq!(v["amount_thh"], 4);
            let rid = parse_id(v["redemption_id"].as_str().unwrap()).unwrap();
            let r = s.ledger.get(&rid).expect("redemption in the ledger");
            assert_eq!(r.amount_thh, 4);
            assert_eq!(r.pool_url, "stratum+tcp://pool.example:3333");
            assert_eq!(r.worker_name, "alice.rig1");
            assert_eq!(unspent(&s, &t).await, 0);
            // The same token again is spent: 400, no second redemption.
            let (code, _, v) = call(&s, paying(&t), body(Some(4))).await;
            assert_eq!(code, StatusCode::BAD_REQUEST, "{v}");
            assert_eq!(s.ledger.all().len(), 1);
        }

        #[tokio::test]
        async fn overpaying_redeems_the_whole_token() {
            let s = start().await;
            let t = token(&s, 6).await;
            let (code, _, v) = call(&s, paying(&t), body(Some(2))).await;
            assert_eq!(code, StatusCode::OK, "{v}");
            assert_eq!(v["amount_thh"], 6);
        }

        #[tokio::test]
        async fn short_or_foreign_tokens_get_400_and_stay_unspent() {
            let s = start().await;
            let t = token(&s, 2).await;
            let (code, _, v) = call(&s, paying(&t), body(Some(3))).await;
            assert_eq!(code, StatusCode::BAD_REQUEST);
            assert_eq!(v["code"], "insufficient");
            // Same proofs, other mint URL: not in `m`.
            let tok = Token::from_str(&t).unwrap();
            let w = fresh_wallet(&s.internal_url).await.unwrap();
            let keysets = w.get_mint_keysets(KeysetFilter::All).await.unwrap();
            let moved = Token::new(
                MintUrl::from_str("https://other.example").unwrap(),
                tok.proofs(&keysets).unwrap(),
                None,
                thh_unit(),
            )
            .to_string();
            let (code, _, v) = call(&s, paying(&moved), body(Some(1))).await;
            assert_eq!(code, StatusCode::BAD_REQUEST);
            assert_eq!(v["code"], "wrong_mint");
            assert_eq!(unspent(&s, &t).await, 2);
            assert!(s.ledger.all().is_empty());
        }
    }
}
