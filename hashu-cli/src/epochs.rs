//! Epoch-dated issuance (#126) on the operator's mint and every supplier
//! mint, on with `--epochs` (`HASHU_EPOCHS=true`).
//!
//! - [`run`] reads the chain tip every minute, rotates the TiH keyset at
//!   each difficulty epoch ([`hashu_mint::epoch`]), and sizes the live
//!   stock to what is left of the epoch: live issuance sells only the
//!   current epoch.
//! - [`router`] serves:
//!   - `GET /v1/hashu/epochs`: the current epoch, the next
//!     epochs the seller's horizon reaches, with their capacity and keysets, and which keysets
//!     are dated or perpetual.
//!   - `GET /v1/hashu/epochs/{epoch}/keys`: an epoch's public keys, also
//!     for epochs that have not started (to build outputs and check DLEQ).
//!   - `GET /v1/hashu/epochs/convert/{quote}`: a conversion melt's epoch,
//!     keyset and amount.
//!   - `POST /v1/hashu/epochs/convert/{quote}` `{"outputs": [...]}`: sign
//!     its outputs with that epoch's key.
//!
//!   - `POST /v1/hashu/epochs/{epoch}/mint/quote/{method}`: buy a future
//!     epoch (within the seller's horizon) over a payment rail. Same body and
//!     answer as `POST /v1/mint/quote/{method}`; the quote is checked
//!     against that epoch's capacity instead of the live stock. Pay it and
//!     follow it at the usual `GET /v1/mint/quote/{method}/{quote}`.
//!   - `POST /v1/hashu/epochs/mint/{quote}` `{"outputs": [...]}`: issue a
//!     paid future-epoch quote with that epoch's keyset. The standard
//!     `POST /v1/mint/{method}` refuses such a quote.
//!
//! Converting: melt the vouchers with method `epoch` and request
//! `{"epoch": "auto" | <n>, "amount_tih": <x>}` (`POST /v1/melt/quote/epoch`
//! then `POST /v1/melt/epoch`), then fetch and sign as above. `auto` goes to
//! the current epoch if it has room, else the next epoch that has; an
//! epoch number within the horizon buys into that epoch.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use cdk::cdk_database::MintKeysDatabase;
use cdk::nuts::{BlindedMessage, MeltQuoteState, PaymentMethod};
use cdk::Mint;
use hashu_core::epoch::{start_height, EpochClock, Supply, TARGET_BLOCK_SECS};
use hashu_mint::capacity::{now_ms, CapacityBook};
use hashu_mint::epoch::{conversion_id, public_keyset, Epochs, PlanInput, METHOD};
use hashu_mint::oracle::esplora::EsploraClient;
use hashu_mint::payment::tih_unit;
use serde::Deserialize;
use serde_json::{json, Value};

/// How often the chain tip is read.
const TICK: Duration = Duration::from_secs(60);

pub type KeysDb = Arc<dyn MintKeysDatabase<Err = cdk::cdk_database::Error> + Send + Sync>;

static OPERATOR: OnceLock<Arc<Epochs>> = OnceLock::new();

tokio::task_local! {
    /// Set while a future-epoch quote is created, so the rail's pricer
    /// skips the live-stock check.
    pub static FUTURE_EPOCH_QUOTE: ();
}

/// Make `epochs` the operator mint's, so its obligations count the TiH
/// signed outside cdk.
pub fn set_operator(epochs: Arc<Epochs>) {
    let _ = OPERATOR.set(epochs);
}

/// The operator mint's epochs, when on.
pub fn operator() -> Option<Arc<Epochs>> {
    OPERATOR.get().cloned()
}

/// TiH a mint's epochs signed outside cdk that cdk can now see spent
/// (epochs up to the current one): owed on top of cdk's count.
pub fn outside_cdk(epochs: &Epochs) -> u64 {
    epochs
        .clock()
        .map(|c| epochs.signed_outside_cdk(c.current()))
        .unwrap_or(0)
}

