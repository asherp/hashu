//! `hashu proxy` subcommands — run the Stratum V1 listener.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use hashu_core::redemption::RedemptionLedger;
use hashu_proxy::control::{serve as serve_control, ControlState};
use hashu_proxy::credit::{CreditContext, ShareCrediter};
use hashu_proxy::router::{Router, SwitchMode};
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
    /// How a redemption moves miners between pools: `seamless` swaps the
    /// upstream behind the open miner connection (no reconnect; falls back
    /// to a reconnect when the miner can't be switched in place),
    /// `reconnect` closes the miner's connection so it reconnects.
    #[arg(long, env = "HASHU_SWITCH_MODE", default_value = "seamless")]
    pub switch_mode: SwitchMode,
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
    #[command(flatten)]
    pub home: HomePoolArgs,
}

/// Home pools for small miners on the one public endpoint (see
/// `hashu_proxy::home`): a miner whose user is a Bitcoin address idles
/// on the solo pool with its own address; `pool=host:port` in the
/// password picks another allowed pool.
#[derive(Debug, Clone, Args)]
pub struct HomePoolArgs {
    /// Solo pool for miners whose user is a mainnet Bitcoin address
    /// (`host:port` or a `stratum+tcp://` / `stratum+ssl://` URL).
    /// `off` turns address detection off.
    #[arg(long, env = "HASHU_HOME_SOLO_UPSTREAM", default_value = hashu_proxy::home::DEFAULT_SOLO_UPSTREAM)]
    pub home_solo_upstream: String,
    /// Pool hosts a miner may name with `pool=` in its password,
    /// comma-separated. Empty turns `pool=` off.
    #[arg(long, env = "HASHU_HOME_POOLS", default_value_t = hashu_proxy::home::DEFAULT_ALLOWED_HOSTS.join(","))]
    pub home_pools: String,
    /// Turn home pools off: every miner idles on the default pool, as
    /// before, and new connections are placed without waiting for their
    /// authorize.
    #[arg(long, env = "HASHU_HOME_POOLS_OFF")]
    pub home_pools_off: bool,
}

impl HomePoolArgs {
    pub fn config(&self) -> Result<hashu_proxy::home::HomeConfig> {
        use hashu_proxy::home::HomeConfig;
        if self.home_pools_off {
            return Ok(HomeConfig::off());
        }
        let solo = self.home_solo_upstream.trim();
        let solo_upstream = if solo.is_empty() || solo.eq_ignore_ascii_case("off") {
            None
        } else {
            Some(normalize_upstream(solo).with_context(|| {
                format!("--home-solo-upstream must be host:port or a stratum URL (got {solo})")
            })?)
        };
        let allowed_hosts = self
            .home_pools
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        Ok(HomeConfig {
            solo_upstream,
            allowed_hosts,
        })
    }
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
    let router = Arc::new(
        Router::new(upstream, ledger.clone())
            .with_switch_mode(args.switch_mode)
            .with_home_pools(args.home.config()?),
    );
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
            router: Some(router.clone()),
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
        // Reconcile tick: how soon a new redemption starts moving miners.
        Duration::from_millis(250),
    )
    .await
    .context("listener loop")
}
