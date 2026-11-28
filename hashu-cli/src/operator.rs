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
//! - `GET /api/hashrate?since=<unix ms>`: the hashrate series, and when
//!   each rig was connected (`sessions`, recorded since `sessions_since_ms`).
//! - `GET /api/track?id=<redemption id>`: one redemption's delivered work
//!   over time (the current redemption without `id`).
//! - `POST /api/controls`: `{ "issuance_paused": bool, "markup_bps": int,
//!   "heat_j_per_th": number }`, every field optional. Pausing makes every
//!   rail refuse new mint quotes; redemptions and the proxy keep running.
//!   `heat_j_per_th` is the J/TH assumed for rigs not named after a model.
//! - `POST /api/test-mint` `{ "amount_tih": n }`: mint test TiH without a
//!   payment and return the token, to rehearse a redemption on a live mint.
//!   It counts as issued ecash (an obligation) like any other TiH.
//! - `POST /api/test-redeem` `{ token, pool_url, worker_name }`: redeem it.
//! - `POST /api/inbox/send-hashes` `{ "peer", "amount_tih", "note"? }`: mint
//!   hashes the same way and send them to `peer` (npub or hex) in a NIP-17
//!   direct message from the mint whose content is a NUT-18 payment payload
//!   `{mint, unit, memo, proofs}`, the note as its memo. Cashu wallets that
//!   take NUT-18 payments over Nostr (Cashu.me, hashu.me) receive it as an
//!   incoming payment. If the message cannot be sent the hashes come back
//!   as a `cashuB` token in the error so they are not lost.
//! - `GET /api/capacity`: rented hashrate, TiH it can still deliver, TiH
//!   owed and TiH available to sell (see `hashu_mint::capacity`).
//! - `GET /api/operator` also carries `heat`: the heat the hashrate turned
//!   into, estimated as work × J/TH (see `hashu_mint::heat`).
//! - `GET /api/mrr`: whether MRR rental sync is connected, from env or the
//!   GUI, the key's last 4 characters and the rental filter (never the
//!   secret). `POST /api/mrr`: `{ "key", "secret", "rental_ids"? }`
//!   connects after one test call to MRR, `{ "rental_ids" }` changes the
//!   filter, `{ "disconnect": true }` stops. Saved to `mrr.json` (0600) and
//!   preferred over `MRR_API_KEY` on restart (see `crate::mrr_sync`).
//! - `POST /api/capacity`: `{ "rentals": [{ "ths", "hours" | "end_ms",
//!   "label"?, "id"? }], "limit": bool | null, "live_horizon_hours":
//!   hours | null }`, every field optional. `rentals` replaces the
//!   operator's time-limited hashrate (the GUI's list on the capacity card;
//!   MRR rentals sync on their own): its TH/s come out of the live average
//!   at once and it sells only until it ends; `limit: null` goes back to
//!   automatic.
//!   `live_horizon_hours` sells a stock backed by the connected rigs
//!   (above what rentals declare): topped up every hour to their
//!   EWMA hashrate (half-life horizon / 2) × the horizon, drawn down by
//!   sales in between (#92); `null` turns it off. `"restock_live": true`
//!   restocks now instead of waiting for the hour.
//!
//! `POST /api/controls` settings are saved to `operator-settings.json` in the
//! data dir and capacity to `capacity.json`, so both survive a restart; a
//! saved value wins over its command-line flag.
//!
//! The controls change what the mint sells, so like `POST /redemptions` they
//! trust whoever can reach the port: keep `--control-bind` on loopback.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use cdk::mint::Mint;
use hashu_core::denom::{sats_per_th_day, TIH_PER_PIH};
use hashu_core::redemption::{RedemptionLedger, HASHES_PER_TIH};
use hashu_mint::capacity::{rental_from_json, CapacityBook};
use hashu_mint::heat;
use hashu_mint::payment::tih_unit;
use hashu_mint::pricing::{PricingError, TihPricer};
use hashu_proxy::control::{route as control_route, ControlState};
use hashu_proxy::meter::HashrateMeter;
use serde_json::{json, Value};

use crate::checkout::{self, CheckoutState};
use hashu_proxy::reliability::OPERATOR_MINT;

const PAGE: &str = include_str!("operator.html");
/// The page with the checkout's amount picker inlined.
static PAGE_HTML: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::checkout::with_amount_js(PAGE));

/// Lowest markup the GUI accepts: −90%, so a price can never reach zero.
pub const MIN_MARKUP_BPS: i64 = -9_000;
/// Highest markup the GUI accepts: +1000%.
pub const MAX_MARKUP_BPS: i64 = 100_000;
/// Range of miner efficiencies the heat estimate accepts, J/TH.
pub const HEAT_J_PER_TH_RANGE: std::ops::RangeInclusive<f64> = 1.0..=500.0;

/// Live mint settings the operator can change while the daemon runs.
#[derive(Debug)]
pub struct MintControls {
    issuance_paused: AtomicBool,
    markup_bps: AtomicI64,
    /// Default J/TH for the heat estimate, as f64 bits.
    heat_j_per_th: AtomicU64,
    /// Rented hashrate and what is owed against it: caps issuance.
    pub capacity: Arc<CapacityBook>,
    /// Where the settings above are saved (see [`MintControls::persist_to`]).
    /// The lock also serialises writes of the file.
    settings_path: Mutex<Option<PathBuf>>,
}

/// The operator settings saved to `operator-settings.json` in the data dir,
/// so a restart keeps what the operator set in the GUI. Capacity (rentals,
/// limit) is saved separately in `capacity.json` by [`CapacityBook`].
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SavedSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    issuance_paused: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    markup_bps: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    heat_j_per_th: Option<f64>,
}

impl MintControls {
    pub fn new(markup_bps: i64) -> Self {
        Self::with_capacity(markup_bps, Arc::new(CapacityBook::new()))
    }
    pub fn with_capacity(markup_bps: i64, capacity: Arc<CapacityBook>) -> Self {
        Self {
            issuance_paused: AtomicBool::new(false),
            markup_bps: AtomicI64::new(markup_bps.clamp(MIN_MARKUP_BPS, MAX_MARKUP_BPS)),
            heat_j_per_th: AtomicU64::new(heat::DEFAULT_J_PER_TH.to_bits()),
            capacity,
            settings_path: Mutex::new(None),
        }
    }

