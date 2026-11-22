//! Epoch-dated issuance end to end (#126).
//!
//! A real cdk mint over HTTP with epochs on (the chain clock is set by
//! hand), a fake Lightning rail and a stock cdk wallet. Vouchers sold in
//! epoch 10 expire when epoch 11 starts; the wallet converts them with the
//! `epoch` melt and has the outputs signed with the new epoch's key. A
//! forward conversion dates vouchers to an epoch that has not started, and
//! the mint refuses them until it does.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use cdk::amount::SplitTarget;
use cdk::mint::{MintBuilder, MintMeltLimits, UnitConfig};
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::nut00::PreMintSecrets;
use cdk::nuts::{BlindSignature, CurrencyUnit, KeySet, MeltQuoteState, PaymentMethod};
use cdk::wallet::Wallet;
use cdk::Amount;
use cdk_fake_wallet::FakeWallet;
use hashu_core::epoch::{EpochClock, Supply};
use hashu_core::hashprice::{HashpriceOracle, HashpriceSample, SampleBuffer};
use hashu_core::redemption::RedemptionLedger;
use hashu_mint::epoch::{EpochConverter, Epochs, METHOD as EPOCH};
use hashu_mint::lightning::{add_lightning_issuance, lightning_issuer};
use hashu_mint::melt::{HashrateMelter, METHOD as HASHRATE};
use hashu_mint::payment::tih_unit;
use hashu_mint::pricing::MsatPerPihFromHashprice;

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

const SEED: [u8; 32] = [5u8; 32];

fn denoms() -> Vec<u64> {
    (0..41).map(|i| 1u64 << i).collect()
}

fn tip_in(epoch: u64) -> EpochClock {
    EpochClock::new(epoch * 2016 + 100)
}

struct Harness {
    mint: Arc<cdk::Mint>,
    keys: Arc<cdk_sqlite::MintSqliteDatabase>,
    epochs: Arc<Epochs>,
    url: String,
    http: reqwest::Client,
    _server: tokio::task::JoinHandle<()>,
}