/// What the planner sells against: the live stock's average and the
/// running rentals, and everything owed now.
pub fn plan_inputs(capacity: Arc<CapacityBook>) -> hashu_mint::epoch::PlanInputs {
    Arc::new(move || {
        let now = now_ms();
        let s = capacity.snapshot(now);
        let mut supply = Vec::new();
        if s.live.on {
            supply.push(Supply {
                ths: s.live.stock.ewma_ths,
                hours_left: None,
            });
        }
        for r in s.rentals.iter().filter(|r| r.rental.end_ms > now) {
            supply.push(Supply {
                ths: r.rental.ths,
                hours_left: Some(r.hours_left),
            });
        }
        PlanInput {
            supply,
            carried_tih: s.owed_tih,
            horizon_hours: capacity
                .live_horizon_hours()
                .unwrap_or(hashu_mint::capacity::EPOCH_DEFAULT_HORIZON_HOURS),
        }
    })
}

/// The chain tip as an epoch clock, shared by every mint in the process
/// (the operator's and the supplier mints): one Esplora read a minute.
pub type ClockFeed = tokio::sync::watch::Receiver<Option<EpochClock>>;

/// Read the chain tip from `esplora` every minute into a [`ClockFeed`].
pub fn chain_clock(esplora: EsploraClient) -> ClockFeed {
    let (tx, rx) = tokio::sync::watch::channel(None);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            match esplora.tip_height().await {
                Ok(h) => {
                    if tx.send(Some(EpochClock::new(h))).is_err() {
                        return;
                    }
                }
                Err(e) => tracing::warn!(error = %e, "epochs: could not read the chain tip"),
            }
        }
    });
    rx
}

/// Follow the chain: date issuance to the current epoch. `on_tick` gets
/// each clock reading (to size the live stock to the epoch), `on_rotate`
/// runs after the keyset rotated.
pub async fn run(
    epochs: Arc<Epochs>,
    mint: Arc<Mint>,
    keys: KeysDb,
    mut clock: ClockFeed,
    on_tick: Arc<dyn Fn(EpochClock) + Send + Sync>,
    on_rotate: Arc<dyn Fn() + Send + Sync>,
) {
    let mut first = true;
    loop {
        // Wake on a new reading, and at least every minute so a rotation
        // that waited for open redemptions is retried.
        if !first {
            let _ = tokio::time::timeout(TICK, clock.changed()).await;
        }
        first = false;
        let reading = *clock.borrow_and_update();
        if let Some(c) = reading {
            epochs.set_clock(c);
        }
        let Some(now) = epochs.clock() else {
            continue;
        };
        on_tick(now);
        let open_redemption = match mint.melt_quotes().await {
            Ok(q) => q
                .iter()
                .any(|q| q.unit == tih_unit() && q.state == MeltQuoteState::Pending),
            Err(e) => {
                tracing::warn!(error = %e, "epochs: could not read melt quotes");
                true
            }
        };
        prune_purchases(&epochs, &mint).await;
        match epochs.sync(&mint, keys.as_ref(), !open_redemption).await {
            Ok(r) if !r.is_empty() => on_rotate(),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "epochs: keyset sync failed"),
        }
    }
}

/// Drop reservations whose quote expired unpaid (or is gone).
async fn prune_purchases(epochs: &Epochs, mint: &Mint) {
    let purchases = epochs.purchases();
    if purchases.is_empty() {
        return;
    }
    let Ok(quotes) = mint.mint_quotes().await else {
        return;
    };
    let now_s = now_ms() / 1000;
    for (id, p) in purchases {
        let q = quotes.iter().find(|q| q.id.to_string() == id);
        let unpaid =
            q.is_none_or(|q| q.amount_paid().value() == 0 && q.amount_issued().value() == 0);
        if unpaid && p.expiry_s < now_s {
            epochs.drop_purchase(&id);
        }
    }
}

#[derive(Clone)]
struct Ctx {
    epochs: Arc<Epochs>,
    mint: Arc<Mint>,
}

pub fn router(epochs: Arc<Epochs>, mint: Arc<Mint>) -> axum::Router {
    axum::Router::new()
        .route("/v1/hashu/epochs", get(index))
        .route("/v1/hashu/epochs/{epoch}/keys", get(keys))
        .route(
            "/v1/hashu/epochs/convert/{quote}",
            get(conversion).post(sign),
        )
        .route(
            "/v1/hashu/epochs/{epoch}/mint/quote/{method}",
            axum::routing::post(future_quote),
        )
        .route(
            "/v1/hashu/epochs/mint/{quote}",
            axum::routing::post(future_mint),
        )
        .layer(axum::middleware::from_fn(cors))
        .with_state(Ctx { epochs, mint })
}

/// Let web wallets on another origin (app.hashu.io) call these routes,
/// as cdk does for `/v1/*`: its CORS layer covers only its own routes.
async fn cors(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let res = if req.method() == axum::http::Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    with_cors(res)
}

