//! Tips to the operator on the mint's Nostr profile (`HASHU_TIPS`).
//!
//! - `on` (default): with phoenixd as the Lightning backend and a public
//!   `https://` mint URL, the mint runs its own tip jar at
//!   `tips@<mint host>`, with NIP-57 zap receipts signed by its receipt key
//!   ([`hashu_mint::tips`]). Otherwise no tips.
//! - a Lightning address or LNURL-pay link: the profile points there
//!   instead (zaps then work only if that endpoint supports NIP-57).
//! - `off`: no tips.
//!
//! A tip is a gift to support Hashu's development. It issues no vouchers.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::extract::{Query, State};
use axum::http::{header, HeaderValue};
use axum::middleware::{from_fn, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Json;
use hashu_mint::discovery::Tip;
use hashu_mint::lnaddress::PayEndpoint;
use hashu_mint::phoenixd::{PhoenixdConfig, PhoenixdTips};
use hashu_mint::tips::{TipJar, CALLBACK_PATH, LNURLP_PATH};
use serde::Deserialize;
use serde_json::json;

/// How often open zaps are checked for payment.
const TICK: Duration = Duration::from_secs(5);

/// What `HASHU_TIPS` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TipsSetting {
    Off,
    /// Run the mint's own tip jar when it can.
    On,
    /// Point the profile at this endpoint.
    External(PayEndpoint),
}

impl std::str::FromStr for TipsSetting {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "on" | "true" | "1" => Ok(Self::On),
            "off" | "false" | "0" | "none" => Ok(Self::Off),
            _ => s
                .parse::<PayEndpoint>()
                .map(Self::External)
                .map_err(|e| format!("HASHU_TIPS: {e}")),
        }
    }
}

/// Decide where tips go. Returns the profile's tip and, for the mint's own
/// tip jar, the routes to serve (the zap watcher is already running).
pub fn setup(
    setting: &TipsSetting,
    phoenixd: bool,
    mint_url: Option<&str>,
    data_dir: &std::path::Path,
    relays: Vec<String>,
) -> Result<(Option<Tip>, Option<axum::Router>)> {
    match setting {
        TipsSetting::Off => Ok((None, None)),
        TipsSetting::External(e) => {
            tracing::info!(tips = %e, "tips: profile points at an external endpoint");
            Ok((Some(Tip::operator(e)), None))
        }
        TipsSetting::On => {
            let Some(url) = mint_url
                .map(str::trim)
                .filter(|u| u.starts_with("https://"))
            else {
                tracing::info!("tips off: no public https mint URL");
                return Ok((None, None));
            };
            if !phoenixd {
                tracing::info!("tips off: Lightning backend is not phoenixd");
                return Ok((None, None));
            }
            let cfg = PhoenixdConfig::from_env().map_err(|e| anyhow::anyhow!("phoenixd: {e}"))?;
            let node = PhoenixdTips::new(&cfg).map_err(|e| anyhow::anyhow!("phoenixd: {e}"))?;
            let keys = hashu_mint::reliability::receipt_keys(&data_dir.join("reliability"))
                .context("load reliability receipt key")?;
            let jar = Arc::new(TipJar::open(
                Box::new(node),
                keys,
                url,
                relays,
                &data_dir.join("tips"),
            )?);
            let address: PayEndpoint = jar
                .address()
                .parse()
                .map_err(|e: String| anyhow::anyhow!(e))?;
            tracing::info!(address = %jar.address(), "tips on: zaps and tips support Hashu's development");
            tokio::spawn(watch(jar.clone()));
            Ok((Some(Tip::operator(&address)), Some(router(jar))))
        }
    }
}

async fn watch(jar: Arc<TipJar>) {
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        jar.tick(now_s()).await;
    }
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// LUD-16 and the LUD-06 callback, with CORS (web wallets call them).
pub fn router(jar: Arc<TipJar>) -> axum::Router {
    axum::Router::new()
        .route(LNURLP_PATH, get(pay_request))
        .route(CALLBACK_PATH, get(callback))
        .layer(from_fn(cors))
        .with_state(jar)
}

async fn cors(req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    res.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    res
}

async fn pay_request(State(jar): State<Arc<TipJar>>) -> Response {
    Json(jar.pay_request()).into_response()
}

#[derive(Deserialize)]
struct CallbackQuery {
    amount: Option<String>,
    nostr: Option<String>,
}

async fn callback(State(jar): State<Arc<TipJar>>, Query(q): Query<CallbackQuery>) -> Response {
    let Some(amount) = q
        .amount
        .as_deref()
        .and_then(|a| a.trim().parse::<u64>().ok())
    else {
        return lnurl_error("amount (msat) is required");
    };
    match jar.invoice(amount, q.nostr.as_deref(), now_s()).await {
        Ok(v) => Json(v).into_response(),
        Err(reason) => lnurl_error(&reason),
    }
}

/// LUD-06 errors are HTTP 200 with `status: ERROR`.
fn lnurl_error(reason: &str) -> Response {
    Json(json!({ "status": "ERROR", "reason": reason })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_setting() {
        assert_eq!("".parse::<TipsSetting>().unwrap(), TipsSetting::On);
        assert_eq!("off".parse::<TipsSetting>().unwrap(), TipsSetting::Off);
        assert!(matches!(
            "tips@wallet.example".parse::<TipsSetting>().unwrap(),
            TipsSetting::External(PayEndpoint::Address(_))
        ));
        assert!("not an address".parse::<TipsSetting>().is_err());
    }

    #[tokio::test]
    async fn off_without_phoenixd_or_public_url() {
        let dir = tempfile::tempdir().unwrap();
        let (tip, routes) = setup(
            &TipsSetting::On,
            false,
            Some("https://m.example"),
            dir.path(),
            vec![],
        )
        .unwrap();
        assert!(tip.is_none() && routes.is_none());
        let (tip, _) = setup(&TipsSetting::On, true, None, dir.path(), vec![]).unwrap();
        assert!(tip.is_none());
        let ext = "tips@wallet.example".parse().unwrap();
        let (tip, routes) = setup(&ext, false, None, dir.path(), vec![]).unwrap();
        assert_eq!(tip.unwrap().value, "tips@wallet.example");
        assert!(routes.is_none());
    }
}
