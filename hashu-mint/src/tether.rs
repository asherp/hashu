//! Stablecoin payment rail: buy `TiH` with dollars, as USDT (Tether) or
//! USDC (Circle) on an EVM chain. ARCHITECTURE.md §4.5.1.
//!
//! Each coin is its own backend instance and its own NUT-04 method
//! (`usdt`, `usdc`); both coins have 6 decimals, so "micro-USDT" below
//! means the token's base unit for either coin.
//!
//! The pitch is USD -> hashrate -> BTC without an exchange: a buyer pays
//! USDT, receives `TiH` ecash, and redeems it as hashrate pointed at their
//! own pool, which pays them in bitcoin. The mint only ever *receives*
//! USDT. Nothing on this rail pays out, so every outgoing call is refused.
//!
//! cdk sees this as a payment backend registered for
//! `(TiH, PaymentMethod::Custom("usdt"))` (or `"usdc"`):
//!
//! - **mint quote** (`create_incoming_payment_request`): the wallet asks for
//!   N `TiH`. A [`TihPricer`] prices that in micro-USDT (USDT has 6
//!   decimals). The price is rounded up to whole cents and a random tag in
//!   the sub-cent digits (1..=9999 micro-USDT, under one cent) makes the
//!   amount unique among open quotes. The quote's `request` is an EIP-681
//!   URI (`ethereum:<token>@<chain>/transfer?address=..&uint256=..`) that
//!   wallets such as MetaMask can scan; `extra_json` carries the same
//!   fields spelled out.
//! - **payment**: a [`UsdtSource`] reports confirmed ERC-20 `Transfer`
//!   logs into the operator's deposit address. A transfer whose value
//!   equals an open quote's amount settles it, and the backend emits
//!   `Event::PaymentReceived` with the amount **in TiH**, so cdk issues
//!   the quoted `TiH`.
//!
//! Matching by exact amount means one deposit address serves every quote
//! and no per-quote key derivation is needed. A transfer that matches no
//! quote (wrong amount, or after the quote was purged) is logged and left
//! for the operator to refund by hand.
//!
//! Real funds: with [`TetherPayments::with_persistence`] open quotes,
//! settled quotes, handled transfers and the next block to scan are
//! written to a JSON file after every change, so a restart neither forgets
//! a buyer's quote nor misses transfers made while the mint was down.
//! Transfers count once they are `confirmations` blocks deep. Remaining
//! limit: [`EvmRpcSource`] trusts its RPC endpoint.

use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cdk::cdk_payment::{
    CreateIncomingPaymentResponse, Error, Event, IncomingPaymentOptions, MakePaymentResponse,
    MintPayment, OutgoingPaymentOptions, PaymentIdentifier, PaymentQuoteResponse, SettingsResponse,
    WaitPaymentResponse,
};
use cdk::nuts::CurrencyUnit;
use cdk::Amount;
use futures::stream::{self, BoxStream};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};

use crate::pricing::{PricingError, TihPricer};

/// Method name as it appears in mint requests and NUT-04 settings.
pub const METHOD: &str = "usdt";
/// Method name of the USDC rail.
pub const USDC_METHOD: &str = "usdc";

/// Tether USD on Ethereum mainnet (6 decimals).
pub const MAINNET_USDT: &str = "0xdAC17F958D2ee523a2206206994597C13D831ec7";
/// Circle USDC on Ethereum mainnet (6 decimals).
pub const MAINNET_USDC: &str = "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48";
pub const MAINNET_CHAIN_ID: u64 = 1;
/// Circle's native USDC on Base mainnet (6 decimals).
pub const BASE_USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";
pub const BASE_CHAIN_ID: u64 = 8453;

/// Which dollar stablecoin a rail takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stablecoin {
    Usdt,
    Usdc,
}

impl Stablecoin {
    /// NUT-04 method name.
    pub fn method(self) -> &'static str {
        match self {
            Stablecoin::Usdt => METHOD,
            Stablecoin::Usdc => USDC_METHOD,
        }
    }

    /// Ticker shown to buyers.
    pub fn symbol(self) -> &'static str {
        match self {
            Stablecoin::Usdt => "USDT",
            Stablecoin::Usdc => "USDC",
        }
    }

    /// The coin's token contract on Ethereum mainnet.
    pub fn mainnet_token(self) -> &'static str {
        match self {
            Stablecoin::Usdt => MAINNET_USDT,
            Stablecoin::Usdc => MAINNET_USDC,
        }
    }
}

/// USDT and USDC both have 6 decimals on every chain they are issued on.
pub const MICRO_PER_USDT: u64 = 1_000_000;

/// Quotes are rounded up to whole cents; the digits below carry the tag.
const MICRO_PER_CENT: u64 = 10_000;

/// keccak256("Transfer(address,address,uint256)"), the ERC-20 event topic.
pub const TRANSFER_TOPIC: &str =
    "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";

/// Default time a buyer has to pay a quote.
pub const DEFAULT_QUOTE_TTL: Duration = Duration::from_secs(60 * 60);

/// Default poll cadence for new transfers.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// How long a quote stays matchable after it expires, so a late payment
/// still settles instead of needing a manual refund.
const GRACE_AFTER_EXPIRY: Duration = Duration::from_secs(24 * 60 * 60);

const TIH: &str = hashu_core::denom::UNIT_TIH;