/// Add the CORS headers cdk puts on its own `/v1/*` answers, for answers
/// made here in its place.
fn with_cors(mut res: Response) -> Response {
    use axum::http::{header, HeaderValue};
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("*"),
    );
    res
}

fn error(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({ "detail": detail.into() }))).into_response()
}

async fn index(State(c): State<Ctx>) -> Response {
    let Some(clock) = c.epochs.clock() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the epoch clock has not read the chain yet",
        );
    };
    let current = clock.current();
    let now_s = now_ms() / 1000;
    let rows = c.epochs.plan().unwrap_or_default();
    let last = c.epochs.last_epoch().unwrap_or(current);
    let epochs: Vec<Value> = (current..=last)
        .map(|e| {
            let row = rows.iter().find(|r| r.epoch == e);
            json!({
                "epoch": e,
                "start_height": start_height(e),
                "end_height": start_height(e + 1),
                "starts_at": now_s + (clock.hours_until(e) * 3600.0) as u64,
                "keyset_id": c.epochs.keyset_for(e).map(|k| k.id.to_string()),
                "current": e == current,
                "hours_in_window": row.map(|r| r.hours_in_window),
                "capacity_tih": row.map(|r| r.capacity_tih.floor() as u64),
                "owed_tih": row.map(|r| r.owed_tih.ceil() as u64),
                "free_tih": row.map(|r| r.free_tih.floor() as u64),
            })
        })
        .collect();
    let dated = c.epochs.keysets();
    let keysets: serde_json::Map<String, Value> = c
        .mint
        .keysets()
        .keysets
        .into_iter()
        .filter(|k| k.unit == tih_unit())
        .map(|k| {
            let id = k.id.to_string();
            let epoch = dated.get(&id).copied();
            (
                id,
                json!({ "epoch": epoch, "perpetual": epoch.is_none(), "active": k.active }),
            )
        })
        .collect();
    Json(json!({
        "tip_height": clock.tip_height,
        "epoch": current,
        "blocks_left": clock.blocks_left(),
        "ends_at": now_s + clock.blocks_left() * TARGET_BLOCK_SECS,
        "horizon_hours": c.epochs.horizon_hours(),
        "epochs": epochs,
        "keysets": keysets,
        "convert": {
            "method": METHOD,
            "request": { "epoch": "auto | <epoch>", "amount_tih": "<TiH>" },
        },
    }))
    .into_response()
}

async fn keys(State(c): State<Ctx>, Path(epoch): Path<u64>) -> Response {
    let Some(clock) = c.epochs.clock() else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "epoch clock not ready");
    };
    if epoch > c.epochs.last_epoch().unwrap_or(clock.current()) {
        return error(StatusCode::NOT_FOUND, "epoch beyond the horizon");
    }
    match c.epochs.keyset_for(epoch) {
        Some(k) => {
            let active = Some(epoch == clock.current());
            Json(json!({ "keysets": [public_keyset(&k, active)] })).into_response()
        }
        None => error(StatusCode::NOT_FOUND, "no keyset for that epoch"),
    }
}

/// The conversion behind paid `epoch` melt quote `quote`.
async fn lookup(c: &Ctx, quote: &str) -> Result<(String, hashu_mint::epoch::Conversion), Response> {
    let quotes = c
        .mint
        .melt_quotes()
        .await
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let q = quotes
        .into_iter()
        .find(|q| q.id.to_string() == quote)
        .filter(|q| q.payment_method == PaymentMethod::Custom(METHOD.into()))
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "no epoch conversion with that quote"))?;
    if q.state != MeltQuoteState::Paid {
        return Err(error(
            StatusCode::CONFLICT,
            format!("melt the quote first (state {})", q.state),
        ));
    }
    let id = conversion_id(&q.request.to_string());
    let conv = c
        .epochs
        .conversion(&id)
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "conversion not recorded"))?;
    Ok((id, conv))
}

async fn conversion(State(c): State<Ctx>, Path(quote): Path<String>) -> Response {
    match lookup(&c, &quote).await {
        Ok((_, conv)) => Json(json!({
            "quote": quote,
            "epoch": conv.epoch,
            "keyset_id": conv.keyset_id,
            "amount_tih": conv.amount_tih,
            "signatures": conv.signatures,
        }))
        .into_response(),
        Err(r) => r,
    }
}

