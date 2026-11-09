//! A redemption that settles early returns the undelivered THH as change.
//!
//! Runs a real cdk mint over HTTP with the `hashrate` melt and a fake
//! Lightning rail, and a stock cdk wallet against it. The wallet buys THH,
//! redeems all of it, and the ledger is credited part of the way before the
//! redemption is settled. The wallet's own melt flow sends the blank outputs
//! (the quote puts the refundable part in the fee reserve) and unblinds the
//! change the mint signs on them.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cdk::amount::SplitTarget;
use cdk::mint::{MintBuilder, MintMeltLimits, UnitConfig};
use cdk::mint_url::MintUrl;
use cdk::nuts::nut00::PreMintSecrets;
use cdk::nuts::nut00::{KnownMethod, ProofsMethods};
use cdk::nuts::Token;
use cdk::nuts::{CurrencyUnit, MeltQuoteState, PaymentMethod};
use cdk::wallet::{MeltOutcome, ReceiveOptions, Wallet};
use cdk::Amount;
use cdk_fake_wallet::FakeWallet;
use hashu_cli::checkout::CheckoutState;
use hashu_cli::operator::MintControls;
use hashu_cli::orders::OrderBook;
use hashu_core::hashprice::{HashpriceOracle, HashpriceSample, SampleBuffer};
use hashu_core::redemption::{RedemptionLedger, RedemptionState, HASHES_PER_THH};
use hashu_mint::lightning::{add_lightning_issuance, lightning_issuer};
use hashu_mint::melt::{redemption_id_for, HashrateMelter, METHOD as HASHRATE};
use hashu_mint::payment::thh_unit;
use hashu_mint::pricing::SatsPerThhFromHashprice;
use hashu_proxy::meter::HashrateMeter;
use hashu_proxy::ProxyMetrics;

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
    mint: Arc<cdk::mint::Mint>,
    url: String,
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
    let mint = Arc::new(
        b.build_with_seed(keystore, &rand::random::<[u8; 32]>())
            .await
            .unwrap(),
    );
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
        std::array::from_fn(|_| rand::random()),
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
        mint,
        url,
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
    // An async melt runs the backend in the background, so the redemption
    // can open just after the wallet's call returns.
    assert_eq!(opened(&h.ledger, &rid).await.floor_thh, 1);
    (quote.id, rid)
}