async fn harness() -> Harness {
    let ledger = Arc::new(RedemptionLedger::new());
    let epochs = Arc::new(Epochs::open(&SEED, denoms(), None).unwrap());
    epochs.set_clock(tip_in(10));
    epochs.set_plan_inputs(Arc::new(|| {
        (
            vec![Supply {
                ths: 1.0,
                hours_left: None,
            }],
            0.0,
        )
    }));
    let mut b = MintBuilder::new(Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap()));
    b.configure_unit(
        tih_unit(),
        UnitConfig {
            amounts: denoms(),
            input_fee_ppk: 0,
        },
    )
    .unwrap();
    b.add_payment_processor(
        tih_unit(),
        PaymentMethod::Custom(HASHRATE.into()),
        MintMeltLimits::new(1, 1 << 20),
        Arc::new(HashrateMelter::new(ledger.clone()).with_min_tih(1)),
    )
    .await
    .unwrap();
    b.add_payment_processor(
        tih_unit(),
        PaymentMethod::Custom(EPOCH.into()),
        MintMeltLimits::new(1, 1 << 20),
        Arc::new(EpochConverter::new(epochs.clone())),
    )
    .await
    .unwrap();
    let mut prices = SampleBuffer::new(4, Duration::from_secs(3600));
    prices.push(HashpriceSample::from_sats_per_th_per_day(
        SystemTime::now(),
        48.0,
    ));
    let pricer = MsatPerPihFromHashprice::new(Arc::new(FixedOracle(prices)));
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
        MintMeltLimits::new(1, 1 << 20),
    )
    .await
    .unwrap();
    let keys = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
    let mint = Arc::new(b.build_with_seed(keys.clone(), &SEED).await.unwrap());
    mint.start().await.unwrap();

    let app = cdk_axum::create_mint_router(mint.clone(), vec![HASHRATE.into(), EPOCH.into()])
        .await
        .unwrap()
        .merge(hashu_cli::epochs::router(epochs.clone(), mint.clone()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Harness {
        mint,
        keys,
        epochs,
        url,
        http: reqwest::Client::new(),
        _server: server,
    }
}

impl Harness {
    async fn sync(&self) {
        self.epochs
            .sync(&self.mint, self.keys.as_ref(), true)
            .await
            .unwrap();
    }

    fn active(&self) -> cdk::nuts::Id {
        self.mint
            .keysets()
            .keysets
            .into_iter()
            .find(|k| k.unit == tih_unit() && k.active)
            .unwrap()
            .id
    }

    async fn wallet(&self) -> Wallet {
        Wallet::new(
            &self.url,
            CurrencyUnit::Custom("tih".into()),
            Arc::new(cdk_sqlite::wallet::memory::empty().await.unwrap()),
            std::array::from_fn(|_| rand::random()),
            None,
        )
        .unwrap()
    }

    async fn get(&self, path: &str) -> serde_json::Value {
        let r = self
            .http
            .get(format!("{}{path}", self.url))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }

    /// Convert `amount` of the wallet's TiH to `epoch` ("auto" or a
    /// number). Returns the epoch it went to and the signed proofs.
    async fn convert(
        &self,
        wallet: &Wallet,
        epoch: serde_json::Value,
        amount: u64,
    ) -> (u64, cdk::nuts::Proofs) {
        let request = serde_json::json!({ "epoch": epoch, "amount_tih": amount }).to_string();
        let quote = wallet
            .melt_quote(PaymentMethod::Custom(EPOCH.into()), request, None, None)
            .await
            .unwrap();
        let prepared = wallet
            .prepare_melt(&quote.id, HashMap::new())
            .await
            .unwrap();
        // The mint settles it at once, but cdk 0.16's wallet can miss the
        // socket notice of that; stop waiting and ask instead.
        let _ = tokio::time::timeout(Duration::from_secs(2), prepared.confirm()).await;
        let mut state = MeltQuoteState::Unknown;
        for _ in 0..100 {
            state = wallet
                .check_melt_quote_status(&quote.id)
                .await
                .unwrap()
                .state;
            if state == MeltQuoteState::Paid {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if state != MeltQuoteState::Paid {
            let q: Vec<_> = self
                .mint
                .melt_quotes()
                .await
                .unwrap()
                .into_iter()
                .map(|q| (q.id.to_string(), q.state))
                .collect();
            panic!("wallet sees {state}, mint has {q:?}, quote {}", quote.id);
        }
        let conv = self
            .get(&format!("/v1/hashu/epochs/convert/{}", quote.id))
            .await;
        let target = conv["epoch"].as_u64().unwrap();
        let keys = self.get(&format!("/v1/hashu/epochs/{target}/keys")).await;
        let keyset: KeySet = serde_json::from_value(keys["keysets"][0].clone()).unwrap();
        assert_eq!(keyset.id.to_string(), conv["keyset_id"].as_str().unwrap());
        let premint = PreMintSecrets::random(
            keyset.id,
            Amount::from(amount),
            &SplitTarget::default(),
            &(0, denoms()).into(),
        )
        .unwrap();
        let r = self
            .http
            .post(format!("{}/v1/hashu/epochs/convert/{}", self.url, quote.id))
            .json(&serde_json::json!({ "outputs": premint.blinded_messages() }))
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success(), "sign: {}", r.text().await.unwrap());
        let body: serde_json::Value = r.json().await.unwrap();
        let sigs: Vec<BlindSignature> = serde_json::from_value(body["signatures"].clone()).unwrap();
        let proofs =
            cdk::dhke::construct_proofs(sigs, premint.rs(), premint.secrets(), &keyset.keys)
                .unwrap();
        (target, proofs)
    }
}

async fn buy(wallet: &Wallet, tih: u64) -> cdk::nuts::Proofs {
    let quote = wallet
        .mint_quote(
            PaymentMethod::Known(KnownMethod::Bolt11),
            Some(Amount::from(tih)),
            None,
            None,
        )
        .await
        .unwrap();
    for _ in 0..50 {
        if let Ok(p) = wallet.mint(&quote.id, SplitTarget::default(), None).await {
            return p;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("mint quote never paid")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn vouchers_follow_the_epochs() {
    let h = harness().await;

    // Sold before epochs were on: perpetual.
    let early = h.wallet().await;
    let perpetual = buy(&early, 1024).await;
    let perpetual_keyset = perpetual[0].keyset_id;

    // Epochs on: live issuance is dated to the current epoch.
    h.sync().await;
    let k10 = h.active();
    assert_ne!(k10, perpetual_keyset);
    let index = h.get("/v1/hashu/epochs").await;
    assert_eq!(index["epoch"], 10);
    assert_eq!(index["epochs"][0]["keyset_id"], k10.to_string());
    assert_eq!(
        index["keysets"][perpetual_keyset.to_string()]["perpetual"],
        true
    );
    assert_eq!(index["keysets"][k10.to_string()]["epoch"], 10);

    let wallet = h.wallet().await;
    let dated = buy(&wallet, 2048).await;
    assert!(dated.iter().all(|p| p.keyset_id == k10));

    // Epoch 11: a new keyset; epoch 10's vouchers have expired but stay
    // valid, and convert to the current epoch, which has room.
    h.epochs.set_clock(tip_in(11));
    h.sync().await;
    let k11 = h.active();
    assert_ne!(k11, k10);
    h.mint.verify_proofs(dated.clone()).await.unwrap();
    wallet.refresh_keysets().await.unwrap();
    let (epoch, converted) = h.convert(&wallet, "auto".into(), 2048).await;
    assert_eq!(epoch, 11);
    assert!(converted.iter().all(|p| p.keyset_id == k11));
    h.mint.verify_proofs(converted.clone()).await.unwrap();
    assert_eq!(u64::from(wallet.total_balance().await.unwrap()), 0);

    // Perpetual vouchers are still good.
    h.mint.verify_proofs(perpetual).await.unwrap();

    // Selling a later epoch separately: date 1,024 TiH to epoch 13.
    let wallet2 = h.wallet().await;
    wallet2
        .receive_proofs(converted, Default::default(), None, None)
        .await
        .unwrap();
    let (epoch, forward) = h.convert(&wallet2, 13.into(), 1024).await;
    assert_eq!(epoch, 13);
    assert_eq!(u64::from(wallet2.total_balance().await.unwrap()), 1024);
    // Not spendable until epoch 13 starts...
    assert!(h.mint.verify_proofs(forward.clone()).await.is_err());
    let index = h.get("/v1/hashu/epochs").await;
    let e13 = &index["epochs"][2];
    assert_eq!(e13["epoch"], 13);
    assert_eq!(e13["owed_tih"], 1024);
    // ...and then they are.
    h.epochs.set_clock(tip_in(13));
    h.sync().await;
    h.mint.verify_proofs(forward).await.unwrap();
}
