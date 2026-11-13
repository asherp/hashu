//! phoenixd Lightning node: receive real (mainnet) sats for `TiH` quotes.
//! ARCHITECTURE.md §4.5.
//!
//! [phoenixd](https://phoenix.acinq.co/server) is ACINQ's self-custodial,
//! receive-friendly Lightning daemon with automatic inbound liquidity. It is
//! the same node setup the overpay marketplace runs (`docker/phoenixd`,
//! `PHOENIXD_URL` / `PHOENIXD_PASSWORD`), so a Hashu operator can reuse it.
//!
//! [`Phoenixd`] is a cdk [`MintPayment`] backend in sats. Wrap it with
//! [`crate::lightning::lightning_issuer`] to sell `TiH` over bolt11:
//!
//! - **invoice**: `POST /createinvoice` (form `amountSat`, `description`,
//!   `expirySeconds`), HTTP Basic auth with an empty user and the node's
//!   http-password.
//! - **settlement, instant**: phoenixd's `/websocket` pushes a
//!   `payment_received` frame the moment an invoice settles, and that frame
//!   becomes a cdk payment event straight away. Same design as overpay's
//!   `PhoenixdSubscriber`: phoenixd pings every 10 s, so the link is only
//!   treated as dead after 60 s with no frame at all (never on idle, which
//!   would strand handlers inside phoenixd). It reconnects with backoff
//!   (2 s up to 60 s) and runs a catch-up poll on every connect.
//! - **settlement, fallback**: `GET /payments/incoming/{paymentHash}` for
//!   every open invoice, every `poll_interval` (5 s by default), so a
//!   payment is still seen while the socket is down. cdk also checks on
//!   demand when a wallet asks about its quote. Whichever path sees a
//!   payment first credits it; the other then ignores it.
//! - **amount credited**: the **invoiced** amount, not `receivedSat`. When
//!   phoenixd opens a channel to receive a payment it deducts the opening
//!   fee from `receivedSat`. The payer still paid the whole invoice, so the
//!   shortfall is the operator's cost, not an underpayment (same rule as
//!   overpay's `LightningPaymentChecker`). The same holds for payments
//!   phoenixd parks as **fee credit** before its first channel exists:
//!   the invoice settles and the buyer gets the full `TiH`. The mint
//!   absorbs whatever its LSP charges. See deploy/README.md §5.
//! - **outgoing**: refused. The mint never pays out.
//!
//! **Payment network ≠ redemption network.** phoenixd runs on mainnet: buyers
//! pay real sats. The pools that `TiH` is later redeemed against are chosen
//! per redemption and may be on any network, Testnet4 included. Nothing here
//! looks at pool networks; [`Phoenixd::connect`] only checks the node's own
//! chain against [`PhoenixdConfig::expect_chain`].

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use cdk::cdk_payment::{
    Bolt11Settings, CreateIncomingPaymentResponse, Error, Event, IncomingPaymentOptions,
    MakePaymentResponse, MintPayment, OutgoingPaymentOptions, PaymentIdentifier,
    PaymentQuoteResponse, SettingsResponse, WaitPaymentResponse,
};
use cdk::nuts::CurrencyUnit;
use cdk::{Amount, Bolt11Invoice};
use futures::stream::{self, BoxStream, StreamExt};
use serde::Deserialize;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Default poll cadence for open invoices.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default invoice lifetime when cdk doesn't give an expiry.
pub const DEFAULT_EXPIRY_SECS: u64 = 3600;

/// Open invoices are still polled this long after they expire, in case a
/// payment landed right at the deadline.
const EXPIRY_GRACE_SECS: u64 = 300;

/// phoenixd pings every 10 s; six missed pings and no message = dead link.
const WS_SILENCE_TIMEOUT: Duration = Duration::from_secs(60);
const WS_BASE_BACKOFF: Duration = Duration::from_secs(2);
const WS_MAX_BACKOFF: Duration = Duration::from_secs(60);

const NO_PAYOUTS: &str = "phoenixd backend is receive-only; this mint does not pay out";

/// How to reach phoenixd.
#[derive(Clone)]
pub struct PhoenixdConfig {
    /// HTTP API base URL, e.g. `http://127.0.0.1:9740`.
    pub url: String,
    /// phoenixd's `http-password`.
    pub password: String,
    /// How often open invoices are polled.
    pub poll_interval: Duration,
    /// Chain the node must report from `/getinfo` (default `mainnet`).
    /// `None` skips the check.
    pub expect_chain: Option<String>,
    /// phoenixd's push socket, by default `url` with `ws(s)://` and
    /// `/websocket`. `None` turns push off and leaves only polling.
    pub websocket_url: Option<String>,
}

