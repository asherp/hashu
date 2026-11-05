//! Selling `THH` over a payment rail. ARCHITECTURE.md §5.1.
//!
//! cdk ties every payment backend to one unit, and it records a payment by
//! converting the backend's reported amount into the quote's unit. It can
//! convert sats to msat but not sats to `THH`. So a rail that settles in
//! sats (or micro-USDT) cannot be registered for `THH` as is.
//!
//! [`ThhIssuer`] closes that gap. It wraps a rail-native backend (for
//! Lightning: any cdk backend in sats, such as `cdk-lnd`, `cdk-cln` or
//! `cdk-ldk-node`) and registers in its place for unit `THH`:
//!
//! - **mint quote**: the buyer asks for `N` THH. The issuer prices `N` with
//!   its [`ThhPricer`], asks the inner backend for a payment request in the
//!   rail's unit (a bolt11 invoice for that many sats), and remembers the
//!   lookup id → `N` THH.
//! - **payment received**: the inner backend reports sats. The issuer
//!   translates that into `THH` for the quote (capped at `N`, pro rata if
//!   short), so cdk sees a payment in the quote's own unit.
//! - **outgoing**: refused. A Hashu mint sells computation and never pays
//!   out bitcoin (ARCHITECTURE.md §1), so `THH` can't be melted to a rail.
//!
//! Limitation: the lookup id → THH book is in memory. A quote created before
//! a restart and paid after it is not credited automatically.

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use cdk::cdk_payment::{
    CreateIncomingPaymentResponse, DynMintPayment, Error, Event, IncomingPaymentOptions,
    MakePaymentResponse, MintPayment, OutgoingPaymentOptions, PaymentIdentifier,
    PaymentQuoteResponse, SettingsResponse, WaitPaymentResponse,
};
use cdk::nuts::CurrencyUnit;
use cdk::Amount;
use futures::stream::{BoxStream, StreamExt};

use crate::pricing::ThhPricer;

/// Book entries are pruned this long after their quote expires.
const BOOK_GRACE_SECS: u64 = 24 * 3600;

/// Error text for every outgoing call.
pub const NO_PAYOUTS: &str = "THH is redeemed for hashrate only; this mint does not pay out";

/// The `THH` unit as cdk holds it: lowercase `thh`.
///
/// cdk serializes units in lowercase (NUT-01 units are lowercase on the
/// wire) and parses them as written, so anything that went through JSON,
/// including cdk's own stored mint info, compares as `Custom("thh")`.
/// `Custom("THH")` (and `CurrencyUnit::custom`, which uppercases) would
/// never match it.
pub fn thh_unit() -> CurrencyUnit {
    CurrencyUnit::Custom(hashu_core::denom::UNIT_THH.to_lowercase())
}

/// What a mint quote sold, kept until its payment arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sale {
    amount_thh: u64,
    /// Price in the rail's unit.
    price: Amount<CurrencyUnit>,
    /// Unix expiry of the quote, if known.
    expiry: Option<u64>,
}

#[derive(Debug, Default)]
struct Book(Mutex<HashMap<PaymentIdentifier, Sale>>);

impl Book {
    fn insert(&self, id: PaymentIdentifier, sale: Sale) {
        let now = unix_now();
        let mut book = self.0.lock().expect("book lock");
        book.retain(|_, s| s.expiry.is_none_or(|e| e + BOOK_GRACE_SECS > now));
        book.insert(id, sale);
    }

    fn get(&self, id: &PaymentIdentifier) -> Option<Sale> {
        self.0.lock().expect("book lock").get(id).cloned()
    }

    /// Re-denominate a rail payment in `THH`, or `None` if it isn't one of
    /// ours or is too small to buy a single `THH`.
    fn translate(&self, payment: WaitPaymentResponse) -> Option<WaitPaymentResponse> {
        let Some(sale) = self.get(&payment.payment_identifier) else {
            tracing::warn!(
                id = ?payment.payment_identifier,
                "payment for an unknown THH quote; not credited"
            );
            return None;
        };
        let paid = match payment.payment_amount.convert_to(sale.price.unit()) {
            Ok(a) => a.value(),
            Err(e) => {
                tracing::error!(
                    id = ?payment.payment_identifier,
                    "payment unit {} does not match price unit {}: {e}",
                    payment.payment_amount.unit(),
                    sale.price.unit()
                );
                return None;
            }
        };
        let thh = thh_for(paid, sale.price.value(), sale.amount_thh);
        if thh == 0 {
            tracing::warn!(
                id = ?payment.payment_identifier,
                paid,
                price = sale.price.value(),
                "payment too small for 1 THH; not credited"
            );
            return None;
        }
        Some(WaitPaymentResponse {
            payment_identifier: payment.payment_identifier,
            payment_amount: Amount::new(thh, thh_unit()),
            payment_id: payment.payment_id,
        })
    }
}

