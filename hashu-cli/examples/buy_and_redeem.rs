//! Demo client for `hashu mint run`: buy THH, then redeem it as hashrate.
//!
//! ```text
//! cargo run -p hashu --example buy_and_redeem -- \
//!     http://127.0.0.1:8085 bolt11 2 stratum+tcp://your-pool.example.com:3333 you.worker1
//! ```
//!
//! Arguments: mint URL, payment method (`bolt11` or `usdt`), THH to buy and
//! redeem, redemption pool URL, worker name. It prints the payment request,
//! waits until the mint sees it paid, mints the ecash, then melts it with
//! the `hashrate` method. The mint opens a redemption and the proxy routes
//! miners to the pool until it is filled.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cdk::amount::SplitTarget;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::{CurrencyUnit, MintQuoteState, PaymentMethod};
use cdk::wallet::{MeltOutcome, Wallet};
use cdk::Amount;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [mint_url, method, amount, pool_url, worker] = args.as_slice() else {
        bail!("usage: buy_and_redeem <mint_url> <bolt11|usdt> <thh> <pool_url> <worker>");
    };
    let amount: u64 = amount.parse().context("thh")?;
    let method = match method.as_str() {
        "bolt11" => PaymentMethod::Known(KnownMethod::Bolt11),
        other => PaymentMethod::Custom(other.to_string()),
    };

    let db = Arc::new(cdk_sqlite::wallet::memory::empty().await?);
    let seed: [u8; 64] = std::array::from_fn(|_| rand::random());
    let wallet = Wallet::new(mint_url, CurrencyUnit::Custom("thh".into()), db, seed, None)?;

    // 1. Buy.
    let quote = wallet
        .mint_quote(method, Some(Amount::from(amount)), None, None)
        .await?;
    println!("pay this to buy {amount} THH:\n  {}\n", quote.request);
    loop {
        let q = wallet.check_mint_quote_status(&quote.id).await?;
        if matches!(q.state, MintQuoteState::Paid | MintQuoteState::Issued) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let proofs = wallet.mint(&quote.id, SplitTarget::default(), None).await?;
    let got: u64 = proofs.iter().map(|p| u64::from(p.amount)).sum();
    println!("minted {got} THH of ecash");

    // 2. Redeem.
    let request = serde_json::json!({
        "pool_url": pool_url,
        "worker_name": worker,
        "amount_thh": amount,
        // Lets the same wallet redeem the same amount to the same worker again.
        "nonce": hex::encode(rand::random::<[u8; 8]>()),
    });
    let melt = wallet
        .melt_quote(
            PaymentMethod::Custom("hashrate".into()),
            request,
            None,
            None,
        )
        .await?;
    println!("redeem quote {} for {} THH", melt.id, melt.amount);
    let prepared = wallet.prepare_melt(&melt.id, HashMap::new()).await?;
    match prepared.confirm_prefer_async().await? {
        MeltOutcome::Paid(done) => println!("redemption filled: {done:?}"),
        MeltOutcome::Pending(_) => println!(
            "redemption open: hashrate now goes to {pool_url} as {worker} until {amount} THH is filled"
        ),
    }
    Ok(())
}