/// cdk lowercases custom units whenever it serialises them (mint info,
/// database, wire), so the in-memory unit must be lowercase to compare
/// equal with what comes back.
fn tih_unit() -> CurrencyUnit {
    crate::melt::HashrateMelter::unit()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Normalise a `0x`-prefixed 20-byte hex address to lowercase.
pub fn parse_address(s: &str) -> Result<String, Error> {
    let hex_part = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .ok_or_else(|| Error::Custom(format!("address {s} must start with 0x")))?;
    if hex_part.len() != 40 || !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::Custom(format!("address {s} is not 20 bytes of hex")));
    }
    Ok(format!("0x{}", hex_part.to_ascii_lowercase()))
}

/// Format micro-USDT as a decimal string, e.g. `12340017` -> `"12.340017"`.
pub fn format_usdt(micro: u64) -> String {
    format!("{}.{:06}", micro / MICRO_PER_USDT, micro % MICRO_PER_USDT)
}

/// Where a stablecoin is paid: an ERC-20 token on an EVM chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsdtRail {
    pub coin: Stablecoin,
    /// EIP-155 chain id, e.g. 1 for Ethereum mainnet.
    pub chain_id: u64,
    /// Token contract.
    pub token: String,
    /// Operator's address that receives every payment.
    pub deposit_address: String,
}

impl UsdtRail {
    pub fn new(
        coin: Stablecoin,
        chain_id: u64,
        token: &str,
        deposit_address: &str,
    ) -> Result<Self, Error> {
        Ok(Self {
            coin,
            chain_id,
            token: parse_address(token)?,
            deposit_address: parse_address(deposit_address)?,
        })
    }

    /// EIP-681 ERC-20 transfer URI for `amount_micro`.
    pub fn payment_uri(&self, amount_micro: u64) -> String {
        format!(
            "ethereum:{}@{}/transfer?address={}&uint256={}",
            self.token, self.chain_id, self.deposit_address, amount_micro
        )
    }
}

/// Details of a USDT mint quote, sent to the wallet as `extra_json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsdtPaymentRequest {
    /// `USDT` or `USDC`.
    #[serde(default = "default_symbol")]
    pub symbol: String,
    pub chain_id: u64,
    pub token: String,
    pub to: String,
    /// Exact amount to send, as a decimal string of the coin (the name
    /// says USDT for compatibility; it is USDC on the USDC rail). Serialized as
    /// `amount_usdt`: cdk flattens `extra_json` into the NUT-04 quote,
    /// whose own `amount` is the TiH amount, and a second `amount` key made
    /// cdk wallets fail to parse the quote.
    #[serde(rename = "amount_usdt", alias = "amount")]
    pub amount: String,
    /// Exact amount to send, in micro-USDT (the token's base unit).
    pub amount_micro: u64,
    /// `TiH` issued once the transfer confirms.
    pub amount_tih: u64,
    /// Unix time after which the quote is no longer offered.
    pub expiry: u64,
}

fn default_symbol() -> String {
    "USDT".into()
}

/// One confirmed ERC-20 transfer of the rail's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsdtTransfer {
    pub tx_hash: String,
    pub log_index: u64,
    pub block: u64,
    /// Recipient, lowercase `0x` hex.
    pub to: String,
    /// Amount in micro-USDT.
    pub value: u128,
}

/// Source of confirmed USDT transfers into the deposit address.
#[async_trait]
pub trait UsdtSource: Send + Sync + Debug {
    /// Transfers confirmed since `from_block` (inclusive). `None` means
    /// "start from now". Returns the transfers and the next block to ask
    /// for.
    async fn poll(&self, from_block: Option<u64>) -> Result<(Vec<UsdtTransfer>, u64), Error>;
}

/// [`UsdtSource`] over plain EVM JSON-RPC (`eth_blockNumber` +
/// `eth_getLogs`). Works on any EVM chain that carries USDT: Ethereum,
/// Sepolia, Arbitrum, Plasma, and the USDT0 chains.
#[derive(Debug, Clone)]
pub struct EvmRpcSource {
    client: reqwest::Client,
    rpc_url: String,
    rail: UsdtRail,
    confirmations: u64,
    /// Widest block range per `eth_getLogs` call; public RPCs cap this.
    max_range: u64,
}

impl EvmRpcSource {
    pub fn new(rpc_url: impl Into<String>, rail: UsdtRail, confirmations: u64) -> Self {
        Self {
            client: reqwest::Client::new(),
            rpc_url: rpc_url.into(),
            rail,
            confirmations,
            max_range: 1_000,
        }
    }

    async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, Error> {
        let body =
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp: serde_json::Value = self
            .client
            .post(&self.rpc_url)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Custom(format!("{method}: {e}")))?
            .json()
            .await
            .map_err(|e| Error::Custom(format!("{method}: {e}")))?;
        if let Some(err) = resp.get("error") {
            return Err(Error::Custom(format!("{method}: {err}")));
        }
        resp.get("result")
            .cloned()
            .ok_or_else(|| Error::Custom(format!("{method}: no result")))
    }
}

fn parse_quantity(v: &serde_json::Value) -> Result<u128, Error> {
    let s = v
        .as_str()
        .ok_or_else(|| Error::Custom(format!("expected hex quantity, got {v}")))?;
    let digits = s.strip_prefix("0x").unwrap_or(s).trim_start_matches('0');
    if digits.is_empty() {
        return Ok(0);
    }
    u128::from_str_radix(digits, 16).map_err(|e| Error::Custom(format!("bad quantity {s}: {e}")))
}