    /// Load the settings saved at `path`, if any, over the current ones (the
    /// command-line defaults), and save every later change there. A saved
    /// value wins over its flag: it is what the operator last chose.
    pub fn persist_to(&self, path: PathBuf) -> anyhow::Result<()> {
        let saved: SavedSettings = match std::fs::read(&path) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SavedSettings::default(),
            Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
        };
        if let Some(p) = saved.issuance_paused {
            self.issuance_paused.store(p, Ordering::Relaxed);
        }
        if let Some(bps) = saved.markup_bps {
            if let Err(e) = self.store_markup_bps(bps) {
                tracing::warn!(path = %path.display(), "ignoring saved markup: {e}");
            }
        }
        if let Some(j) = saved.heat_j_per_th {
            if let Err(e) = self.store_heat_j_per_th(j) {
                tracing::warn!(path = %path.display(), "ignoring saved heat J/TH: {e}");
            }
        }
        tracing::info!(
            path = %path.display(),
            issuance_paused = self.issuance_paused(),
            markup_bps = self.markup_bps(),
            heat_j_per_th = self.heat_j_per_th(),
            "operator settings"
        );
        *self.settings_path.lock().unwrap() = Some(path);
        Ok(())
    }

    /// Write the current settings to the settings file, if there is one.
    fn save(&self) {
        let guard = self.settings_path.lock().unwrap();
        let Some(path) = guard.as_ref() else { return };
        let s = SavedSettings {
            issuance_paused: Some(self.issuance_paused()),
            markup_bps: Some(self.markup_bps()),
            heat_j_per_th: Some(self.heat_j_per_th()),
        };
        let tmp = path.with_extension("json.tmp");
        let r = serde_json::to_vec_pretty(&s)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            tracing::warn!(path = %path.display(), error = %e, "could not save operator settings");
        }
    }
    pub fn issuance_paused(&self) -> bool {
        self.issuance_paused.load(Ordering::Relaxed)
    }
    pub fn set_issuance_paused(&self, paused: bool) {
        self.issuance_paused.store(paused, Ordering::Relaxed);
        self.save();
    }
    pub fn markup_bps(&self) -> i64 {
        self.markup_bps.load(Ordering::Relaxed)
    }
    pub fn set_markup_bps(&self, bps: i64) -> Result<(), String> {
        self.store_markup_bps(bps)?;
        self.save();
        Ok(())
    }
    fn store_markup_bps(&self, bps: i64) -> Result<(), String> {
        if !(MIN_MARKUP_BPS..=MAX_MARKUP_BPS).contains(&bps) {
            return Err(format!(
                "markup_bps must be between {MIN_MARKUP_BPS} and {MAX_MARKUP_BPS}"
            ));
        }
        self.markup_bps.store(bps, Ordering::Relaxed);
        Ok(())
    }
    /// J/TH assumed for rigs whose name does not name a known model.
    pub fn heat_j_per_th(&self) -> f64 {
        f64::from_bits(self.heat_j_per_th.load(Ordering::Relaxed))
    }
    pub fn set_heat_j_per_th(&self, j: f64) -> Result<(), String> {
        self.store_heat_j_per_th(j)?;
        self.save();
        Ok(())
    }
    fn store_heat_j_per_th(&self, j: f64) -> Result<(), String> {
        if !HEAT_J_PER_TH_RANGE.contains(&j) {
            return Err(format!(
                "heat_j_per_th must be between {} and {} J/TH",
                HEAT_J_PER_TH_RANGE.start(),
                HEAT_J_PER_TH_RANGE.end()
            ));
        }
        self.heat_j_per_th.store(j.to_bits(), Ordering::Relaxed);
        Ok(())
    }
}

/// A rail's pricer under operator control: refuses while issuance is
/// paused or the amount exceeds the TiH the rented hashrate can still
/// deliver, and applies the live markup on top of the base price.
pub struct ControlledPricer {
    inner: Arc<dyn TihPricer>,
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
    pub fn new(inner: Arc<dyn TihPricer>, controls: Arc<MintControls>) -> Self {
        Self { inner, controls }
    }

    /// What `amount_tih` would cost now, ignoring a pause.
    pub fn quote(&self, amount_tih: u64) -> Result<u64, PricingError> {
        let base = self.inner.price(amount_tih)?;
        let factor = 1.0 + self.controls.markup_bps() as f64 / 10_000.0;
        let price = (base as f64 * factor).ceil();
        if !price.is_finite() || price >= u64::MAX as f64 {
            return Err(PricingError::Overflow(amount_tih));
        }
        // Never bill zero: that would mint for free.
        Ok((price as u64).max(1))
    }
}

impl TihPricer for ControlledPricer {
    fn price(&self, amount_tih: u64) -> Result<u64, PricingError> {
        if self.controls.issuance_paused() {
            return Err(PricingError::Paused);
        }
        // A future-epoch purchase sells against that epoch's capacity,
        // checked by the epoch endpoint (#126), not the live stock.
        if crate::epochs::FUTURE_EPOCH_QUOTE.try_with(|_| ()).is_err() {
            self.controls.capacity.check(amount_tih, now_ms())?;
        }
        self.quote(amount_tih)
    }
}

/// Current Lightning price in msat per PiH, from a pricer that bills sats:
/// the sats for 1,000 PiH are the msat for one (#90).
pub fn msat_per_pih(sats: &ControlledPricer) -> Option<u64> {
    sats.quote(1_000 * TIH_PER_PIH).ok()
}

/// Current stablecoin price in dollars per PiH, from a pricer that bills
/// micro-dollars (priced over a million PiH to keep the precision).
pub fn usdt_per_pih(micro: &ControlledPricer) -> Option<f64> {
    micro
        .quote(1_000_000 * TIH_PER_PIH)
        .ok()
        .map(|m| m as f64 / 1e12)
}

