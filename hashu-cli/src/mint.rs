//! `hashu mint run` — the Hashu mint daemon. ARCHITECTURE.md §1, §4.
//!
//! One process runs everything a demo needs:
//!
//! - a **cdk mint** for the `THH` unit, served over HTTP (NUT-00..NUT-07 plus
//!   the payment-method routes);
//! - **payment rails** that sell `THH`: Lightning over phoenixd (mainnet
//!   sats, receive-only) and/or stablecoins (USDT on Ethereum mainnet and
//!   USDC on Base by default, each chain configurable);
//! - the **`hashrate` melt method**, which opens redemptions in a
//!   [`RedemptionLedger`] journaled to `<data-dir>/redemptions.jsonl`, so
//!   open redemptions (and their share trees, `share-audit.jsonl`) resume
//!   after a restart;
//! - the **Stratum V1 proxy**, sharing that ledger: while a redemption is
//!   open, miners are routed to the redeemer's pool and accepted shares
//!   credit it, so the melt completes;
//! - the proxy's **control API** (`GET /status`), which the Hashu MDK app
//!   polls to move miners onto the proxy while a redemption is open.
//! - the **operator GUI** on the same port (`http://127.0.0.1:3334/`):
//!   live hashrate, delivery against the current redemption, obligations,
//!   and issuance/pricing controls. See [`crate::operator`].
//!
//! The mint never pays out: bolt11 is offered for minting only, and the
//! only melt method is `hashrate`.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use cdk::cdk_payment::DynMintPayment;
use cdk::mint::{MintBuilder, MintMeltLimits, UnitConfig};
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use clap::{Args, Subcommand, ValueEnum};
use hashu_core::hashprice::{HashpriceOracle, HashpriceSample};
use hashu_core::redemption::RedemptionLedger;
use hashu_mint::capacity::{CapacityBook, Rental};
use hashu_mint::lightning::add_lightning_issuance;
use hashu_mint::melt::{reopen_pending_quotes, HashrateMelter, METHOD as HASHRATE};
use hashu_mint::oracle::{esplora, mrr};
use hashu_mint::payment::{thh_unit, ThhIssuer};
use hashu_mint::phoenixd::{Phoenixd, PhoenixdConfig};
use hashu_mint::pricing::{SatsPerThhFromHashprice, ThhPricer, SECS_PER_THH};
use hashu_mint::tether::{
    EvmRpcSource, FixedUsdtPrice, SatsToUsdt, Stablecoin, TetherPayments, UsdtRail, BASE_CHAIN_ID,
    BASE_USDC, MAINNET_CHAIN_ID, MAINNET_USDT, MICRO_PER_USDT,
};
use hashu_proxy::audit::ShareAudit;
use hashu_proxy::control::ControlState;
use hashu_proxy::credit::{CreditContext, ShareCrediter};
use hashu_proxy::meter::HashrateMeter;
use hashu_proxy::router::{Router, SwitchMode};
use hashu_proxy::stratum::run_routed_listener;
use hashu_proxy::upstream::normalize_upstream;
use hashu_proxy::ProxyMetrics;

use crate::checkout::{self, CheckoutState};
use crate::landing;
use crate::network::{default_esplora_url, parse_network};
use crate::operator::{self, ControlledPricer, DaemonInfo, MintControls, OperatorState};
use crate::orders::OrderBook;

/// Public JSON-RPC endpoints the stablecoin rails watch by default: USDT
/// on Ethereum mainnet, USDC on Base. Fine to start; a mint taking real
/// money should use its own node or a paid provider (`--usdt-rpc`,
/// `--usdc-rpc`).
pub const MAINNET_PUBLIC_RPC: &str = "https://ethereum-rpc.publicnode.com";
pub const BASE_PUBLIC_RPC: &str = "https://base-rpc.publicnode.com";

/// For rehearsals on Sepolia: test USDT, chain id and public RPC.
pub const SEPOLIA_TEST_USDT: &str = "0x7169D38820dfd117C3FA1f22a697dBA58d90BA06";
pub const SEPOLIA_CHAIN_ID: u64 = 11_155_111;
pub const SEPOLIA_PUBLIC_RPC: &str = "https://ethereum-sepolia-rpc.publicnode.com";

/// `THH` denominations the keyset signs: powers of two up to 2^20.
pub(crate) const DENOMS: [u64; 21] = {
    let mut d = [0u64; 21];
    let mut i = 0;
    while i < 21 {
        d[i] = 1 << i;
        i += 1;
    }
    d
};

#[derive(Debug, Args)]
pub struct MintCmd {
    #[command(subcommand)]
    pub action: MintAction,
}

