use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "hashu", version, about = "Hashu operator CLI")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// First-run setup: generate nostr keys + write manifest state.
    Init(hashu_cli::init::InitCmd),
    /// Inspect or update the operator state file.
    #[command(subcommand)]
    Config(hashu_cli::config::ConfigAction),
    /// Operator manifest commands (kind 0 publish / dry-run).
    #[command(subcommand)]
    Manifest(hashu_cli::manifest::ManifestAction),
    /// Hashprice oracle queries.
    Oracle(hashu_cli::oracle::OracleCmd),
    /// Stratum proxy commands.
    #[command(subcommand)]
    Proxy(hashu_cli::proxy::ProxyAction),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Init(c) => hashu_cli::init::run(c).await,
        Cmd::Config(action) => {
            hashu_cli::config::run(hashu_cli::config::ConfigCmd { action }).await
        }
        Cmd::Manifest(action) => {
            hashu_cli::manifest::run(hashu_cli::manifest::ManifestCmd { action }).await
        }
        Cmd::Oracle(c) => hashu_cli::oracle::run(c).await,
        Cmd::Proxy(action) => {
            hashu_cli::proxy::run(hashu_cli::proxy::ProxyCmd { action }).await
        }
    }
}
