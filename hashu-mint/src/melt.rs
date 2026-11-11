//! Custom melt method `hashrate`: redeem `THH` vouchers for routed hashrate.
//! ARCHITECTURE.md §5.1, §7.
//!
//! cdk treats a custom melt method as a payment backend. `HashrateMelter`
//! is that backend, registered for `(THH, "hashrate")`:
//!
//! - **quote**: the request string carries `{pool_url, worker_name,
//!   amount_thh}`. The quote splits `amount_thh` in two: `amount` is the
//!   floor (`min_thh`), which the redeemer pays however little is
//!   delivered, and `fee` (cdk's fee reserve) is the rest. The mint
//!   requires melted proofs ≥ amount + fee, so the redeemer still melts the
//!   full `amount_thh`. Putting the refundable part in the fee reserve is
//!   what makes a stock cdk wallet send blank outputs (NUT-08) for it, the
//!   same way it does for a Lightning fee reserve.
//! - **melt**: opens a redemption in the shared [`RedemptionLedger`] and
//!   returns `Pending`. The proofs stay pending while shares accumulate.
//! - **check**: cdk's pending-melt poller calls `check_outgoing_payment`.
//!   It reports `Paid` once the redemption completes or is settled.
//!   Completion is driven by the proxy crediting accepted shares to the
//!   ledger. A settled redemption reports only its delivered `THH` as
//!   `total_spent`, and cdk signs the blanks for the difference: the
//!   redeemer gets the undelivered hashrate back as fresh `THH`.
//!
//! Incoming payments, invoices and wait-events do not apply to this
//! method.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use cdk::cdk_payment::{
    CreateIncomingPaymentResponse, Error, Event, IncomingPaymentOptions, MakePaymentResponse,
    MintPayment, OutgoingPaymentOptions, PaymentIdentifier, PaymentQuoteResponse, SettingsResponse,
    WaitPaymentResponse,
};
use cdk::nuts::{CurrencyUnit, MeltQuoteState};
use cdk::Amount;
use futures::stream::{self, BoxStream};
use hashu_core::redemption::{Redemption, RedemptionId, RedemptionLedger, RedemptionState};
use serde::{Deserialize, Serialize};

/// Method name as it appears in melt requests and NUT-05 settings.
pub const METHOD: &str = "hashrate";

/// Unit string for terahash-hour vouchers (see `hashu_core::denom`).
const THH: &str = hashu_core::denom::UNIT_THH;

/// Smallest accepted redemption, in THH. ARCHITECTURE.md §9 Q5 leaves the
/// real floor open; below roughly a Th-minute, share variance dominates
/// the integral. This default is a conservative placeholder.
pub const DEFAULT_MIN_THH: u64 = 1;

/// Payload carried in the melt request's `request` string (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HashrateRequest {
    /// Redeemer's stratum endpoint, e.g. `stratum+tcp://pool.example:3333`.
    pub pool_url: String,
    /// Worker name the redeemer's pool expects.
    pub worker_name: String,
    /// Password for the worker, if the pool needs one (default `x`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_password: Option<String>,
    /// `THH` to redeem. Must be covered by the melted proofs.
    pub amount_thh: u64,
    /// Optional wallet-chosen value that makes the request unique. The
    /// redemption id is derived from the request string, and cdk refuses a
    /// second melt of an identical request, so a wallet that redeems the
    /// same amount to the same pool and worker twice sets a fresh nonce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

impl HashrateRequest {
    /// Parse and validate the request string. Checks the shape only, not
    /// the minimum amount, which the melter enforces.
    pub fn parse(request: &str) -> Result<Self, Error> {
        let parsed: Self = serde_json::from_str(request)
            .map_err(|e| Error::Custom(format!("invalid hashrate request: {e}")))?;
        if !is_routable_pool_url(&parsed.pool_url) {
            return Err(Error::Custom(
                "pool_url must be stratum+tcp://host:port or stratum+ssl://host:port".into(),
            ));
        }
        if parsed.worker_name.is_empty() {
            return Err(Error::Custom("worker_name must not be empty".into()));
        }
        Ok(parsed)
    }
}