/// Decode one `eth_getLogs` entry for an ERC-20 `Transfer`.
pub fn parse_transfer_log(log: &serde_json::Value) -> Result<UsdtTransfer, Error> {
    let field = |k: &str| {
        log.get(k)
            .ok_or_else(|| Error::Custom(format!("log missing {k}")))
    };
    let topics = field("topics")?
        .as_array()
        .ok_or_else(|| Error::Custom("topics is not an array".into()))?;
    if topics.len() != 3
        || topics[0].as_str().map(str::to_ascii_lowercase).as_deref() != Some(TRANSFER_TOPIC)
    {
        return Err(Error::Custom("not an ERC-20 Transfer log".into()));
    }
    let to_topic = topics[2]
        .as_str()
        .ok_or_else(|| Error::Custom("bad to topic".into()))?;
    let to = parse_address(&format!(
        "0x{}",
        &to_topic[to_topic.len().saturating_sub(40)..]
    ))?;
    Ok(UsdtTransfer {
        tx_hash: field("transactionHash")?
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase(),
        log_index: parse_quantity(field("logIndex")?)? as u64,
        block: parse_quantity(field("blockNumber")?)? as u64,
        to,
        value: parse_quantity(field("data")?)?,
    })
}

#[async_trait]
impl UsdtSource for EvmRpcSource {
    async fn poll(&self, from_block: Option<u64>) -> Result<(Vec<UsdtTransfer>, u64), Error> {
        let head =
            parse_quantity(&self.rpc("eth_blockNumber", serde_json::json!([])).await?)? as u64;
        let safe = head.saturating_sub(self.confirmations);
        let from = from_block.unwrap_or(safe + 1);
        if from > safe {
            return Ok((Vec::new(), from));
        }
        let to = safe.min(from + self.max_range - 1);
        let to_topic = format!("0x{:0>64}", &self.rail.deposit_address[2..]);
        let logs = self
            .rpc(
                "eth_getLogs",
                serde_json::json!([{
                    "fromBlock": format!("0x{from:x}"),
                    "toBlock": format!("0x{to:x}"),
                    "address": self.rail.token,
                    "topics": [TRANSFER_TOPIC, null, to_topic],
                }]),
            )
            .await?;
        let transfers = logs
            .as_array()
            .ok_or_else(|| Error::Custom("eth_getLogs: result is not an array".into()))?
            .iter()
            .map(parse_transfer_log)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((transfers, to + 1))
    }
}

/// Prices `TiH` at a fixed number of micro-USDT per PiH (1,024 TiH),
/// rounded up. Handy for a demo.
#[derive(Debug, Clone, Copy)]
pub struct FixedUsdtPrice {
    pub micro_per_pih: u64,
}

impl TihPricer for FixedUsdtPrice {
    fn price(&self, amount_tih: u64) -> Result<u64, PricingError> {
        // Same rounding as msat: the unit cost per PiH, pro rata, rounded up.
        hashu_core::denom::msat_for_tih(amount_tih, self.micro_per_pih)
            .ok_or(PricingError::Overflow(amount_tih))
    }
}

/// Prices `TiH` in micro-USDT from a sats pricer (normally the hashprice
/// one the Lightning rail uses) and a BTC/USD rate, so both rails quote
/// the same hashrate at the same price.
#[derive(Debug, Clone)]
pub struct SatsToUsdt {
    pub sats: Arc<dyn TihPricer>,
    /// USD per BTC.
    pub usd_per_btc: f64,
}

impl TihPricer for SatsToUsdt {
    fn price(&self, amount_tih: u64) -> Result<u64, PricingError> {
        if !self.usd_per_btc.is_finite() || self.usd_per_btc <= 0.0 {
            return Err(PricingError::InvalidPrice(self.usd_per_btc));
        }
        let sats = self.sats.price(amount_tih)?;
        // micro-USD = sats / 1e8 BTC * usd_per_btc * 1e6
        let micro = (sats as f64 * self.usd_per_btc / 100.0).ceil();
        if !micro.is_finite() || micro >= u64::MAX as f64 {
            return Err(PricingError::Overflow(amount_tih));
        }
        Ok((micro as u64).max(1))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenQuote {
    id: String,
    amount_tih: u64,
    expires_at: u64,
}

/// A settled quote: what cdk is told was paid.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Paid {
    amount_tih: u64,
    /// `tx_hash:log_index` of the paying transfer.
    payment_id: String,
}

impl Paid {
    fn response(&self, id: &str) -> WaitPaymentResponse {
        WaitPaymentResponse {
            payment_identifier: PaymentIdentifier::CustomId(id.to_string()),
            payment_amount: Amount::new(self.amount_tih, tih_unit()),
            payment_id: self.payment_id.clone(),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    /// Open quotes keyed by their exact micro-USDT amount.
    open: HashMap<u64, OpenQuote>,
    /// Settled quotes by lookup id.
    paid: HashMap<String, Paid>,
    /// Transfers already handled, as `(tx_hash, log_index)`.
    seen: HashSet<(String, u64)>,
    next_block: Option<u64>,
}

impl State {
    fn load(path: &Path) -> Result<Self, Error> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| Error::Custom(format!("read {}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(Error::Custom(format!("read {}: {e}", path.display()))),
        }
    }

    /// Write atomically (temp file + rename) so a crash mid-write cannot
    /// leave a torn file.
    fn save(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, path)
    }
}

#[derive(Debug)]
struct Inner {
    rail: UsdtRail,
    pricer: Arc<dyn TihPricer>,
    source: Arc<dyn UsdtSource>,
    quote_ttl: Duration,
    poll_interval: Duration,
    state: Mutex<State>,
    /// Where `state` is saved, if anywhere.
    persist: Option<PathBuf>,
    active: AtomicBool,
    /// Set by `cancel_wait_invoice`; checked every tick so a cancel that
    /// lands mid-poll is not lost.
    stop: AtomicBool,
    cancel: Notify,
}

/// `usdt` / `usdc` mint backend. Cheap to clone; clones share state.
#[derive(Debug, Clone)]
pub struct TetherPayments {
    inner: Arc<Inner>,
}

impl TetherPayments {
    pub fn new(rail: UsdtRail, pricer: Arc<dyn TihPricer>, source: Arc<dyn UsdtSource>) -> Self {
        Self {
            inner: Arc::new(Inner {
                rail,
                pricer,
                source,
                quote_ttl: DEFAULT_QUOTE_TTL,
                poll_interval: DEFAULT_POLL_INTERVAL,
                state: Mutex::new(State::default()),
                persist: None,
                active: AtomicBool::new(false),
                stop: AtomicBool::new(false),
                cancel: Notify::new(),
            }),
        }
    }