/// phoenixd's push socket for an HTTP API base URL.
pub fn websocket_url_for(http_url: &str) -> String {
    let base = http_url.trim_end_matches('/');
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_string()
    };
    format!("{base}/websocket")
}

impl std::fmt::Debug for PhoenixdConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhoenixdConfig")
            .field("url", &self.url)
            .field("password", &"<redacted>")
            .field("poll_interval", &self.poll_interval)
            .field("expect_chain", &self.expect_chain)
            .field("websocket_url", &self.websocket_url)
            .finish()
    }
}

impl PhoenixdConfig {
    pub fn new(url: impl Into<String>, password: impl Into<String>) -> Self {
        let url = url.into().trim_end_matches('/').to_string();
        Self {
            websocket_url: Some(websocket_url_for(&url)),
            url,
            password: password.into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            expect_chain: Some("mainnet".into()),
        }
    }

    /// Read the same variables overpay uses:
    ///
    /// - `PHOENIXD_URL`, or `PHOENIXD_HOSTPORT` (used as `http://<hostport>`)
    /// - `PHOENIXD_PASSWORD`
    /// - `PHOENIXD_POLL_SECS` (optional, default 5)
    /// - `PHOENIXD_CHAIN` (optional, default `mainnet`; `any` skips the check)
    /// - `PHOENIXD_WEBSOCKET` (optional: `off` for polling only, or a
    ///   `ws://`/`wss://` URL; default derived from the HTTP URL)
    pub fn from_env() -> Result<Self, Error> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let url = var("PHOENIXD_URL")
            .or_else(|| var("PHOENIXD_HOSTPORT").map(|hp| format!("http://{hp}")))
            .ok_or_else(|| Error::Custom("PHOENIXD_URL or PHOENIXD_HOSTPORT must be set".into()))?;
        let password = var("PHOENIXD_PASSWORD")
            .ok_or_else(|| Error::Custom("PHOENIXD_PASSWORD must be set".into()))?;
        let mut cfg = Self::new(url, password);
        if let Some(secs) = var("PHOENIXD_POLL_SECS") {
            let secs: u64 = secs
                .parse()
                .map_err(|e| Error::Custom(format!("PHOENIXD_POLL_SECS: {e}")))?;
            cfg.poll_interval = Duration::from_secs(secs.max(1));
        }
        if let Some(chain) = var("PHOENIXD_CHAIN") {
            cfg.expect_chain = (chain != "any").then_some(chain);
        }
        if let Some(ws) = var("PHOENIXD_WEBSOCKET") {
            cfg.websocket_url = match ws.as_str() {
                "off" | "0" | "false" => None,
                _ => Some(ws),
            };
        }
        Ok(cfg)
    }
}

/// Subset of phoenixd's `/getinfo`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub node_id: String,
    pub chain: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreatedInvoice {
    payment_hash: String,
    serialized: String,
}

/// Subset of `GET /payments/incoming/{hash}`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IncomingPayment {
    #[serde(default)]
    is_paid: bool,
    #[serde(default)]
    received_sat: u64,
    #[serde(default)]
    invoice: Option<String>,
}

/// An invoice we issued and haven't seen paid yet.
#[derive(Debug, Clone)]
struct OpenInvoice {
    amount_sat: u64,
    expiry: u64,
}

#[derive(Clone)]
struct Client {
    http: reqwest::Client,
    url: String,
    password: String,
}

impl Client {
    async fn get_info(&self) -> Result<NodeInfo, Error> {
        let resp = self
            .send(self.http.get(format!("{}/getinfo", self.url)))
            .await?;
        resp.json().await.map_err(http_err)
    }

    async fn create_invoice(
        &self,
        amount_sat: u64,
        description: &str,
        expiry_secs: u64,
    ) -> Result<CreatedInvoice, Error> {
        let form = [
            ("amountSat", amount_sat.to_string()),
            ("description", description.to_string()),
            ("expirySeconds", expiry_secs.to_string()),
        ];
        let req = self
            .http
            .post(format!("{}/createinvoice", self.url))
            .form(&form);
        self.send(req).await?.json().await.map_err(http_err)
    }