/// Whether the proxy can dial `pool_url`: `stratum+tcp://host:port` or
/// `stratum+ssl://host:port`, with a port and no path. Checked at quote
/// time so a wallet can't pay for a redemption the router would skip.
/// Mirrors `hashu_proxy::upstream::normalize_upstream`, which this crate
/// can't depend on.
fn is_routable_pool_url(pool_url: &str) -> bool {
    let Some(rest) = pool_url
        .strip_prefix("stratum+tcp://")
        .or_else(|| pool_url.strip_prefix("stratum+ssl://"))
    else {
        return false;
    };
    match rest.rsplit_once(':') {
        Some((host, port)) => {
            !host.is_empty() && !rest.contains('/') && port.parse::<u16>().is_ok_and(|p| p > 0)
        }
        None => false,
    }
}

/// `hashrate` melt backend. Holds a handle to the shared ledger so that
/// the proxy path and the melt path see the same redemptions.
#[derive(Debug)]
pub struct HashrateMelter {
    ledger: Arc<RedemptionLedger>,
    min_thh: u64,
    max_thh: Option<u64>,
    /// The registered supplier whose mint this is; `None` for the
    /// operator's own mint. Its redemptions go only to that supplier's rigs.
    supplier: Option<String>,
}

impl HashrateMelter {
    pub fn new(ledger: Arc<RedemptionLedger>) -> Self {
        Self {
            ledger,
            min_thh: DEFAULT_MIN_THH,
            max_thh: None,
            supplier: None,
        }
    }

    /// Melter for supplier `name`'s mint: its redemptions carry the name,
    /// so only that supplier's rigs fill them, and their ids are scoped to
    /// it so two mints never share one redemption.
    pub fn with_supplier(mut self, name: impl Into<String>) -> Self {
        self.supplier = Some(name.into());
        self
    }

    fn id_for(&self, request: &str) -> RedemptionId {
        redemption_id_in(self.supplier.as_deref(), request)
    }

    fn open_redemption(&self, r: Redemption) -> Redemption {
        r.with_supplier(self.supplier.clone())
    }

    /// Largest accepted redemption. cdk's melt limits only see the quote
    /// amount, which is the floor, so the melter checks the full amount.
    pub fn with_max_thh(mut self, max_thh: u64) -> Self {
        self.max_thh = Some(max_thh);
        self
    }

    /// Non-refundable part of a redemption of `amount_thh`: the minimum
    /// redemption, at least 1 so the quote amount is never zero.
    fn floor_for(&self, amount_thh: u64) -> u64 {
        self.min_thh.max(1).min(amount_thh)
    }

    pub fn with_min_thh(mut self, min_thh: u64) -> Self {
        self.min_thh = min_thh;
        self
    }

    /// The unit this backend accepts. Lowercase, because cdk lowercases
    /// custom units whenever it serialises them (mint info, database,
    /// wire): a wallet's `THH` arrives as `Custom("thh")`.
    pub fn unit() -> CurrencyUnit {
        CurrencyUnit::Custom(THH.to_lowercase())
    }

    fn check_unit(&self, unit: &CurrencyUnit) -> Result<(), Error> {
        if *unit != Self::unit() {
            return Err(Error::UnsupportedUnit);
        }
        Ok(())
    }

    fn validated(&self, request: &str) -> Result<HashrateRequest, Error> {
        let req = HashrateRequest::parse(request)?;
        if req.amount_thh < self.min_thh {
            return Err(Error::Custom(format!(
                "amount_thh {} is below the minimum of {}",
                req.amount_thh, self.min_thh
            )));
        }
        if let Some(max) = self.max_thh.filter(|m| req.amount_thh > *m) {
            return Err(Error::Custom(format!(
                "amount_thh {} is above the maximum of {max}",
                req.amount_thh
            )));
        }
        Ok(req)
    }
}

