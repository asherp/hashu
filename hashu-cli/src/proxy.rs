//! `hashu proxy` subcommands — run the Stratum V1 listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use hashu_proxy::stratum::{run_listener, strip_stratum_prefix, ListenerConfig};
use hashu_proxy::ProxyMetrics;

#[derive(Debug, Args)]
pub struct ProxyCmd {
    #[command(subcommand)]
    pub action: ProxyAction,
}

#[derive(Debug, Subcommand)]
pub enum ProxyAction {
    /// Run the Stratum V1 listener and forward every accepted miner to the
    /// configured upstream pool.
    Run(RunArgs),
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Address to bind for inbound miners.
    #[arg(long, default_value = "0.0.0.0:3333")]
    pub bind: SocketAddr,
    /// Upstream pool. Accepts `host:port` or a `stratum+tcp://host:port` URL.
    #[arg(long)]
    pub upstream: String,
    /// Periodic metrics log interval (seconds). 0 disables.
    #[arg(long, default_value_t = 30)]
    pub metrics_interval: u64,
}

pub async fn run(cmd: ProxyCmd) -> Result<()> {
    match cmd.action {
        ProxyAction::Run(args) => run_listener_cmd(args).await,
    }
}

async fn run_listener_cmd(args: RunArgs) -> Result<()> {
    let upstream = strip_stratum_prefix(&args.upstream).to_string();
    if !upstream.contains(':') {
        anyhow::bail!(
            "upstream must include a port (got {upstream}); e.g. stratum+tcp://pool.example.com:3333"
        );
    }
    let cfg = ListenerConfig {
        bind: args.bind,
        upstream,
    };
    let metrics = Arc::new(ProxyMetrics::default());

    if args.metrics_interval > 0 {
        let m = metrics.clone();
        let interval = Duration::from_secs(args.metrics_interval);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await; // skip initial immediate tick
            loop {
                tick.tick().await;
                let s = m.snapshot();
                tracing::info!(
                    connections = s.connections_accepted,
                    submitted = s.shares_submitted,
                    accepted = s.shares_accepted,
                    rejected = s.shares_rejected,
                    "proxy metrics",
                );
            }
        });
    }

    run_listener(cfg, metrics).await.context("listener loop")
}
