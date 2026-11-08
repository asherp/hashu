//! A redemption that settles early returns the undelivered THH as change.
//!
//! Runs a real cdk mint over HTTP with the `hashrate` melt and a fake
//! Lightning rail, and a stock cdk wallet against it. The wallet buys THH,
//! redeems all of it, and the ledger is credited part of the way before the
//! redemption is settled. The wallet's own melt flow sends the blank outputs
//! (the quote puts the refundable part in the fee reserve) and unblinds the
//! change the mint signs on them.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cdk::amount::SplitTarget;
use cdk::mint::{MintBuilder, MintMeltLimits, UnitConfig};
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, MeltQuoteState, PaymentMethod};
use cdk::wallet::{MeltOutcome, Wallet};
use cdk::Amount;
use cdk_fake_wallet::FakeWallet;
use hashu_core::hashprice::{HashpriceOracle, HashpriceSample, SampleBuffer};
use hashu_core::redemption::{RedemptionLedger, RedemptionState, HASHES_PER_THH};
use hashu_mint::lightning::{add_lightning_issuance, lightning_issuer};
use hashu_mint::melt::{redemption_id_for, HashrateMelter, METHOD as HASHRATE};
use hashu_mint::payment::thh_unit;
use hashu_mint::pricing::SatsPerThhFromHashprice;

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

struct Harness {
    ledger: Arc<RedemptionLedger>,
    wallet: Wallet,
    _server: tokio::task::JoinHandle<()>,
}

