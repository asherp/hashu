//! Epoch-dated issuance on the operator's mint (#126), on with
//! `--epochs` (`HASHU_EPOCHS=true`).
//!
//! - [`run`] reads the chain tip every minute, rotates the TiH keyset at
//!   each difficulty epoch ([`hashu_mint::epoch`]), and sizes the live
//!   stock to what is left of the epoch: live issuance sells only the
//!   current epoch.
//! - [`router`] serves:
//!   - `GET /v1/hashu/epochs`: the current epoch, the next
//!     [`LOOKAHEAD`] with their capacity and keysets, and which keysets
//!     are dated or perpetual.
//!   - `GET /v1/hashu/epochs/{epoch}/keys`: an epoch's public keys, also
//!     for epochs that have not started (to build outputs and check DLEQ).
//!   - `GET /v1/hashu/epochs/convert/{quote}`: a conversion melt's epoch,
//!     keyset and amount.
//!   - `POST /v1/hashu/epochs/convert/{quote}` `{"outputs": [...]}`: sign
//!     its outputs with that epoch's key.
//!
//! Converting: melt the vouchers with method `epoch` and request
//! `{"epoch": "auto" | <n>, "amount_tih": <x>}` (`POST /v1/melt/quote/epoch`
//! then `POST /v1/melt/epoch`), then fetch and sign as above. `auto` goes to
//! the current epoch if it has room, else the next epoch that has; an
//! epoch number up to [`LOOKAHEAD`] ahead buys into that epoch.

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
use hashu_mint::epoch::{conversion_id, public_keyset, Epochs, LOOKAHEAD, METHOD};
use hashu_mint::oracle::esplora::EsploraClient;
use hashu_mint::payment::tih_unit;
use serde::Deserialize;
use serde_json::{json, Value};

/// How often the chain tip is read.
const TICK: Duration = Duration::from_secs(60);

pub type KeysDb = Arc<dyn MintKeysDatabase<Err = cdk::cdk_database::Error> + Send + Sync>;

static OPERATOR: OnceLock<Arc<Epochs>> = OnceLock::new();

/// Make `epochs` the operator mint's, so its obligations count the TiH
/// signed outside cdk.
pub fn set_operator(epochs: Arc<Epochs>) {
    let _ = OPERATOR.set(epochs);
}

/// TiH the operator's epochs signed outside cdk that cdk can now see
/// spent (epochs up to the current one): owed on top of cdk's count.
pub fn operator_outside_cdk() -> u64 {
    OPERATOR
        .get()
        .and_then(|e| Some(e.signed_outside_cdk(e.clock()?.current())))
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
        (supply, s.owed_tih)
    })
}

/// Follow the chain: date issuance to the current epoch.
pub async fn run(
    epochs: Arc<Epochs>,
    mint: Arc<Mint>,
    keys: KeysDb,
    esplora: EsploraClient,
    capacity: Arc<CapacityBook>,
    on_rotate: Arc<dyn Fn() + Send + Sync>,
) {
    let mut tick = tokio::time::interval(TICK);
    loop {
        tick.tick().await;
        match esplora.tip_height().await {
            Ok(h) => epochs.set_clock(EpochClock::new(h)),
            Err(e) => tracing::warn!(error = %e, "epochs: could not read the chain tip"),
        }
        let Some(clock) = epochs.clock() else {
            continue;
        };
        capacity.set_epoch(clock.current(), clock.hours_left());
        let open_redemption = match mint.melt_quotes().await {
            Ok(q) => q
                .iter()
                .any(|q| q.unit == tih_unit() && q.state == MeltQuoteState::Pending),
            Err(e) => {
                tracing::warn!(error = %e, "epochs: could not read melt quotes");
                true
            }
        };
        match epochs.sync(&mint, keys.as_ref(), !open_redemption).await {
            Ok(r) if !r.is_empty() => on_rotate(),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "epochs: keyset sync failed"),
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
        .with_state(Ctx { epochs, mint })
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
    let rows = c.epochs.plan(LOOKAHEAD).unwrap_or_default();
    let epochs: Vec<Value> = (current..=current + LOOKAHEAD)
        .map(|e| {
            let row = rows.iter().find(|r| r.epoch == e);
            json!({
                "epoch": e,
                "start_height": start_height(e),
                "end_height": start_height(e + 1),
                "starts_at": now_s + (clock.hours_until(e) * 3600.0) as u64,
                "keyset_id": c.epochs.keyset_for(e).map(|k| k.id.to_string()),
                "current": e == current,
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
        "lookahead": LOOKAHEAD,
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
    if epoch > clock.current() + LOOKAHEAD {
        return error(StatusCode::NOT_FOUND, "epoch too far ahead");
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