/// THH earned by paying `paid` toward a quote of `amount_thh` priced at
/// `price`: all of it once paid in full, pro rata (rounded down) before.
fn thh_for(paid: u64, price: u64, amount_thh: u64) -> u64 {
    if price == 0 || paid >= price {
        return amount_thh;
    }
    ((paid as u128 * amount_thh as u128) / price as u128) as u64
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A `THH` payment backend over a rail-native backend. See the module docs.
pub struct ThhIssuer {
    pricer: Arc<dyn ThhPricer>,
    rail: DynMintPayment,
    rail_unit: CurrencyUnit,
    book: Arc<Book>,
}

impl Debug for ThhIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThhIssuer")
            .field("pricer", &self.pricer)
            .field("rail_unit", &self.rail_unit)
            .finish_non_exhaustive()
    }
}

impl ThhIssuer {
    /// `rail` is the rail-native backend, and `rail_unit` the unit `pricer`
    /// prices in (e.g. `CurrencyUnit::Sat` for Lightning).
    pub fn new(pricer: Arc<dyn ThhPricer>, rail: DynMintPayment, rail_unit: CurrencyUnit) -> Self {
        Self {
            pricer,
            rail,
            rail_unit,
            book: Arc::default(),
        }
    }

    /// Price `amount` (which must be in `THH`) on the rail.
    fn price(&self, amount: &Amount<CurrencyUnit>) -> Result<Amount<CurrencyUnit>, Error> {
        if *amount.unit() != thh_unit() {
            return Err(Error::UnsupportedUnit);
        }
        let price = self
            .pricer
            .price(amount.value())
            .map_err(|e| Error::Custom(format!("cannot price {} THH: {e}", amount.value())))?;
        Ok(Amount::new(price, self.rail_unit.clone()))
    }
}

#[async_trait]
impl MintPayment for ThhIssuer {
    type Err = Error;

    async fn start(&self) -> Result<(), Self::Err> {
        self.rail.start().await
    }

