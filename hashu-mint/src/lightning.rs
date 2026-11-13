//! Lightning payment rail: buy `TiH` with a bolt11 invoice. ARCHITECTURE.md
//! §4.5, §5.1.
//!
//! ```text
//! Buyer ──▶ POST /v1/mint/quote/bolt11 { unit: "TiH", amount: N }
//! Mint  ──▶ { quote, request: <bolt11 for N × sats/TiH × (1 + markup)> }
//! Buyer ──▶ pays the invoice
//! Buyer ──▶ POST /v1/mint/bolt11 { quote, outputs }  ──▶ N TiH of ecash
//! ```
//!
//! Buyers pay **real mainnet sats**. The node is any cdk backend that
//! settles in sats: [`crate::phoenixd::Phoenixd`] (the default, same node
//! setup as overpay), or `cdk-lnd` / `cdk-cln` / `cdk-ldk-node`;
//! `cdk-fake-wallet` in tests. The payment network is independent of the
//! redemption side: `TiH` bought with mainnet sats can be redeemed against
//! any pool, Testnet4 included. [`TihIssuer`] prices quotes with [`MsatPerPihFromHashprice`]
//! and translates the node's payments into `TiH`.
//!
//! Lightning is mint-only. [`add_lightning_issuance`] registers bolt11 for
//! `TiH` and then withdraws it from the NUT-05 melt methods cdk advertises,
//! because a Hashu mint never pays out sats (ARCHITECTURE.md §1).

use std::sync::Arc;

use cdk::cdk_payment::DynMintPayment;
use cdk::mint::{MintBuilder, MintMeltLimits};
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, PaymentMethod};

use crate::payment::{tih_unit, TihIssuer};
use crate::pricing::MsatPerPihFromHashprice;

/// A `TiH` issuer that sells over `node`, a cdk Lightning backend in sats.
pub fn lightning_issuer(pricer: MsatPerPihFromHashprice, node: DynMintPayment) -> TihIssuer {
    TihIssuer::new(Arc::new(pricer), node, CurrencyUnit::Sat)
}

