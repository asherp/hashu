//! Mint operator GUI and its API, served on the control port (issue #3).
//!
//! `hashu mint run` serves this on `--control-bind` (127.0.0.1:3334 by
//! default) instead of the bare proxy control server. Everything the proxy
//! control API served still answers the same way (`GET /status`,
//! `POST /redemptions`), so the MDK app keeps working. Added:
//!
//! - `GET /`: the operator page (`operator.html`, no external assets).
//! - `GET /api/operator`: mint state, price, obligations, redemptions with
//!   progress, proxy counters and the current hashrate.
//! - `GET /api/hashrate?since=<unix ms>`: the hashrate series.
//! - `GET /api/track?id=<redemption id>`: one redemption's delivered work
//!   over time (the current redemption without `id`).
//! - `POST /api/controls`: `{ "issuance_paused": bool, "markup_bps": int }`,
//!   either field optional. Pausing makes every rail refuse new mint
//!   quotes; redemptions and the proxy keep running.
//!
//! The controls change what the mint sells, so like `POST /redemptions` they
//! trust whoever can reach the port: keep `--control-bind` on loopback.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use cdk::mint::Mint;
use hashu_core::redemption::{RedemptionState, HASHES_PER_THH};
use hashu_mint::payment::thh_unit;
use hashu_mint::pricing::{PricingError, ThhPricer};
use hashu_proxy::control::{route as control_route, ControlState};
use hashu_proxy::meter::HashrateMeter;
use serde_json::{json, Value};

const PAGE: &str = include_str!("operator.html");

/// Lowest markup the GUI accepts: −90%, so a price can never reach zero.
pub const MIN_MARKUP_BPS: i64 = -9_000;
/// Highest markup the GUI accepts: +1000%.
pub const MAX_MARKUP_BPS: i64 = 100_000;

/// Live mint settings the operator can change while the daemon runs.
#[derive(Debug)]
pub struct MintControls {
    issuance_paused: AtomicBool,
    markup_bps: AtomicI64,
}

impl MintControls {
    pub fn new(markup_bps: i64) -> Self {
        Self {
            issuance_paused: AtomicBool::new(false),
            markup_bps: AtomicI64::new(markup_bps.clamp(MIN_MARKUP_BPS, MAX_MARKUP_BPS)),
        }
    }
    pub fn issuance_paused(&self) -> bool {
        self.issuance_paused.load(Ordering::Relaxed)
    }
    pub fn set_issuance_paused(&self, paused: bool) {
        self.issuance_paused.store(paused, Ordering::Relaxed);
    }
    pub fn markup_bps(&self) -> i64 {
        self.markup_bps.load(Ordering::Relaxed)
    }
    pub fn set_markup_bps(&self, bps: i64) -> Result<(), String> {
        if !(MIN_MARKUP_BPS..=MAX_MARKUP_BPS).contains(&bps) {
            return Err(format!(
                "markup_bps must be between {MIN_MARKUP_BPS} and {MAX_MARKUP_BPS}"
            ));
        }
        self.markup_bps.store(bps, Ordering::Relaxed);
        Ok(())
    }
}

/// A rail's pricer under operator control: refuses while issuance is
/// paused, and applies the live markup on top of the base price.
pub struct ControlledPricer {
    inner: Arc<dyn ThhPricer>,
    controls: Arc<MintControls>,
}

impl fmt::Debug for ControlledPricer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlledPricer")
            .field("inner", &self.inner)
            .field("controls", &self.controls)
            .finish()
    }
}

impl ControlledPricer {
    pub fn new(inner: Arc<dyn ThhPricer>, controls: Arc<MintControls>) -> Self {
        Self { inner, controls }
    }

    /// What `amount_thh` would cost now, ignoring a pause.
    pub fn quote(&self, amount_thh: u64) -> Result<u64, PricingError> {
        let base = self.inner.price(amount_thh)?;
        let factor = 1.0 + self.controls.markup_bps() as f64 / 10_000.0;
        let price = (base as f64 * factor).ceil();
        if !price.is_finite() || price >= u64::MAX as f64 {
            return Err(PricingError::Overflow(amount_thh));
        }
        // Never bill zero: that would mint for free.
        Ok((price as u64).max(1))
    }
}

impl ThhPricer for ControlledPricer {
    fn price(&self, amount_thh: u64) -> Result<u64, PricingError> {
        if self.controls.issuance_paused() {
            return Err(PricingError::Paused);
        }
        self.quote(amount_thh)
    }
}

/// Facts about the daemon that do not change while it runs.
#[derive(Debug, Clone, Default)]
pub struct DaemonInfo {
    pub name: String,
    pub mint_listen: String,
    pub mint_url: Option<String>,
    pub stratum_bind: String,
    pub rails: Vec<String>,
    pub min_redeem_thh: u64,
    pub max_thh: u64,
}