    /// Set quote lifetime and poll cadence. Call before sharing clones.
    pub fn with_timing(self, quote_ttl: Duration, poll_interval: Duration) -> Self {
        let inner = Arc::try_unwrap(self.inner).expect("with_timing before cloning");
        Self {
            inner: Arc::new(Inner {
                quote_ttl,
                poll_interval,
                ..inner
            }),
        }
    }

    /// Keep quote state in `path` (JSON), loading what is already there.
    /// Call before sharing clones.
    pub fn with_persistence(self, path: impl Into<PathBuf>) -> Result<Self, Error> {
        let path = path.into();
        let state = State::load(&path)?;
        let inner = Arc::try_unwrap(self.inner).expect("with_persistence before cloning");
        Ok(Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                persist: Some(path),
                ..inner
            }),
        })
    }

    /// Save `state` if persistence is on. A failed save is logged, not
    /// fatal: the in-memory state is still right until the next restart.
    fn save(&self, state: &State) {
        if let Some(path) = &self.inner.persist {
            if let Err(e) = state.save(path) {
                tracing::error!(path = %path.display(), "saving {} quotes failed: {e}", self.inner.rail.coin.symbol());
            }
        }
    }

    /// The unit this backend issues.
    pub fn unit() -> CurrencyUnit {
        tih_unit()
    }

    pub fn rail(&self) -> &UsdtRail {
        &self.inner.rail
    }

    /// Price `amount_tih` and reserve a unique amount for it.
    fn open_quote(&self, amount_tih: u64) -> Result<(String, UsdtPaymentRequest), Error> {
        if amount_tih == 0 {
            return Err(Error::Custom(PricingError::ZeroAmount.to_string()));
        }
        let price = self
            .inner
            .pricer
            .price(amount_tih)
            .map_err(|e| Error::Custom(format!("pricing: {e}")))?;
        let base = price
            .div_ceil(MICRO_PER_CENT)
            .checked_mul(MICRO_PER_CENT)
            .ok_or_else(|| Error::Custom("price overflow".into()))?;

        let now = now_secs();
        let expiry = now + self.inner.quote_ttl.as_secs();
        let id = hex::encode(rand::random::<[u8; 16]>());

        let mut state = self.inner.state.lock().expect("tether state poisoned");
        let amount_micro = (0..256)
            .map(|_| base + rand::random::<u64>() % (MICRO_PER_CENT - 1) + 1)
            .find(|a| !state.open.contains_key(a))
            .ok_or_else(|| Error::Custom("too many open quotes at this price".into()))?;
        state.open.insert(
            amount_micro,
            OpenQuote {
                id: id.clone(),
                amount_tih,
                expires_at: expiry,
            },
        );
        self.save(&state);
        drop(state);

        Ok((
            id,
            UsdtPaymentRequest {
                symbol: self.inner.rail.coin.symbol().to_string(),
                chain_id: self.inner.rail.chain_id,
                token: self.inner.rail.token.clone(),
                to: self.inner.rail.deposit_address.clone(),
                amount: format_usdt(amount_micro),
                amount_micro,
                amount_tih,
                expiry,
            },
        ))
    }

    /// Match transfers against open quotes. Returns newly settled payments.
    fn settle(&self, transfers: Vec<UsdtTransfer>) -> Vec<WaitPaymentResponse> {
        let mut state = self.inner.state.lock().expect("tether state poisoned");
        let mut settled = Vec::new();
        for t in transfers {
            if t.to != self.inner.rail.deposit_address
                || !state.seen.insert((t.tx_hash.clone(), t.log_index))
            {
                continue;
            }
            let quote = u64::try_from(t.value)
                .ok()
                .and_then(|v| state.open.remove(&v));
            let Some(quote) = quote else {
                tracing::warn!(
                    tx = %t.tx_hash,
                    amount = %format_usdt(u64::try_from(t.value).unwrap_or(u64::MAX)),
                    coin = self.inner.rail.coin.symbol(),
                    "stablecoin transfer matches no open quote; refund by hand"
                );
                continue;
            };
            let paid = Paid {
                amount_tih: quote.amount_tih,
                payment_id: format!("{}:{}", t.tx_hash, t.log_index),
            };
            let resp = paid.response(&quote.id);
            tracing::info!(quote = %quote.id, tih = quote.amount_tih, tx = %t.tx_hash, coin = self.inner.rail.coin.symbol(), "stablecoin quote paid");
            state.paid.insert(quote.id, paid);
            settled.push(resp);
        }
        let cutoff = now_secs().saturating_sub(GRACE_AFTER_EXPIRY.as_secs());
        state.open.retain(|_, q| q.expires_at > cutoff);
        settled
    }

    /// Ask the source for new transfers once and settle any matches.
    pub async fn poll_once(&self) -> Result<Vec<WaitPaymentResponse>, Error> {
        let from = self
            .inner
            .state
            .lock()
            .expect("tether state poisoned")
            .next_block;
        let (transfers, next) = self.inner.source.poll(from).await?;
        let settled = self.settle(transfers);
        let mut state = self.inner.state.lock().expect("tether state poisoned");
        // Settlements and the scan position are saved together, so a crash
        // can only make the next start rescan blocks (harmless: `seen`
        // dedups), never skip them.
        state.next_block = Some(next);
        self.save(&state);
        Ok(settled)
    }
}

