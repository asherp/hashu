//! Receive-only Lightning rail that invoices a supplier's own wallet
//! through their Lightning address.
//!
//! A supplier mint (`alice.mint.hashu.io`) never holds buyer funds: each
//! THH sale is an invoice from Alice's own wallet. For a Lightning address
//! `alice@wallet.example`:
//!
//! 1. LUD-16: `GET https://wallet.example/.well-known/lnurlp/alice` gives the
//!    LNURL-pay parameters (callback, min and max amount).
//! 2. LUD-06: `GET <callback>?amount=<msat>` returns a bolt11 invoice, and
//!    LUD-21 adds a `verify` URL for it.
//! 3. LUD-21: `GET <verify>` says whether the invoice is settled. The rail
//!    polls it until the invoice settles or expires, and cdk marks the
//!    quote paid.
//!
//! A provider without LUD-21 can't tell the mint a payment arrived, so its
//! addresses are refused when the invoice comes back without `verify`
//! ([`probe`] checks this at sign-up).
//!
//! Open invoices are kept in a JSON file next to the mint's database, so a
//! payment made while the mint restarts is still seen.

use std::collections::HashMap;
use std::path::PathBuf;
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
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

const NO_PAYOUTS: &str = "Lightning address rail is receive-only; this mint does not pay out";
/// How long a settled-or-not check of an expired invoice keeps going.
const EXPIRY_GRACE_SECS: u64 = 300;
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

/// A Lightning address, `user@domain`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LightningAddress {
    pub user: String,
    pub domain: String,
}

impl FromStr for LightningAddress {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim().to_ascii_lowercase();
        let (user, domain) = s
            .split_once('@')
            .ok_or_else(|| format!("{s:?} is not a Lightning address (user@domain)"))?;
        let user_ok = !user.is_empty()
            && user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.+".contains(&b));
        let domain_ok = domain.contains('.')
            && !domain.starts_with('.')
            && !domain.ends_with('.')
            && domain
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b));
        if !user_ok || !domain_ok {
            return Err(format!("{s:?} is not a Lightning address (user@domain)"));
        }
        Ok(Self {
            user: user.into(),
            domain: domain.into(),
        })
    }
}

impl std::fmt::Display for LightningAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.user, self.domain)
    }
}

impl LightningAddress {
    /// LUD-16 well-known URL.
    pub fn lnurlp_url(&self) -> String {
        format!("https://{}/.well-known/lnurlp/{}", self.domain, self.user)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PayParams {
    callback: String,
    min_sendable: u64,
    max_sendable: u64,
    #[serde(default)]
    tag: String,
}

#[derive(Debug, Deserialize)]
struct InvoiceResponse {
    pr: String,
    #[serde(default)]
    verify: Option<String>,
}

#[derive(Debug, Deserialize)]
struct VerifyResponse {
    #[serde(default)]
    settled: bool,
    #[serde(default)]
    preimage: Option<String>,
}

/// An LNURL error response: `{"status":"ERROR","reason":"..."}`.
#[derive(Debug, Deserialize)]
struct LnurlStatus {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenInvoice {
    amount_msat: u64,
    verify: String,
    expiry: u64,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// What a Lightning address can be invoiced for, checked at sign-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub min_msat: u64,
    pub max_msat: u64,
}

/// Fetch `addr`'s LNURL-pay parameters and request the smallest invoice
/// it allows, to check the address works and its provider supports LUD-21
/// `verify`. The invoice is never shown to anyone and simply expires.
pub async fn probe(addr: &LightningAddress) -> Result<Probe, String> {
    let client = http_client()?;
    probe_at(&client, &addr.lnurlp_url()).await
}

async fn probe_at(client: &reqwest::Client, lnurlp_url: &str) -> Result<Probe, String> {
    let params = pay_params(client, lnurlp_url).await?;
    let inv = request_invoice(client, &params, params.min_sendable.max(1000)).await?;
    if inv.verify.is_none() {
        return Err(
            "this wallet does not support payment verification (LUD-21); \
             use a Lightning address from a provider that does"
                .into(),
        );
    }
    Ok(Probe {
        min_msat: params.min_sendable,
        max_msat: params.max_sendable,
    })
}

fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, String> {
    let body = client
        .get(url)
        .send()
        .await
        .map_err(|e| format!("{url}: {e}"))?
        .text()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    if let Ok(LnurlStatus {
        status: Some(s),
        reason,
    }) = serde_json::from_str::<LnurlStatus>(&body)
    {
        if s.eq_ignore_ascii_case("error") {
            return Err(reason.unwrap_or_else(|| "wallet returned an error".into()));
        }
    }
    serde_json::from_str(&body).map_err(|e| format!("{url}: unexpected answer: {e}"))
}

async fn pay_params(client: &reqwest::Client, lnurlp_url: &str) -> Result<PayParams, String> {
    let p: PayParams = get_json(client, lnurlp_url).await?;
    if !p.tag.is_empty() && p.tag != "payRequest" {
        return Err(format!("{lnurlp_url} is not an LNURL-pay endpoint"));
    }
    Ok(p)
}

async fn request_invoice(
    client: &reqwest::Client,
    params: &PayParams,
    amount_msat: u64,
) -> Result<InvoiceResponse, String> {
    if amount_msat < params.min_sendable || amount_msat > params.max_sendable {
        return Err(format!(
            "the wallet accepts {} to {} sats per invoice, not {}",
            params.min_sendable.div_ceil(1000),
            params.max_sendable / 1000,
            amount_msat / 1000
        ));
    }
    let sep = if params.callback.contains('?') {
        '&'
    } else {
        '?'
    };
    let url = format!("{}{sep}amount={amount_msat}", params.callback);
    get_json(client, &url).await
}

/// The rail. See the module docs.
pub struct LnAddressRail {
    lnurlp_url: String,
    client: reqwest::Client,
    open: Arc<Mutex<HashMap<[u8; 32], OpenInvoice>>>,
    path: Option<PathBuf>,
    poll_interval: Duration,
    waiting: Arc<AtomicBool>,
    cancel: Mutex<Option<watch::Sender<bool>>>,
}

impl std::fmt::Debug for LnAddressRail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LnAddressRail")
            .field("lnurlp_url", &self.lnurlp_url)
            .finish_non_exhaustive()
    }
}

impl LnAddressRail {
    /// Rail invoicing `addr`. Open invoices persist in `path` when given.
    pub fn new(addr: &LightningAddress, path: Option<PathBuf>) -> Result<Self, Error> {
        Self::at(addr.lnurlp_url(), path)
    }