#[derive(Deserialize)]
struct SignBody {
    outputs: Vec<BlindedMessage>,
}

async fn sign(
    State(c): State<Ctx>,
    Path(quote): Path<String>,
    Json(body): Json<SignBody>,
) -> Response {
    let (id, _) = match lookup(&c, &quote).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    match c.epochs.sign(&id, &body.outputs) {
        Ok(sigs) => Json(json!({ "signatures": sigs })).into_response(),
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

/// `POST /v1/hashu/epochs/{epoch}/mint/quote/{method}`.
async fn future_quote(
    State(c): State<Ctx>,
    Path((epoch, method)): Path<(u64, String)>,
    Json(payload): Json<Value>,
) -> Response {
    use cdk::nuts::{MintQuoteBolt11Request, MintQuoteBolt11Response, MintQuoteCustomRequest};
    let amount = payload["amount"].as_u64().unwrap_or(0);
    if amount == 0 {
        return error(StatusCode::BAD_REQUEST, "amount (TiH) is required");
    }
    if !payload["pubkey"].is_null() {
        return error(
            StatusCode::BAD_REQUEST,
            "future-epoch quotes can't be locked to a key (NUT-20)",
        );
    }
    // Refuse before billing anyone; checked again when reserved.
    if let Err(e) = c.epochs.reserve_check(epoch, amount) {
        return error(StatusCode::CONFLICT, e);
    }
    let created = FUTURE_EPOCH_QUOTE
        .scope((), async {
            if method == "bolt11" {
                let req: MintQuoteBolt11Request = serde_json::from_value(payload.clone())
                    .map_err(|e| format!("bad bolt11 quote request: {e}"))?;
                let q = c
                    .mint
                    .get_mint_quote(req.into())
                    .await
                    .map_err(|e| e.to_string())?;
                let r: MintQuoteBolt11Response<cdk::mint::QuoteId> =
                    q.try_into().map_err(|e: cdk::Error| e.to_string())?;
                Ok::<_, String>((
                    r.quote.to_string(),
                    r.expiry.unwrap_or(0),
                    serde_json::to_value(r).unwrap_or_default(),
                ))
            } else {
                let req: MintQuoteCustomRequest = serde_json::from_value(payload.clone())
                    .map_err(|e| format!("bad {method} quote request: {e}"))?;
                let q = c
                    .mint
                    .get_mint_quote(cdk::mint::MintQuoteRequest::Custom {
                        method: method.clone(),
                        request: req,
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                match q {
                    cdk::mint::MintQuoteResponse::Custom { response, .. } => Ok((
                        response.quote.to_string(),
                        response.expiry.unwrap_or(0),
                        serde_json::to_value(response).unwrap_or_default(),
                    )),
                    _ => Err(format!("unexpected answer for {method}")),
                }
            }
        })
        .await;
    let (id, expiry, body) = match created {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, e),
    };
    match c.epochs.reserve_purchase(&id, epoch, amount, expiry) {
        Ok(p) => {
            let mut body = body;
            body["epoch"] = json!(epoch);
            body["keyset_id"] = json!(p.dated.keyset_id);
            Json(body).into_response()
        }
        // Someone took the room in between; the quote is never paid.
        Err(e) => error(StatusCode::CONFLICT, e),
    }
}

/// `POST /v1/hashu/epochs/mint/{quote}`: issue a paid future-epoch quote.
async fn future_mint(
    State(c): State<Ctx>,
    Path(quote): Path<String>,
    Json(body): Json<SignBody>,
) -> Response {
    use cdk::nuts::MintQuoteState;
    let Some(p) = c.epochs.purchase(&quote) else {
        return error(
            StatusCode::NOT_FOUND,
            "no future-epoch purchase with that quote",
        );
    };
    let Ok(qid) = quote.parse::<cdk::mint::QuoteId>() else {
        return error(StatusCode::BAD_REQUEST, "bad quote id");
    };
    let total: u64 = body.outputs.iter().map(|o| u64::from(o.amount)).sum();
    let db = c.mint.localstore();
    let result: Result<Vec<cdk::nuts::BlindSignature>, (StatusCode, String)> = async {
        let internal =
            |e: cdk::cdk_database::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string());
        let mut tx = db.begin_transaction().await.map_err(internal)?;
        let mut q = tx
            .get_mint_quote(&qid)
            .await
            .map_err(internal)?
            .ok_or((StatusCode::NOT_FOUND, "unknown quote".to_string()))?;
        match q.state() {
            MintQuoteState::Unpaid => {
                return Err((StatusCode::CONFLICT, "the quote is not paid yet".into()))
            }
            MintQuoteState::Issued => {
                // A retry: the same outputs get their signatures back.
                return c
                    .epochs
                    .sign_purchase(&quote, &body.outputs, true)
                    .map_err(|e| (StatusCode::BAD_REQUEST, e));
            }
            MintQuoteState::Paid => {}
        }
        let mintable = q.amount_mintable().value();
        if total != mintable || total != p.dated.amount_tih {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("outputs add up to {total} TiH; the quote is for {mintable} TiH"),
            ));
        }
        let sigs = c
            .epochs
            .sign_purchase(&quote, &body.outputs, false)
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
        q.add_issuance(cdk::Amount::new(total, tih_unit()))
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        tx.update_mint_quote(&mut q).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        Ok(sigs)
    }
    .await;
    match result {
        Ok(sigs) => {
            tracing::info!(quote = %quote, epoch = p.dated.epoch, amount_tih = total, "future epoch issued");
            Json(json!({ "signatures": sigs })).into_response()
        }
        Err((status, e)) => error(status, e),
    }
}

