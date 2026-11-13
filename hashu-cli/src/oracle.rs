//! `hashu oracle` subcommands — live hashprice queries.

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use hashu_core::hashprice::HashpriceSample;
use hashu_mint::oracle::{esplora, luxor, mrr};

use crate::network::{default_esplora_url, parse_network};

#[derive(Debug, Args)]
pub struct OracleCmd {
    #[command(subcommand)]
    pub action: OracleAction,
}

#[derive(Debug, Subcommand)]
pub enum OracleAction {
    /// Fetch one current hashprice sample and print it.
    Ping(PingArgs),
}

#[derive(Debug, Args)]
pub struct PingArgs {
    /// Hashprice source.
    #[arg(long, value_enum, default_value_t = Source::Esplora)]
    pub source: Source,

    /// Override the API base URL (e.g. self-hosted Esplora).
    #[arg(long)]
    pub base_url: Option<String>,

    /// Bitcoin network whose public Esplora to read: `bitcoin` (default),
    /// `testnet4`, `testnet`, `signet` (`regtest` needs --base-url with
    /// esplora). For esplora it is the hashprice source; for mrr it supplies
    /// the real difficulty used to price one expected block.
    #[arg(long, default_value = "bitcoin")]
    pub network: String,

    /// MRR algorithm category (mrr only): `sha256ab` (default) or `sha256`.
    #[arg(long, default_value = mrr::DEFAULT_ALGO)]
    pub algo: String,

    /// Window of recent blocks to average over (Esplora only).
    #[arg(long, default_value_t = esplora::DEFAULT_WINDOW_BLOCKS)]
    pub window: usize,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Source {
    /// Self-derived from public Bitcoin chain data via Blockstream Esplora.
    Esplora,
    /// Luxor Hashrate Index (requires LUXOR_API_KEY env var).
    Luxor,
    /// MiningRigRentals SHA-256 rental floor (no key needed). Prices what
    /// hashrate costs to rent, whichever chain it mines; use with
    /// `--network testnet4` for the Testnet4 demo.
    Mrr,
}

pub async fn run(cmd: OracleCmd) -> Result<()> {
    match cmd.action {
        OracleAction::Ping(args) => ping(args).await,
    }
}

async fn ping(args: PingArgs) -> Result<()> {
    let started = std::time::Instant::now();
    let (sample, base_url) = match args.source {
        Source::Esplora => fetch_esplora(&args).await?,
        Source::Luxor => fetch_luxor(&args).await?,
        Source::Mrr => fetch_mrr(&args).await?,
    };
    let elapsed = started.elapsed();
    print_sample(args.source, &base_url, &sample, elapsed);
    Ok(())
}

async fn fetch_esplora(args: &PingArgs) -> Result<(HashpriceSample, String)> {
    let base_url = match &args.base_url {
        Some(url) => url.clone(),
        None => {
            let network = parse_network(&args.network)?;
            default_esplora_url(network)
                .with_context(|| format!("no public Esplora for {network}; pass --base-url"))?
                .to_string()
        }
    };
    let client = esplora::EsploraClient::with_base(base_url.clone());
    let tip = client.tip_hash().await.context("fetch tip hash")?;
    eprintln!("tip: {tip}");
    let mut window = Vec::with_capacity(args.window);
    let mut hash = tip;
    for i in 0..args.window {
        let block = client
            .block(&hash)
            .await
            .with_context(|| format!("fetch block {i} ({hash})"))?;
        let next = block.previousblockhash.clone();
        eprintln!(
            "  block {} (h={}, diff={:.2e}, coinbase={} sats)",
            i, block.height, block.difficulty, block.coinbase_total_sats
        );
        window.push(block);
        match next {
            Some(p) => hash = p,
            None => break,
        }
    }
    let sample =
        esplora::compute_hashprice(&window).context("compute hashprice from empty window")?;
    if let Some(diff) = esplora::real_difficulty(&window) {
        let thh = esplora::thh_per_expected_block(diff);
        let tih = esplora::tih_per_expected_block(diff);
        println!("real difficulty:  {diff:.3e}");
        println!(
            "work per block:   {tih:.0} TiH ({:.2} PH-hours)",
            thh / 1000.0
        );
    }
    Ok((sample, base_url))
}

async fn fetch_luxor(args: &PingArgs) -> Result<(HashpriceSample, String)> {
    let key = std::env::var(luxor::ENV_LUXOR_API_KEY)
        .with_context(|| format!("missing {} env var", luxor::ENV_LUXOR_API_KEY))?;
    let base_url = args
        .base_url
        .clone()
        .unwrap_or_else(|| luxor::DEFAULT_BASE_URL.to_string());
    let client = luxor::LuxorClient::with_base(key, base_url.clone());
    let sample = client
        .fetch_latest()
        .await
        .context("fetch luxor hashprice")?;
    Ok((sample, base_url))
}

async fn fetch_mrr(args: &PingArgs) -> Result<(HashpriceSample, String)> {
    let base_url = args
        .base_url
        .clone()
        .unwrap_or_else(|| mrr::DEFAULT_BASE_URL.to_string());
    let client = mrr::MrrClient::with_base(base_url.clone(), args.algo.clone());
    let market = client.fetch_floor().await.context("fetch mrr rig floor")?;
    let sample = market.sample(std::time::SystemTime::now());
    let f = &market.floor;
    println!(
        "floor rig:        {} ({:.3} PH/s, {}h min, {:.8} BTC for the minimum)",
        f.id,
        f.ph,
        f.min_hours,
        f.min_rental_btc()
    );
    println!(
        "rigs:             {} eligible of {} listed ({})",
        market.rigs_seen, market.total_listed, args.algo
    );

    // Best effort: cost of one expected block on --network at the floor.
    let network = parse_network(&args.network)?;
    if let Some(url) = default_esplora_url(network) {
        let esplora = esplora::EsploraClient::with_base(url.to_string());
        match esplora::fetch_real_difficulty(&esplora, 500).await {
            Ok(Some((diff, height))) => {
                let thh = esplora::thh_per_expected_block(diff);
                let tih = esplora::tih_per_expected_block(diff);
                let btc = f.btc_per_ph_day * (thh / 1000.0) / 24.0;
                println!("{network} real difficulty: {diff:.3e} (height {height})");
                println!(
                    "work per block:   {tih:.0} TiH ({:.2} PH-hours)",
                    thh / 1000.0
                );
                println!("BTC per block:    {btc:.8} at the floor");
            }
            Ok(None) => eprintln!("no real-difficulty block found on {network}"),
            Err(e) => eprintln!("could not read {network} difficulty: {e}"),
        }
    }
    Ok((sample, base_url))
}

fn print_sample(source: Source, base_url: &str, sample: &HashpriceSample, elapsed: Duration) {
    let unix = sample
        .t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1);
    println!("source:           {source:?} ({base_url})");
    println!("sample timestamp: {unix} (unix)");
    println!("sats/Th-sec:      {:.6e}", sample.sats_per_ths);
    println!("sats/Th/day:      {:.4}", sample.sats_per_th_per_day());
    println!("sats/PH/day:      {:.2}", sample.sats_per_ph_per_day());
    println!("BTC/PH/day:       {:.8}", sample.btc_per_ph_per_day());
    println!("elapsed:          {:.2}s", elapsed.as_secs_f64());
}