#[async_trait]
impl MintPayment for TetherPayments {
    type Err = Error;

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        let mut custom = HashMap::new();
        custom.insert(
            self.inner.rail.coin.method().to_string(),
            serde_json::json!({
                "symbol": self.inner.rail.coin.symbol(),
                "chain_id": self.inner.rail.chain_id,
                "token": self.inner.rail.token,
                "deposit_address": self.inner.rail.deposit_address,
            })
            .to_string(),
        );
        Ok(SettingsResponse {
            unit: TIH.to_string(),
            bolt11: None,
            bolt12: None,
            custom,
        })
    }

    async fn create_incoming_payment_request(
        &self,
        options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        let IncomingPaymentOptions::Custom(custom) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };
        if custom.method != self.inner.rail.coin.method() {
            return Err(Error::UnsupportedPaymentOption);
        }
        if *custom.amount.unit() != tih_unit() {
            return Err(Error::UnsupportedUnit);
        }
        let (id, req) = self.open_quote(custom.amount.value())?;
        Ok(CreateIncomingPaymentResponse {
            request_lookup_id: PaymentIdentifier::CustomId(id),
            request: self.inner.rail.payment_uri(req.amount_micro),
            expiry: Some(req.expiry),
            extra_json: Some(serde_json::to_value(&req).map_err(|e| Error::Custom(e.to_string()))?),
        })
    }

    async fn get_payment_quote(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        // The mint never pays out on this rail.
        Err(Error::UnsupportedPaymentOption)
    }

    async fn make_payment(
        &self,
        _unit: &CurrencyUnit,
        _options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        Err(Error::UnsupportedPaymentOption)
    }

    async fn wait_payment_event(&self) -> Result<BoxStream<'static, Event>, Self::Err> {
        let (tx, rx) = mpsc::channel::<Event>(64);
        let this = self.clone();
        this.inner.stop.store(false, Ordering::SeqCst);
        this.inner.active.store(true, Ordering::SeqCst);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(this.inner.poll_interval);
            loop {
                tokio::select! {
                    _ = this.inner.cancel.notified() => break,
                    _ = tick.tick() => {}
                }
                if this.inner.stop.load(Ordering::SeqCst) {
                    break;
                }
                match this.poll_once().await {
                    Ok(settled) => {
                        for resp in settled {
                            if tx.send(Event::PaymentReceived(resp)).await.is_err() {
                                this.inner.active.store(false, Ordering::SeqCst);
                                return;
                            }
                        }
                    }
                    Err(e) => tracing::warn!("USDT poll failed: {e}"),
                }
            }
            this.inner.active.store(false, Ordering::SeqCst);
        });
        Ok(Box::pin(stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|ev| (ev, rx))
        })))
    }

    fn is_wait_invoice_active(&self) -> bool {
        self.inner.active.load(Ordering::SeqCst)
    }

    fn cancel_wait_invoice(&self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.cancel.notify_waiters();
    }

    async fn check_incoming_payment_status(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        let PaymentIdentifier::CustomId(id) = payment_identifier else {
            return Ok(Vec::new());
        };
        let state = self.inner.state.lock().expect("tether state poisoned");
        Ok(state
            .paid
            .get(id)
            .map(|p| p.response(id))
            .into_iter()
            .collect())
    }

    async fn check_outgoing_payment(
        &self,
        _payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        Err(Error::UnsupportedPaymentOption)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::cdk_payment::CustomIncomingPaymentOptions;
    use futures::StreamExt;

    const TOKEN: &str = "0x7169D38820dfd117C3FA1f22a697dBA58d90BA06";
    const DEPOSIT: &str = "0x00000000000000000000000000000000000000Aa";

    /// Scripted source: hands out queued batches, one per poll.
    #[derive(Debug, Default)]
    struct FakeSource {
        batches: Mutex<Vec<Vec<UsdtTransfer>>>,
        polls: Mutex<Vec<Option<u64>>>,
    }

    impl FakeSource {
        fn push(&self, batch: Vec<UsdtTransfer>) {
            self.batches.lock().unwrap().push(batch);
        }
    }

    #[async_trait]
    impl UsdtSource for FakeSource {
        async fn poll(&self, from: Option<u64>) -> Result<(Vec<UsdtTransfer>, u64), Error> {
            self.polls.lock().unwrap().push(from);
            let mut b = self.batches.lock().unwrap();
            let next = from.unwrap_or(100) + 1;
            if b.is_empty() {
                Ok((Vec::new(), next))
            } else {
                Ok((b.remove(0), next))
            }
        }
    }

    #[test]
    fn quote_extra_does_not_shadow_the_tih_amount() {
        // cdk flattens extra_json into the NUT-04 response, which has its
        // own `amount` (TiH); a second `amount` broke wallets.
        let req = UsdtPaymentRequest {
            symbol: "USDT".into(),
            chain_id: 1,
            token: "t".into(),
            to: "a".into(),
            amount: "0.050000".into(),
            amount_micro: 50_000,
            amount_tih: 1,
            expiry: 0,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("amount").is_none());
        assert_eq!(v["amount_usdt"], "0.050000");
        let back: UsdtPaymentRequest = serde_json::from_value(v).unwrap();
        assert_eq!(back, req);
    }

    fn rail() -> UsdtRail {
        UsdtRail::new(Stablecoin::Usdt, 11_155_111, TOKEN, DEPOSIT).unwrap()
    }

    #[test]
    fn fixed_price_is_per_pih_rounded_up() {
        // $0.0006 per PiH: 2 PiH = 1,200 micro-USDT; one TiH rounds up to 1.
        let p = FixedUsdtPrice { micro_per_pih: 600 };
        assert_eq!(p.price(2 * 1024).unwrap(), 1_200);
        assert_eq!(p.price(1).unwrap(), 1);
    }

    /// $2.50 per TiH ($2,560 per PiH), so amounts stay readable.
    fn backend() -> (TetherPayments, Arc<FakeSource>) {
        let src = Arc::new(FakeSource::default());
        let p = TetherPayments::new(
            rail(),
            Arc::new(FixedUsdtPrice {
                micro_per_pih: 2_560_000_000,
            }),
            src.clone(),
        );
        (p, src)
    }

    fn mint_opts(tih: u64) -> IncomingPaymentOptions {
        IncomingPaymentOptions::Custom(Box::new(CustomIncomingPaymentOptions {
            method: METHOD.to_string(),
            description: None,
            amount: Amount::new(tih, tih_unit()),
            unix_expiry: None,
            extra_json: None,
        }))
    }

    fn details(r: &CreateIncomingPaymentResponse) -> UsdtPaymentRequest {
        serde_json::from_value(r.extra_json.clone().unwrap()).unwrap()
    }

    fn transfer(tx: &str, value: u64) -> UsdtTransfer {
        UsdtTransfer {
            tx_hash: tx.into(),
            log_index: 0,
            block: 101,
            to: rail().deposit_address,
            value: value.into(),
        }
    }

    #[test]
    fn addresses_and_amounts_format() {
        assert_eq!(
            parse_address(TOKEN).unwrap(),
            "0x7169d38820dfd117c3fa1f22a697dba58d90ba06"
        );
        assert!(parse_address("7169d38820dfd117c3fa1f22a697dba58d90ba06").is_err());
        assert!(parse_address("0x1234").is_err());
        assert_eq!(format_usdt(12_340_017), "12.340017");
        assert_eq!(format_usdt(5), "0.000005");
        assert_eq!(
            rail().payment_uri(2_500_017),
            "ethereum:0x7169d38820dfd117c3fa1f22a697dba58d90ba06@11155111/transfer?address=0x00000000000000000000000000000000000000aa&uint256=2500017"
        );
    }

    #[test]
    fn sats_to_usdt_converts_and_rounds_up() {
        #[derive(Debug)]
        struct Sats(u64);
        impl TihPricer for Sats {
            fn price(&self, tih: u64) -> Result<u64, PricingError> {
                Ok(self.0 * tih)
            }
        }
        // 3 sats/TiH at $100k/BTC = $0.003 = 3000 micro-USDT per TiH.
        let p = SatsToUsdt {
            sats: Arc::new(Sats(3)),
            usd_per_btc: 100_000.0,
        };
        assert_eq!(p.price(2).unwrap(), 6_000);
        let p = SatsToUsdt {
            sats: Arc::new(Sats(1)),
            usd_per_btc: 1.5,
        };
        assert_eq!(p.price(1).unwrap(), 1); // 0.015 micro rounds up
    }

    #[tokio::test]
    async fn quote_prices_tih_with_unique_sub_cent_tag() {
        let (p, _) = backend();
        let mut amounts = HashSet::new();
        for _ in 0..50 {
            let r = p
                .create_incoming_payment_request(mint_opts(4))
                .await
                .unwrap();
            let d = details(&r);
            // $10.00 base plus a 0.000001..0.009999 tag.
            assert!(d.amount_micro > 10_000_000 && d.amount_micro < 10_010_000);
            assert_eq!(d.amount_tih, 4);
            assert_eq!(d.to, rail().deposit_address);
            assert_eq!(r.request, rail().payment_uri(d.amount_micro));
            assert!(amounts.insert(d.amount_micro), "amounts must be unique");
        }
    }

    #[tokio::test]
    async fn quote_rejects_wrong_method_unit_or_zero() {
        let (p, _) = backend();
        let mut sat = mint_opts(1);
        if let IncomingPaymentOptions::Custom(c) = &mut sat {
            c.amount = Amount::new(1, CurrencyUnit::Sat);
        }
        assert!(p.create_incoming_payment_request(sat).await.is_err());
        let mut other = mint_opts(1);
        if let IncomingPaymentOptions::Custom(c) = &mut other {
            c.method = "paypal".into();
        }
        assert!(p.create_incoming_payment_request(other).await.is_err());
        assert!(p
            .create_incoming_payment_request(mint_opts(0))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn exact_transfer_settles_quote_in_tih() {
        let (p, src) = backend();
        let r = p
            .create_incoming_payment_request(mint_opts(3))
            .await
            .unwrap();
        let d = details(&r);

        // A wrong amount and a transfer to someone else do nothing.
        src.push(vec![
            transfer("0xaa", d.amount_micro + 1),
            UsdtTransfer {
                to: "0x".to_string() + &"b".repeat(40),
                ..transfer("0xbb", d.amount_micro)
            },
        ]);
        assert!(p.poll_once().await.unwrap().is_empty());
        assert!(p
            .check_incoming_payment_status(&r.request_lookup_id)
            .await
            .unwrap()
            .is_empty());

        src.push(vec![transfer("0xcc", d.amount_micro)]);
        let settled = p.poll_once().await.unwrap();
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].payment_identifier, r.request_lookup_id);
        assert_eq!(settled[0].payment_amount, Amount::new(3, tih_unit()));
        assert_eq!(settled[0].payment_id, "0xcc:0");

        let status = p
            .check_incoming_payment_status(&r.request_lookup_id)
            .await
            .unwrap();
        assert_eq!(status.len(), 1);

        // Replaying the same log, or paying the amount twice, settles nothing new.
        src.push(vec![
            transfer("0xcc", d.amount_micro),
            transfer("0xdd", d.amount_micro),
        ]);
        assert!(p.poll_once().await.unwrap().is_empty());

        // The source is asked to resume where it left off.
        assert_eq!(*src.polls.lock().unwrap(), vec![None, Some(101), Some(102)]);
    }

    #[tokio::test]
    async fn wait_payment_event_streams_settlements() {
        let src = Arc::new(FakeSource::default());
        let p = TetherPayments::new(
            rail(),
            Arc::new(FixedUsdtPrice {
                micro_per_pih: 1_024_000_000,
            }),
            src.clone(),
        )
        .with_timing(DEFAULT_QUOTE_TTL, Duration::from_millis(5));
        let r = p
            .create_incoming_payment_request(mint_opts(1))
            .await
            .unwrap();
        src.push(vec![transfer("0xee", details(&r).amount_micro)]);

        let mut events = p.wait_payment_event().await.unwrap();
        assert!(p.is_wait_invoice_active());
        let Event::PaymentReceived(got) =
            tokio::time::timeout(Duration::from_secs(2), events.next())
                .await
                .unwrap()
                .unwrap();
        assert_eq!(got.payment_identifier, r.request_lookup_id);
        assert_eq!(got.payment_amount, Amount::new(1, tih_unit()));

        p.cancel_wait_invoice();
        tokio::time::timeout(Duration::from_secs(2), async {
            while p.is_wait_invoice_active() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn outgoing_is_refused() {
        let (p, _) = backend();
        let opts = OutgoingPaymentOptions::Custom(Box::new(
            cdk::cdk_payment::CustomOutgoingPaymentOptions {
                method: METHOD.into(),
                request: "x".into(),
                max_fee_amount: None,
                timeout_secs: None,
                melt_options: None,
                extra_json: None,
            },
        ));
        assert!(p
            .get_payment_quote(&tih_unit(), opts.clone())
            .await
            .is_err());
        assert!(p.make_payment(&tih_unit(), opts).await.is_err());
        let s = p.get_settings().await.unwrap();
        assert_eq!(s.unit, "TiH");
        assert!(s.custom.contains_key(METHOD));
    }

    #[tokio::test]
    async fn usdc_rail_answers_only_its_own_method() {
        let rail = UsdtRail::new(Stablecoin::Usdc, 1, MAINNET_USDC, DEPOSIT).unwrap();
        let p = TetherPayments::new(
            rail,
            Arc::new(FixedUsdtPrice {
                micro_per_pih: 1_024_000_000,
            }),
            Arc::new(FakeSource::default()),
        );
        assert!(p
            .create_incoming_payment_request(mint_opts(1))
            .await
            .is_err());
        let mut usdc = mint_opts(2);
        if let IncomingPaymentOptions::Custom(c) = &mut usdc {
            c.method = USDC_METHOD.into();
        }
        let r = p.create_incoming_payment_request(usdc).await.unwrap();
        let d = details(&r);
        assert_eq!(d.symbol, "USDC");
        assert_eq!(d.chain_id, 1);
        assert_eq!(d.token, MAINNET_USDC.to_ascii_lowercase());
        let s = p.get_settings().await.unwrap();
        assert!(s.custom.contains_key(USDC_METHOD));
        assert!(!s.custom.contains_key(METHOD));
    }

    #[tokio::test]
    async fn quotes_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usdt-quotes.json");
        let open = |src: Arc<FakeSource>| {
            TetherPayments::new(
                rail(),
                Arc::new(FixedUsdtPrice {
                    micro_per_pih: 2_560_000_000,
                }),
                src,
            )
            .with_persistence(&path)
            .unwrap()
        };

        // Quote opened, one block scanned, then the mint stops.
        let src = Arc::new(FakeSource::default());
        let p = open(src.clone());
        let r = p
            .create_incoming_payment_request(mint_opts(3))
            .await
            .unwrap();
        let d = details(&r);
        p.poll_once().await.unwrap();
        drop(p);

        // After the restart the buyer's transfer still settles the quote,
        // and scanning resumes where it stopped.
        let src = Arc::new(FakeSource::default());
        let p = open(src.clone());
        src.push(vec![transfer("0x11", d.amount_micro)]);
        assert_eq!(p.poll_once().await.unwrap().len(), 1);
        assert_eq!(*src.polls.lock().unwrap(), vec![Some(101)]);
        drop(p);

        // And the settlement itself is remembered: cdk can still ask.
        let p = open(Arc::new(FakeSource::default()));
        let status = p
            .check_incoming_payment_status(&r.request_lookup_id)
            .await
            .unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].payment_amount, Amount::new(3, tih_unit()));
        // Replaying the transfer after a restart pays nothing twice.
        let src = Arc::new(FakeSource::default());
        let p = open(src.clone());
        src.push(vec![transfer("0x11", d.amount_micro)]);
        assert!(p.poll_once().await.unwrap().is_empty());
    }

    #[test]
    fn parses_erc20_transfer_log() {
        let log = serde_json::json!({
            "address": TOKEN,
            "topics": [
                TRANSFER_TOPIC,
                "0x000000000000000000000000111111111111111111111111111111111111111a",
                "0x00000000000000000000000000000000000000000000000000000000000000AA"
            ],
            "data": "0x0000000000000000000000000000000000000000000000000000000000989691",
            "blockNumber": "0x10",
            "transactionHash": "0xABC",
            "logIndex": "0x2"
        });
        let t = parse_transfer_log(&log).unwrap();
        assert_eq!(t.to, rail().deposit_address);
        assert_eq!(t.value, 10_000_017);
        assert_eq!(t.block, 16);
        assert_eq!(t.log_index, 2);
        assert_eq!(t.tx_hash, "0xabc");

        let mut not_transfer = log.clone();
        not_transfer["topics"][0] = serde_json::json!("0x00");
        assert!(parse_transfer_log(&not_transfer).is_err());
    }

    #[tokio::test]
    async fn cdk_mint_quote_is_paid_by_matching_usdt_transfer() {
        use cdk::mint::{MintBuilder, MintMeltLimits, UnitConfig};
        use cdk::nuts::PaymentMethod;

        let localstore = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
        let mut builder = MintBuilder::new(localstore.clone());
        builder
            .configure_unit(
                tih_unit(),
                UnitConfig {
                    amounts: vec![1, 2, 4, 8, 16, 32],
                    input_fee_ppk: 0,
                },
            )
            .unwrap();
        let (p, src) = backend();
        builder
            .add_payment_processor(
                tih_unit(),
                PaymentMethod::Custom(METHOD.to_string()),
                MintMeltLimits::new(1, 1000),
                Arc::new(p.clone()),
            )
            .await
            .unwrap();
        let info = builder.current_mint_info();
        let method = PaymentMethod::Custom(METHOD.to_string());
        assert!(info.nuts.nut04.methods.iter().any(|m| m.method == method));

        // Full quote round trip through a real cdk mint.
        let mint = builder
            .build_with_seed(localstore, &[7u8; 64])
            .await
            .unwrap();
        let quote = mint
            .get_mint_quote(cdk::mint::MintQuoteRequest::Custom {
                method: METHOD.to_string(),
                request: cdk::nuts::MintQuoteCustomRequest {
                    amount: cdk::Amount::from(2),
                    unit: tih_unit(),
                    description: None,
                    pubkey: None,
                    extra: serde_json::Value::Null,
                },
            })
            .await
            .unwrap();
        let cdk::mint::MintQuoteResponse::Custom { response, .. } = quote else {
            panic!("expected a custom quote");
        };
        assert_eq!(response.state, cdk::nuts::nut23::QuoteState::Unpaid);
        // $5.00 for 2 TiH at $2.50, plus the sub-cent tag.
        let amount_micro: u64 = response
            .request
            .rsplit_once("uint256=")
            .unwrap()
            .1
            .parse()
            .unwrap();
        assert!(amount_micro > 5_000_000 && amount_micro < 5_010_000);

        src.push(vec![transfer("0xff", amount_micro)]);
        assert_eq!(p.poll_once().await.unwrap().len(), 1);
        let checked = mint.check_mint_quote(&response.quote).await.unwrap();
        let cdk::mint::MintQuoteResponse::Custom { response, .. } = checked else {
            panic!("expected a custom quote");
        };
        assert_eq!(response.state, cdk::nuts::nut23::QuoteState::Paid);
    }
}