/// Facts about the daemon that do not change while it runs.
#[derive(Debug, Clone, Default)]
pub struct DaemonInfo {
    pub name: String,
    pub mint_listen: String,
    pub mint_url: Option<String>,
    pub stratum_bind: String,
    /// The public address miners connect to (`HASHU_STRATUM_URL`).
    pub stratum: Option<crate::stratum_addr::StratumAddr>,
    pub rails: Vec<String>,
    pub min_redeem_tih: u64,
    pub max_tih: u64,
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
    /// For the operator's test mint and one-click redeem.
    pub checkout: Arc<CheckoutState>,
    /// Hosted-mint waitlist (`--waitlist`), listed at `/waitlist`.
    pub waitlist: Option<Arc<crate::waitlist::Waitlist>>,
    /// MRR rental sync, whose credentials the GUI can change.
    pub mrr: Arc<crate::mrr_sync::MrrSync>,
    /// Operator login and cross-site request checks.
    pub auth: Arc<crate::operator_auth::OperatorAuth>,
    /// Hashrate planner, with epochs on (#126).
    pub planner: Option<Arc<crate::planner::Planner>>,
    /// The operator mint's record, for its direct messages (`/api/inbox`).
    pub reliability: Option<Arc<crate::reliability::Reliability>>,
    /// Hourly supply rows for the dashboard's supply charts.
    pub supply: Option<Arc<crate::supply::SupplyHistory>>,
}

/// Largest test mint in one go: 2^22 TiH (≈ 1,281 THH).
pub const MAX_TEST_MINT_TIH: u64 = 1 << 22;

pub fn router(state: Arc<OperatorState>) -> axum::Router {
    let mut r = axum::Router::new();
    if let Some(w) = &state.waitlist {
        r = r.merge(crate::waitlist::operator_router(w.clone()));
    }
    if let Some(rel) = &state.reliability {
        // Behind the operator login below, like every other route here.
        r = r.merge(crate::inbox::router(rel.clone(), "", None));
        r = r.route("/api/inbox/send-hashes", post(send_hashes));
    }
    let auth = state.auth.clone();
    r.route("/", get(page))
        .route("/status", get(public_status))
        .route("/scan.js", get(crate::checkout::scan_js))
        .route("/api/operator", get(operator))
        .route("/api/hashrate", get(hashrate))
        .route("/api/track", get(track))
        .route("/api/controls", post(set_controls))
        .route("/api/test-mint", post(test_mint))
        .route("/api/test-redeem", post(test_redeem))
        .route("/api/capacity", get(capacity).post(set_capacity))
        .route("/api/mrr", get(mrr_status).post(set_mrr))
        .route("/api/plan", get(plan_status).post(set_plan))
        .route("/api/supply", get(supply_status))
        .fallback(proxy_control)
        .with_state(state)
        .layer(axum::middleware::from_fn_with_state(
            auth,
            crate::operator_auth::guard,
        ))
}

/// `GET /status`: the control API's status. Without an operator session
/// (the public `/control/status` route the MDK app polls) each redemption
/// leaves out its pool and worker, which name the buyer's pool account.
async fn public_status(
    State(s): State<Arc<OperatorState>>,
    authed: Option<axum::Extension<crate::operator_auth::Authed>>,
) -> Response {
    let (code, mut v) = control_route("GET", "/status", &[], &s.control);
    if !authed.is_some_and(|a| a.0 .0) {
        redact_status(&mut v);
    }
    let code = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (code, Json(v)).into_response()
}