/// Mint on an ephemeral port, and a wallet holding `thh` THH from it.
async fn harness(thh: u64) -> Harness {
    let ledger = Arc::new(RedemptionLedger::new());
    let mut b = MintBuilder::new(Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap()));
    b.configure_unit(
        thh_unit(),
        UnitConfig {
            amounts: (0..11).map(|i| 1u64 << i).collect(),
            input_fee_ppk: 0,
        },
    )
    .unwrap();
    b.add_payment_processor(
        thh_unit(),
        PaymentMethod::Custom(HASHRATE.into()),
        MintMeltLimits::new(1, 1000),
        Arc::new(HashrateMelter::new(ledger.clone()).with_max_thh(1000)),
    )
    .await
    .unwrap();
    let mut prices = SampleBuffer::new(4, Duration::from_secs(3600));
    prices.push(HashpriceSample::from_sats_per_th_per_day(
        SystemTime::now(),
        48.0,
    ));
    let pricer = SatsPerThhFromHashprice::new(Arc::new(FixedOracle(prices)));
    let node = Arc::new(FakeWallet::new(
        cdk::types::FeeReserve {
            min_fee_reserve: 1.into(),
            percent_fee_reserve: 0.0,
        },
        HashMap::new(),
        HashSet::new(),
        0,
        CurrencyUnit::Sat,
    ));
    let b = add_lightning_issuance(
        b,
        lightning_issuer(pricer, node),
        MintMeltLimits::new(1, 1000),
    )
    .await
    .unwrap();
    let keystore = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
    let mint = Arc::new(b.build_with_seed(keystore, &[7u8; 32]).await.unwrap());
    mint.start().await.unwrap();

    let app = cdk_axum::create_mint_router(mint.clone(), vec![HASHRATE.to_string()])
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let wallet = Wallet::new(
        &url,
        CurrencyUnit::Custom("thh".into()),
        Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap()),
        [3u8; 64],
        None,
    )
    .unwrap();
    let quote = wallet
        .mint_quote(
            PaymentMethod::Known(KnownMethod::Bolt11),
            Some(Amount::from(thh)),
            None,
            None,
        )
        .await
        .unwrap();
    // The fake node marks the invoice paid on its own, shortly after.
    let mut minted = None;
    for _ in 0..50 {
        if let Ok(p) = wallet.mint(&quote.id, SplitTarget::default(), None).await {
            minted = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(minted.is_some(), "mint quote never paid");
    assert_eq!(u64::from(wallet.total_balance().await.unwrap()), thh);

    Harness {
        ledger,
        wallet,
        _server: server,
    }
}

/// Melt everything the wallet holds with the `hashrate` method. Returns
/// the melt quote id and the redemption id.
async fn redeem_all(
    h: &Harness,
    amount_thh: u64,
) -> (String, hashu_core::redemption::RedemptionId) {
    let request = serde_json::json!({
        "pool_url": "stratum+tcp://pool.example:3333",
        "worker_name": "bob.rig1",
        "amount_thh": amount_thh,
    })
    .to_string();
    let rid = redemption_id_for(&request);
    let quote = h
        .wallet
        .melt_quote(PaymentMethod::Custom(HASHRATE.into()), request, None, None)
        .await
        .unwrap();
    // The floor is the quote amount; the rest is the refundable reserve.
    assert_eq!(u64::from(quote.amount), 1);
    assert_eq!(u64::from(quote.fee_reserve), amount_thh - 1);
    let prepared = h
        .wallet
        .prepare_melt(&quote.id, HashMap::new())
        .await
        .unwrap();
    match prepared.confirm_prefer_async().await.unwrap() {
        MeltOutcome::Pending(_) => {}
        MeltOutcome::Paid(_) => panic!("a redemption can't be paid at once"),
    }
    assert_eq!(u64::from(h.wallet.total_balance().await.unwrap()), 0);
    assert_eq!(h.ledger.get(&rid).unwrap().floor_thh, 1);
    (quote.id, rid)
}

/// The wallet's view of the melt after checking with the mint. cdk 0.16
/// finalizes a pending melt during the check but answers with the state it
/// read before, so the first check after completion still says pending and
/// the wallet picks up the result on the next one (real wallets poll).
async fn paid_state(h: &Harness, quote_id: &str) -> MeltQuoteState {
    let mut state = MeltQuoteState::Unknown;
    for _ in 0..3 {
        state = h
            .wallet
            .check_melt_quote_status(quote_id)
            .await
            .unwrap()
            .state;
        if state == MeltQuoteState::Paid {
            break;
        }
    }
    state
}

#[tokio::test(flavor = "multi_thread")]
async fn settled_redemption_returns_undelivered_thh_as_change() {
    let h = harness(10).await;
    let (quote_id, rid) = redeem_all(&h, 10).await;

    // 3.5 THH delivered, then the miner drops off.
    let half_share = (HASHES_PER_THH * 7 / 2) as f64 / (1u64 << 32) as f64;
    h.ledger.credit_share(&rid, half_share).unwrap();
    assert_ne!(paid_state(&h, &quote_id).await, MeltQuoteState::Paid);
    assert_eq!(u64::from(h.wallet.total_balance().await.unwrap()), 0);

    let s = h.ledger.settle(&rid).unwrap();
    assert_eq!((s.spent_thh, s.change_thh), (4, 6));

    assert_eq!(paid_state(&h, &quote_id).await, MeltQuoteState::Paid);
    assert_eq!(h.ledger.get(&rid).unwrap().state, RedemptionState::Settled);
    // The wallet unblinded the change from its own blanks: 6 fresh THH.
    assert_eq!(u64::from(h.wallet.total_balance().await.unwrap()), 6);
    let proofs = h.wallet.get_unspent_proofs().await.unwrap();
    let mut amounts: Vec<u64> = proofs.iter().map(|p| u64::from(p.amount)).collect();
    amounts.sort();
    assert_eq!(amounts, vec![2, 4], "change in power-of-two denominations");
}

#[tokio::test(flavor = "multi_thread")]
async fn completed_redemption_returns_no_change() {
    let h = harness(5).await;
    let (quote_id, rid) = redeem_all(&h, 5).await;
    h.ledger.credit_share(&rid, 1e7).unwrap(); // ~4.3e16 hashes > 5 THH
    assert!(h.ledger.get(&rid).unwrap().is_complete());
    assert_eq!(paid_state(&h, &quote_id).await, MeltQuoteState::Paid);
    assert_eq!(u64::from(h.wallet.total_balance().await.unwrap()), 0);
}
