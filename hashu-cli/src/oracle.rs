//! `hashu oracle` subcommands — live hashprice queries.

use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use hashu_core::hashprice::HashpriceSample;
use hashu_mint::oracle::{esplora, luxor};

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
}

pub async fn run(cmd: OracleCmd) -> Result<()> {
    match cmd.action {
        OracleAction::Ping(args) => ping(args).await,
    }
}

async fn ping(args: PingArgs) -> Result<()> {
    let started = std::time::Instant::now();
    let sample = match args.source {
        Source::Esplora => fetch_esplora(&args).await?,
        Source::Luxor => fetch_luxor(&args).await?,
    };
    let elapsed = started.elapsed();
    print_sample(args.source, &sample, elapsed);
    Ok(())
}

async fn fetch_esplora(args: &PingArgs) -> Result<HashpriceSample> {
    let client = match &args.base_url {
        Some(url) => esplora::EsploraClient::with_base(url.clone()),
        None => esplora::EsploraClient::new(),
    };
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
    esplora::compute_hashprice(&window).context("compute hashprice from empty window")
}

async fn fetch_luxor(args: &PingArgs) -> Result<HashpriceSample> {
    let key = std::env::var(luxor::ENV_LUXOR_API_KEY)
        .with_context(|| format!("missing {} env var", luxor::ENV_LUXOR_API_KEY))?;
    let client = match &args.base_url {
        Some(url) => luxor::LuxorClient::with_base(key, url.clone()),
        None => luxor::LuxorClient::new(key),
    };
    client.fetch_latest().await.context("fetch luxor hashprice")
}

fn print_sample(source: Source, sample: &HashpriceSample, elapsed: Duration) {
    let unix = sample
        .t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1);
    println!("source:           {source:?}");
    println!("sample timestamp: {unix} (unix)");
    println!("sats/Th-sec:      {:.6e}", sample.sats_per_ths);
    println!("sats/Th/day:      {:.4}", sample.sats_per_th_per_day());
    println!("sats/PH/day:      {:.2}", sample.sats_per_ph_per_day());
    println!("BTC/PH/day:       {:.8}", sample.btc_per_ph_per_day());
    println!("elapsed:          {:.2}s", elapsed.as_secs_f64());
}