/// Drop what identifies a buyer from a control `/status` body.
pub fn redact_status(v: &mut Value) {
    if let Some(list) = v.get_mut("redemptions").and_then(Value::as_array_mut) {
        for r in list {
            if let Some(o) = r.as_object_mut() {
                o.remove("pool_url");
                o.remove("worker_name");
            }
        }
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE_HTML.as_str())
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

/// How far back `/api/hashrate` lists rig sessions.
const SESSIONS_SPAN_MS: u64 = 3_900_000;

async fn hashrate(State(s): State<Arc<OperatorState>>, uri: Uri) -> Json<Value> {
    let since = query_param(&uri, "since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    // Rig sessions over the chart's longest span (an hour) and a bit, so
    // the page can tell a rig that was down from one that was slow.
    let (sessions, sessions_since) = match &s.control.router {
        Some(r) => {
            let from = (SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default())
            .saturating_sub(SESSIONS_SPAN_MS);
            let (v, since) = r.rig_sessions(from);
            (v, Some(since))
        }
        None => (Vec::new(), None),
    };
    Json(json!({
        "window_s": s.meter.window().as_secs_f64(),
        "samples": s.meter.samples_since(since),
        "sessions": sessions,
        "sessions_since_ms": sessions_since,
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
    if let Some(j) = v.get("heat_j_per_th").filter(|x| !x.is_null()) {
        let Some(j) = j.as_f64() else {
            return bad("heat_j_per_th must be a number".into());
        };
        if let Err(e) = s.controls.set_heat_j_per_th(j) {
            return bad(e);
        }
        tracing::info!(heat_j_per_th = j, "operator changed heat efficiency");
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

/// `POST /api/test-mint` `{ "amount_tih": n }`: mint TiH without payment
/// and return the token. Operator-only, like everything on this port.
async fn test_mint(State(s): State<Arc<OperatorState>>, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let Some(amount) = v["amount_tih"]
        .as_u64()
        .filter(|a| (1..=MAX_TEST_MINT_TIH).contains(a))
    else {
        return bad(format!("amount_tih must be 1 to {MAX_TEST_MINT_TIH}"));
    };
    match checkout::operator_test_mint(&s.checkout, &s.mint, amount, "Hashu test TiH (operator)")
        .await
    {
        Ok((token, quote)) => {
            let token_qr = crate::checkout::token_qr_frames(&token);
            Json(json!({ "token": token, "token_qr": token_qr, "amount_tih": amount, "quote": quote }))
                .into_response()
        }
        Err((code, msg)) => (code, Json(json!({ "error": msg }))).into_response(),
    }
}

/// `POST /api/inbox/send-hashes` `{ peer, amount_tih, note? }`: mint hashes
/// like the test mint and send them to `peer` as a token in a direct
/// message. The token is in the answer either way.
async fn send_hashes(State(s): State<Arc<OperatorState>>, body: Bytes) -> Response {
    let Some(rel) = s.reliability.clone() else {
        return bad("direct messages are off".into());
    };
    let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let peer = v["peer"].as_str().unwrap_or_default().trim().to_string();
    if hashu_mint::inbox::parse_peer(&peer).is_err() {
        return bad("peer must be an npub or a hex public key".into());
    }
    let Some(amount) = v["amount_tih"]
        .as_u64()
        .filter(|a| (1..=MAX_TEST_MINT_TIH).contains(a))
    else {
        return bad(format!("amount_tih must be 1 to {MAX_TEST_MINT_TIH}"));
    };
    let note = v["note"].as_str().unwrap_or_default().trim();
    if note.chars().count() > 1000 {
        return bad("note longer than 1000 characters".into());
    }
    // Relays are checked before minting, so nothing is minted to go nowhere.
    if rel.configured_relays().is_empty() {
        return bad("no Nostr relays configured (HASHU_NOSTR_RELAYS)".into());
    }
    let (proofs, quote) = match checkout::operator_mint_proofs(&s.checkout, &s.mint, amount).await {
        Ok(x) => x,
        Err((code, msg)) => return (code, Json(json!({ "error": msg }))).into_response(),
    };
    let memo = hashes_memo(note, amount, &s.checkout.public_url);
    // The token too: shown if the message cannot be sent.
    let token = match cdk::mint_url::MintUrl::from_str(&s.checkout.public_url) {
        Ok(u) => cdk::nuts::Token::new(
            u,
            proofs.clone(),
            Some(memo.clone()),
            cdk::nuts::CurrencyUnit::Custom("tih".into()),
        )
        .to_string(),
        Err(e) => return bad(e.to_string()),
    };
    let text = hashes_payload(&s.checkout.public_url, &memo, &proofs);
    match rel.send_reply(OPERATOR_MINT, &peer, &text).await {
        Ok(m) => Json(json!({ "message": m, "token": token, "amount_tih": amount, "quote": quote }))
            .into_response(),
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({
                "error": format!("minted, but the message was not sent: {e}. The token is below: send it another way or redeem it."),
                "token": token,
                "amount_tih": amount,
            })),
        )
            .into_response(),
    }
}

/// The note sent with hashes: the operator's, or one naming the amount.
fn hashes_memo(note: &str, amount_tih: u64, mint_url: &str) -> String {
    if note.is_empty() {
        format!("{} of hashes from {mint_url}", fmt_hashes(amount_tih))
    } else {
        note.to_string()
    }
}

/// A hashes DM: a NUT-18 payment payload (`{mint, unit, memo, proofs}`) as
/// the message content, which Cashu wallets that take payments over NIP-17
/// (Cashu.me and its forks, hashu.me) receive as an incoming payment.
fn hashes_payload(mint_url: &str, memo: &str, proofs: &cdk::nuts::Proofs) -> String {
    json!({
        "mint": mint_url,
        "unit": "tih",
        "memo": memo,
        "proofs": proofs,
    })
    .to_string()
}

/// `4 EiH (4,194,304 TiH)`, `1.5 PiH (1,536 TiH)`, `12 TiH`.
fn fmt_hashes(tih: u64) -> String {
    let (unit, f) = if tih >= 1 << 20 {
        ("EiH", (1u64 << 20) as f64)
    } else if tih >= 1 << 10 {
        ("PiH", 1024.0)
    } else {
        return format!("{tih} TiH");
    };
    let v = format!("{:.2}", tih as f64 / f);
    let v = v.trim_end_matches('0').trim_end_matches('.');
    format!("{v} {unit} ({} TiH)", fmt_thousands(tih))
}

fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `POST /api/test-redeem` `{ token, pool_url, worker_name }`: same as the
/// checkout's redeem, from the operator page.
async fn test_redeem(State(s): State<Arc<OperatorState>>, body: Bytes) -> Response {
    checkout::redeem(State(s.checkout.clone()), body).await
}

async fn capacity(State(s): State<Arc<OperatorState>>) -> Json<Value> {
    Json(json!(s.controls.capacity.snapshot(now_ms())))
}

async fn set_capacity(State(s): State<Arc<OperatorState>>, body: Bytes) -> Response {
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad(format!("invalid JSON: {e}")),
    };
    let now = now_ms();
    if let Some(list) = v.get("rentals").filter(|x| !x.is_null()) {
        let Some(list) = list.as_array() else {
            return bad("rentals must be a list".into());
        };
        let rentals = match list.iter().map(|r| rental_from_json(r, now)).collect() {
            Ok(r) => r,
            Err(e) => return bad(e),
        };
        s.controls.capacity.set_manual(rentals);
    }
    if let Some(l) = v.get("limit") {
        let limit = match l {
            Value::Null => None,
            Value::Bool(b) => Some(*b),
            _ => return bad("limit must be true, false or null".into()),
        };
        s.controls.capacity.set_limit(limit);
    }
    if let Some(h) = v.get("live_horizon_hours") {
        let hours = match h {
            Value::Null => None,
            Value::Number(n) => n.as_f64(),
            _ => return bad("live_horizon_hours must be a number of hours or null".into()),
        };
        if let Err(e) = s.controls.capacity.set_live_horizon(hours) {
            return bad(e);
        }
    }
    // Owed may have moved since the last refresh; recount before answering
    // (this also stocks a first live batch for a newly set horizon).
    if let Ok(o) = obligations(&s.mint, &s.control.ledger).await {
        s.controls.capacity.set_owed(o.total_owed(), now_ms());
        s.controls.capacity.set_sold(o.stock_sold as f64);
        s.controls.capacity.refresh_live(now_ms());
    }
    if v.get("restock_live").and_then(Value::as_bool) == Some(true) {
        if let Err(e) = s.controls.capacity.restock_live(now_ms()) {
            return bad(e);
        }
    }
    let snap = s.controls.capacity.snapshot(now_ms());
    tracing::info!(
        rented_ths = snap.rented_ths,
        live_horizon_hours = ?snap.live.horizon_hours,
        capacity_tih = snap.capacity_tih,
        available_tih = ?snap.available_tih,
        "operator changed capacity"
    );
    Json(json!(snap)).into_response()
}

async fn mrr_status(State(s): State<Arc<OperatorState>>) -> Json<Value> {
    Json(json!(s.mrr.status()))
}

/// `{ "key", "secret", "rental_ids"? }` connects (checked against the MRR
/// API first), `{ "rental_ids" }` alone changes the filter on the running
/// key, `{ "disconnect": true }` stops syncing.
async fn set_mrr(State(s): State<Arc<OperatorState>>, body: Bytes) -> Response {
    use crate::mrr_sync::MrrCredentials;
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad(format!("invalid JSON: {e}")),
    };
    if v.get("disconnect").and_then(Value::as_bool) == Some(true) {
        if let Err(e) = s.mrr.set(None) {
            return server_error(e.to_string());
        }
        tracing::info!("operator disconnected MRR");
        return Json(json!(s.mrr.status())).into_response();
    }
    let ids: Option<Vec<String>> = match v.get("rental_ids") {
        None | Some(Value::Null) => None,
        Some(Value::String(t)) => Some(t.split(',').map(str::to_string).collect()),
        Some(Value::Array(a)) => {
            let mut out = Vec::new();
            for x in a {
                match x {
                    Value::String(t) => out.push(t.clone()),
                    Value::Number(n) => out.push(n.to_string()),
                    _ => return bad("rental_ids must be ids".into()),
                }
            }
            Some(out)
        }
        _ => return bad("rental_ids must be a list or a comma-separated string".into()),
    };
    let text = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let (key, secret) = (text("key"), text("secret"));
    let creds = if key.is_empty() && secret.is_empty() {
        // Only the rental filter changes; keep the running key.
        let Some(cur) = s.mrr.current() else {
            return bad("enter the MRR API key and secret".into());
        };
        let Some(ids) = ids else {
            return bad("nothing to change".into());
        };
        match MrrCredentials::new(&cur.key, &cur.secret, &ids) {
            Some(c) => c,
            None => return bad("enter the MRR API key and secret".into()),
        }
    } else {
        let Some(c) = MrrCredentials::new(&key, &secret, &ids.unwrap_or_default()) else {
            return bad("enter both the MRR API key and secret".into());
        };
        // Check them before they replace working ones.
        if let Err(e) = s.mrr.account(&c).active_rentals().await {
            return bad(format!("MRR rejected these credentials: {e}"));
        }
        c
    };
    if let Err(e) = s.mrr.set(Some(creds)) {
        return server_error(e.to_string());
    }
    let st = s.mrr.status();
    tracing::info!(key = ?st.key_hint, rental_ids = ?st.rental_ids, "operator set MRR credentials");
    Json(json!(st)).into_response()
}

fn server_error(msg: String) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": msg })),
    )
        .into_response()
}

/// `GET /api/plan`: the hashrate planner (404 with epochs off).
async fn plan_status(State(s): State<Arc<OperatorState>>) -> Response {
    match &s.planner {
        Some(p) => crate::planner::get_plan(State(p.clone())).await,
        None => no_planner(),
    }
}

/// `POST /api/plan` `{"epochs": {"<epoch>": <TH/s> | null}}`.
async fn set_plan(State(s): State<Arc<OperatorState>>, body: String) -> Response {
    match &s.planner {
        Some(p) => crate::planner::post_plan(State(p.clone()), body).await,
        None => no_planner(),
    }
}

fn no_planner() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "the hashrate planner needs epochs on (HASHU_EPOCHS=true)" })),
    )
        .into_response()
}

