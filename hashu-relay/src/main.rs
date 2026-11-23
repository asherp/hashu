//! `hashu-relay`: holds miners' Stratum connections in front of the Hashu
//! daemon, so they stay connected while it restarts. See the crate docs.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Stratum relay that keeps miners connected while the Hashu daemon restarts"
)]
struct Args {
    /// Where miners connect.
    #[arg(long, env = "HASHU_RELAY_LISTEN", default_value = "0.0.0.0:3333")]
    listen: SocketAddr,
    /// The Hashu daemon's Stratum port (host:port), resolved on every dial.
    #[arg(long, env = "HASHU_RELAY_UPSTREAM", default_value = "hashu:3333")]
    upstream: String,
    /// Seconds to hold a miner while the daemon is unreachable before
    /// closing its connection.
    #[arg(long, env = "HASHU_RELAY_HOLD_SECS", default_value_t = 300)]
    hold_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();
    let mut cfg = hashu_relay::Config::new(args.upstream);
    cfg.hold = Duration::from_secs(args.hold_secs);
    tokio::select! {
        r = hashu_relay::run(args.listen, cfg) => r,
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            Ok(())
        }
    }
}

/// Ctrl-C, or SIGTERM (`docker stop`; as PID 1 an unhandled SIGTERM is
/// ignored and Docker kills the relay after its grace period).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