    /// Rail for an LNURL-pay endpoint URL (tests point it at a local server).
    pub fn at(lnurlp_url: String, path: Option<PathBuf>) -> Result<Self, Error> {
        let open = match &path {
            Some(p) if p.exists() => {
                let raw = std::fs::read(p).map_err(|e| Error::Custom(format!("{p:?}: {e}")))?;
                let list: Vec<(String, OpenInvoice)> = serde_json::from_slice(&raw)
                    .map_err(|e| Error::Custom(format!("{p:?}: {e}")))?;
                list.into_iter()
                    .filter_map(|(h, i)| parse_hash(&h).ok().map(|h| (h, i)))
                    .collect()
            }
            _ => HashMap::new(),
        };
        Ok(Self {
            lnurlp_url,
            client: http_client().map_err(Error::Custom)?,
            open: Arc::new(Mutex::new(open)),
            path,
            poll_interval: Duration::from_secs(3),
            waiting: Arc::default(),
            cancel: Mutex::new(None),
        })
    }

    pub fn with_poll_interval(mut self, every: Duration) -> Self {
        self.poll_interval = every;
        self
    }

    fn save(&self) {
        save(&self.path, &self.open);
    }
}

fn save(path: &Option<PathBuf>, open: &Mutex<HashMap<[u8; 32], OpenInvoice>>) {
    let Some(path) = path else { return };
    let list: Vec<(String, OpenInvoice)> = open
        .lock()
        .expect("open invoices lock")
        .iter()
        .map(|(h, i)| (hex::encode(h), i.clone()))
        .collect();
    let tmp = path.with_extension("tmp");
    let r = serde_json::to_vec(&list)
        .map_err(|e| e.to_string())
        .and_then(|b| std::fs::write(&tmp, b).map_err(|e| e.to_string()))
        .and_then(|_| std::fs::rename(&tmp, path).map_err(|e| e.to_string()));
    if let Err(e) = r {
        tracing::warn!(?path, error = %e, "could not save open Lightning address invoices");
    }
}

fn parse_hash(s: &str) -> Result<[u8; 32], Error> {
    hex::decode(s)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Custom(format!("bad payment hash {s:?}")))
}

/// Check one open invoice. `Some(true)` settled, `Some(false)` not yet,
/// `None` the check failed.
async fn verify(client: &reqwest::Client, hash: &[u8; 32], inv: &OpenInvoice) -> Option<bool> {
    match get_json::<VerifyResponse>(client, &inv.verify).await {
        Ok(v) if v.settled => {
            // A preimage, when given, must match the invoice.
            if let Some(p) = v.preimage.as_deref().and_then(|p| hex::decode(p).ok()) {
                if Sha256::digest(&p).as_slice() != hash {
                    tracing::warn!(hash = %hex::encode(hash), "verify returned a preimage that does not match; ignoring");
                    return Some(false);
                }
            }
            Some(true)
        }
        Ok(_) => Some(false),
        Err(e) => {
            tracing::debug!(hash = %hex::encode(hash), error = %e, "verify failed");
            None
        }
    }
}