#[derive(Debug, Subcommand)]
pub enum MintAction {
    /// Run the mint daemon: cdk mint + payment rails + Stratum proxy +
    /// control API, in one process.
    Run(Box<RunArgs>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum PriceSource {
    /// MiningRigRentals SHA-256 rental floor (live, no key).
    Mrr,
    /// Hashprice derived from recent blocks on `--network` via Esplora.
    Esplora,
    /// A fixed `--sats-per-thh`.
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum LightningBackend {
    /// phoenixd over HTTP: PHOENIXD_URL (or PHOENIXD_HOSTPORT) and
    /// PHOENIXD_PASSWORD. Mainnet by default (PHOENIXD_CHAIN).
    Phoenixd,
    /// cdk's fake wallet: every invoice pays itself. Rehearsals only.
    Fake,
    /// No Lightning rail.
    Off,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    // ---- mint -------------------------------------------------------
    /// Address the mint's HTTP API binds to.
    #[arg(long, env = "HASHU_MINT_LISTEN", default_value = "0.0.0.0:8085")]
    pub listen: SocketAddr,
    /// Public URL of the mint, advertised in its info (NUT-06) and written
    /// into every token it sells. Unset: the local `--listen` address,
    /// which only works for a rehearsal on this machine.
    #[arg(long, env = "HASHU_MINT_URL")]
    pub mint_url: Option<String>,
    /// Directory for the mint database and seed (created if missing).
    #[arg(long, env = "HASHU_DATA_DIR", default_value = "hashu-data")]
    pub data_dir: PathBuf,
    /// Mint name (NUT-06).
    #[arg(long, env = "HASHU_MINT_NAME", default_value = "Hashu")]
    pub name: String,
    /// Mint description (NUT-06).
    #[arg(
        long,
        env = "HASHU_MINT_DESCRIPTION",
        default_value = "Hashrate vouchers in terahash-hours (THH). Pay with Lightning or USDT; redeem by pointing hashrate at your pool."
    )]
    pub description: String,
    /// How to reach the operator (NUT-06 `contact`), comma separated
    /// `method:info` pairs, e.g. `email:ops@example.com,nostr:npub1...`.
    /// Shown on the landing page. Empty: no contact.
    #[arg(long, env = "HASHU_MINT_CONTACT", value_delimiter = ',')]
    pub contact: Vec<String>,
    /// Largest mint quote or redemption, in THH.
    #[arg(long, default_value_t = 1_000_000)]
    pub max_thh: u64,
    /// Smallest redemption, in THH.
    #[arg(long, default_value_t = hashu_mint::melt::DEFAULT_MIN_THH)]
    pub min_redeem_thh: u64,

    // ---- pricing ----------------------------------------------------
    /// Where the sats price of a THH comes from.
    #[arg(long, value_enum, default_value_t = PriceSource::Mrr)]
    pub price: PriceSource,
    /// Fixed price in sats per THH. Required with `--price fixed`; with a
    /// live source it is used until the first sample arrives and whenever
    /// the source goes stale.
    #[arg(long, env = "HASHU_SATS_PER_THH")]
    pub sats_per_thh: Option<f64>,
    /// Operator markup in basis points, applied to every rail's price.
    /// The operator GUI can change it while the mint runs.
    #[arg(long)]
    pub markup_bps: Option<u32>,
    /// Miner efficiency, J/TH, for the operator GUI's heat estimate when a
    /// rental's rig name does not name a known model. The GUI can change it.
    #[arg(long, env = "HASHU_HEAT_J_PER_TH", default_value_t = hashu_mint::heat::DEFAULT_J_PER_TH)]
    pub heat_j_per_th: f64,
    /// Network for the esplora price source (`testnet4`, `bitcoin`, …).
    #[arg(long, default_value = "testnet4")]
    pub network: String,
    /// Seconds between live price refreshes.
    #[arg(long, default_value_t = 300)]
    pub price_refresh_secs: u64,

    // ---- Lightning --------------------------------------------------
    /// Lightning backend. Default: phoenixd when PHOENIXD_URL or
    /// PHOENIXD_HOSTPORT is set, otherwise off.
    #[arg(long, value_enum)]
    pub lightning: Option<LightningBackend>,

    // ---- Stablecoins (USDT, USDC) -------------------------------------
    /// EVM address that receives USDT. Turns the USDT rail on.
    #[arg(long, env = "HASHU_USDT_DEPOSIT")]
    pub usdt_deposit: Option<String>,
    /// USDT token contract (Ethereum mainnet USDT by default).
    #[arg(long, env = "HASHU_USDT_TOKEN", default_value = MAINNET_USDT)]
    pub usdt_token: String,
    /// EVM JSON-RPC endpoint the USDT rail watches.
    #[arg(long, env = "HASHU_USDT_RPC", default_value = MAINNET_PUBLIC_RPC)]
    pub usdt_rpc: String,
    /// Chain id of `--usdt-rpc` (1 = Ethereum mainnet).
    #[arg(long, env = "HASHU_USDT_CHAIN_ID", default_value_t = MAINNET_CHAIN_ID)]
    pub usdt_chain_id: u64,
    /// Blocks a USDT transfer must be buried under before it pays a quote.
    /// 12 is about 2.5 minutes on Ethereum (finality is ~64).
    #[arg(long, env = "HASHU_USDT_CONFIRMATIONS", default_value_t = 12)]
    pub usdt_confirmations: u64,
    /// EVM address that receives USDC. Turns the USDC rail on. May be the
    /// same address as `--usdt-deposit`.
    #[arg(long, env = "HASHU_USDC_DEPOSIT")]
    pub usdc_deposit: Option<String>,
    /// USDC token contract (Circle's native USDC on Base by default).
    #[arg(long, env = "HASHU_USDC_TOKEN", default_value = BASE_USDC)]
    pub usdc_token: String,
    /// EVM JSON-RPC endpoint the USDC rail watches.
    #[arg(long, env = "HASHU_USDC_RPC", default_value = BASE_PUBLIC_RPC)]
    pub usdc_rpc: String,
    /// Chain id of `--usdc-rpc` (8453 = Base mainnet).
    #[arg(long, env = "HASHU_USDC_CHAIN_ID", default_value_t = BASE_CHAIN_ID)]
    pub usdc_chain_id: u64,
    /// Blocks a USDC transfer must be buried under before it pays a quote.
    /// 30 is about a minute on Base (2 s blocks).
    #[arg(long, env = "HASHU_USDC_CONFIRMATIONS", default_value_t = 30)]
    pub usdc_confirmations: u64,
    /// Fixed dollar price per THH (e.g. 0.05), used for both USDT and
    /// USDC. Takes precedence over `--usd-per-btc`.
    #[arg(long, env = "HASHU_USDT_PER_THH")]
    pub usdt_per_thh: Option<f64>,
    /// BTC/USD rate, to price USDT quotes from the sats price so both rails
    /// sell THH at the same price.
    #[arg(long, env = "HASHU_USD_PER_BTC")]
    pub usd_per_btc: Option<f64>,

    // ---- proxy ------------------------------------------------------
    /// Address the Stratum proxy binds for miners.
    #[arg(long, default_value = "0.0.0.0:3333")]
    pub stratum_bind: SocketAddr,
    /// How a redemption moves miners between pools: `seamless` swaps the
    /// upstream behind the open miner connection (no reconnect; falls back
    /// to a reconnect when the miner can't be switched in place),
    /// `reconnect` closes the miner's connection so it reconnects.
    #[arg(long, env = "HASHU_SWITCH_MODE", default_value = "seamless")]
    pub switch_mode: SwitchMode,
    /// Default pool: where miners point when no redemption is open.
    /// Required: there is no built-in pool.
    #[arg(long, env = "HASHU_UPSTREAM")]
    pub upstream: Option<String>,
    /// Control API (`GET /status`) the MDK app polls, and the operator GUI
    /// at `/`. Pass an empty value to turn it off.
    #[arg(long, default_value = "127.0.0.1:3334")]
    pub control_bind: Option<String>,
    /// Also serve `POST /redemptions` on the control API, bypassing the
    /// mint. Anyone who can reach the control port can then redirect the
    /// hashrate; keep it on loopback.
    #[arg(long)]
    pub control_allow_open: bool,
    /// Periodic proxy metrics log interval (seconds). 0 disables.
    #[arg(long, default_value_t = 30)]
    pub metrics_interval: u64,
    /// Window the operator GUI's hashrate averages accepted shares over
    /// (seconds). Shares are sparse, so short windows are noisy.
    #[arg(long, default_value_t = 60)]
    pub hashrate_window: u64,

    // ---- capacity ---------------------------------------------------
    /// MiningRigRentals API key. With `--mrr-api-secret`, the mint reads the
    /// account's active rentals (hashrate and end time) as the capacity it
    /// can sell. Without it, set rentals in the operator GUI or
    /// `POST /api/capacity`.
    #[arg(long, env = "MRR_API_KEY", hide_env_values = true)]
    pub mrr_api_key: Option<String>,
    #[arg(long, env = "MRR_API_SECRET", hide_env_values = true)]
    pub mrr_api_secret: Option<String>,
    /// Only count these MRR rental ids (comma separated), e.g. when other
    /// rentals on the account mine somewhere other than this proxy.
    #[arg(long, env = "HASHU_MRR_RENTAL_IDS", value_delimiter = ',')]
    pub mrr_rental_ids: Vec<String>,
    /// MRR API base URL (for a test double).
    #[arg(long, env = "MRR_API_BASE", default_value = mrr::DEFAULT_BASE_URL)]
    pub mrr_api_base: String,
    /// Seconds between MRR rental syncs.
    #[arg(long, default_value_t = 60)]
    pub mrr_refresh_secs: u64,
}

pub async fn run(cmd: MintCmd) -> Result<()> {
    match cmd.action {
        MintAction::Run(args) => run_daemon(*args).await,
    }
}

/// A hashprice that never changes and never goes stale.
#[derive(Debug, Clone, Copy)]
struct FixedHashprice {
    sats_per_ths: f64,
}

impl HashpriceOracle for FixedHashprice {
    fn sample_at(&self, _t: SystemTime) -> Option<f64> {
        Some(self.sats_per_ths)
    }
    fn latest(&self) -> Option<HashpriceSample> {
        Some(HashpriceSample::from_sats_per_ths(
            SystemTime::now(),
            self.sats_per_ths,
        ))
    }
}

/// A live oracle, falling back to a fixed price while it has no fresh
/// sample.
struct WithFallback<O> {
    live: O,
    fallback: Option<FixedHashprice>,
}

impl<O: HashpriceOracle> HashpriceOracle for WithFallback<O> {
    fn sample_at(&self, t: SystemTime) -> Option<f64> {
        self.live
            .sample_at(t)
            .or_else(|| self.fallback.and_then(|f| f.sample_at(t)))
    }
    fn latest(&self) -> Option<HashpriceSample> {
        self.live
            .latest()
            .or_else(|| self.fallback.and_then(|f| f.latest()))
    }
}

fn fixed_from_sats_per_thh(sats_per_thh: f64) -> Result<FixedHashprice> {
    if !sats_per_thh.is_finite() || sats_per_thh <= 0.0 {
        bail!("--sats-per-thh must be a positive number (got {sats_per_thh})");
    }
    Ok(FixedHashprice {
        sats_per_ths: sats_per_thh / SECS_PER_THH,
    })
}

/// Build the hashprice oracle and start its poller, if any.
async fn build_oracle(args: &RunArgs) -> Result<Arc<dyn HashpriceOracle + Send + Sync>> {
    let fallback = args.sats_per_thh.map(fixed_from_sats_per_thh).transpose()?;
    let every = Duration::from_secs(args.price_refresh_secs.max(10));
    // A sample stays usable for three refreshes, so one failed fetch
    // doesn't stop quoting.
    let stale = every * 3;
    match args.price {
        PriceSource::Fixed => {
            let f = fallback.context("--price fixed needs --sats-per-thh")?;
            Ok(Arc::new(f))
        }
        PriceSource::Mrr => {
            let oracle = mrr::MrrOracle::new(64, stale);
            let client = mrr::MrrClient::new();
            // Warm start so the first quote has a price.
            match client.fetch_floor().await {
                Ok(m) => {
                    tracing::info!(
                        btc_per_ph_day = m.floor.btc_per_ph_day,
                        rig = m.floor.id,
                        "mrr hashprice"
                    );
                    oracle.push(m.sample(SystemTime::now()));
                }
                Err(e) => tracing::warn!(error = %e, "mrr fetch failed; will retry"),
            }
            oracle.spawn_poller(client, every);
            Ok(Arc::new(WithFallback {
                live: oracle,
                fallback,
            }))
        }
        PriceSource::Esplora => {
            let network = parse_network(&args.network)?;
            let url = default_esplora_url(network)
                .with_context(|| format!("no public Esplora for {network}"))?;
            let oracle = esplora::EsploraOracle::new(64, stale);
            // The poller's first tick fires at once.
            oracle.spawn_poller(
                esplora::EsploraClient::with_base(url.to_string()),
                esplora::DEFAULT_WINDOW_BLOCKS,
                every,
            );
            Ok(Arc::new(WithFallback {
                live: oracle,
                fallback,
            }))
        }
    }
}

fn sats_pricer(
    oracle: &Arc<dyn HashpriceOracle + Send + Sync>,
    markup_bps: Option<u32>,
) -> SatsPerThhFromHashprice {
    let p = SatsPerThhFromHashprice::new(oracle.clone());
    match markup_bps {
        Some(bps) => p.with_markup_bps(bps),
        None => p,
    }
}

/// cdk saves the mint info (NUT-06) in the database on first boot and
/// serves that copy from then on, so a later `--mint-url`, a newly enabled
/// payment rail or a new description would never show up in `/v1/info`.
/// Overwrite the stored copy with this boot's, keeping the pubkey cdk
/// derived from the keysets.
async fn refresh_stored_mint_info(
    mint: &cdk::mint::Mint,
    mut info: cdk::nuts::MintInfo,
) -> Result<()> {
    let stored = mint.mint_info().await.context("read stored mint info")?;
    info.pubkey = stored.pubkey.or(info.pubkey);
    info.time = None;
    if info != stored {
        mint.set_mint_info(info)
            .await
            .context("update stored mint info")?;
        tracing::info!("mint info updated from this run's configuration");
    }
    Ok(())
}

/// Load the mint seed from `dir/seed.hex`, creating it on first run.
fn load_or_create_seed(dir: &Path) -> Result<Vec<u8>> {
    let path = dir.join("seed.hex");
    if path.exists() {
        let s = std::fs::read_to_string(&path).with_context(|| format!("read {path:?}"))?;
        let seed = hex::decode(s.trim()).with_context(|| format!("decode {path:?}"))?;
        if seed.len() != 64 {
            bail!("{path:?} must hold 64 hex-encoded bytes");
        }
        return Ok(seed);
    }
    let seed: [u8; 64] = {
        let mut b = [0u8; 64];
        rand::Rng::fill(&mut rand::thread_rng(), &mut b[..]);
        b
    };
    std::fs::write(&path, hex::encode(seed)).with_context(|| format!("write {path:?}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(?path, "generated new mint seed; back it up");
    Ok(seed.to_vec())
}

async fn lightning_node(args: &RunArgs) -> Result<Option<(DynMintPayment, &'static str)>> {
    let has_env = ["PHOENIXD_URL", "PHOENIXD_HOSTPORT"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| !v.trim().is_empty()));
    let backend = args.lightning.unwrap_or(if has_env {
        LightningBackend::Phoenixd
    } else {
        LightningBackend::Off
    });
    match backend {
        LightningBackend::Off => Ok(None),
        LightningBackend::Phoenixd => {
            let cfg = PhoenixdConfig::from_env().map_err(|e| anyhow::anyhow!("phoenixd: {e}"))?;
            let node = Phoenixd::connect(cfg)
                .await
                .map_err(|e| anyhow::anyhow!("phoenixd: {e}"))?;
            Ok(Some((Arc::new(node), "phoenixd")))
        }
        LightningBackend::Fake => {
            tracing::warn!("Lightning rail is cdk's FAKE wallet: invoices pay themselves");
            let node = cdk_fake_wallet::FakeWallet::new(
                cdk::types::FeeReserve {
                    min_fee_reserve: 1.into(),
                    percent_fee_reserve: 0.0,
                },
                Default::default(),
                Default::default(),
                2,
                CurrencyUnit::Sat,
            );
            Ok(Some((Arc::new(node), "fake")))
        }
    }
}

fn usdt_pricer(
    args: &RunArgs,
    oracle: &Arc<dyn HashpriceOracle + Send + Sync>,
) -> Result<Arc<dyn ThhPricer>> {
    if let Some(usdt) = args.usdt_per_thh {
        let micro = (usdt * MICRO_PER_USDT as f64).round();
        if !micro.is_finite() || micro < 1.0 {
            bail!("--usdt-per-thh must be at least 0.000001 (got {usdt})");
        }
        return Ok(Arc::new(FixedUsdtPrice {
            micro_per_thh: micro as u64,
        }));
    }
    if let Some(usd_per_btc) = args.usd_per_btc {
        return Ok(Arc::new(SatsToUsdt {
            sats: Arc::new(sats_pricer(oracle, None)),
            usd_per_btc,
        }));
    }
    bail!("the stablecoin rails need a price: pass --usdt-per-thh or --usd-per-btc")
}

/// Poll MRR for the account's active rentals, if a key is configured.
fn spawn_mrr_sync(args: &RunArgs, capacity: &Arc<CapacityBook>) {
    let (Some(key), Some(secret)) = (
        args.mrr_api_key.clone().filter(|k| !k.trim().is_empty()),
        args.mrr_api_secret.clone().filter(|k| !k.trim().is_empty()),
    ) else {
        return;
    };
    capacity.enable_mrr();
    let ids: Vec<String> = args
        .mrr_rental_ids
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let account = mrr::MrrAccount::with_base(
        args.mrr_api_base.clone(),
        mrr::DEFAULT_ALGO.into(),
        key,
        secret,
    );
    let capacity = capacity.clone();
    let every = Duration::from_secs(args.mrr_refresh_secs.max(15));
    tracing::info!(rental_ids = ?ids, "capacity from MRR active rentals");
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            let result = account
                .active_rentals()
                .await
                .map(|list| {
                    list.into_iter()
                        .filter(|r| ids.is_empty() || ids.contains(&r.id))
                        .map(|r| Rental {
                            id: r.id,
                            label: r.rig_name,
                            ths: r.ths,
                            end_ms: (r.end_unix * 1000.0).max(0.0) as u64,
                        })
                        .collect::<Vec<_>>()
                })
                .map_err(|e| e.to_string());
            match &result {
                Ok(r) => tracing::debug!(rentals = r.len(), "mrr rentals synced"),
                Err(e) => tracing::warn!(error = %e, "mrr rental sync failed"),
            }
            capacity.set_mrr(result, hashu_mint::capacity::now_ms());
        }
    });
}