/// Register `issuer` as the bolt11 mint method for `TiH`, without a bolt11
/// melt method. `limits` bounds mint quotes in `TiH`.
///
/// The `TiH` unit must already be configured on `builder`.
pub async fn add_lightning_issuance(
    mut builder: MintBuilder,
    issuer: TihIssuer,
    limits: MintMeltLimits,
) -> Result<MintBuilder, cdk::Error> {
    let bolt11 = PaymentMethod::Known(KnownMethod::Bolt11);
    let unit = tih_unit();
    builder
        .add_payment_processor(unit.clone(), bolt11.clone(), limits, Arc::new(issuer))
        .await?;

    // cdk advertises every bolt11 processor for melting too. Withdraw it.
    let mut info = builder.current_mint_info();
    let is_ours = |m: &PaymentMethod, u: &CurrencyUnit| *m == bolt11 && *u == unit;
    info.nuts
        .nut05
        .methods
        .retain(|m| !is_ours(&m.method, &m.unit));
    info.nuts.nut05.disabled = info.nuts.nut05.methods.is_empty();
    info.nuts
        .nut15
        .methods
        .retain(|m| !is_ours(&m.method, &m.unit));
    Ok(builder.with_mint_info(info))
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, SystemTime};

    use cdk::amount::SplitTarget;
    use cdk::mint::{MintInput, QuoteId, UnitConfig};
    use cdk::nuts::nut00::PreMintSecrets;
    use cdk::nuts::{MintQuoteBolt11Request, MintQuoteBolt11Response, MintQuoteState, MintRequest};
    use cdk::Amount;
    use cdk_fake_wallet::FakeWallet;
    use hashu_core::hashprice::{HashpriceOracle, HashpriceSample, SampleBuffer};

    use super::*;
    use crate::melt::{HashrateMelter, METHOD as HASHRATE};

    const DENOMS: [u64; 16] = {
        let mut d = [0u64; 16];
        let mut i = 0;
        while i < 16 {
            d[i] = 1 << i;
            i += 1;
        }
        d
    };

    #[derive(Debug)]
    struct FixedOracle(SampleBuffer);

    impl HashpriceOracle for FixedOracle {
        fn sample_at(&self, t: SystemTime) -> Option<f64> {
            self.0.sample_at(t)
        }
        fn latest(&self) -> Option<HashpriceSample> {
            self.0.latest()
        }
    }

    /// 48 sats/TH/day, i.e. ≈ 625.5 msat per PiH (≈ 2 sats per THH).
    fn pricer() -> MsatPerPihFromHashprice {
        let mut buf = SampleBuffer::new(4, Duration::from_secs(3600));
        buf.push(HashpriceSample::from_sats_per_th_per_day(
            SystemTime::now(),
            48.0,
        ));
        MsatPerPihFromHashprice::new(Arc::new(FixedOracle(buf)))
    }

    fn fake_node() -> DynMintPayment {
        Arc::new(FakeWallet::new(
            cdk::types::FeeReserve {
                min_fee_reserve: 1.into(),
                percent_fee_reserve: 0.0,
            },
            HashMap::new(),
            HashSet::new(),
            0,
            CurrencyUnit::Sat,
        ))
    }

    async fn builder() -> MintBuilder {
        let localstore = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
        let mut builder = MintBuilder::new(localstore);
        builder
            .configure_unit(
                tih_unit(),
                UnitConfig {
                    amounts: DENOMS.to_vec(),
                    input_fee_ppk: 0,
                },
            )
            .unwrap();
        builder
    }

    #[tokio::test]
    async fn advertises_bolt11_mint_but_not_melt() {
        let mut b = builder().await;
        b.add_payment_processor(
            tih_unit(),
            PaymentMethod::Custom(HASHRATE.into()),
            MintMeltLimits::new(1, 1 << 20),
            Arc::new(HashrateMelter::new(Default::default())),
        )
        .await
        .unwrap();
        let issuer = lightning_issuer(pricer(), fake_node());
        let b = add_lightning_issuance(b, issuer, MintMeltLimits::new(1, 1 << 20))
            .await
            .unwrap();

        let info = b.current_mint_info();
        let bolt11 = PaymentMethod::Known(KnownMethod::Bolt11);
        assert!(info
            .nuts
            .nut04
            .methods
            .iter()
            .any(|m| m.method == bolt11 && m.unit == tih_unit()));
        assert!(!info.nuts.nut05.methods.iter().any(|m| m.method == bolt11));
        assert!(info.nuts.nut15.methods.is_empty());
        // The hashrate melt is still offered.
        assert!(!info.nuts.nut05.disabled);
        assert!(info
            .nuts
            .nut05
            .methods
            .iter()
            .any(|m| m.method == PaymentMethod::Custom(HASHRATE.into())));
    }

    async fn bolt11_state(mint: &cdk::mint::Mint, id: &QuoteId) -> MintQuoteState {
        let r: MintQuoteBolt11Response<QuoteId> =
            mint.check_mint_quote(id).await.unwrap().try_into().unwrap();
        r.state
    }

    /// Full NUT-04 flow on a real cdk mint: quote 32,000 TiH, the (fake)
    /// node receives 20 sats, the mint issues 32,000 TiH of signatures.
    #[tokio::test]
    async fn buys_tih_over_bolt11_end_to_end() {
        buy_32000_tih(fake_node()).await;
    }

    /// The same flow over phoenixd's HTTP API (mocked): the invoice comes
    /// from `/createinvoice`, settlement from polling `/payments/incoming`,
    /// with a channel-opening fee taken out of what the node received.
    #[tokio::test]
    async fn buys_tih_over_phoenixd_end_to_end() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let invoice = cdk_fake_wallet::create_fake_invoice(20_000, "Hashu: 32000 TiH".into());
        let hash = hex::encode(invoice.payment_hash());
        Mock::given(method("POST"))
            .and(path("/createinvoice"))
            .and(body_string_contains("amountSat=20&"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "amountSat": 20, "paymentHash": hash, "serialized": invoice.to_string(),
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/payments/incoming/{hash}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "isPaid": true, "receivedSat": 12, "invoice": invoice.to_string(),
            })))
            .mount(&server)
            .await;

        let mut cfg = crate::phoenixd::PhoenixdConfig::new(server.uri(), "pw");
        cfg.poll_interval = Duration::from_millis(50);
        buy_32000_tih(Arc::new(crate::phoenixd::Phoenixd::new(cfg).unwrap())).await;
    }

    /// Buy 32,000 TiH (≈ 31.25 PiH ≈ 19.5 sats, billed 20) over `node` and
    /// check 32,000 TiH are issued.
    async fn buy_32000_tih(node: DynMintPayment) {
        let issuer = lightning_issuer(pricer().with_markup_bps(0), node);
        let b = add_lightning_issuance(builder().await, issuer, MintMeltLimits::new(1, 1 << 20))
            .await
            .unwrap();
        let keystore = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
        let mint = b.build_with_seed(keystore, &[42u8; 32]).await.unwrap();
        mint.start().await.unwrap();

        let quote: MintQuoteBolt11Response<QuoteId> = mint
            .get_mint_quote(
                MintQuoteBolt11Request {
                    amount: Amount::from(32_000),
                    unit: tih_unit(),
                    description: None,
                    pubkey: None,
                }
                .into(),
            )
            .await
            .unwrap()
            .try_into()
            .unwrap();
        let quote_id = quote.quote.clone();

        // The invoice is for the priced sats, not for 32,000.
        let invoice: cdk::Bolt11Invoice = quote.request.parse().unwrap();
        assert_eq!(invoice.amount_milli_satoshis(), Some(20_000));

        // The node reports it paid; wait for the mint to see it.
        let mut state = MintQuoteState::Unpaid;
        for _ in 0..50 {
            state = bolt11_state(&mint, &quote_id).await;
            if state == MintQuoteState::Paid {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(state, MintQuoteState::Paid);

        let keyset = mint
            .keysets()
            .keysets
            .into_iter()
            .find(|k| k.unit == tih_unit() && k.active)
            .unwrap();
        let premint = PreMintSecrets::random(
            keyset.id,
            Amount::from(32_000),
            &SplitTarget::default(),
            &(0, DENOMS.to_vec()).into(),
        )
        .unwrap();
        let minted = mint
            .process_mint_request(MintInput::Single(MintRequest {
                quote: quote_id.clone(),
                outputs: premint.blinded_messages(),
                signature: None,
            }))
            .await
            .unwrap();
        let total: u64 = minted.signatures.iter().map(|s| u64::from(s.amount)).sum();
        assert_eq!(total, 32_000);

        assert_eq!(bolt11_state(&mint, &quote_id).await, MintQuoteState::Issued);
        mint.stop().await.unwrap();
    }
}