    /// `None` when phoenixd has no record of the invoice.
    async fn incoming(&self, hash: &[u8; 32]) -> Result<Option<IncomingPayment>, Error> {
        let url = format!("{}/payments/incoming/{}", self.url, hex::encode(hash));
        let resp = self
            .http
            .get(url)
            .basic_auth("", Some(&self.password))
            .send()
            .await
            .map_err(http_err)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = check_status(resp).await?;
        resp.json().await.map(Some).map_err(http_err)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, Error> {
        let resp = req
            .basic_auth("", Some(&self.password))
            .send()
            .await
            .map_err(http_err)?;
        check_status(resp).await
    }
}

async fn check_status(resp: reqwest::Response) -> Result<reqwest::Response, Error> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(Error::Custom(format!("phoenixd returned {status}: {body}")))
}

fn http_err(e: reqwest::Error) -> Error {
    Error::Custom(format!("phoenixd: {e}"))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_hash(hex_hash: &str) -> Result<[u8; 32], Error> {
    hex::decode(hex_hash)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Custom(format!("phoenixd returned a bad payment hash: {hex_hash}")))
}

/// A settled payment as cdk wants it, crediting the invoiced amount.
///
/// The invoiced amount comes from our own record of the invoice, or from
/// the bolt11 phoenixd returns (covers invoices from before a restart).
/// `receivedSat` is the last resort, as it is net of channel-opening fees.
fn settled(
    hash: &[u8; 32],
    known_amount_sat: Option<u64>,
    payment: &IncomingPayment,
) -> WaitPaymentResponse {
    let invoiced_msat = known_amount_sat.map(|s| s * 1000).or_else(|| {
        payment
            .invoice
            .as_deref()
            .and_then(|s| Bolt11Invoice::from_str(s).ok())
            .and_then(|inv| inv.amount_milli_satoshis())
    });
    let amount = match invoiced_msat {
        Some(msat) => Amount::new(msat, CurrencyUnit::Msat),
        None => Amount::new(payment.received_sat, CurrencyUnit::Sat),
    };
    WaitPaymentResponse {
        payment_identifier: PaymentIdentifier::PaymentHash(*hash),
        payment_amount: amount,
        payment_id: hex::encode(hash),
    }
}

/// phoenixd as a receive-only cdk Lightning backend. See the module docs.
pub struct Phoenixd {
    client: Client,
    poll_interval: Duration,
    websocket_url: Option<String>,
    open: Arc<Mutex<HashMap<[u8; 32], OpenInvoice>>>,
    waiting: Arc<AtomicBool>,
    cancel: Mutex<Option<watch::Sender<bool>>>,
}

impl std::fmt::Debug for Phoenixd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Phoenixd")
            .field("url", &self.client.url)
            .finish_non_exhaustive()
    }
}

impl Phoenixd {
    /// Build without contacting the node.
    pub fn new(config: PhoenixdConfig) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(http_err)?;
        Ok(Self {
            client: Client {
                http,
                url: config.url,
                password: config.password,
            },
            poll_interval: config.poll_interval,
            websocket_url: config.websocket_url,
            open: Arc::default(),
            waiting: Arc::default(),
            cancel: Mutex::default(),
        })
    }

    /// Build, then check the node is reachable and on the expected chain.
    pub async fn connect(config: PhoenixdConfig) -> Result<Self, Error> {
        let expect = config.expect_chain.clone();
        let node = Self::new(config)?;
        let info = node.get_info().await?;
        if let Some(expect) = expect {
            if !info.chain.eq_ignore_ascii_case(&expect) {
                return Err(Error::Custom(format!(
                    "phoenixd is on {}, expected {expect}",
                    info.chain
                )));
            }
        }
        tracing::info!(node_id = %info.node_id, chain = %info.chain, "phoenixd connected");
        Ok(node)
    }

    pub async fn get_info(&self) -> Result<NodeInfo, Error> {
        self.client.get_info().await
    }
}

/// One poll pass: settle every open invoice phoenixd reports paid, and drop
/// the ones long expired.
async fn poll_open(
    client: &Client,
    open: &Mutex<HashMap<[u8; 32], OpenInvoice>>,
) -> Vec<WaitPaymentResponse> {
    let now = unix_now();
    let snapshot: Vec<([u8; 32], OpenInvoice)> = {
        let mut open = open.lock().expect("open invoices lock");
        open.retain(|_, inv| inv.expiry + EXPIRY_GRACE_SECS > now);
        open.iter().map(|(h, i)| (*h, i.clone())).collect()
    };
    let mut paid = Vec::new();
    for (hash, inv) in snapshot {
        match client.incoming(&hash).await {
            Ok(Some(p)) if p.is_paid => {
                open.lock().expect("open invoices lock").remove(&hash);
                paid.push(settled(&hash, Some(inv.amount_sat), &p));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(hash = %hex::encode(hash), "phoenixd poll failed: {e}"),
        }
    }
    paid
}

/// Resolves once `cancel` is set (or its sender is gone).
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    let _ = cancel.wait_for(|c| *c).await;
}

