//! phoenixd Lightning node: receive real (mainnet) sats for `THH` quotes.
//! ARCHITECTURE.md §4.5.
//!
//! [phoenixd](https://phoenix.acinq.co/server) is ACINQ's self-custodial,
//! receive-friendly Lightning daemon with automatic inbound liquidity. It is
//! the same node setup the overpay marketplace runs (`docker/phoenixd`,
//! `PHOENIXD_URL` / `PHOENIXD_PASSWORD`), so a Hashu operator can reuse it.
//!
//! [`Phoenixd`] is a cdk [`MintPayment`] backend in sats. Wrap it with
//! [`crate::lightning::lightning_issuer`] to sell `THH` over bolt11:
//!
//! - **invoice**: `POST /createinvoice` (form `amountSat`, `description`,
//!   `expirySeconds`), HTTP Basic auth with an empty user and the node's
//!   http-password.
//! - **settlement**: `GET /payments/incoming/{paymentHash}`. Open invoices
//!   are polled every `poll_interval` (5 s by default) to feed
//!   `wait_payment_event`, and cdk also checks on demand when a wallet
//!   asks about its quote.
//! - **amount credited**: the **invoiced** amount, not `receivedSat`. When
//!   phoenixd opens a channel to receive a payment it deducts the opening
//!   fee from `receivedSat`. The payer still paid the whole invoice, so the
//!   shortfall is the operator's cost, not an underpayment (same rule as
//!   overpay's `LightningPaymentChecker`).
//! - **outgoing**: refused. The mint never pays out.
//!
//! **Payment network ≠ redemption network.** phoenixd runs on mainnet: buyers
//! pay real sats. The pools that `THH` is later redeemed against are chosen
//! per redemption and may be on any network, Testnet4 included. Nothing here
//! looks at pool networks; [`Phoenixd::connect`] only checks the node's own
//! chain against [`PhoenixdConfig::expect_chain`].

use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cdk::cdk_payment::{
    Bolt11Settings, CreateIncomingPaymentResponse, Error, Event, IncomingPaymentOptions,
    MakePaymentResponse, MintPayment, OutgoingPaymentOptions, PaymentIdentifier,
    PaymentQuoteResponse, SettingsResponse, WaitPaymentResponse,
};
use cdk::nuts::CurrencyUnit;
use cdk::{Amount, Bolt11Invoice};
use futures::stream::{self, BoxStream, StreamExt};
use serde::Deserialize;

/// Default poll cadence for open invoices.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default invoice lifetime when cdk doesn't give an expiry.
pub const DEFAULT_EXPIRY_SECS: u64 = 3600;

/// Open invoices are still polled this long after they expire, in case a
/// payment landed right at the deadline.
const EXPIRY_GRACE_SECS: u64 = 300;

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
}

impl std::fmt::Debug for PhoenixdConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PhoenixdConfig")
            .field("url", &self.url)
            .field("password", &"<redacted>")
            .field("poll_interval", &self.poll_interval)
            .field("expect_chain", &self.expect_chain)
            .finish()
    }
}

impl PhoenixdConfig {
    pub fn new(url: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            url: url.into().trim_end_matches('/').to_string(),
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
#[derive(Debug, Deserialize)]
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
    open: Arc<Mutex<HashMap<[u8; 32], OpenInvoice>>>,
    waiting: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
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
            open: Arc::default(),
            waiting: Arc::default(),
            cancelled: Arc::default(),
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
        self.cancelled.store(false, Ordering::SeqCst);
        self.waiting.store(true, Ordering::SeqCst);
        let state = (
            self.client.clone(),
            self.open.clone(),
            self.cancelled.clone(),
            self.waiting.clone(),
            self.poll_interval,
            VecDeque::<WaitPaymentResponse>::new(),
        );
        Ok(stream::unfold(
            state,
            |(client, open, cancelled, waiting, every, mut ready)| async move {
                loop {
                    if let Some(p) = ready.pop_front() {
                        let next = (client, open, cancelled, waiting, every, ready);
                        return Some((Event::PaymentReceived(p), next));
                    }
                    if cancelled.load(Ordering::SeqCst) {
                        waiting.store(false, Ordering::SeqCst);
                        return None;
                    }
                    tokio::time::sleep(every).await;
                    ready.extend(poll_open(&client, &open).await);
                }
            },
        )
        .boxed())
    }

    fn is_wait_invoice_active(&self) -> bool {
        self.waiting.load(Ordering::SeqCst)
    }

    fn cancel_wait_invoice(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
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
            description: Some("Hashu: 10 THH".into()),
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
