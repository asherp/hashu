//! Custom melt method `hashrate`: redeem `THH` vouchers for routed hashrate.
//! ARCHITECTURE.md §5.1, §7.
//!
//! cdk treats a custom melt method as a payment backend. `HashrateMelter`
//! is that backend, registered for `(THH, "hashrate")`:
//!
//! - **quote**: the request string carries `{pool_url, worker_name,
//!   amount_thh}`. The quote amount is `amount_thh`, with zero fee. The
//!   mint then requires melted proofs ≥ amount, and returns any excess as
//!   change.
//! - **melt**: opens a redemption in the shared [`RedemptionLedger`] and
//!   returns `Pending`. The proofs stay pending while shares accumulate.
//! - **check**: cdk's pending-melt poller calls `check_outgoing_payment`.
//!   It reports `Paid` once the redemption completes. Completion is
//!   driven by the proxy crediting accepted shares to the ledger.
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
    /// `THH` to redeem. Must be covered by the melted proofs.
    pub amount_thh: u64,
}

impl HashrateRequest {
    /// Parse and validate the request string. Checks the shape only, not
    /// the minimum amount, which the melter enforces.
    pub fn parse(request: &str) -> Result<Self, Error> {
        let parsed: Self = serde_json::from_str(request)
            .map_err(|e| Error::Custom(format!("invalid hashrate request: {e}")))?;
        if !(parsed.pool_url.starts_with("stratum+tcp://")
            || parsed.pool_url.starts_with("stratum+ssl://"))
        {
            return Err(Error::Custom(
                "pool_url must start with stratum+tcp:// or stratum+ssl://".into(),
            ));
        }
        if parsed.worker_name.is_empty() {
            return Err(Error::Custom("worker_name must not be empty".into()));
        }
        Ok(parsed)
    }
}

/// `hashrate` melt backend. Holds a handle to the shared ledger so that
/// the proxy path and the melt path see the same redemptions.
#[derive(Debug)]
pub struct HashrateMelter {
    ledger: Arc<RedemptionLedger>,
    min_thh: u64,
}

impl HashrateMelter {
    pub fn new(ledger: Arc<RedemptionLedger>) -> Self {
        Self {
            ledger,
            min_thh: DEFAULT_MIN_THH,
        }
    }

    pub fn with_min_thh(mut self, min_thh: u64) -> Self {
        self.min_thh = min_thh;
        self
    }

    /// The unit this backend accepts.
    pub fn unit() -> CurrencyUnit {
        CurrencyUnit::Custom(THH.to_string())
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
        Ok(req)
    }
}

/// Random 16-byte handle for a new redemption.
fn new_redemption_id() -> RedemptionId {
    RedemptionId::from_bytes(rand::random())
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
        Ok(PaymentQuoteResponse {
            request_lookup_id: None,
            amount: Amount::new(req.amount_thh, unit.clone()),
            fee: Amount::new(0, unit.clone()),
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

        let id = new_redemption_id();
        self.ledger
            .open(Redemption::new(
                id,
                req.amount_thh,
                req.pool_url,
                req.worker_name,
            ))
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
        // polling `check_outgoing_payment`.
        Ok(Box::pin(stream::empty()))
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
            .ok_or_else(|| Error::Custom(format!("unknown redemption {id}")))?;

        let status = match redemption.state {
            RedemptionState::Pending | RedemptionState::Active => MeltQuoteState::Pending,
            RedemptionState::Completed => MeltQuoteState::Paid,
        };
        Ok(MakePaymentResponse {
            payment_lookup_id: payment_identifier.clone(),
            payment_proof: None,
            status,
            total_spent: Amount::new(redemption.amount_thh, Self::unit()),
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

    #[test]
    fn parse_accepts_well_formed_request() {
        let r = HashrateRequest::parse(&request(3)).unwrap();
        assert_eq!(r.pool_url, POOL);
        assert_eq!(r.worker_name, "rig1");
        assert_eq!(r.amount_thh, 3);
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

    #[tokio::test]
    async fn quote_amount_is_requested_thh_with_zero_fee() {
        let (m, _) = melter();
        let q = m
            .get_payment_quote(&HashrateMelter::unit(), custom(request(5)))
            .await
            .unwrap();
        assert_eq!(q.amount, Amount::new(5, HashrateMelter::unit()));
        assert_eq!(q.fee, Amount::new(0, HashrateMelter::unit()));
        assert_eq!(q.state, MeltQuoteState::Unpaid);
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