fn settled(hash: &[u8; 32], amount_msat: u64) -> WaitPaymentResponse {
    WaitPaymentResponse {
        payment_identifier: PaymentIdentifier::PaymentHash(*hash),
        payment_amount: Amount::new(amount_msat, CurrencyUnit::Msat),
        payment_id: hex::encode(hash),
    }
}

/// Check every open invoice once; settled ones are removed and returned,
/// and ones long expired are dropped.
async fn poll_open(
    client: &reqwest::Client,
    open: &Mutex<HashMap<[u8; 32], OpenInvoice>>,
    path: &Option<PathBuf>,
) -> Vec<WaitPaymentResponse> {
    let now = unix_now();
    let snapshot: Vec<_> = open
        .lock()
        .expect("open invoices lock")
        .iter()
        .map(|(h, i)| (*h, i.clone()))
        .collect();
    let mut out = Vec::new();
    let mut done = Vec::new();
    for (hash, inv) in snapshot {
        match verify(client, &hash, &inv).await {
            Some(true) => {
                out.push(settled(&hash, inv.amount_msat));
                done.push(hash);
            }
            _ if now > inv.expiry + EXPIRY_GRACE_SECS => done.push(hash),
            _ => {}
        }
    }
    if !done.is_empty() {
        let mut o = open.lock().expect("open invoices lock");
        for h in &done {
            o.remove(h);
        }
        drop(o);
        save(path, open);
    }
    out
}