fn bad(msg: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn controls_json(s: &OperatorState) -> Value {
    json!({
        "issuance_paused": s.controls.issuance_paused(),
        "markup_bps": s.controls.markup_bps(),
        "heat_j_per_th": s.controls.heat_j_per_th(),
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
        Ok(mut v) => {
            // Whether the page shows "Log out".
            v["login"] = json!(s.auth.login_enabled());
            Json(v).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

/// TiH the mint owes: ecash out, paid-but-unminted quotes, and the
/// undelivered rest of open redemptions.
#[derive(Debug, Default)]
pub struct Obligations {
    pub signed: u64,
    pub spent: u64,
    pub melting: u64,
    pub in_circulation: u64,
    pub sold: BTreeMap<String, u64>,
    pub paid_not_minted: u64,
    pub quotes_open: u64,
    /// TiH minted by the operator's test mint, all time.
    pub test_minted: u64,
    pub open_count: u64,
    pub open_remaining_tih: f64,
    /// Running total a stock is drawn down by (#92): TiH paid for (or
    /// issued) over every quote, plus open quotes that have not expired,
    /// which reserve stock until they do.
    pub stock_sold: u64,
}

impl Obligations {
    pub fn total_owed(&self) -> f64 {
        self.in_circulation as f64 + self.paid_not_minted as f64 + self.open_remaining_tih
    }
}

pub async fn obligations(mint: &Mint, ledger: &RedemptionLedger) -> anyhow::Result<Obligations> {
    let epochs = crate::epochs::operator();
    obligations_for(mint, ledger, None, epochs.as_deref()).await
}

/// [`obligations`] of supplier `supplier`'s mint (`None`: the operator's),
/// counting only that mint's redemptions in the shared ledger. `epochs`:
/// that mint's epochs, when on (#126).
pub async fn obligations_for(
    mint: &Mint,
    ledger: &RedemptionLedger,
    supplier: Option<&str>,
    epochs: Option<&hashu_mint::epoch::Epochs>,
) -> anyhow::Result<Obligations> {
    // keyset_amounts counts every signature and every spent proof, swaps
    // included; swaps add equally to both, so the difference is the TiH
    // ecash still in holders' hands.
    let tih_keysets: Vec<_> = mint
        .keysets()
        .keysets
        .into_iter()
        .filter(|k| k.unit == tih_unit())
        .map(|k| k.id)
        .collect();
    let issued = mint.total_issued().await?;
    let redeemed = mint.total_redeemed().await?;
    let sum = |m: &std::collections::HashMap<_, cdk::Amount>| -> u64 {
        tih_keysets
            .iter()
            .filter_map(|id| m.get(id))
            .map(|a| u64::from(*a))
            .sum()
    };
    let (signed, spent) = (sum(&issued), sum(&redeemed));
    // A hashrate melt stays pending until its redemption is filled, and its
    // inputs only count as spent then. Those TiH are owed as an open
    // redemption, so take them out of circulation to avoid counting twice.
    let melting: u64 = mint
        .melt_quotes()
        .await?
        .iter()
        .filter(|q| q.unit == tih_unit() && q.state == cdk::nuts::MeltQuoteState::Pending)
        .map(|q| q.amount().value())
        .sum();
    // With epochs on (#126), conversions are signed outside cdk: cdk sees
    // their vouchers spent but never signed, so add them back.
    let outside = epochs.map(crate::epochs::outside_cdk).unwrap_or(0);
    let in_circulation = (signed + outside)
        .saturating_sub(spent)
        .saturating_sub(melting);

    // TiH sold over each rail, from the mint quotes.
    let mut sold: BTreeMap<String, u64> = BTreeMap::new();
    let mut paid_not_minted = 0u64;
    let mut quotes_open = 0u64;
    let mut test_minted = 0u64;
    let mut stock_sold = 0u64;
    let now_s = now_ms() / 1000;
    for q in mint.mint_quotes().await? {
        if q.unit != tih_unit() {
            continue;
        }
        // Future-epoch purchases are owed in their epoch, not now (#126).
        if epochs.is_some_and(|e| e.purchase(&q.id.to_string()).is_some()) {
            continue;
        }
        let issued = q.amount_issued().value();
        let paid = q.amount_paid().value();
        stock_sold += if paid.max(issued) > 0 {
            paid.max(issued)
        } else if q.expiry > now_s {
            q.amount.as_ref().map(|a| a.value()).unwrap_or(0)
        } else {
            0
        };
        // Operator test mints are marked paid with an `operator-test-` id.
        if q.payments
            .iter()
            .any(|p| p.payment_id.starts_with("operator-test-"))
        {
            test_minted += issued;
            paid_not_minted += paid.saturating_sub(issued);
            continue;
        }
        *sold.entry(q.payment_method.to_string()).or_default() += issued;
        paid_not_minted += paid.saturating_sub(issued);
        if paid == 0 {
            quotes_open += 1;
        }
    }

    let mut open_count = 0;
    let mut open_remaining_tih = 0.0;
    // Each mint owes only its own redemptions.
    for r in ledger
        .all()
        .into_iter()
        .filter(|r| r.supplier.as_deref() == supplier)
    {
        if !r.state.is_finished() {
            let filled = r.accumulated_hashes as f64 / HASHES_PER_TIH as f64;
            open_remaining_tih += (r.amount_tih as f64 - filled).max(0.0);
            open_count += 1;
        }
    }
    Ok(Obligations {
        signed,
        spent,
        melting,
        in_circulation,
        sold,
        paid_not_minted,
        quotes_open,
        test_minted,
        open_count,
        open_remaining_tih,
        stock_sold,
    })
}

/// What the supply rows need from the obligations and the ledger.
pub fn supply_counts(o: &Obligations, ledger: &RedemptionLedger) -> crate::supply::Counts {
    let delivered: f64 = ledger
        .all()
        .iter()
        .filter(|r| r.supplier.is_none())
        .map(|r| (r.accumulated_hashes as f64 / HASHES_PER_TIH as f64).min(r.amount_tih as f64))
        .sum();
    crate::supply::Counts {
        redeeming_tih: o.open_remaining_tih,
        open_redemptions: o.open_count,
        sold_total_tih: o.sold.values().sum::<u64>() as f64,
        delivered_total_tih: delivered,
    }
}

/// `GET /api/supply`: the dashboard's supply charts. Rows are unfiltered
/// (redeeming split out at any count, delivered every hour) and a live
/// row for right now is added; future epochs on sale come from the epoch
/// plan, mined work from the reliability record.
async fn supply_status(State(s): State<Arc<OperatorState>>) -> Response {
    let Some(h) = s.supply.clone() else {
        return bad("supply history is off".into());
    };
    let now = now_ms();
    let live = match obligations(&s.mint, &s.control.ledger).await {
        Ok(o) => crate::supply::row_from(
            &s.controls.capacity.snapshot(now),
            supply_counts(&o, &s.control.ledger),
            now,
        ),
        Err(e) => return bad(e.to_string()),
    };
    let epochs = crate::epochs::operator().and_then(|e| {
        let clock = e.clock()?;
        let rows = e.plan()?;
        let now_s = now / 1000;
        Some(
            rows.iter()
                .filter(|r| r.epoch != clock.current())
                .map(|r| {
                    json!({
                        "epoch": r.epoch,
                        "starts_at": now_s + (clock.hours_until(r.epoch) * 3600.0) as u64,
                        "hours_in_window": r.hours_in_window,
                        "capacity_tih": r.capacity_tih,
                        "owed_tih": r.owed_tih,
                        "free_tih": r.free_tih,
                    })
                })
                .collect::<Vec<_>>(),
        )
    });
    let mined: Vec<Value> = s
        .reliability
        .as_ref()
        .and_then(|r| r.store(OPERATOR_MINT))
        .map(|st| {
            st.summaries(crate::supply::KEEP_ROWS)
                .into_iter()
                .map(|h| json!({ "hour": h.hour, "work_tih": h.work_tih }))
                .collect()
        })
        .unwrap_or_default();
    Json(json!({
        "tih_per_thh": hashu_core::denom::TIH_PER_THH,
        "rows": h.rows(),
        "live": live,
        "epochs": epochs.unwrap_or_default(),
        "mined": mined,
    }))
    .into_response()
}

/// Keep the capacity book's "owed" current so quotes are capped against it.
pub async fn run_owed_refresher(
    mint: Arc<Mint>,
    ledger: Arc<RedemptionLedger>,
    capacity: Arc<CapacityBook>,
    every: std::time::Duration,
    supply: Option<Arc<crate::supply::SupplyHistory>>,
) {
    let mut tick = tokio::time::interval(every);
    loop {
        tick.tick().await;
        match obligations(&mint, &ledger).await {
            Ok(o) => {
                let now = now_ms();
                capacity.set_owed(o.total_owed(), now);
                capacity.set_sold(o.stock_sold as f64);
                capacity.refresh_live(now);
                if let Some(h) = &supply {
                    h.observe(&capacity.snapshot(now), supply_counts(&o, &ledger), now);
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not count obligations"),
        }
    }
}

async fn operator_json(s: &OperatorState) -> anyhow::Result<Value> {
    let o = obligations(&s.mint, &s.control.ledger).await?;
    s.controls.capacity.set_owed(o.total_owed(), now_ms());
    s.controls.capacity.set_sold(o.stock_sold as f64);
    let sold_total: u64 = o.sold.values().sum();

    // ---- redemptions --------------------------------------------------
    let tracks = s.meter.tracks();
    // Which rigs each redemption's hashrate comes from right now.
    let rigs_on = s
        .control
        .router
        .as_ref()
        .map(|r| r.rigs_by_redemption())
        .unwrap_or_default();
    let redemptions: Vec<Value> = s
        .control
        .ledger
        .all()
        .iter()
        .map(|r| {
            let id = r.id.to_string();
            let filled = r.accumulated_hashes as f64 / HASHES_PER_TIH as f64;
            let remaining = (r.amount_tih as f64 - filled).max(0.0);
            let t = tracks.iter().find(|t| t.id == id);
            json!({
                "id": id,
                "pool_url": r.pool_url,
                "worker_name": r.worker_name,
                "amount_tih": r.amount_tih,
                "filled_tih": filled,
                // Whole TiH each pool got, adding up to the delivered total.
                "pool_tih": r.pool_tih().iter().map(|(p, tih)| json!({
                    "pool_url": p.pool_url,
                    "worker_name": p.worker_name,
                    "tih": tih,
                })).collect::<Vec<_>>(),
                "remaining_tih": remaining,
                "stop_charge_tih": r.settle_preview().map(|(c, _)| c),
                "stop_change_tih": r.settle_preview().map(|(_, m)| m),
                "progress": if r.target_hashes == 0 { 1.0 } else {
                    (r.accumulated_hashes as f64 / r.target_hashes as f64).min(1.0)
                },
                "state": r.state.as_str(),
                // "redeemer" or "operator" while paused, else null.
                "paused_by": r.paused_by.map(|p| p.as_str()),
                "spent_tih": r.is_finished().then(|| r.spent_tih()),
                "change_tih": r.is_finished().then(|| r.change_tih()),
                "opened_ms": t.map(|t| t.opened_ms),
                "completed_ms": t.and_then(|t| t.completed_ms),
                // Rigs routed to it now, with their hashrate estimate
                // (null until a rig has enough shares).
                "rigs": rigs_on.get(&r.id).map(|v| v.iter().map(|(rig, hs)| json!({
                    "rig": rig,
                    "hashrate_hs": hs,
                })).collect::<Vec<_>>()).unwrap_or_default(),
            })
        })
        .collect();

    // ---- price --------------------------------------------------------
    let msat_per_pih = s.sats.as_deref().and_then(msat_per_pih);
    let usdt_per_pih = s.usdt.as_deref().and_then(usdt_per_pih);

    let m = s.control.metrics.snapshot();
    let latest = s.meter.latest();
    let now = now_ms();
    let capacity = s.controls.capacity.snapshot(now);
    let heat = heat_json(
        s,
        &capacity,
        latest.as_ref().map(|l| l.hashrate_hs).unwrap_or(0.0),
        latest.as_ref().map(|l| l.total_hashes).unwrap_or(0.0),
        now,
    );
    Ok(json!({
        "now_ms": now_ms(),
        "started_ms": s.started_ms,
        "mint": {
            "name": s.info.name,
            "listen": s.info.mint_listen,
            "url": s.info.mint_url,
            "rails": s.info.rails,
            "min_redeem_tih": s.info.min_redeem_tih,
            "max_tih": s.info.max_tih,
            "controls": controls_json(s),
            "price": {
                "msat_per_pih": msat_per_pih,
                "usdt_per_pih": usdt_per_pih,
                "sats_per_th_day": msat_per_pih.map(|m| sats_per_th_day(m as f64)),
            },
        },
        "obligations": {
            "tih_sold": sold_total,
            "test_minted_tih": o.test_minted,
            "sold_by_rail": o.sold,
            "paid_not_minted_tih": o.paid_not_minted,
            "unpaid_quotes": o.quotes_open,
            "tih_in_circulation": o.in_circulation,
            "open_redemptions": o.open_count,
            "open_redemption_remaining_tih": o.open_remaining_tih,
            "total_tih_owed": o.total_owed(),
            "signed_tih": o.signed,
            "spent_tih": o.spent,
            "melting_tih": o.melting,
        },
        "capacity": capacity,
        "mrr": s.mrr.status(),
        "heat": heat,
        "proxy": {
            "stratum_bind": s.info.stratum_bind,
            // `{url, host, port}` miners connect to; differs from the
            // bind behind a port mapping.
            "stratum": s.info.stratum,
            "default_upstream": s.control.default_upstream,
            // How a new rig names itself to mine for this mint (the
            // operator app's "Add worker"); null without a proxy router.
            "workers": s.control.router.as_ref().map(|r| r.rig_naming()),
            "metrics": {
                "connections_accepted": m.connections_accepted,
                "shares_submitted": m.shares_submitted,
                "shares_accepted": m.shares_accepted,
                "shares_rejected": m.shares_rejected,
                "shares_credited": m.shares_credited,
            },
            "hashrate_hs": latest.as_ref().map(|l| l.hashrate_hs).unwrap_or(0.0),
            "total_hashes": latest.as_ref().map(|l| l.total_hashes).unwrap_or(0.0),
            "window_s": s.meter.window().as_secs_f64(),
        },
        "redemptions": redemptions,
        "current": s.meter.current_track(),
        // Hosted-mint waitlist sign-ups, or null when --waitlist is off.
        "waitlist": s.waitlist.as_ref().map(|w| w.len()),
    }))
}

/// Heat estimate: the work through the proxy and into redemptions, times
/// the fleet's J/TH (from rig names, else the operator's default).
fn heat_json(
    s: &OperatorState,
    capacity: &hashu_mint::capacity::CapacitySnapshot,
    hashrate_hs: f64,
    total_hashes: f64,
    now: u64,
) -> Value {
    let eff = heat::fleet_efficiency(&capacity.rentals, s.controls.heat_j_per_th(), now);
    let j = eff.j_per_th;
    // Every accepted share, default pool included (carried across restarts
    // with an ops database, else since the daemon started).
    let proxy_tih = total_hashes / HASHES_PER_TIH as f64;
    // Work delivered to redeemers, all time (the ledger is persisted).
    let redeemed_tih: f64 = s
        .control
        .ledger
        .all()
        .iter()
        .map(|r| r.accumulated_hashes as f64 / HASHES_PER_TIH as f64)
        .fold(0.0, |a, b| a + b);
    let now_ths = hashrate_hs / 1e12;
    let kwh = |tih: f64| heat::kwh(tih, j);
    json!({
        "efficiency": eff,
        "kwh_per_tih": kwh(1.0),
        "now_kw": heat::kw(now_ths, j),
        "now_btu_per_h": heat::kw(now_ths, j) * heat::BTU_PER_KWH,
        "rented_kw": heat::kw(capacity.rented_ths, j),
        "proxy_since_start": {
            "tih": proxy_tih,
            "kwh": kwh(proxy_tih),
            "btu": kwh(proxy_tih) * heat::BTU_PER_KWH,
        },
        "redeemed": {
            "tih": redeemed_tih,
            "kwh": kwh(redeemed_tih),
            "btu": kwh(redeemed_tih) * heat::BTU_PER_KWH,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Flat(u64);
    impl TihPricer for Flat {
        fn price(&self, tih: u64) -> Result<u64, PricingError> {
            Ok(self.0 * tih)
        }
    }

    #[test]
    fn hashes_dm_is_a_nut18_payload() {
        assert_eq!(fmt_hashes(12), "12 TiH");
        assert_eq!(fmt_hashes(1536), "1.5 PiH (1,536 TiH)");
        assert_eq!(fmt_hashes(1 << 22), "4 EiH (4,194,304 TiH)");
        assert_eq!(
            hashes_memo("", 1024, "https://mint.example"),
            "1 PiH (1,024 TiH) of hashes from https://mint.example"
        );
        assert_eq!(hashes_memo("Thanks!", 1, "u"), "Thanks!");
        let proof: cdk::nuts::Proof = serde_json::from_value(json!({
            "amount": 1024,
            "id": "00ad268c4d1f5826",
            "secret": "a".repeat(64),
            "C": "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        }))
        .unwrap();
        let v: Value = serde_json::from_str(&hashes_payload(
            "https://mint.example",
            "Thanks!",
            &vec![proof],
        ))
        .unwrap();
        assert_eq!(v["mint"], "https://mint.example");
        assert_eq!(v["unit"], "tih");
        assert_eq!(v["memo"], "Thanks!");
        assert_eq!(v["proofs"][0]["amount"], 1024);
        assert_eq!(v["proofs"][0]["id"], "00ad268c4d1f5826");
        assert!(v["proofs"][0]["C"].as_str().unwrap().starts_with("02"));
    }

    #[test]
    fn public_status_leaves_out_buyers_pools() {
        let mut v = json!({
            "default_upstream": "pool:3333",
            "redemptions": [{ "id": "ab", "pool_url": "stratum+tcp://p:3333", "worker_name": "alice.1", "progress": 0.5 }],
        });
        redact_status(&mut v);
        let r = &v["redemptions"][0];
        assert!(r.get("pool_url").is_none() && r.get("worker_name").is_none());
        assert_eq!(r["progress"], 0.5);
        assert_eq!(v["default_upstream"], "pool:3333");
    }

    #[test]
    fn markup_applies_and_pause_refuses() {
        let c = Arc::new(MintControls::new(0));
        c.capacity.set_limit(Some(false));
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
    fn capacity_caps_quotes() {
        let book = Arc::new(CapacityBook::new());
        let c = Arc::new(MintControls::with_capacity(0, book.clone()));
        let p = ControlledPricer::new(Arc::new(Flat(40)), c);
        // No rentals configured: nothing to sell.
        assert!(matches!(
            p.price(1),
            Err(PricingError::SoldOut {
                requested: 1,
                available: 0
            })
        ));
        book.set_manual(vec![hashu_mint::capacity::Rental {
            id: "r1".into(),
            label: String::new(),
            ths: 10.0,
            end_ms: now_ms() + 3 * 3_600_000 + 60_000,
        }]);
        // 10 TH/s for ~3 h ≈ 30.2 THH, 5 THH owed: ≈ 25.2 THH ≈ 82,400 TiH.
        // The rental burns ≈ 9 TiH per ms, so leave some slack.
        let thh = hashu_core::denom::TIH_PER_THH;
        book.set_owed(5.0 * thh, now_ms());
        let avail = book.snapshot(now_ms()).available_tih.unwrap();
        assert!(
            (25.0 * thh..25.2 * thh).contains(&(avail as f64)),
            "{avail}"
        );
        assert_eq!(p.price(80_000).unwrap(), 3_200_000);
        assert!(matches!(
            p.price(avail + 1),
            Err(PricingError::SoldOut { requested, .. }) if requested == avail + 1
        ));
    }

    #[test]
    fn heat_default_is_bounded() {
        let c = MintControls::new(0);
        assert_eq!(c.heat_j_per_th(), heat::DEFAULT_J_PER_TH);
        c.set_heat_j_per_th(17.5).unwrap();
        assert_eq!(c.heat_j_per_th(), 17.5);
        assert!(c.set_heat_j_per_th(0.0).is_err());
        assert!(c.set_heat_j_per_th(f64::NAN).is_err());
        assert_eq!(c.heat_j_per_th(), 17.5);
    }

    #[test]
    fn markup_is_bounded_and_never_bills_zero() {
        let c = Arc::new(MintControls::new(0));
        assert!(c.set_markup_bps(-10_000).is_err());
        assert!(c.set_markup_bps(MAX_MARKUP_BPS + 1).is_err());
        c.set_markup_bps(MIN_MARKUP_BPS).unwrap();
        c.capacity.set_limit(Some(false));
        let p = ControlledPricer::new(Arc::new(Flat(1)), c);
        assert_eq!(p.price(1).unwrap(), 1);
    }

    #[test]
    fn settings_survive_restart() {
        let dir = std::env::temp_dir().join(format!(
            "hashu-settings-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("operator-settings.json");

        let c = MintControls::new(0);
        c.persist_to(path.clone()).unwrap();
        // Nothing saved yet: the flags stand.
        assert_eq!(c.markup_bps(), 0);
        assert!(!c.issuance_paused());
        c.set_markup_bps(1_500).unwrap();
        c.set_issuance_paused(true);
        c.set_heat_j_per_th(21.0).unwrap();

        // A restart with different flags keeps what the operator chose.
        let c2 = MintControls::new(300);
        c2.set_heat_j_per_th(30.0).unwrap();
        c2.persist_to(path.clone()).unwrap();
        assert_eq!(c2.markup_bps(), 1_500);
        assert!(c2.issuance_paused());
        assert_eq!(c2.heat_j_per_th(), 21.0);

        // Out-of-range saved values are ignored, the flag stays.
        std::fs::write(&path, r#"{"markup_bps": 999999999}"#).unwrap();
        let c3 = MintControls::new(300);
        c3.persist_to(path.clone()).unwrap();
        assert_eq!(c3.markup_bps(), 300);
        assert!(!c3.issuance_paused());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