/// Refuse the standard `POST /v1/mint/{method}` for a future-epoch quote:
/// it would sign the live keyset for TiH sold against a later epoch.
pub async fn guard(
    State(epochs): State<Arc<Epochs>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path();
    let is_mint = req.method() == axum::http::Method::POST
        && path.starts_with("/v1/mint/")
        && !path.starts_with("/v1/mint/quote");
    if !is_mint || epochs.purchases().is_empty() {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 20).await else {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "request too large");
    };
    if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
        let mut ids: Vec<&str> = v["quote"].as_str().into_iter().collect();
        if let Some(qs) = v["quotes"].as_array() {
            ids.extend(
                qs.iter()
                    .filter_map(|q| q.as_str().or_else(|| q["quote"].as_str())),
            );
        }
        if let Some(id) = ids.into_iter().find(|id| epochs.purchase(id).is_some()) {
            return with_cors(error(
                StatusCode::BAD_REQUEST,
                format!("quote {id} buys a future epoch; issue it at /v1/hashu/epochs/mint/{id}"),
            ));
        }
    }
    next.run(axum::extract::Request::from_parts(
        parts,
        axum::body::Body::from(bytes),
    ))
    .await
}

/// List the future epochs the horizon reaches as inactive keysets in
/// `GET /v1/keysets`, and serve their keys at `GET /v1/keys/{id}`, so
/// wallets can recognise a future-epoch voucher. cdk itself still does not
/// know them, so the mint refuses to swap or redeem them until their epoch
/// starts.
pub async fn list_future_keysets(
    State(epochs): State<Arc<Epochs>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if req.method() != axum::http::Method::GET {
        return next.run(req).await;
    }
    let path = req.uri().path().to_string();
    let future: Vec<_> = epochs
        .future_epochs()
        .filter_map(|e| epochs.keyset_for(e))
        .collect();
    if let Some(id) = path.strip_prefix("/v1/keys/") {
        if let Some(k) = future.iter().find(|k| k.id.to_string() == id) {
            return with_cors(
                Json(json!({ "keysets": [public_keyset(k, Some(false))] })).into_response(),
            );
        }
        return next.run(req).await;
    }
    if path != "/v1/keysets" {
        return next.run(req).await;
    }
    let res = next.run(req).await;
    if !res.status().is_success() {
        return res;
    }
    let (parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 1 << 22).await else {
        return error(StatusCode::INTERNAL_SERVER_ERROR, "keysets");
    };
    let Ok(mut v) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    };
    if let Some(list) = v["keysets"].as_array_mut() {
        for k in &future {
            let id = k.id.to_string();
            if !list.iter().any(|x| x["id"] == id.as_str()) {
                list.push(json!({
                    "id": id,
                    "unit": k.unit.to_string(),
                    "active": false,
                    "input_fee_ppk": k.input_fee_ppk,
                }));
            }
        }
    }
    // Keep cdk's headers (CORS for web wallets) but not its length.
    let mut res = Json(v).into_response();
    *res.status_mut() = parts.status;
    for (name, value) in parts.headers.iter() {
        if name != axum::http::header::CONTENT_LENGTH {
            res.headers_mut().insert(name.clone(), value.clone());
        }
    }
    res
}