/// Fallback path: poll open invoices every `every` until cancelled.
async fn poll_task(
    client: Client,
    open: Arc<Mutex<HashMap<[u8; 32], OpenInvoice>>>,
    every: Duration,
    tx: mpsc::Sender<WaitPaymentResponse>,
    mut cancel: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(every) => {}
            _ = cancelled(&mut cancel) => return,
        }
        for p in poll_open(&client, &open).await {
            if tx.send(p).await.is_err() {
                return;
            }
        }
    }
}

/// A `payment_received` push frame.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payment_hash: Option<String>,
    #[serde(default)]
    amount_sat: u64,
}

/// Settle the open invoice a push frame names, if any. A frame for an
/// invoice that is not open (already settled by the poller, or from before
/// a restart) is ignored; cdk's on-demand check still covers the latter.
fn settle_push(
    raw: &str,
    open: &Mutex<HashMap<[u8; 32], OpenInvoice>>,
) -> Option<WaitPaymentResponse> {
    let frame: PushFrame = match serde_json::from_str(raw) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("phoenixd websocket: ignoring frame: {e}");
            return None;
        }
    };
    if frame.kind != "payment_received" {
        return None;
    }
    let hash = parse_hash(frame.payment_hash.as_deref()?).ok()?;
    let Some(inv) = open.lock().expect("open invoices lock").remove(&hash) else {
        tracing::debug!(hash = %hex::encode(hash), "phoenixd push for an invoice that is not open");
        return None;
    };
    let pushed = IncomingPayment {
        is_paid: true,
        received_sat: frame.amount_sat,
        invoice: None,
    };
    Some(settled(&hash, Some(inv.amount_sat), &pushed))
}

/// Instant path: listen on phoenixd's push socket until cancelled,
/// reconnecting with backoff.
async fn websocket_task(
    url: String,
    client: Client,
    open: Arc<Mutex<HashMap<[u8; 32], OpenInvoice>>>,
    tx: mpsc::Sender<WaitPaymentResponse>,
    mut cancel: watch::Receiver<bool>,
) {
    let auth = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!(":{}", client.password))
    );
    let mut backoff = WS_BASE_BACKOFF;
    loop {
        let request = url.as_str().into_client_request().map(|mut r| {
            if let Ok(v) = auth.parse() {
                r.headers_mut().insert("authorization", v);
            }
            r
        });
        let connected = match request {
            Ok(r) => tokio::select! {
                c = tokio_tungstenite::connect_async(r) => c,
                _ = cancelled(&mut cancel) => return,
            },
            Err(e) => {
                tracing::error!("phoenixd websocket url {url}: {e}; push disabled");
                return;
            }
        };
        match connected {
            Ok((mut ws, _)) => {
                tracing::info!("phoenixd websocket connected");
                backoff = WS_BASE_BACKOFF;
                // Catch up on anything that settled while disconnected.
                for p in poll_open(&client, &open).await {
                    if tx.send(p).await.is_err() {
                        return;
                    }
                }
                loop {
                    let next = tokio::select! {
                        n = tokio::time::timeout(WS_SILENCE_TIMEOUT, ws.next()) => n,
                        _ = cancelled(&mut cancel) => {
                            let _ = ws.close(None).await;
                            return;
                        }
                    };
                    match next {
                        Err(_) => {
                            tracing::warn!("phoenixd websocket silent for 60 s; reconnecting");
                            break;
                        }
                        Ok(None) => break,
                        Ok(Some(Err(e))) => {
                            tracing::warn!("phoenixd websocket error: {e}");
                            break;
                        }
                        Ok(Some(Ok(Message::Text(text)))) => {
                            if let Some(p) = settle_push(text.as_str(), &open) {
                                tracing::info!(id = %p.payment_id, "phoenixd pushed a payment");
                                if tx.send(p).await.is_err() {
                                    return;
                                }
                            }
                        }
                        // Pings (answered by tungstenite) and other frames
                        // just prove the link is alive.
                        Ok(Some(Ok(_))) => {}
                    }
                }
            }
            Err(e) => tracing::warn!("phoenixd websocket connect failed: {e}"),
        }
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = cancelled(&mut cancel) => return,
        }
        backoff = (backoff * 2).min(WS_MAX_BACKOFF);
    }
}