    async fn stop(&self) -> Result<(), Self::Err> {
        self.rail.stop().await
    }

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        let rail = self.rail.get_settings().await?;
        Ok(SettingsResponse {
            unit: hashu_core::denom::UNIT_THH.to_string(),
            // Quotes must name an amount to be priced, and MPP is a melt
            // feature, which THH doesn't offer over a rail.
            bolt11: rail.bolt11.map(|b| cdk::cdk_payment::Bolt11Settings {
                mpp: false,
                amountless: false,
                invoice_description: b.invoice_description,
            }),
            // Bolt12 offers are reusable and amountless by nature, so a
            // locked-in hashprice doesn't fit them. Not offered.
            bolt12: None,
            custom: rail.custom,
        })
    }

    async fn create_incoming_payment_request(
        &self,
        options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        let (rail_options, amount_thh, price, expiry) = match options {
            IncomingPaymentOptions::Bolt11(mut o) => {
                let amount_thh = o.amount.value();
                let price = self.price(&o.amount)?;
                o.amount = price.clone();
                o.description
                    .get_or_insert_with(|| format!("Hashu: {amount_thh} THH"));
                let expiry = o.unix_expiry;
                (IncomingPaymentOptions::Bolt11(o), amount_thh, price, expiry)
            }
            IncomingPaymentOptions::Custom(mut o) => {
                let amount_thh = o.amount.value();
                let price = self.price(&o.amount)?;
                o.amount = price.clone();
                let expiry = o.unix_expiry;
                (IncomingPaymentOptions::Custom(o), amount_thh, price, expiry)
            }
            IncomingPaymentOptions::Bolt12(_) => return Err(Error::UnsupportedPaymentOption),
        };

        let response = self
            .rail
            .create_incoming_payment_request(rail_options)
            .await?;
        tracing::info!(
            amount_thh,
            price = %price.display_with_unit(),
            id = ?response.request_lookup_id,
            "THH mint quote priced"
        );
        self.book.insert(
            response.request_lookup_id.clone(),
            Sale {
                amount_thh,
                price,
                expiry: response.expiry.or(expiry),
            },
        );
        Ok(response)
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
        let book = self.book.clone();
        let events = self.rail.wait_payment_event().await?;
        Ok(events
            .filter_map(move |event| {
                let Event::PaymentReceived(payment) = event;
                let translated = book.translate(payment).map(Event::PaymentReceived);
                async move { translated }
            })
            .boxed())
    }

    fn is_wait_invoice_active(&self) -> bool {
        self.rail.is_wait_invoice_active()
    }

    fn cancel_wait_invoice(&self) {
        self.rail.cancel_wait_invoice()
    }

    async fn check_incoming_payment_status(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        if self.book.get(payment_identifier).is_none() {
            return Ok(Vec::new());
        }
        let payments = self
            .rail
            .check_incoming_payment_status(payment_identifier)
            .await?;
        Ok(payments
            .into_iter()
            .filter_map(|p| self.book.translate(p))
            .collect())
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
    use crate::pricing::PricingError;
    use cdk::cdk_payment::{Bolt11IncomingPaymentOptions, CustomIncomingPaymentOptions};

    /// Two sats per THH, or no price at all.
    #[derive(Debug)]
    struct FixedPricer(Option<u64>);

    impl ThhPricer for FixedPricer {
        fn price(&self, amount_thh: u64) -> Result<u64, PricingError> {
            self.0
                .map(|per| per * amount_thh)
                .ok_or(PricingError::NoPrice)
        }
    }

    /// Records what the issuer asked the rail for, and replays payments.
    #[derive(Default)]
    struct FakeRail {
        asked: Mutex<Vec<IncomingPaymentOptions>>,
        paid: Mutex<Vec<WaitPaymentResponse>>,
    }

    const HASH: [u8; 32] = [7; 32];

    #[async_trait]
    impl MintPayment for FakeRail {
        type Err = Error;
        async fn get_settings(&self) -> Result<SettingsResponse, Error> {
            Ok(SettingsResponse {
                unit: "sat".into(),
                bolt11: Some(cdk::cdk_payment::Bolt11Settings {
                    mpp: true,
                    amountless: true,
                    invoice_description: true,
                }),
                bolt12: Some(cdk::cdk_payment::Bolt12Settings { amountless: true }),
                custom: HashMap::new(),
            })
        }
        async fn create_incoming_payment_request(
            &self,
            options: IncomingPaymentOptions,
        ) -> Result<CreateIncomingPaymentResponse, Error> {
            self.asked.lock().unwrap().push(options);
            Ok(CreateIncomingPaymentResponse {
                request_lookup_id: PaymentIdentifier::PaymentHash(HASH),
                request: "lntb1fake".into(),
                expiry: None,
                extra_json: None,
            })
        }
        async fn get_payment_quote(
            &self,
            _: &CurrencyUnit,
            _: OutgoingPaymentOptions,
        ) -> Result<PaymentQuoteResponse, Error> {
            unreachable!("issuer must not forward outgoing calls")
        }
        async fn make_payment(
            &self,
            _: &CurrencyUnit,
            _: OutgoingPaymentOptions,
        ) -> Result<MakePaymentResponse, Error> {
            unreachable!("issuer must not forward outgoing calls")
        }
        async fn wait_payment_event(&self) -> Result<BoxStream<'static, Event>, Error> {
            let paid = self.paid.lock().unwrap().clone();
            Ok(futures::stream::iter(paid.into_iter().map(Event::PaymentReceived)).boxed())
        }
        fn is_wait_invoice_active(&self) -> bool {
            true
        }
        fn cancel_wait_invoice(&self) {}
        async fn check_incoming_payment_status(
            &self,
            id: &PaymentIdentifier,
        ) -> Result<Vec<WaitPaymentResponse>, Error> {
            Ok(self
                .paid
                .lock()
                .unwrap()
                .iter()
                .filter(|p| &p.payment_identifier == id)
                .cloned()
                .collect())
        }
        async fn check_outgoing_payment(
            &self,
            _: &PaymentIdentifier,
        ) -> Result<MakePaymentResponse, Error> {
            unreachable!("issuer must not forward outgoing calls")
        }
    }

    fn issuer(per_thh: Option<u64>) -> (ThhIssuer, Arc<FakeRail>) {
        let rail = Arc::new(FakeRail::default());
        let issuer = ThhIssuer::new(
            Arc::new(FixedPricer(per_thh)),
            rail.clone(),
            CurrencyUnit::Sat,
        );
        (issuer, rail)
    }

    fn bolt11(amount: Amount<CurrencyUnit>) -> IncomingPaymentOptions {
        IncomingPaymentOptions::Bolt11(Bolt11IncomingPaymentOptions {
            description: None,
            amount,
            unix_expiry: Some(unix_now() + 600),
        })
    }

    fn thh(n: u64) -> Amount<CurrencyUnit> {
        Amount::new(n, thh_unit())
    }

    fn paid(amount: Amount<CurrencyUnit>) -> WaitPaymentResponse {
        WaitPaymentResponse {
            payment_identifier: PaymentIdentifier::PaymentHash(HASH),
            payment_amount: amount,
            payment_id: "p1".into(),
        }
    }

    #[test]
    fn thh_for_is_full_once_paid_and_pro_rata_before() {
        assert_eq!(thh_for(20, 20, 10), 10);
        assert_eq!(thh_for(25, 20, 10), 10);
        assert_eq!(thh_for(10, 20, 10), 5);
        assert_eq!(thh_for(1, 20, 10), 0);
    }

    #[tokio::test]
    async fn settings_are_thh_bolt11_with_amount_and_no_bolt12() {
        let (i, _) = issuer(Some(2));
        let s = i.get_settings().await.unwrap();
        assert_eq!(s.unit, "THH");
        let b = s.bolt11.unwrap();
        assert!(!b.amountless && !b.mpp && b.invoice_description);
        assert!(s.bolt12.is_none());
    }

    #[tokio::test]
    async fn bolt11_quote_for_thh_asks_rail_for_priced_sats() {
        let (i, rail) = issuer(Some(2));
        let r = i
            .create_incoming_payment_request(bolt11(thh(10)))
            .await
            .unwrap();
        assert_eq!(r.request, "lntb1fake");

        let asked = rail.asked.lock().unwrap();
        let IncomingPaymentOptions::Bolt11(o) = &asked[0] else {
            panic!("expected bolt11");
        };
        assert_eq!(o.amount, Amount::new(20, CurrencyUnit::Sat));
        assert_eq!(o.description.as_deref(), Some("Hashu: 10 THH"));
    }

    #[tokio::test]
    async fn custom_quote_is_priced_too() {
        let (i, rail) = issuer(Some(3));
        let opts = IncomingPaymentOptions::Custom(Box::new(CustomIncomingPaymentOptions {
            method: "usdt".into(),
            description: None,
            amount: thh(4),
            unix_expiry: None,
            extra_json: None,
        }));
        i.create_incoming_payment_request(opts).await.unwrap();
        let asked = rail.asked.lock().unwrap();
        let IncomingPaymentOptions::Custom(o) = &asked[0] else {
            panic!("expected custom");
        };
        assert_eq!(o.amount, Amount::new(12, CurrencyUnit::Sat));
    }

    #[tokio::test]
    async fn quote_refuses_other_units_bolt12_and_missing_price() {
        let (i, rail) = issuer(Some(2));
        let sats = Amount::new(10, CurrencyUnit::Sat);
        assert!(i
            .create_incoming_payment_request(bolt11(sats))
            .await
            .is_err());
        let b12 = IncomingPaymentOptions::Bolt12(Box::default());
        assert!(i.create_incoming_payment_request(b12).await.is_err());

        let (no_price, _) = issuer(None);
        assert!(no_price
            .create_incoming_payment_request(bolt11(thh(1)))
            .await
            .is_err());
        assert!(rail.asked.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn payment_events_are_reported_in_thh() {
        let (i, rail) = issuer(Some(2));
        i.create_incoming_payment_request(bolt11(thh(10)))
            .await
            .unwrap();
        // LN backends commonly report msat.
        rail.paid
            .lock()
            .unwrap()
            .push(paid(Amount::new(20_000, CurrencyUnit::Msat)));

        let events: Vec<Event> = i.wait_payment_event().await.unwrap().collect().await;
        assert_eq!(events.len(), 1);
        let Event::PaymentReceived(p) = &events[0];
        assert_eq!(p.payment_amount, thh(10));
        assert_eq!(p.payment_id, "p1");
    }

    #[tokio::test]
    async fn check_reports_thh_and_drops_unknown_or_dust() {
        let (i, rail) = issuer(Some(2));
        let id = PaymentIdentifier::PaymentHash(HASH);

        // Unknown quote: nothing, even if the rail saw a payment.
        rail.paid
            .lock()
            .unwrap()
            .push(paid(Amount::new(20, CurrencyUnit::Sat)));
        assert!(i
            .check_incoming_payment_status(&id)
            .await
            .unwrap()
            .is_empty());

        i.create_incoming_payment_request(bolt11(thh(10)))
            .await
            .unwrap();
        let got = i.check_incoming_payment_status(&id).await.unwrap();
        assert_eq!(got[0].payment_amount, thh(10));

        // Paying 1 sat of a 20 sat price for 10 THH buys nothing.
        rail.paid.lock().unwrap()[0] = paid(Amount::new(1, CurrencyUnit::Sat));
        assert!(i
            .check_incoming_payment_status(&id)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn never_pays_out() {
        let (i, _) = issuer(Some(2));
        let id = PaymentIdentifier::PaymentHash(HASH);
        let err = i.check_outgoing_payment(&id).await.unwrap_err();
        assert!(err.to_string().contains("does not pay out"));
    }
}
