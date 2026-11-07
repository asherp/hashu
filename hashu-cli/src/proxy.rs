//! `hashu proxy` subcommands — run the Stratum V1 listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use hashu_core::redemption::RedemptionLedger;
use hashu_proxy::control::{serve as serve_control, ControlState};
use hashu_proxy::credit::{CreditContext, ShareCrediter};
use hashu_proxy::router::Router;
use hashu_proxy::stratum::run_routed_listener;
use hashu_proxy::upstream::normalize_upstream;
use hashu_proxy::ProxyMetrics;

#[derive(Debug, Args)]
pub struct ProxyCmd {
    #[command(subcommand)]
    pub action: ProxyAction,
}

#[derive(Debug, Subcommand)]
pub enum ProxyAction {
    /// Run the Stratum V1 listener. Miners go to the upstream pool, or to a
    /// redeemer's pool while a `hashrate` redemption is active.
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
    /// Serve the HTTP control API (`GET /status`) here, e.g.
    /// `127.0.0.1:3334`. The Hashu MDK app polls it to know when to move
    /// miners onto the proxy. Off unless set.
    #[arg(long)]
    pub control_bind: Option<SocketAddr>,
    /// Also serve `POST /redemptions` on the control API, so a demo can
    /// open a redemption without a mint in this process. Anyone who can
    /// reach the control port can then redirect the hashrate; keep it on
    /// loopback.
    #[arg(long, requires = "control_bind")]
    pub control_allow_open: bool,
}

pub async fn run(cmd: ProxyCmd) -> Result<()> {
    match cmd.action {
        ProxyAction::Run(args) => run_listener_cmd(args).await,
    }
}

async fn run_listener_cmd(args: RunArgs) -> Result<()> {
    let Some(upstream) = normalize_upstream(&args.upstream) else {
        anyhow::bail!(
            "upstream must include a port (got {}); e.g. stratum+tcp://pool.example.com:3333 or stratum+ssl://pool.example.com:1300",
            args.upstream
        );
    };
    // The ledger is empty until a mint in this process opens redemptions
    // in it; until then every miner goes to the default upstream.
    let ledger = Arc::new(RedemptionLedger::new());
    let router = Arc::new(Router::new(upstream, ledger.clone()));
    // The router is also the binding: shares credit the redemption the
    // connection was routed to.
    let crediter = Arc::new(ShareCrediter::new(ledger));
    let audit = crediter.audit().clone();
    let credit = CreditContext::new(crediter, router.clone());
    let metrics = Arc::new(ProxyMetrics::default());

    if let Some(bind) = args.control_bind {
        let state = ControlState {
            ledger: router.ledger().clone(),
            metrics: metrics.clone(),
            default_upstream: router.default_upstream().to_string(),
            audit,
            allow_open: args.control_allow_open,
        };
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind control API on {bind}"))?;
        tracing::info!(%bind, allow_open = args.control_allow_open, "control API listening");
        tokio::spawn(async move {
            if let Err(e) = serve_control(listener, state).await {
                tracing::error!(error = %e, "control API stopped");
            }
        });
    }

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
                    committed = s.shares_committed,
                    credited = s.shares_credited,
                    "proxy metrics",
                );
            }
        });
    }

    run_routed_listener(
        args.bind,
        router,
        metrics,
        Some(credit),
        Duration::from_secs(1),
    )
    .await
    .context("listener loop")
}