/// The redemption `rid`, once the mint has opened it.
async fn opened(
    ledger: &RedemptionLedger,
    rid: &hashu_core::redemption::RedemptionId,
) -> hashu_core::redemption::Redemption {
    for _ in 0..100 {
        if let Some(r) = ledger.get(rid) {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("redemption {rid} never opened")
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

/// Serve the buyer checkout against the harness mint, with `orders`.
async fn serve_checkout(h: &Harness, orders: Arc<OrderBook>) -> String {
    let ledger = h.ledger.clone();
    let meter = Arc::new(HashrateMeter::new(
        Arc::new(ProxyMetrics::default()),
        ledger.clone(),
        Duration::from_secs(60),
    ));
    let state = Arc::new(CheckoutState::new(
        h.url.clone(),
        h.url.clone(),
        Arc::new(MintControls::new(0)),
        None,
        None,
        vec!["bolt11".into()],
        1,
        1000,
        ledger,
        h.mint.clone(),
        meter,
        orders,
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = hashu_cli::checkout::router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

async fn get_json(url: &str) -> serde_json::Value {
    let r = reqwest::get(url).await.unwrap();
    assert!(r.status().is_success(), "{url}: {}", r.status());
    r.json().await.unwrap()
}

/// The redeemer's whole token as a cashuB string.
async fn token_of(h: &Harness) -> String {
    let proofs = h.wallet.get_unspent_proofs().await.unwrap();
    Token::new(
        MintUrl::from_str(&h.url).unwrap(),
        proofs,
        None,
        CurrencyUnit::Custom("thh".into()),
    )
    .to_string()
}

async fn post_json(url: &str, body: serde_json::Value) -> serde_json::Value {
    let r = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
    let ok = r.status().is_success();
    let v: serde_json::Value = r.json().await.unwrap();
    assert!(ok, "{url}: {v}");
    v
}

/// The checkout page's flow, with the browser played by this test: it
/// makes the blank outputs, the server melts the token with them, and once
/// the redemption settles the "browser" restores and unblinds the change
/// itself. The server never holds anything that can spend it.
#[tokio::test(flavor = "multi_thread")]
async fn checkout_order_change_is_unblinded_by_the_redeemer() {
    let h = harness(10).await;
    let dir = tempfile::tempdir().unwrap();
    let orders_path = dir.path().join("redemption-orders.json");
    let checkout = serve_checkout(&h, Arc::new(OrderBook::persistent(&orders_path).unwrap())).await;

    // The browser's blanks: 4 cover change below 16 THH.
    let keyset = h
        .mint
        .keysets()
        .keysets
        .into_iter()
        .find(|k| k.unit == thh_unit() && k.active)
        .unwrap();
    let blanks = PreMintSecrets::blank(keyset.id, Amount::from(10)).unwrap();
    assert_eq!(blanks.blinded_messages().len(), 4);

    let r = post_json(
        &format!("{checkout}/checkout/api/redeem"),
        serde_json::json!({
            "token": token_of(&h).await,
            "pool_url": "stratum+tcp://pool.example:3333",
            "worker_name": "bob.rig1",
            "outputs": blanks.blinded_messages(),
        }),
    )
    .await;
    let order_id = r["order_id"].as_str().expect("order id").to_string();
    assert_eq!(r["order_path"], format!("/checkout/order/{order_id}"));
    let rid: hashu_core::redemption::RedemptionId =
        r["redemption_id"].as_str().unwrap().parse().unwrap();
    // The token was melted as it was: the redeemer's wallet no longer
    // counts it as spendable, and nothing passed through a server wallet.
    let state = h
        .mint
        .check_state(&cdk::nuts::CheckStateRequest {
            ys: h.wallet.get_unspent_proofs().await.unwrap().ys().unwrap(),
        })
        .await
        .unwrap();
    assert!(state
        .states
        .iter()
        .all(|s| s.state != cdk::nuts::State::Unspent));

    let page = reqwest::get(format!("{checkout}/checkout/order/{order_id}"))
        .await
        .unwrap();
    assert!(page.status().is_success());
    let js = reqwest::get(format!("{checkout}/checkout/change.js"))
        .await
        .unwrap();
    assert!(js.status().is_success());
    let order_api = format!("{checkout}/checkout/api/order/{order_id}");
    let o = get_json(&order_api).await;
    assert!(
        ["opening", "pending"].contains(&o["state"].as_str().unwrap()),
        "{o}"
    );
    assert_eq!(o["change"]["state"], "open");
    let missing = reqwest::get(format!(
        "{checkout}/checkout/api/order/{}",
        hashu_cli::orders::new_order_id()
    ))
    .await
    .unwrap();
    assert_eq!(missing.status(), 404);

    // 3.5 THH delivered, then settled: 6 THH of change, signed on our blanks.
    opened(&h.ledger, &rid).await;
    let share = (HASHES_PER_THH * 7 / 2) as f64 / (1u64 << 32) as f64;
    h.ledger.credit_share(&rid, share).unwrap();
    h.ledger.settle(&rid).unwrap();
    let o = get_json(&order_api).await;
    assert_eq!(o["state"], "settled");
    assert_eq!(o["change"]["state"], "ready", "{o}");
    assert_eq!(o["change"]["thh"], 6);
    assert_eq!(o["change"]["mint_url"], h.url);
    // The server reports, it doesn't hold: no token, and nothing secret on disk.
    assert!(o["change"].get("token").is_none());
    let saved = std::fs::read_to_string(&orders_path).unwrap();
    assert!(
        !saved.contains("seed") && !saved.contains("token"),
        "{saved}"
    );

    // The "browser" restores the signatures on its blanks over the mint's
    // public API (NUT-09) and unblinds them.
    let restored: cdk::nuts::RestoreResponse = serde_json::from_value(
        post_json(
            &format!("{}/v1/restore", h.url),
            serde_json::json!({ "outputs": blanks.blinded_messages() }),
        )
        .await,
    )
    .unwrap();
    let mut rs = Vec::new();
    let mut secrets = Vec::new();
    for out in &restored.outputs {
        let p = blanks
            .iter()
            .find(|p| p.blinded_message.blinded_secret == out.blinded_secret)
            .unwrap();
        rs.push(p.r.clone());
        secrets.push(p.secret.clone());
    }
    let keys = h.mint.keyset_pubkeys(&keyset.id).unwrap().keysets[0]
        .keys
        .clone();
    let proofs = cdk::dhke::construct_proofs(restored.signatures, rs, secrets, &keys).unwrap();
    let mut amounts: Vec<u64> = proofs.iter().map(|p| u64::from(p.amount)).collect();
    amounts.sort();
    assert_eq!(amounts, vec![2, 4]);

    // A restart (same orders file) still reports the change as ready.
    let restarted =
        serve_checkout(&h, Arc::new(OrderBook::persistent(&orders_path).unwrap())).await;
    let again = get_json(&format!("{restarted}/checkout/api/order/{order_id}")).await;
    assert_eq!(again["change"]["state"], "ready");

    // The change is real THH: another wallet can receive it.
    let change = Token::new(
        MintUrl::from_str(&h.url).unwrap(),
        proofs,
        None,
        CurrencyUnit::Custom("thh".into()),
    )
    .to_string();
    let other = Wallet::new(
        &h.url,
        CurrencyUnit::Custom("thh".into()),
        Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap()),
        [5u8; 64],
        None,
    )
    .unwrap();
    let got = other
        .receive(&change, ReceiveOptions::default())
        .await
        .unwrap();
    assert_eq!(u64::from(got), 6);
}

/// Without blank outputs (an API client that sent none), an early settle
/// has nowhere to sign the change: the order says so.
#[tokio::test(flavor = "multi_thread")]
async fn order_without_blanks_reports_unclaimable_change() {
    let h = harness(4).await;
    let checkout = serve_checkout(&h, Arc::new(OrderBook::new())).await;
    let r = post_json(
        &format!("{checkout}/checkout/api/redeem"),
        serde_json::json!({
            "token": token_of(&h).await,
            "pool_url": "stratum+tcp://pool.example:3333",
            "worker_name": "bob.rig1",
        }),
    )
    .await;
    let rid: hashu_core::redemption::RedemptionId =
        r["redemption_id"].as_str().unwrap().parse().unwrap();
    opened(&h.ledger, &rid).await;
    h.ledger.settle(&rid).unwrap();
    let o = get_json(&format!(
        "{checkout}/checkout/api/order/{}",
        r["order_id"].as_str().unwrap()
    ))
    .await;
    assert_eq!(o["change"]["state"], "unclaimable", "{o}");
    assert_eq!(o["change"]["thh"], 3);
}

/// A redemption delivered in full has no change to hand back.
#[tokio::test(flavor = "multi_thread")]
async fn order_without_change_reports_none() {
    let h = harness(3).await;
    let checkout = serve_checkout(&h, Arc::new(OrderBook::new())).await;
    let r = post_json(
        &format!("{checkout}/checkout/api/redeem"),
        serde_json::json!({
            "token": token_of(&h).await,
            "pool_url": "stratum+tcp://pool.example:3333",
            "worker_name": "bob.rig1",
        }),
    )
    .await;
    let rid: hashu_core::redemption::RedemptionId =
        r["redemption_id"].as_str().unwrap().parse().unwrap();
    opened(&h.ledger, &rid).await;
    h.ledger.credit_share(&rid, 1e7).unwrap(); // more than 3 THH
    let o = get_json(&format!(
        "{checkout}/checkout/api/order/{}",
        r["order_id"].as_str().unwrap()
    ))
    .await;
    assert_eq!(o["state"], "completed");
    assert_eq!(o["change"]["state"], "none");
}

async fn post_status(url: &str, body: serde_json::Value) -> (u16, serde_json::Value) {
    let r = reqwest::Client::new()
        .post(url)
        .json(&body)
        .send()
        .await
        .unwrap();
    let code = r.status().as_u16();
    (code, r.json().await.unwrap_or_default())
}

/// The redeemer stops a running redemption from the order page: only the
/// stop secret (derived in the browser from the link's key) works, and the
/// undelivered THH come back as change on the redeemer's blanks.
#[tokio::test(flavor = "multi_thread")]
async fn redeemer_stops_the_redemption_with_the_link_key() {
    use sha2::{Digest, Sha256};
    let h = harness(10).await;
    let checkout = serve_checkout(&h, Arc::new(OrderBook::new())).await;
    let keyset = h
        .mint
        .keysets()
        .keysets
        .into_iter()
        .find(|k| k.unit == thh_unit() && k.active)
        .unwrap();
    let blanks = PreMintSecrets::blank(keyset.id, Amount::from(10)).unwrap();
    let secret = [7u8; 32];
    let r = post_json(
        &format!("{checkout}/checkout/api/redeem"),
        serde_json::json!({
            "token": token_of(&h).await,
            "pool_url": "stratum+tcp://pool.example:3333",
            "worker_name": "bob.rig1",
            "outputs": blanks.blinded_messages(),
            "stop_hash": hex::encode(Sha256::digest(secret)),
        }),
    )
    .await;
    let order_id = r["order_id"].as_str().unwrap().to_string();
    let rid: hashu_core::redemption::RedemptionId =
        r["redemption_id"].as_str().unwrap().parse().unwrap();
    opened(&h.ledger, &rid).await;
    let order_api = format!("{checkout}/checkout/api/order/{order_id}");
    let o = get_json(&order_api).await;
    assert_eq!(o["can_stop"], true, "{o}");
    assert_eq!(o["floor_thh"], 1);

    // 2.5 THH delivered, then the redeemer stops.
    let share = (HASHES_PER_THH * 5 / 2) as f64 / (1u64 << 32) as f64;
    h.ledger.credit_share(&rid, share).unwrap();
    let stop = format!("{order_api}/stop");
    let (code, _) = post_status(
        &stop,
        serde_json::json!({ "stop_secret": hex::encode([8u8; 32]) }),
    )
    .await;
    assert_eq!(code, 403, "a wrong secret can't stop it");
    let (code, _) = post_status(&stop, serde_json::json!({})).await;
    assert_eq!(code, 400);
    assert!(!h.ledger.get(&rid).unwrap().is_finished());

    let (code, o) = post_status(
        &stop,
        serde_json::json!({ "stop_secret": hex::encode(secret) }),
    )
    .await;
    assert_eq!(code, 200, "{o}");
    assert_eq!(o["state"], "settled");
    assert_eq!(o["spent_thh"], 3);
    assert_eq!(o["change"]["state"], "ready", "{o}");
    assert_eq!(o["change"]["thh"], 7);
    // Stopping again changes nothing.
    let (code, again) = post_status(
        &stop,
        serde_json::json!({ "stop_secret": hex::encode(secret) }),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(again["change"]["thh"], 7);

    // The change is on the redeemer's blanks: 7 = 1 + 2 + 4.
    let restored: cdk::nuts::RestoreResponse = serde_json::from_value(
        post_json(
            &format!("{}/v1/restore", h.url),
            serde_json::json!({ "outputs": blanks.blinded_messages() }),
        )
        .await,
    )
    .unwrap();
    let mut amounts: Vec<u64> = restored
        .signatures
        .iter()
        .map(|s| u64::from(s.amount))
        .collect();
    amounts.sort();
    assert_eq!(amounts, vec![1, 2, 4]);
}

/// Without a stop hash, only the operator can stop a redemption.
#[tokio::test(flavor = "multi_thread")]
async fn order_without_stop_hash_cannot_be_stopped_by_the_redeemer() {
    let h = harness(4).await;
    let checkout = serve_checkout(&h, Arc::new(OrderBook::new())).await;
    let r = post_json(
        &format!("{checkout}/checkout/api/redeem"),
        serde_json::json!({
            "token": token_of(&h).await,
            "pool_url": "stratum+tcp://pool.example:3333",
            "worker_name": "bob.rig1",
        }),
    )
    .await;
    let rid: hashu_core::redemption::RedemptionId =
        r["redemption_id"].as_str().unwrap().parse().unwrap();
    opened(&h.ledger, &rid).await;
    let order_api = format!(
        "{checkout}/checkout/api/order/{}",
        r["order_id"].as_str().unwrap()
    );
    assert_eq!(get_json(&order_api).await["can_stop"], false);
    let (code, _) = post_status(
        &format!("{order_api}/stop"),
        serde_json::json!({ "stop_secret": hex::encode([7u8; 32]) }),
    )
    .await;
    assert_eq!(code, 403);
    assert!(!h.ledger.get(&rid).unwrap().is_finished());
    // A malformed stop hash is refused before any token is spent.
    let (code, _) = post_status(
        &format!("{checkout}/checkout/api/redeem"),
        serde_json::json!({
            "token": token_of(&h).await,
            "pool_url": "stratum+tcp://pool.example:3333",
            "worker_name": "bob.rig1",
            "stop_hash": "nope",
        }),
    )
    .await;
    assert_eq!(code, 400);
}