/// Parse `--contact` entries (`method:info`) into NUT-06 contact info.
fn parse_contacts(raw: &[String]) -> Result<Vec<cdk::nuts::ContactInfo>> {
    raw.iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .map(|c| match c.split_once(':') {
            Some((m, i)) if !m.trim().is_empty() && !i.trim().is_empty() => Ok(
                cdk::nuts::ContactInfo::new(m.trim().to_string(), i.trim().to_string()),
            ),
            _ => bail!("HASHU_MINT_CONTACT entries must be method:info, e.g. email:ops@example.com (got {c})"),
        })
        .collect()
}

async fn run_daemon(args: RunArgs) -> Result<()> {
    // ---- shared state -------------------------------------------------
    let raw_upstream = args.upstream.as_deref().map(str::trim).unwrap_or("");
    if raw_upstream.is_empty() {
        bail!(
            "HASHU_UPSTREAM is not set: set it (or pass --upstream) to the pool your miners \
             mine to when no redemption is open, e.g. stratum+tcp://solo.ckpool.org:3333"
        );
    }
    let Some(upstream) = normalize_upstream(raw_upstream) else {
        bail!(
            "HASHU_UPSTREAM must be stratum+tcp://host:port or stratum+ssl://host:port (got {raw_upstream})"
        );
    };
    let contacts = parse_contacts(&args.contact)?;
    if args.mint_url.as_deref().is_none_or(|u| u.trim().is_empty())
        && !args.listen.ip().is_loopback()
    {
        tracing::warn!(
            "HASHU_MINT_URL is not set: tokens will name this mint by its local address; \
             set HASHU_MINT_URL to the public https URL before selling"
        );
    }
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("create {:?}", args.data_dir))?;
    let ledger_path = args.data_dir.join("redemptions.jsonl");
    let ledger = Arc::new(
        RedemptionLedger::persistent(&ledger_path)
            .with_context(|| format!("open redemption ledger {ledger_path:?}"))?,
    );
    let audit_path = args.data_dir.join("share-audit.jsonl");
    let audit = Arc::new(
        ShareAudit::persistent(&audit_path)
            .with_context(|| format!("open share audit {audit_path:?}"))?,
    );
    let oracle = build_oracle(&args).await?;
    let limits = MintMeltLimits::new(1, args.max_thh);
    let started_ms = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("create {:?}", args.data_dir))?;
    // Rented hashrate: what the mint can still sell (operator GUI / MRR).
    let capacity = Arc::new(CapacityBook::load(args.data_dir.join("capacity.json"))?);
    spawn_mrr_sync(&args, &capacity);
    // Live operator settings. Rails price at the base rate and the
    // controlled pricer adds the markup, so the GUI can change it.
    let controls = Arc::new(MintControls::with_capacity(
        args.markup_bps.unwrap_or(0) as i64,
        capacity.clone(),
    ));
    if let Err(e) = controls.set_heat_j_per_th(args.heat_j_per_th) {
        bail!("--heat-j-per-th: {e}");
    }
    // Settings changed in the operator GUI survive a restart.
    controls.persist_to(args.data_dir.join("operator-settings.json"))?;
    let sats_controlled = Arc::new(ControlledPricer::new(
        Arc::new(sats_pricer(&oracle, None)),
        controls.clone(),
    ));
    let mut usdt_controlled: Option<Arc<ControlledPricer>> = None;

    // ---- mint ---------------------------------------------------------
    let seed = load_or_create_seed(&args.data_dir)?;
    let db_path = args.data_dir.join("mint.sqlite");
    let db = Arc::new(
        cdk_sqlite::MintSqliteDatabase::new(db_path.to_string_lossy().as_ref())
            .await
            .with_context(|| format!("open {db_path:?}"))?,
    );
    // Keep the ledger consistent with cdk's melt quotes: any redemption the
    // mint still holds proofs pending for must exist before `mint.start()`
    // re-checks it.
    {
        use cdk::cdk_database::MintQuotesDatabase;
        let quotes = db
            .get_melt_quotes()
            .await
            .map_err(|e| anyhow::anyhow!("read melt quotes: {e}"))?;
        reopen_pending_quotes(&ledger, &quotes);
    }

    let mut builder = MintBuilder::new(db.clone())
        .with_name(args.name.clone())
        .with_description(args.description.clone());
    for c in contacts {
        builder = builder.with_contact_info(c);
    }
    if let Some(url) = &args.mint_url {
        builder = builder.with_urls(vec![url.clone()]);
    }
    builder
        .configure_unit(
            thh_unit(),
            UnitConfig {
                amounts: DENOMS.to_vec(),
                input_fee_ppk: 0,
            },
        )
        .context("configure THH unit")?;

    // Redemption: the only way out of the mint.
    builder
        .add_payment_processor(
            thh_unit(),
            PaymentMethod::Custom(HASHRATE.into()),
            MintMeltLimits::new(args.min_redeem_thh.max(1), args.max_thh),
            Arc::new(
                HashrateMelter::new(ledger.clone())
                    .with_min_thh(args.min_redeem_thh)
                    .with_max_thh(args.max_thh),
            ),
        )
        .await
        .context("register hashrate melt")?;
    let mut methods = vec![HASHRATE.to_string()];
    let mut rails = Vec::new();

    // Lightning: mint-only.
    if let Some((node, kind)) = lightning_node(&args).await? {
        let issuer = ThhIssuer::new(sats_controlled.clone(), node, CurrencyUnit::Sat);
        builder = add_lightning_issuance(builder, issuer, limits)
            .await
            .context("register Lightning rail")?;
        methods.push("bolt11".into());
        rails.push(format!("bolt11 ({kind})"));
    }

    // Stablecoins: mint-only. One dollar price serves both coins.
    let coins = [
        (
            Stablecoin::Usdt,
            &args.usdt_deposit,
            &args.usdt_token,
            &args.usdt_rpc,
            args.usdt_chain_id,
            args.usdt_confirmations,
        ),
        (
            Stablecoin::Usdc,
            &args.usdc_deposit,
            &args.usdc_token,
            &args.usdc_rpc,
            args.usdc_chain_id,
            args.usdc_confirmations,
        ),
    ];
    for (coin, deposit, token, rpc, chain_id, confirmations) in coins {
        let Some(deposit) = deposit.as_deref().filter(|d| !d.trim().is_empty()) else {
            continue;
        };
        let symbol = coin.symbol();
        let rail = UsdtRail::new(coin, chain_id, token, deposit)
            .map_err(|e| anyhow::anyhow!("{symbol} rail: {e}"))?;
        let source = EvmRpcSource::new(rpc.clone(), rail.clone(), confirmations);
        let pricer = match &usdt_controlled {
            Some(p) => p.clone(),
            None => {
                let p = Arc::new(ControlledPricer::new(
                    usdt_pricer(&args, &oracle)?,
                    controls.clone(),
                ));
                usdt_controlled = Some(p.clone());
                p
            }
        };
        let quotes = args.data_dir.join(format!("{}-quotes.json", coin.method()));
        let payments = TetherPayments::new(rail, pricer, Arc::new(source))
            .with_persistence(&quotes)
            .map_err(|e| anyhow::anyhow!("{symbol} rail: {e}"))?;
        builder
            .add_payment_processor(
                thh_unit(),
                PaymentMethod::Custom(coin.method().into()),
                limits,
                Arc::new(payments),
            )
            .await
            .with_context(|| format!("register {symbol} rail"))?;
        methods.push(coin.method().into());
        rails.push(format!(
            "{} (chain {}, to {deposit})",
            coin.method(),
            chain_id
        ));
    }
    // cdk advertises every processor for both minting and melting. A Hashu
    // mint sells THH over its payment rails and redeems it only as
    // hashrate, so keep NUT-04 to the rails and NUT-05 to `hashrate`.
    let mut info = builder.current_mint_info();
    let hashrate = PaymentMethod::Custom(HASHRATE.into());
    info.nuts.nut04.methods.retain(|m| m.method != hashrate);
    info.nuts.nut05.methods.retain(|m| m.method == hashrate);
    info.nuts.nut15.methods.clear();
    builder = builder.with_mint_info(info.clone());

    if rails.is_empty() {
        tracing::warn!(
            "no payment rail enabled: set PHOENIXD_URL/PHOENIXD_PASSWORD, --lightning fake, --usdt-deposit or --usdc-deposit"
        );
    }

    let mint = Arc::new(
        builder
            .build_with_seed(db, &seed)
            .await
            .context("build mint")?,
    );
    refresh_stored_mint_info(&mint, info).await?;
    mint.start().await.context("start mint")?;
    // Count what is owed before the first quote, then keep it current.
    match operator::obligations(&mint, &ledger).await {
        Ok(o) => capacity.set_owed(o.total_owed(), hashu_mint::capacity::now_ms()),
        Err(e) => tracing::warn!(error = %e, "could not count obligations"),
    }
    tokio::spawn(operator::run_owed_refresher(
        mint.clone(),
        ledger.clone(),
        capacity.clone(),
        Duration::from_secs(5),
    ));
    let cap = capacity.snapshot(hashu_mint::capacity::now_ms());
    tracing::info!(
        limit = cap.limit,
        rented_ths = cap.rented_ths,
        capacity_thh = cap.capacity_thh,
        owed_thh = cap.owed_thh,
        available_thh = ?cap.available_thh,
        "capacity"
    );

    let price_now = sats_controlled.quote(1).ok();
    tracing::info!(
        listen = %args.listen,
        rails = ?rails,
        sats_per_thh = ?price_now,
        "mint ready (unit THH, melt method `hashrate`); buyer checkout at http://{}/checkout/",
        args.listen
    );

    // ---- hashrate meter + buyer checkout --------------------------------
    let metrics = Arc::new(ProxyMetrics::default());
    let meter = Arc::new(HashrateMeter::new(
        metrics.clone(),
        ledger.clone(),
        Duration::from_secs(args.hashrate_window.max(1)),
    ));
    tokio::spawn(meter.clone().run(Duration::from_secs(1)));
    let internal_url = if args.listen.ip().is_unspecified() {
        format!("http://127.0.0.1:{}", args.listen.port())
    } else {
        format!("http://{}", args.listen)
    };
    let checkout = Arc::new(CheckoutState::new(
        internal_url.clone(),
        args.mint_url.clone().unwrap_or(internal_url),
        controls.clone(),
        Some(sats_controlled.clone()),
        usdt_controlled.clone(),
        methods.iter().filter(|m| *m != HASHRATE).cloned().collect(),
        args.min_redeem_thh.max(1),
        args.max_thh,
        ledger.clone(),
        mint.clone(),
        meter.clone(),
        Arc::new(
            OrderBook::persistent(&args.data_dir.join("redemption-orders.json"))
                .context("open redemption orders")?,
        ),
    ));

    let app = cdk_axum::create_mint_router(mint.clone(), methods)
        .await
        .context("build mint HTTP router")?
        .merge(checkout::router(checkout.clone()))
        .merge(landing::router());
    let http = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind mint API on {}", args.listen))?;
    let http_task = tokio::spawn(async move { axum::serve(http, app).await });

    // ---- proxy --------------------------------------------------------
    let router =
        Arc::new(Router::new(upstream.clone(), ledger.clone()).with_switch_mode(args.switch_mode));
    let crediter = Arc::new(ShareCrediter::with_audit(ledger.clone(), audit.clone()));
    let credit = CreditContext::new(crediter, router.clone());

    if args.metrics_interval > 0 {
        let m = metrics.clone();
        let l = ledger.clone();
        let interval = Duration::from_secs(args.metrics_interval);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.tick().await;
            loop {
                tick.tick().await;
                let s = m.snapshot();
                tracing::info!(
                    connections = s.connections_accepted,
                    accepted = s.shares_accepted,
                    rejected = s.shares_rejected,
                    credited = s.shares_credited,
                    open_redemptions = l.active().len(),
                    "proxy metrics",
                );
            }
        });
    }

    if let Some(bind) = args
        .control_bind
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        let bind: SocketAddr = bind
            .parse()
            .with_context(|| format!("--control-bind {bind:?}"))?;
        let control = ControlState {
            ledger: ledger.clone(),
            metrics: metrics.clone(),
            default_upstream: upstream.clone(),
            audit,
            allow_open: args.control_allow_open,
        };
        let state = Arc::new(OperatorState {
            mint: mint.clone(),
            control,
            meter: meter.clone(),
            controls: controls.clone(),
            sats: Some(sats_controlled.clone()),
            usdt: usdt_controlled.clone(),
            info: DaemonInfo {
                name: args.name.clone(),
                mint_listen: args.listen.to_string(),
                mint_url: args.mint_url.clone(),
                stratum_bind: args.stratum_bind.to_string(),
                rails: rails.clone(),
                min_redeem_thh: args.min_redeem_thh,
                max_thh: args.max_thh,
            },
            started_ms,
            checkout: checkout.clone(),
        });
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .with_context(|| format!("bind control API on {bind}"))?;
        tracing::info!(
            %bind,
            allow_open = args.control_allow_open,
            "control API listening; operator GUI at http://{bind}/"
        );
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, operator::router(state)).await {
                tracing::error!(error = %e, "control API stopped");
            }
        });
    }

    tracing::info!(bind = %args.stratum_bind, %upstream, "stratum proxy listening");
    let proxy = run_routed_listener(
        args.stratum_bind,
        router,
        metrics,
        Some(credit),
        // Reconcile tick: how soon a new redemption starts moving miners.
        Duration::from_millis(250),
    );

    let result = tokio::select! {
        r = proxy => r.context("stratum proxy"),
        r = http_task => match r {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(anyhow::Error::from(e).context("mint HTTP server")),
            Err(e) => Err(anyhow::Error::from(e).context("mint HTTP task")),
        },
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutting down");
            Ok(())
        }
    };
    if let Err(e) = mint.stop().await {
        tracing::warn!(error = %e, "mint stop");
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_price_round_trips_to_sats_per_thh() {
        let oracle: Arc<dyn HashpriceOracle + Send + Sync> =
            Arc::new(fixed_from_sats_per_thh(50.0).unwrap());
        let p = sats_pricer(&oracle, Some(0));
        assert_eq!(p.price(10).unwrap(), 500);
    }

    #[test]
    fn fallback_covers_an_empty_live_oracle() {
        let o = WithFallback {
            live: mrr::MrrOracle::new(4, Duration::from_secs(60)),
            fallback: Some(fixed_from_sats_per_thh(36.0).unwrap()),
        };
        let per_ths = o.sample_at(SystemTime::now()).unwrap();
        assert!((per_ths * SECS_PER_THH - 36.0).abs() < 1e-9);
    }

    #[test]
    fn denominations_are_powers_of_two() {
        assert_eq!(DENOMS[0], 1);
        assert_eq!(DENOMS[20], 1 << 20);
    }

    #[test]
    fn rejects_non_positive_fixed_price() {
        assert!(fixed_from_sats_per_thh(0.0).is_err());
        assert!(fixed_from_sats_per_thh(-1.0).is_err());
    }
}

#[cfg(test)]
mod contact_tests {
    use super::parse_contacts;

    #[test]
    fn parses_method_info_pairs() {
        let c = parse_contacts(&[
            "email:ops@example.com".into(),
            " nostr : npub1abc ".into(),
            "".into(),
        ])
        .unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].method, "email");
        assert_eq!(c[0].info, "ops@example.com");
        assert_eq!(c[1].method, "nostr");
        assert_eq!(c[1].info, "npub1abc");
    }

    #[test]
    fn rejects_entries_without_a_method() {
        assert!(parse_contacts(&["ops@example.com".into()]).is_err());
        assert!(parse_contacts(&["email:".into()]).is_err());
    }
}