#[async_trait]
impl MintPayment for LnAddressRail {
    type Err = Error;

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        Ok(SettingsResponse {
            unit: CurrencyUnit::Sat.to_string(),
            bolt11: Some(Bolt11Settings {
                mpp: false,
                amountless: false,
                invoice_description: false,
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
        let amount_msat = o.amount.convert_to(&CurrencyUnit::Msat)?.value();
        if amount_msat == 0 {
            return Err(Error::Custom("invoices need an amount".into()));
        }
        let params = pay_params(&self.client, &self.lnurlp_url)
            .await
            .map_err(Error::Custom)?;
        let resp = request_invoice(&self.client, &params, amount_msat)
            .await
            .map_err(Error::Custom)?;
        let verify = resp.verify.ok_or_else(|| {
            Error::Custom(
                "the supplier's wallet does not support payment verification (LUD-21)".into(),
            )
        })?;
        let invoice = Bolt11Invoice::from_str(&resp.pr)
            .map_err(|e| Error::Custom(format!("wallet returned a bad invoice: {e}")))?;
        if invoice.amount_milli_satoshis() != Some(amount_msat) {
            return Err(Error::Custom(format!(
                "wallet invoiced {:?} msat, not {amount_msat}",
                invoice.amount_milli_satoshis()
            )));
        }
        let hash = *invoice.payment_hash().as_ref();
        let expiry = invoice
            .expires_at()
            .map(|d| d.as_secs())
            .unwrap_or_else(|| unix_now() + 600);
        self.open.lock().expect("open invoices lock").insert(
            hash,
            OpenInvoice {
                amount_msat,
                verify,
                expiry,
            },
        );
        self.save();
        Ok(CreateIncomingPaymentResponse {
            request_lookup_id: PaymentIdentifier::PaymentHash(hash),
            request: resp.pr,
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
        let (cancel_tx, mut cancel_rx) = watch::channel(false);
        if let Some(old) = self.cancel.lock().expect("cancel lock").replace(cancel_tx) {
            let _ = old.send(true);
        }
        self.waiting.store(true, Ordering::SeqCst);
        let (client, open, path, every) = (
            self.client.clone(),
            self.open.clone(),
            self.path.clone(),
            self.poll_interval,
        );
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {}
                    r = cancel_rx.changed() => if r.is_err() || *cancel_rx.borrow() { return },
                }
                for p in poll_open(&client, &open, &path).await {
                    if tx.send(p).await.is_err() {
                        return;
                    }
                }
            }
        });
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
        let inv = self
            .open
            .lock()
            .expect("open invoices lock")
            .get(hash)
            .cloned();
        let Some(inv) = inv else {
            return Ok(Vec::new());
        };
        match verify(&self.client, hash, &inv).await {
            Some(true) => Ok(vec![settled(hash, inv.amount_msat)]),
            _ => Ok(Vec::new()),
        }
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
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parses_lightning_addresses() {
        let a: LightningAddress = " Alice@Wallet.Example ".parse().unwrap();
        assert_eq!(a.to_string(), "alice@wallet.example");
        assert_eq!(
            a.lnurlp_url(),
            "https://wallet.example/.well-known/lnurlp/alice"
        );
        for bad in [
            "alice",
            "@wallet.example",
            "alice@",
            "alice@localhost",
            "a b@x.io",
            "a@x.io/p",
        ] {
            assert!(bad.parse::<LightningAddress>().is_err(), "{bad}");
        }
    }

    async fn wallet(server: &MockServer, verify: bool, amount_msat: u64) -> [u8; 32] {
        let inv = cdk_fake_wallet::create_fake_invoice(amount_msat, "test".into());
        let hash = *inv.payment_hash().as_ref();
        Mock::given(method("GET"))
            .and(path("/.well-known/lnurlp/alice"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "tag": "payRequest",
                "callback": format!("{}/cb", server.uri()),
                "minSendable": 1000,
                "maxSendable": 100_000_000,
                "metadata": "[]",
            })))
            .mount(server)
            .await;
        let mut body = serde_json::json!({ "pr": inv.to_string(), "routes": [] });
        if verify {
            body["verify"] = format!("{}/verify/{}", server.uri(), hex::encode(hash)).into();
        }
        Mock::given(method("GET"))
            .and(path("/cb"))
            .and(query_param("amount", amount_msat.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(server)
            .await;
        hash
    }

    async fn mock_verify(server: &MockServer, hash: &[u8; 32], settled: bool) {
        Mock::given(method("GET"))
            .and(path(format!("/verify/{}", hex::encode(hash))))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "OK", "settled": settled, "preimage": null, "pr": "",
            })))
            .mount(server)
            .await;
    }

    fn bolt11(sats: u64) -> IncomingPaymentOptions {
        IncomingPaymentOptions::Bolt11(Bolt11IncomingPaymentOptions {
            description: None,
            amount: Amount::new(sats, CurrencyUnit::Sat),
            unix_expiry: None,
        })
    }

    fn url(server: &MockServer) -> String {
        format!("{}/.well-known/lnurlp/alice", server.uri())
    }

    #[tokio::test]
    async fn invoices_the_suppliers_wallet_and_sees_it_settle() {
        let server = MockServer::start().await;
        let hash = wallet(&server, true, 21_000).await;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("open.json");
        let rail = LnAddressRail::at(url(&server), Some(file.clone()))
            .unwrap()
            .with_poll_interval(Duration::from_millis(20));
        let r = rail
            .create_incoming_payment_request(bolt11(21))
            .await
            .unwrap();
        assert_eq!(r.request_lookup_id, PaymentIdentifier::PaymentHash(hash));
        assert!(file.exists());

        mock_verify(&server, &hash, false).await;
        assert!(rail
            .check_incoming_payment_status(&r.request_lookup_id)
            .await
            .unwrap()
            .is_empty());

        // A restart still knows the invoice.
        let rail = LnAddressRail::at(url(&server), Some(file))
            .unwrap()
            .with_poll_interval(Duration::from_millis(20));
        server.reset().await;
        mock_verify(&server, &hash, true).await;
        let mut events = rail.wait_payment_event().await.unwrap();
        let Event::PaymentReceived(p) = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(p.payment_identifier, PaymentIdentifier::PaymentHash(hash));
        assert_eq!(p.payment_amount, Amount::new(21_000, CurrencyUnit::Msat));
    }

    #[tokio::test]
    async fn refuses_wallets_without_verify() {
        let server = MockServer::start().await;
        wallet(&server, false, 1000).await;
        let client = http_client().unwrap();
        let e = probe_at(&client, &url(&server)).await.unwrap_err();
        assert!(e.contains("LUD-21"), "{e}");
        let rail = LnAddressRail::at(url(&server), None).unwrap();
        assert!(rail
            .create_incoming_payment_request(bolt11(1))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn probe_accepts_a_verifying_wallet() {
        let server = MockServer::start().await;
        wallet(&server, true, 1000).await;
        let client = http_client().unwrap();
        let p = probe_at(&client, &url(&server)).await.unwrap();
        assert_eq!((p.min_msat, p.max_msat), (1000, 100_000_000));
    }

    #[tokio::test]
    async fn amounts_outside_the_wallets_range_are_refused() {
        let server = MockServer::start().await;
        wallet(&server, true, 1000).await;
        let rail = LnAddressRail::at(url(&server), None).unwrap();
        let e = rail
            .create_incoming_payment_request(bolt11(200_000))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("accepts"), "{e}");
    }
}