/// Redemption id for a melt request: the first 16 bytes of the SHA-256 of
/// the request string.
///
/// It has to be known at quote time. cdk stores the quote's lookup id, and
/// when `make_payment` returns `Pending` it keeps checking that id, not the
/// one `make_payment` returns; and `make_payment` is only handed the
/// request, not the quote. Deriving the id from the request ties the two
/// together. Identical requests share an id, which cdk treats like two
/// quotes for one invoice: once one is pending or paid the other is
/// refused, so nobody pays twice for one redemption.
pub fn redemption_id_for(request: &str) -> RedemptionId {
    redemption_id_in(None, request)
}

/// [`redemption_id_for`] on supplier `supplier`'s mint. Every mint in one
/// process writes to the same ledger, so a supplier's ids hash the name in
/// too: the same request melted on two mints is two redemptions. The
/// operator's mint (`None`) keeps the plain hash, so its existing ids hold.
pub fn redemption_id_in(supplier: Option<&str>, request: &str) -> RedemptionId {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    if let Some(s) = supplier {
        h.update(b"hashu-supplier:");
        h.update(s.as_bytes());
        h.update(b"\n");
    }
    h.update(request.as_bytes());
    let digest = h.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    RedemptionId::from_bytes(id)
}

/// Lookup id the mint stores for a redemption. Its hex is the handle.
fn lookup_id(id: &RedemptionId) -> PaymentIdentifier {
    PaymentIdentifier::CustomId(id.to_string())
}

fn parse_lookup_id(payment_identifier: &PaymentIdentifier) -> Result<RedemptionId, Error> {
    let PaymentIdentifier::CustomId(s) = payment_identifier else {
        return Err(Error::Custom("hashrate expects a custom lookup id".into()));
    };
    let bytes = hex::decode(s).map_err(|e| Error::Custom(format!("bad redemption id: {e}")))?;
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| Error::Custom("redemption id must be 16 bytes".into()))?;
    Ok(RedemptionId::from_bytes(bytes))
}

/// Reopen, in `ledger`, every pending `hashrate` melt quote the mint's
/// database holds but the ledger does not know. Run before the mint starts
/// so cdk's startup check finds the redemption.
///
/// The ledger journal normally restores these with their progress. This
/// covers a journal that is missing (state from before persistence, or a
/// lost file): the redemption restarts from zero credited hashes, so the
/// redeemer is owed at least what they melted, never less, and the
/// proofs cdk holds pending can still settle instead of hanging forever.
/// Returns the ids reopened.
pub fn reopen_pending_quotes(
    ledger: &RedemptionLedger,
    quotes: &[cdk::mint::MeltQuote],
) -> Vec<RedemptionId> {
    reopen_pending_quotes_in(None, ledger, quotes)
}

/// [`reopen_pending_quotes`] for supplier `supplier`'s mint.
pub fn reopen_pending_quotes_in(
    supplier: Option<&str>,
    ledger: &RedemptionLedger,
    quotes: &[cdk::mint::MeltQuote],
) -> Vec<RedemptionId> {
    let hashrate = cdk::nuts::PaymentMethod::Custom(METHOD.into());
    let mut reopened = Vec::new();
    for quote in quotes {
        if quote.payment_method != hashrate || quote.state != MeltQuoteState::Pending {
            continue;
        }
        let request = quote.request.to_string();
        let id = redemption_id_in(supplier, &request);
        if ledger.get(&id).is_some() {
            continue;
        }
        let Ok(req) = HashrateRequest::parse(&request) else {
            tracing::warn!(quote = %quote.id, "pending hashrate quote has an unreadable request");
            continue;
        };
        let floor = req.amount_thh.saturating_sub(quote.fee_reserve().value());
        let r = Redemption::new(id, req.amount_thh, req.pool_url, req.worker_name)
            .with_password(req.worker_password)
            .with_floor(floor)
            .with_supplier(supplier.map(str::to_string));
        if ledger.open(r).is_ok() {
            tracing::warn!(quote = %quote.id, redemption = %id, "reopened pending redemption missing from the ledger journal");
            reopened.push(id);
        }
    }
    reopened
}