#[async_trait]
impl MintPayment for Phoenixd {
    type Err = Error;

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        Ok(SettingsResponse {
            unit: CurrencyUnit::Sat.to_string(),
            bolt11: Some(Bolt11Settings {
                mpp: false,
                amountless: false,
                invoice_description: true,
            }),
            bolt12: None,
            custom: HashMap::new(),
        })
    }

    async fn create_incoming_payment_request(
        &self,
        options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        let IncomingPaymentOptions::Bolt11(o) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };
        let amount_sat = o.amount.convert_to(&CurrencyUnit::Sat)?.value();
        if amount_sat == 0 {
            return Err(Error::Custom("phoenixd invoices need an amount".into()));
        }
        let now = unix_now();
        let expiry_secs = o
            .unix_expiry
            .map(|e| e.saturating_sub(now).max(60))
            .unwrap_or(DEFAULT_EXPIRY_SECS);
        let description = o.description.unwrap_or_else(|| "Hashu".into());

        let created = self
            .client
            .create_invoice(amount_sat, &description, expiry_secs)
            .await?;
        let hash = parse_hash(&created.payment_hash)?;
        let expiry = now + expiry_secs;
        self.open
            .lock()
            .expect("open invoices lock")
            .insert(hash, OpenInvoice { amount_sat, expiry });
        Ok(CreateIncomingPaymentResponse {
            request_lookup_id: PaymentIdentifier::PaymentHash(hash),
            request: created.serialized,
            expiry: Some(expiry),
            extra_json: None,
        })
    }

    async fn get_payment_quote(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        Err(Error::Custom(NO_PAYOUTS.into()))
    }

    async fn make_payment(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        Err(Error::Custom(NO_PAYOUTS.into()))
    }

    async fn wait_payment_event(&self) -> Result<BoxStream<'static, Event>, Self::Err> {
        let (tx, rx) = mpsc::channel(64);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        if let Some(old) = self.cancel.lock().expect("cancel lock").replace(cancel_tx) {
            // A previous stream is replaced: stop its tasks.
            let _ = old.send(true);
        }
        self.waiting.store(true, Ordering::SeqCst);

        tokio::spawn(poll_task(
            self.client.clone(),
            self.open.clone(),
            self.poll_interval,
            tx.clone(),
            cancel_rx.clone(),
        ));
        if let Some(url) = self.websocket_url.clone() {
            tokio::spawn(websocket_task(
                url,
                self.client.clone(),
                self.open.clone(),
                tx,
                cancel_rx,
            ));
        }

        // Ends once both tasks have stopped (on cancel).
        let waiting = self.waiting.clone();
        Ok(
            stream::unfold((rx, waiting), |(mut rx, waiting)| async move {
                match rx.recv().await {
                    Some(p) => Some((Event::PaymentReceived(p), (rx, waiting))),
                    None => {
                        waiting.store(false, Ordering::SeqCst);
                        None
                    }
                }
            })
            .boxed(),
        )
    }

    fn is_wait_invoice_active(&self) -> bool {
        self.waiting.load(Ordering::SeqCst)
    }

    fn cancel_wait_invoice(&self) {
        if let Some(cancel) = self.cancel.lock().expect("cancel lock").as_ref() {
            let _ = cancel.send(true);
        }
    }

    async fn check_incoming_payment_status(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        let PaymentIdentifier::PaymentHash(hash) = payment_identifier else {
            return Ok(Vec::new());
        };
        let Some(p) = self.client.incoming(hash).await? else {
            return Ok(Vec::new());
        };
        if !p.is_paid {
            return Ok(Vec::new());
        }
        let known = self
            .open
            .lock()
            .expect("open invoices lock")
            .get(hash)
            .map(|i| i.amount_sat);
        Ok(vec![settled(hash, known, &p)])
    }

    async fn check_outgoing_payment(
        &self,
        _payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        Err(Error::Custom(NO_PAYOUTS.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::cdk_payment::Bolt11IncomingPaymentOptions;
    use futures::SinkExt;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PASSWORD: &str = "s3cret";
    /// `base64(":s3cret")`
    const AUTH: &str = "Basic OnMzY3JldA==";

    fn fake_invoice(amount_msat: u64) -> (Bolt11Invoice, [u8; 32]) {
        let inv = cdk_fake_wallet::create_fake_invoice(amount_msat, "test".into());
        let hash = *inv.payment_hash().as_ref();
        (inv, hash)
    }

    async fn node(server: &MockServer) -> Phoenixd {
        let mut cfg = PhoenixdConfig::new(server.uri(), PASSWORD);
        cfg.poll_interval = Duration::from_millis(20);
        Phoenixd::new(cfg).unwrap()
    }

    fn bolt11(sats: u64) -> IncomingPaymentOptions {
        IncomingPaymentOptions::Bolt11(Bolt11IncomingPaymentOptions {
            description: Some("Hashu: 10 TiH".into()),
            amount: Amount::new(sats, CurrencyUnit::Sat),
            unix_expiry: Some(unix_now() + 600),
        })
    }

    async fn mock_create(server: &MockServer, inv: &Bolt11Invoice, hash: &[u8; 32]) {
        Mock::given(method("POST"))
            .and(path("/createinvoice"))
            .and(header("authorization", AUTH))
            .and(body_string_contains("amountSat=20"))
            .and(body_string_contains("description=Hashu"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "amountSat": 20,
                "paymentHash": hex::encode(hash),
                "serialized": inv.to_string(),
            })))
            .mount(server)
            .await;
    }

    async fn mock_incoming(server: &MockServer, hash: &[u8; 32], body: serde_json::Value) {
        Mock::given(method("GET"))
            .and(path(format!("/payments/incoming/{}", hex::encode(hash))))
            .and(header("authorization", AUTH))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
    }

    #[test]
    fn config_redacts_password_and_trims_url() {
        let cfg = PhoenixdConfig::new("http://x:9740/", PASSWORD);
        assert_eq!(cfg.url, "http://x:9740");
        assert!(!format!("{cfg:?}").contains(PASSWORD));
        assert_eq!(cfg.expect_chain.as_deref(), Some("mainnet"));
    }

    #[tokio::test]
    async fn creates_invoice_for_the_priced_sats() {
        let server = MockServer::start().await;
        let (inv, hash) = fake_invoice(20_000);
        mock_create(&server, &inv, &hash).await;

        let r = node(&server)
            .await
            .create_incoming_payment_request(bolt11(20))
            .await
            .unwrap();
        assert_eq!(r.request_lookup_id, PaymentIdentifier::PaymentHash(hash));
        assert_eq!(r.request, inv.to_string());
    }

    #[tokio::test]
    async fn credits_invoiced_amount_not_net_of_channel_fees() {
        let server = MockServer::start().await;
        let (inv, hash) = fake_invoice(20_000);
        mock_create(&server, &inv, &hash).await;
        // phoenixd took a channel-opening fee: only 5 of 20 sats received.
        mock_incoming(
            &server,
            &hash,
            serde_json::json!({"isPaid": true, "receivedSat": 5, "invoice": inv.to_string()}),
        )
        .await;

        let n = node(&server).await;
        n.create_incoming_payment_request(bolt11(20)).await.unwrap();
        let got = n
            .check_incoming_payment_status(&PaymentIdentifier::PaymentHash(hash))
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].payment_amount,
            Amount::new(20_000, CurrencyUnit::Msat)
        );
    }

    /// A fresh node with no channel holds small payments as fee credit
    /// (lightning-kmp `Part.FeeCredit`) until they cover the channel open.
    /// The invoice still settles (`completedAt` is set, so `isPaid`), the
    /// buyer's payment succeeds, and the mint must credit the whole invoice
    /// even though none of it is spendable balance yet.
    #[tokio::test]
    async fn fee_credit_payment_is_paid_in_full() {
        let server = MockServer::start().await;
        let (inv, hash) = fake_invoice(20_000);
        mock_create(&server, &inv, &hash).await;
        mock_incoming(
            &server,
            &hash,
            serde_json::json!({
                "paymentHash": hex::encode(hash),
                "isPaid": true,
                "receivedSat": 0,
                "fees": 0,
                "invoice": inv.to_string(),
            }),
        )
        .await;

        let n = node(&server).await;
        n.create_incoming_payment_request(bolt11(20)).await.unwrap();
        let got = n
            .check_incoming_payment_status(&PaymentIdentifier::PaymentHash(hash))
            .await
            .unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].payment_amount,
            Amount::new(20_000, CurrencyUnit::Msat)
        );
    }

    #[tokio::test]
    async fn after_restart_reads_amount_from_the_invoice() {
        let server = MockServer::start().await;
        let (inv, hash) = fake_invoice(20_000);
        mock_incoming(
            &server,
            &hash,
            serde_json::json!({"isPaid": true, "receivedSat": 5, "invoice": inv.to_string()}),
        )
        .await;
        // A fresh node that never issued this invoice.
        let got = node(&server)
            .await
            .check_incoming_payment_status(&PaymentIdentifier::PaymentHash(hash))
            .await
            .unwrap();
        assert_eq!(
            got[0].payment_amount,
            Amount::new(20_000, CurrencyUnit::Msat)
        );
    }

    #[tokio::test]
    async fn unpaid_or_unknown_reports_nothing() {
        let server = MockServer::start().await;
        let (_, unpaid) = fake_invoice(1000);
        mock_incoming(&server, &unpaid, serde_json::json!({"isPaid": false})).await;
        let n = node(&server).await;
        for h in [unpaid, [1; 32]] {
            // [1; 32] is unknown: wiremock answers 404.
            let id = PaymentIdentifier::PaymentHash(h);
            assert!(n
                .check_incoming_payment_status(&id)
                .await
                .unwrap()
                .is_empty());
        }
    }

    #[tokio::test]
    async fn poller_emits_one_event_per_paid_invoice() {
        let server = MockServer::start().await;
        let (inv, hash) = fake_invoice(20_000);
        mock_create(&server, &inv, &hash).await;
        mock_incoming(
            &server,
            &hash,
            serde_json::json!({"isPaid": true, "receivedSat": 20}),
        )
        .await;

        let n = node(&server).await;
        n.create_incoming_payment_request(bolt11(20)).await.unwrap();
        let mut events = n.wait_payment_event().await.unwrap();
        assert!(n.is_wait_invoice_active());
        let Event::PaymentReceived(p) = events.next().await.unwrap();
        assert_eq!(p.payment_identifier, PaymentIdentifier::PaymentHash(hash));
        assert_eq!(p.payment_amount, Amount::new(20_000, CurrencyUnit::Msat));

        // Settled invoices are no longer polled, and cancel ends the stream.
        n.cancel_wait_invoice();
        assert!(events.next().await.is_none());
        assert!(!n.is_wait_invoice_active());
    }

    #[test]
    fn websocket_url_follows_the_http_url() {
        assert_eq!(websocket_url_for("http://h:9740/"), "ws://h:9740/websocket");
        assert_eq!(websocket_url_for("https://h"), "wss://h/websocket");
        let cfg = PhoenixdConfig::new("http://h:9740", PASSWORD);
        assert_eq!(cfg.websocket_url.as_deref(), Some("ws://h:9740/websocket"));
    }

    /// A fake phoenixd push socket. Serves `sessions` connections in turn,
    /// sending each one's frames (an empty session hangs up at once), and
    /// reports the Authorization header of every connection.
    #[allow(clippy::result_large_err)] // tungstenite's callback signature
    async fn push_socket(
        sessions: Vec<Vec<String>>,
    ) -> (String, mpsc::UnboundedReceiver<Option<String>>) {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/websocket", listener.local_addr().unwrap());
        let (auth_tx, auth_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            for frames in sessions {
                let (tcp, _) = listener.accept().await.unwrap();
                let auth_tx = auth_tx.clone();
                let mut ws = tokio_tungstenite::accept_hdr_async(
                    tcp,
                    move |req: &Request, resp: Response| {
                        let auth = req
                            .headers()
                            .get("authorization")
                            .and_then(|v| v.to_str().ok())
                            .map(String::from);
                        let _ = auth_tx.send(auth);
                        Ok(resp)
                    },
                )
                .await
                .unwrap();
                if frames.is_empty() {
                    let _ = ws.close(None).await;
                    continue;
                }
                for f in frames {
                    ws.send(Message::text(f)).await.unwrap();
                }
                // Hold the session open, as phoenixd does between payments.
                tokio::spawn(async move { while ws.next().await.is_some() {} });
            }
        });
        (url, auth_rx)
    }

    fn pushed(hash: &[u8; 32], amount_sat: u64) -> String {
        serde_json::json!({
            "type": "payment_received",
            "amountSat": amount_sat,
            "paymentHash": hex::encode(hash),
        })
        .to_string()
    }

    /// Polling is effectively off (1 h), so only the push can settle.
    async fn push_only_node(server: &MockServer, ws_url: String) -> Phoenixd {
        let mut cfg = PhoenixdConfig::new(server.uri(), PASSWORD);
        cfg.poll_interval = Duration::from_secs(3600);
        cfg.websocket_url = Some(ws_url);
        Phoenixd::new(cfg).unwrap()
    }

    #[tokio::test]
    async fn websocket_push_settles_instantly() {
        let server = MockServer::start().await;
        let (inv, hash) = fake_invoice(20_000);
        mock_create(&server, &inv, &hash).await;
        let (ws_url, mut auth) = push_socket(vec![vec![
            r#"{"type":"something_else"}"#.into(),
            pushed(&[9; 32], 5),
            // phoenixd took a channel fee: 12 of 20 sats arrived.
            pushed(&hash, 12),
        ]])
        .await;

        let n = push_only_node(&server, ws_url).await;
        n.create_incoming_payment_request(bolt11(20)).await.unwrap();
        let mut events = n.wait_payment_event().await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("push should arrive without waiting for a poll")
            .unwrap();
        let Event::PaymentReceived(p) = got;
        assert_eq!(p.payment_identifier, PaymentIdentifier::PaymentHash(hash));
        assert_eq!(p.payment_amount, Amount::new(20_000, CurrencyUnit::Msat));
        assert_eq!(auth.recv().await.unwrap().as_deref(), Some(AUTH));
        // Settled: no longer open, so a later poll can't credit it again.
        assert!(n.open.lock().unwrap().is_empty());

        n.cancel_wait_invoice();
        let end = tokio::time::timeout(Duration::from_secs(5), events.next()).await;
        assert!(matches!(end, Ok(None)));
    }

    #[tokio::test]
    async fn websocket_reconnects_after_a_drop() {
        let server = MockServer::start().await;
        let (inv, hash) = fake_invoice(20_000);
        mock_create(&server, &inv, &hash).await;
        // First session hangs up at once; the second carries the payment.
        let (ws_url, mut auth) = push_socket(vec![vec![], vec![pushed(&hash, 20)]]).await;

        let n = push_only_node(&server, ws_url).await;
        n.create_incoming_payment_request(bolt11(20)).await.unwrap();
        let mut events = n.wait_payment_event().await.unwrap();

        let got = tokio::time::timeout(Duration::from_secs(10), events.next())
            .await
            .expect("reconnect should deliver the payment")
            .unwrap();
        let Event::PaymentReceived(p) = got;
        assert_eq!(p.payment_identifier, PaymentIdentifier::PaymentHash(hash));
        assert!(auth.recv().await.is_some());
        assert!(auth.recv().await.is_some());
        n.cancel_wait_invoice();
    }

    #[test]
    fn push_frames_settle_only_open_invoices_once() {
        let open = Mutex::new(HashMap::new());
        let hash = [3; 32];
        open.lock().unwrap().insert(
            hash,
            OpenInvoice {
                amount_sat: 20,
                expiry: unix_now() + 600,
            },
        );
        assert!(settle_push("not json", &open).is_none());
        assert!(settle_push(&pushed(&[4; 32], 20), &open).is_none());
        let p = settle_push(&pushed(&hash, 7), &open).unwrap();
        assert_eq!(p.payment_amount, Amount::new(20_000, CurrencyUnit::Msat));
        assert!(settle_push(&pushed(&hash, 7), &open).is_none());
    }

    #[tokio::test]
    async fn connect_checks_chain() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/getinfo"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "nodeId": "02abc", "chain": "testnet", "channels": []
            })))
            .mount(&server)
            .await;
        let cfg = PhoenixdConfig::new(server.uri(), PASSWORD);
        let err = Phoenixd::connect(cfg.clone()).await.unwrap_err();
        assert!(err.to_string().contains("expected mainnet"));

        let any = PhoenixdConfig {
            expect_chain: None,
            ..cfg
        };
        assert!(Phoenixd::connect(any).await.is_ok());
    }

    #[tokio::test]
    async fn refuses_payouts_and_other_methods() {
        let server = MockServer::start().await;
        let n = node(&server).await;
        let id = PaymentIdentifier::PaymentHash([0; 32]);
        assert!(n.check_outgoing_payment(&id).await.is_err());
        let b12 = IncomingPaymentOptions::Bolt12(Box::default());
        assert!(n.create_incoming_payment_request(b12).await.is_err());
        assert!(n.get_settings().await.unwrap().bolt12.is_none());
    }
}