/// Everything the operator API reads.
pub struct OperatorState {
    pub mint: Arc<Mint>,
    pub control: ControlState,
    pub meter: Arc<HashrateMeter>,
    pub controls: Arc<MintControls>,
    /// Lightning price, in sats.
    pub sats: Option<Arc<ControlledPricer>>,
    /// Tether price, in micro-USDT.
    pub usdt: Option<Arc<ControlledPricer>>,
    pub info: DaemonInfo,
    pub started_ms: u64,
}

pub fn router(state: Arc<OperatorState>) -> axum::Router {
    axum::Router::new()
        .route("/", get(page))
        .route("/api/operator", get(operator))
        .route("/api/hashrate", get(hashrate))
        .route("/api/track", get(track))
        .route("/api/controls", post(set_controls))
        .fallback(proxy_control)
        .with_state(state)
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

/// Serve the proxy control API's routes unchanged.
async fn proxy_control(
    State(s): State<Arc<OperatorState>>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    let (code, v) = control_route(method.as_str(), uri.path(), &body, &s.control);
    let code = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, Json(v)).into_response()
}

/// Value of `key` in the query string.
fn query_param<'a>(uri: &'a Uri, key: &str) -> Option<&'a str> {
    uri.query()?.split('&').find_map(|kv| {
        kv.split_once('=')
            .filter(|(k, _)| *k == key)
            .map(|(_, v)| v)
    })
}

/// `GET /api/track?id=<redemption id>`: that redemption's delivered work
/// over time; without `id`, the current one.
async fn track(State(s): State<Arc<OperatorState>>, uri: Uri) -> Response {
    let t = match query_param(&uri, "id") {
        Some(id) => s.meter.tracks().into_iter().find(|t| t.id == id),
        None => s.meter.current_track(),
    };
    match t {
        Some(t) => Json(json!(t)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "no such redemption" })),
        )
            .into_response(),
    }
}

async fn hashrate(State(s): State<Arc<OperatorState>>, uri: Uri) -> Json<Value> {
    let since = query_param(&uri, "since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    Json(json!({
        "window_s": s.meter.window().as_secs_f64(),
        "samples": s.meter.samples_since(since),
    }))
}

async fn set_controls(State(s): State<Arc<OperatorState>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad(format!("invalid JSON: {e}")),
    };
    if let Some(bps) = v.get("markup_bps").filter(|x| !x.is_null()) {
        let Some(bps) = bps.as_i64() else {
            return bad("markup_bps must be an integer".into());
        };
        if let Err(e) = s.controls.set_markup_bps(bps) {
            return bad(e);
        }
        tracing::info!(markup_bps = bps, "operator changed markup");
    }
    if let Some(p) = v.get("issuance_paused").filter(|x| !x.is_null()) {
        let Some(p) = p.as_bool() else {
            return bad("issuance_paused must be true or false".into());
        };
        s.controls.set_issuance_paused(p);
        tracing::info!(
            paused = p,
            "operator {} issuance",
            if p { "paused" } else { "resumed" }
        );
    }
    Json(controls_json(&s)).into_response()
}