#[async_trait]
impl MintPayment for HashrateMelter {
    type Err = Error;

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        let mut custom = HashMap::new();
        custom.insert(
            METHOD.to_string(),
            serde_json::json!({ "min_thh": self.min_thh }).to_string(),
        );
        Ok(SettingsResponse {
            unit: THH.to_string(),
            bolt11: None,
            bolt12: None,
            custom,
        })
    }

    async fn create_incoming_payment_request(
        &self,
        _options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        Err(Error::UnsupportedPaymentOption)
    }

    async fn get_payment_quote(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        self.check_unit(unit)?;
        let OutgoingPaymentOptions::Custom(custom) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };
        if custom.method != METHOD {
            return Err(Error::UnsupportedPaymentOption);
        }
        let req = self.validated(&custom.request)?;
        let floor = self.floor_for(req.amount_thh);
        Ok(PaymentQuoteResponse {
            request_lookup_id: Some(lookup_id(&self.id_for(&custom.request))),
            amount: Amount::new(floor, unit.clone()),
            fee: Amount::new(req.amount_thh - floor, unit.clone()),
            state: MeltQuoteState::Unpaid,
        })
    }

    async fn make_payment(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        self.check_unit(unit)?;
        let OutgoingPaymentOptions::Custom(custom) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };
        if custom.method != METHOD {
            return Err(Error::UnsupportedPaymentOption);
        }
        let req = self.validated(&custom.request)?;
        // cdk passes the quote's fee reserve here: the refundable part. A
        // quote from before the reserve existed has none, so its
        // redemption can't return change.
        let reserve = custom.max_fee_amount.as_ref().map_or(0, |a| a.value());
        let floor = req.amount_thh.saturating_sub(reserve);

        let id = self.id_for(&custom.request);
        if self.ledger.get(&id).is_some() {
            // Already opened (cdk retrying): report where it stands.
            return self.check_outgoing_payment(&lookup_id(&id)).await;
        }
        self.ledger
            .open(
                self.open_redemption(
                    Redemption::new(id, req.amount_thh, req.pool_url, req.worker_name)
                        .with_password(req.worker_password)
                        .with_floor(floor),
                ),
            )
            .map_err(|e| Error::Custom(e.to_string()))?;

        Ok(MakePaymentResponse {
            payment_lookup_id: lookup_id(&id),
            payment_proof: None,
            // Pending: proofs stay locked until the redemption completes.
            status: MeltQuoteState::Pending,
            total_spent: Amount::new(req.amount_thh, unit.clone()),
        })
    }

    async fn wait_payment_event(&self) -> Result<BoxStream<'static, Event>, Self::Err> {
        // Nothing arrives unprompted. Melt completion is observed by
        // polling `check_outgoing_payment`. The stream must stay open
        // (`pending`, not `empty`): cdk calls `wait_payment_event` again as
        // soon as a stream ends, so an empty stream spins that loop without
        // ever yielding, pins a runtime worker at 100% CPU, and on a 1-vCPU
        // host stalls the whole mint before it starts serving.
        Ok(Box::pin(stream::pending()))
    }

    fn is_wait_invoice_active(&self) -> bool {
        false
    }

    fn cancel_wait_invoice(&self) {}

    async fn check_incoming_payment_status(
        &self,
        _payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        Ok(Vec::new())
    }

    async fn check_outgoing_payment(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        let id = parse_lookup_id(payment_identifier)?;
        let redemption = self
            .ledger
            .get(&id)
            // Another mint's redemption is not this mint's to report.
            .filter(|r| r.supplier == self.supplier)
            .ok_or_else(|| Error::Custom(format!("unknown redemption {id}")))?;

        let status = match redemption.state {
            RedemptionState::Pending | RedemptionState::Active => MeltQuoteState::Pending,
            RedemptionState::Completed | RedemptionState::Settled => MeltQuoteState::Paid,
        };
        Ok(MakePaymentResponse {
            payment_lookup_id: payment_identifier.clone(),
            payment_proof: None,
            status,
            // Never below the quote amount (the floor): cdk subtracts it.
            total_spent: Amount::new(redemption.spent_thh(), Self::unit()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POOL: &str = "stratum+tcp://pool.example:3333";

    fn melter() -> (HashrateMelter, Arc<RedemptionLedger>) {
        let ledger = Arc::new(RedemptionLedger::new());
        (HashrateMelter::new(ledger.clone()), ledger)
    }

    /// cdk re-subscribes the moment a payment stream ends, so the melter's
    /// stream has to stay open or the mint's payment loop busy-spins.
    #[tokio::test]
    async fn payment_event_stream_stays_open() {
        use futures::StreamExt;
        let (m, _) = melter();
        let mut events = m.wait_payment_event().await.unwrap();
        let next = tokio::time::timeout(std::time::Duration::from_millis(50), events.next()).await;
        assert!(
            next.is_err(),
            "stream ended or yielded: {:?}",
            next.map(|e| e.is_some())
        );
    }

    fn request(amount_thh: u64) -> String {
        serde_json::json!({
            "pool_url": POOL,
            "worker_name": "rig1",
            "amount_thh": amount_thh,
        })
        .to_string()
    }

    fn custom(request: String) -> OutgoingPaymentOptions {
        OutgoingPaymentOptions::Custom(Box::new(cdk::cdk_payment::CustomOutgoingPaymentOptions {
            method: METHOD.to_string(),
            request,
            max_fee_amount: None,
            timeout_secs: None,
            melt_options: None,
            extra_json: None,
        }))
    }

    /// cdk keeps checking a pending melt by the quote's lookup id, so the
    /// quote must carry the id of the redemption `make_payment` opens.
    #[tokio::test]
    async fn quote_lookup_id_matches_the_redemption() {
        let (m, ledger) = melter();
        let quote = m
            .get_payment_quote(&HashrateMelter::unit(), custom(request(3)))
            .await
            .unwrap();
        let paid = m
            .make_payment(&HashrateMelter::unit(), custom(request(3)))
            .await
            .unwrap();
        assert_eq!(
            quote.request_lookup_id,
            Some(paid.payment_lookup_id.clone())
        );
        assert_eq!(ledger.active().len(), 1);

        // A retry reports the open redemption instead of failing.
        let again = m
            .make_payment(&HashrateMelter::unit(), custom(request(3)))
            .await
            .unwrap();
        assert_eq!(again.payment_lookup_id, paid.payment_lookup_id);
        assert_eq!(again.status, MeltQuoteState::Pending);
        assert_eq!(ledger.active().len(), 1);
    }

    #[test]
    fn supplier_mints_get_their_own_redemption_ids() {
        let r = request(10);
        let alice = redemption_id_in(Some("alice"), &r);
        assert_eq!(redemption_id_in(None, &r), redemption_id_for(&r));
        assert_ne!(alice, redemption_id_for(&r));
        assert_ne!(alice, redemption_id_in(Some("bob"), &r));
    }

    #[tokio::test]
    async fn supplier_melter_opens_redemptions_for_that_supplier_only() {
        let ledger = Arc::new(RedemptionLedger::new());
        let alice = HashrateMelter::new(ledger.clone()).with_supplier("alice");
        let operator = HashrateMelter::new(ledger.clone());
        let r = request(10);
        let paid = alice
            .make_payment(&HashrateMelter::unit(), with_reserve(r.clone(), 0))
            .await
            .unwrap();
        let id = redemption_id_in(Some("alice"), &r);
        assert_eq!(ledger.get(&id).unwrap().supplier.as_deref(), Some("alice"));
        // The operator's mint neither sees nor reports Alice's redemption.
        assert!(operator
            .check_outgoing_payment(&paid.payment_lookup_id)
            .await
            .is_err());
        assert!(alice
            .check_outgoing_payment(&paid.payment_lookup_id)
            .await
            .is_ok());
    }

    #[test]
    fn nonce_makes_identical_redemptions_distinct() {
        let with = |n: &str| {
            serde_json::json!({
                "pool_url": POOL, "worker_name": "rig1", "amount_thh": 3, "nonce": n,
            })
            .to_string()
        };
        assert!(HashrateRequest::parse(&with("a")).is_ok());
        assert_ne!(redemption_id_for(&with("a")), redemption_id_for(&with("b")));
    }

    #[test]
    fn parse_accepts_well_formed_request() {
        let r = HashrateRequest::parse(&request(3)).unwrap();
        assert_eq!(r.pool_url, POOL);
        assert_eq!(r.worker_name, "rig1");
        assert_eq!(r.amount_thh, 3);
    }

    #[test]
    fn parse_accepts_tls_pool() {
        let tls = r#"{"pool_url":"stratum+ssl://btcssl.f2pool.com:1300","worker_name":"w","amount_thh":1}"#;
        assert!(HashrateRequest::parse(tls).is_ok());
    }

    #[test]
    fn parse_rejects_bad_requests() {
        // Not JSON.
        assert!(HashrateRequest::parse("pool").is_err());
        // Unknown field.
        let extra = r#"{"pool_url":"stratum+tcp://p:1","worker_name":"w","amount_thh":1,"x":1}"#;
        assert!(HashrateRequest::parse(extra).is_err());
        // Wrong scheme.
        let http = r#"{"pool_url":"https://p","worker_name":"w","amount_thh":1}"#;
        assert!(HashrateRequest::parse(http).is_err());
        // Stratum V2 isn't supported.
        let sv2 = r#"{"pool_url":"stratum2+tcp://p:3333","worker_name":"w","amount_thh":1}"#;
        assert!(HashrateRequest::parse(sv2).is_err());
        // No port, or a path (e.g. an SV2 URL with a key).
        let no_port = r#"{"pool_url":"stratum+tcp://p","worker_name":"w","amount_thh":1}"#;
        assert!(HashrateRequest::parse(no_port).is_err());
        let path = r#"{"pool_url":"stratum+tcp://p:3333/key","worker_name":"w","amount_thh":1}"#;
        assert!(HashrateRequest::parse(path).is_err());
        // Empty worker.
        let no_worker = r#"{"pool_url":"stratum+tcp://p:1","worker_name":"","amount_thh":1}"#;
        assert!(HashrateRequest::parse(no_worker).is_err());
    }

    #[tokio::test]
    async fn settings_advertise_hashrate_under_custom() {
        let (m, _) = melter();
        let s = m.get_settings().await.unwrap();
        assert_eq!(s.unit, "THH");
        assert!(s.bolt11.is_none());
        assert!(s.custom.contains_key(METHOD));
    }

    /// The quote amount is the floor and the rest of the redemption is the
    /// fee reserve, so a wallet sends blank outputs for the refundable part.
    #[tokio::test]
    async fn quote_splits_floor_and_refundable_reserve() {
        let (m, _) = melter();
        let q = m
            .get_payment_quote(&HashrateMelter::unit(), custom(request(5)))
            .await
            .unwrap();
        assert_eq!(q.amount, Amount::new(1, HashrateMelter::unit()));
        assert_eq!(q.fee, Amount::new(4, HashrateMelter::unit()));
        assert_eq!(q.state, MeltQuoteState::Unpaid);
        // A higher minimum raises the floor.
        let q = m
            .with_min_thh(3)
            .get_payment_quote(&HashrateMelter::unit(), custom(request(5)))
            .await
            .unwrap();
        assert_eq!(q.amount, Amount::new(3, HashrateMelter::unit()));
        assert_eq!(q.fee, Amount::new(2, HashrateMelter::unit()));
    }

    #[tokio::test]
    async fn quote_enforces_maximum() {
        let (m, _) = melter();
        let m = m.with_max_thh(4);
        assert!(m
            .get_payment_quote(&HashrateMelter::unit(), custom(request(5)))
            .await
            .is_err());
        assert!(m
            .get_payment_quote(&HashrateMelter::unit(), custom(request(4)))
            .await
            .is_ok());
    }

    fn with_reserve(request: String, reserve: u64) -> OutgoingPaymentOptions {
        let OutgoingPaymentOptions::Custom(mut c) = custom(request) else {
            unreachable!()
        };
        c.max_fee_amount = Some(Amount::new(reserve, HashrateMelter::unit()));
        OutgoingPaymentOptions::Custom(c)
    }

    /// cdk hands `make_payment` the quote's fee reserve; the redemption's
    /// floor is what's left. Settling then reports only delivered THH.
    #[tokio::test]
    async fn settled_redemption_reports_delivered_thh_as_spent() {
        let (m, ledger) = melter();
        let paid = m
            .make_payment(&HashrateMelter::unit(), with_reserve(request(10), 9))
            .await
            .unwrap();
        let id = redemption_id_for(&request(10));
        assert_eq!(ledger.get(&id).unwrap().floor_thh, 1);
        // 2.5 THH delivered.
        ledger
            .credit_share(&id, 9.0e15 / (1u64 << 32) as f64)
            .unwrap();
        let check = m
            .check_outgoing_payment(&paid.payment_lookup_id)
            .await
            .unwrap();
        assert_eq!(check.status, MeltQuoteState::Pending);
        ledger.settle(&id).unwrap();
        let check = m
            .check_outgoing_payment(&paid.payment_lookup_id)
            .await
            .unwrap();
        assert_eq!(check.status, MeltQuoteState::Paid);
        assert_eq!(check.total_spent, Amount::new(3, HashrateMelter::unit()));
    }

    /// A quote from before the reserve (no fee) can't return change: its
    /// floor is the whole amount, so a settle charges all of it.
    #[tokio::test]
    async fn redemption_without_reserve_charges_everything() {
        let (m, ledger) = melter();
        let paid = m
            .make_payment(&HashrateMelter::unit(), custom(request(4)))
            .await
            .unwrap();
        let id = redemption_id_for(&request(4));
        assert_eq!(ledger.get(&id).unwrap().floor_thh, 4);
        ledger.settle(&id).unwrap();
        let check = m
            .check_outgoing_payment(&paid.payment_lookup_id)
            .await
            .unwrap();
        assert_eq!(check.status, MeltQuoteState::Paid);
        assert_eq!(check.total_spent, Amount::new(4, HashrateMelter::unit()));
    }

    #[tokio::test]
    async fn quote_enforces_minimum_and_unit() {
        let (m, _) = melter();
        let m = m.with_min_thh(4);
        assert!(m
            .get_payment_quote(&HashrateMelter::unit(), custom(request(3)))
            .await
            .is_err());
        assert!(m
            .get_payment_quote(&CurrencyUnit::Sat, custom(request(5)))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn quote_rejects_other_methods() {
        let (m, _) = melter();
        let other = OutgoingPaymentOptions::Custom(Box::new(
            cdk::cdk_payment::CustomOutgoingPaymentOptions {
                method: "paypal".into(),
                request: request(1),
                max_fee_amount: None,
                timeout_secs: None,
                melt_options: None,
                extra_json: None,
            },
        ));
        assert!(m
            .get_payment_quote(&HashrateMelter::unit(), other)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn melt_opens_pending_redemption_and_completes_on_shares() {
        let (m, ledger) = melter();
        let unit = HashrateMelter::unit();

        let made = m.make_payment(&unit, custom(request(2))).await.unwrap();
        assert_eq!(made.status, MeltQuoteState::Pending);
        assert_eq!(made.total_spent, Amount::new(2, unit.clone()));

        // Still pending on check before any shares.
        let checked = m
            .check_outgoing_payment(&made.payment_lookup_id)
            .await
            .unwrap();
        assert_eq!(checked.status, MeltQuoteState::Pending);

        // The redemption exists in the ledger with the right target.
        let id = parse_lookup_id(&made.payment_lookup_id).unwrap();
        let r = ledger.get(&id).unwrap();
        assert_eq!(r.amount_thh, 2);
        assert_eq!(r.target_hashes, 2 * hashu_core::redemption::HASHES_PER_THH);
        assert_eq!(r.pool_url, POOL);

        // Credit a share that is not enough, then one that is.
        ledger.credit_share(&id, 1.0).unwrap();
        let mid = m
            .check_outgoing_payment(&made.payment_lookup_id)
            .await
            .unwrap();
        assert_eq!(mid.status, MeltQuoteState::Pending);

        ledger.credit_share(&id, 1e12).unwrap();
        let done = m
            .check_outgoing_payment(&made.payment_lookup_id)
            .await
            .unwrap();
        assert_eq!(done.status, MeltQuoteState::Paid);
        assert_eq!(done.total_spent, Amount::new(2, unit));
    }

    #[tokio::test]
    async fn check_rejects_unknown_or_malformed_ids() {
        let (m, _) = melter();
        let unknown = lookup_id(&RedemptionId::from_bytes([9; 16]));
        assert!(m.check_outgoing_payment(&unknown).await.is_err());
        let bad = PaymentIdentifier::CustomId("not-hex".into());
        assert!(m.check_outgoing_payment(&bad).await.is_err());
        let short = PaymentIdentifier::CustomId("ab".into());
        assert!(m.check_outgoing_payment(&short).await.is_err());
    }

    #[tokio::test]
    async fn cdk_builder_registers_hashrate_as_custom_melt_method() {
        use cdk::mint::{MintBuilder, MintMeltLimits, UnitConfig};
        use cdk::nuts::PaymentMethod;

        let localstore = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
        let mut builder = MintBuilder::new(localstore);
        builder
            .configure_unit(
                HashrateMelter::unit(),
                UnitConfig {
                    amounts: vec![1, 2, 4, 8, 16, 32],
                    input_fee_ppk: 0,
                },
            )
            .unwrap();

        let (melter, _) = melter();
        builder
            .add_payment_processor(
                HashrateMelter::unit(),
                PaymentMethod::Custom(METHOD.to_string()),
                MintMeltLimits::new(1, 1000),
                Arc::new(melter),
            )
            .await
            .unwrap();

        // cdk accepted the backend: NUT-04 and NUT-05 both advertise it.
        let info = builder.current_mint_info();
        let method = PaymentMethod::Custom(METHOD.to_string());
        assert!(info.nuts.nut04.methods.iter().any(|m| m.method == method));
        assert!(info.nuts.nut05.methods.iter().any(|m| m.method == method));
    }

    #[test]
    fn unit_matches_what_cdk_deserialises() {
        // cdk serialises units lowercase, so any unit that crossed the wire
        // or the database comes back as `thh`.
        let json = serde_json::to_string(&HashrateMelter::unit()).unwrap();
        assert_eq!(json, "\"thh\"");
        let roundtrip: CurrencyUnit = serde_json::from_str(&json).unwrap();
        assert_eq!(roundtrip, HashrateMelter::unit());
    }

    #[tokio::test]
    async fn incoming_side_is_unsupported() {
        let (m, _) = melter();
        assert!(!m.is_wait_invoice_active());
        assert!(m
            .check_incoming_payment_status(&PaymentIdentifier::CustomId("x".into()))
            .await
            .unwrap()
            .is_empty());
    }
}