fn bad(msg: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn controls_json(s: &OperatorState) -> Value {
    json!({
        "issuance_paused": s.controls.issuance_paused(),
        "markup_bps": s.controls.markup_bps(),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

async fn operator(State(s): State<Arc<OperatorState>>) -> Response {
    match operator_json(&s).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn operator_json(s: &OperatorState) -> anyhow::Result<Value> {
    // ---- supply -------------------------------------------------------
    // keyset_amounts counts every signature and every spent proof, swaps
    // included; swaps add equally to both, so the difference is the THH
    // ecash still in holders' hands.
    let thh_keysets: Vec<_> = s
        .mint
        .keysets()
        .keysets
        .into_iter()
        .filter(|k| k.unit == thh_unit())
        .map(|k| k.id)
        .collect();
    let issued = s.mint.total_issued().await?;
    let redeemed = s.mint.total_redeemed().await?;
    let sum = |m: &std::collections::HashMap<_, cdk::Amount>| -> u64 {
        thh_keysets
            .iter()
            .filter_map(|id| m.get(id))
            .map(|a| u64::from(*a))
            .sum()
    };
    let (signed, spent) = (sum(&issued), sum(&redeemed));
    // A hashrate melt stays pending until its redemption is filled, and its
    // inputs only count as spent then. Those THH are owed as an open
    // redemption, so take them out of circulation to avoid counting twice.
    let melting: u64 = s
        .mint
        .melt_quotes()
        .await?
        .iter()
        .filter(|q| q.unit == thh_unit() && q.state == cdk::nuts::MeltQuoteState::Pending)
        .map(|q| q.amount().value())
        .sum();
    let in_circulation = signed.saturating_sub(spent).saturating_sub(melting);

    // THH sold over each rail, from the mint quotes.
    let mut sold: BTreeMap<String, u64> = BTreeMap::new();
    let mut paid_not_minted = 0u64;
    let mut quotes_open = 0u64;
    for q in s.mint.mint_quotes().await? {
        if q.unit != thh_unit() {
            continue;
        }
        let issued = q.amount_issued().value();
        let paid = q.amount_paid().value();
        *sold.entry(q.payment_method.to_string()).or_default() += issued;
        paid_not_minted += paid.saturating_sub(issued);
        if paid == 0 {
            quotes_open += 1;
        }
    }
    let sold_total: u64 = sold.values().sum();

    // ---- redemptions --------------------------------------------------
    let tracks = s.meter.tracks();
    let mut open_remaining_thh = 0.0;
    let mut open_count = 0;
    let redemptions: Vec<Value> = s
        .control
        .ledger
        .all()
        .iter()
        .map(|r| {
            let id = r.id.to_string();
            let filled = r.accumulated_hashes as f64 / HASHES_PER_THH as f64;
            let remaining = (r.amount_thh as f64 - filled).max(0.0);
            if r.state != RedemptionState::Completed {
                open_remaining_thh += remaining;
                open_count += 1;
            }
            let t = tracks.iter().find(|t| t.id == id);
            json!({
                "id": id,
                "pool_url": r.pool_url,
                "worker_name": r.worker_name,
                "amount_thh": r.amount_thh,
                "filled_thh": filled,
                "remaining_thh": remaining,
                "progress": if r.target_hashes == 0 { 1.0 } else {
                    (r.accumulated_hashes as f64 / r.target_hashes as f64).min(1.0)
                },
                "state": match r.state {
                    RedemptionState::Pending => "pending",
                    RedemptionState::Active => "active",
                    RedemptionState::Completed => "completed",
                },
                "opened_ms": t.map(|t| t.opened_ms),
                "completed_ms": t.and_then(|t| t.completed_ms),
            })
        })
        .collect();

    // ---- price --------------------------------------------------------
    let sats_per_thh = s.sats.as_ref().and_then(|p| p.quote(1).ok());
    let usdt_per_thh = s
        .usdt
        .as_ref()
        .and_then(|p| p.quote(1_000_000).ok())
        .map(|micro_per_million_thh| micro_per_million_thh as f64 / 1e12);

    let m = s.control.metrics.snapshot();
    let latest = s.meter.latest();
    Ok(json!({
        "now_ms": now_ms(),
        "started_ms": s.started_ms,
        "mint": {
            "name": s.info.name,
            "listen": s.info.mint_listen,
            "url": s.info.mint_url,
            "rails": s.info.rails,
            "min_redeem_thh": s.info.min_redeem_thh,
            "max_thh": s.info.max_thh,
            "controls": controls_json(s),
            "price": {
                "sats_per_thh": sats_per_thh,
                "usdt_per_thh": usdt_per_thh,
            },
        },
        "obligations": {
            "thh_sold": sold_total,
            "sold_by_rail": sold,
            "paid_not_minted_thh": paid_not_minted,
            "unpaid_quotes": quotes_open,
            "thh_in_circulation": in_circulation,
            "open_redemptions": open_count,
            "open_redemption_remaining_thh": open_remaining_thh,
            "total_thh_owed": in_circulation as f64 + paid_not_minted as f64 + open_remaining_thh,
            "signed_thh": signed,
            "spent_thh": spent,
            "melting_thh": melting,
        },
        "proxy": {
            "stratum_bind": s.info.stratum_bind,
            "default_upstream": s.control.default_upstream,
            "metrics": {
                "connections_accepted": m.connections_accepted,
                "shares_submitted": m.shares_submitted,
                "shares_accepted": m.shares_accepted,
                "shares_rejected": m.shares_rejected,
                "shares_credited": m.shares_credited,
            },
            "hashrate_hs": latest.map(|l| l.hashrate_hs).unwrap_or(0.0),
            "total_hashes": latest.map(|l| l.total_hashes).unwrap_or(0.0),
            "window_s": s.meter.window().as_secs_f64(),
        },
        "redemptions": redemptions,
        "current": s.meter.current_track(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Flat(u64);
    impl ThhPricer for Flat {
        fn price(&self, thh: u64) -> Result<u64, PricingError> {
            Ok(self.0 * thh)
        }
    }

    #[test]
    fn markup_applies_and_pause_refuses() {
        let c = Arc::new(MintControls::new(0));
        let p = ControlledPricer::new(Arc::new(Flat(40)), c.clone());
        assert_eq!(p.price(2).unwrap(), 80);
        c.set_markup_bps(2_500).unwrap();
        assert_eq!(p.price(2).unwrap(), 100);
        c.set_issuance_paused(true);
        assert_eq!(p.price(2), Err(PricingError::Paused));
        // The GUI still shows what it would cost.
        assert_eq!(p.quote(2).unwrap(), 100);
        c.set_issuance_paused(false);
        assert_eq!(p.price(1).unwrap(), 50);
    }

    #[test]
    fn markup_is_bounded_and_never_bills_zero() {
        let c = Arc::new(MintControls::new(0));
        assert!(c.set_markup_bps(-10_000).is_err());
        assert!(c.set_markup_bps(MAX_MARKUP_BPS + 1).is_err());
        c.set_markup_bps(MIN_MARKUP_BPS).unwrap();
        let p = ControlledPricer::new(Arc::new(Flat(1)), c);
        assert_eq!(p.price(1).unwrap(), 1);
    }
}
