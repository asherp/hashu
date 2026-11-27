//! `hashu mint run` — the Hashu mint daemon. ARCHITECTURE.md §1, §4.
//!
//! One process runs everything a demo needs:
//!
//! - a **cdk mint** for the `TiH` unit, served over HTTP (NUT-00..NUT-07 plus
//!   the payment-method routes);
//! - **payment rails** that sell `TiH`: Lightning over phoenixd (mainnet
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
use hashu_core::denom::msat_per_pih_from_sats_per_ths;
use hashu_core::hashprice::{HashpriceOracle, HashpriceSample};
use hashu_core::redemption::RedemptionLedger;
use hashu_mint::capacity::{CapacityBook, MIN_PURCHASE_TIH};
use hashu_mint::epoch::{EpochConverter, Epochs, METHOD as EPOCH};
use hashu_mint::lightning::add_lightning_issuance;
use hashu_mint::melt::{reopen_pending_quotes, HashrateMelter, METHOD as HASHRATE};
use hashu_mint::oracle::{esplora, mrr};
use hashu_mint::payment::{tih_unit, TihIssuer};
use hashu_mint::phoenixd::{Phoenixd, PhoenixdConfig};
use hashu_mint::pricing::{MsatPerPihFromHashprice, TihPricer};
use hashu_mint::tether::{
    EvmRpcSource, FixedUsdtPrice, SatsToUsdt, Stablecoin, TetherPayments, UsdtRail, BASE_CHAIN_ID,
    BASE_USDC, MAINNET_CHAIN_ID, MAINNET_USDT, MICRO_PER_USDT,
};
use hashu_proxy::audit::ShareAudit;
use hashu_proxy::control::ControlState;
use hashu_proxy::credit::{CreditContext, ShareCrediter};
use hashu_proxy::meter::HashrateMeter;
use hashu_proxy::router::{Router, SwitchMode};
use hashu_proxy::stratum::run_routed_listener_on;
use hashu_proxy::upstream::normalize_upstream;
use hashu_proxy::ProxyMetrics;

use crate::checkout::{self, CheckoutState};
use crate::landing;
use crate::mrr_sync::{MrrCredentials, MrrSync};
use crate::network::{default_esplora_url, parse_network};
use crate::operator::{self, ControlledPricer, DaemonInfo, MintControls, OperatorState};
use crate::orders::OrderBook;
use crate::tenants::{self, TenantConfig, Tenants};
use crate::waitlist::{self, Waitlist};

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

/// `TiH` denominations the keyset signs: powers of two up to 2^40 (one
/// 2^40 TiH proof is ≈ 336M THH; a TiH-sized keyset needs far more range
/// than the old THH one, whose 2^20 cap is only ≈ 320 THH in TiH).
pub(crate) const DENOMS: [u64; 41] = {
    let mut d = [0u64; 41];
    let mut i = 0;
    while i < 41 {
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
    /// A fixed `--msat-per-pih`.
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
    /// Domain hosted supplier mints are named under: supplier `alice` gets
    /// `alice.<domain>` (e.g. `example.com` for `alice.example.com`), and
    /// sign-up refuses names the operator needs (app, www, mint, api, ...).
    /// Their older name under the mint's host (`alice.mint.example.com`)
    /// keeps working. Unset or empty: `<name>.<mint host>`.
    #[arg(long, env = "HASHU_SUPPLIER_DOMAIN")]
    pub supplier_domain: Option<String>,
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
        default_value = "Hashrate vouchers in tebihashes (TiH, 2^40 hashes). Pay with Lightning or USDT; redeem by pointing hashrate at your pool."
    )]
    pub description: String,
    /// Mint logo (NUT-06 `icon_url`), shown by wallets beside the mint's
    /// name. Unset: Hashu's mark, served by the mint itself at
    /// `<mint url>/nostr/avatar.png`. Empty: no logo.
    #[arg(long, env = "HASHU_MINT_ICON_URL")]
    pub icon_url: Option<String>,
    /// How to reach the operator (NUT-06 `contact`), comma separated
    /// `method:info` pairs, e.g. `email:ops@example.com,nostr:npub1...`.
    /// Shown on the landing page. Empty: no contact.
    #[arg(long, env = "HASHU_MINT_CONTACT", value_delimiter = ',')]
    pub contact: Vec<String>,
    /// Largest mint quote or redemption, in TiH (default 2^32 ≈ 1.3M THH).
    #[arg(long, default_value_t = 1 << 32)]
    pub max_tih: u64,
    /// Smallest redemption, in TiH.
    #[arg(long, default_value_t = hashu_mint::melt::DEFAULT_MIN_TIH)]
    pub min_redeem_tih: u64,
    /// Redeem only to pools the proxy reaches encrypted: `stratum+ssl://`
    /// (TLS, certificate checked) or `stratum2+tcp://` (Stratum V2).
    /// Quotes and pool changes naming a plain `stratum+tcp://` pool, as
    /// the main pool or a backup, are refused. Plain Stratum V1 can be
    /// hijacked in transit, sending the redeemer's shares elsewhere.
    #[arg(long, env = "HASHU_ENCRYPTED_POOLS_ONLY", default_value_t = false)]
    pub encrypted_pools_only: bool,
    /// Serve the hosted-mint waitlist for miners at `/waitlist/` and store
    /// sign-ups in `<data-dir>/waitlist.jsonl` (operator view at
    /// `/waitlist` on the control API). Off: `/waitlist/` is a 404.
    #[arg(long, env = "HASHU_WAITLIST", default_value_t = false)]
    pub waitlist: bool,
    /// Ops database (Postgres URL) the operator dashboard's live feed is
    /// saved to, so restarts keep the hashrate chart, per-rig work, proxy
    /// counters and redemption times (see `telemetry.rs`). Empty: kept in
    /// memory only.
    #[arg(
        long,
        env = "HASHU_OPS_DATABASE_URL",
        default_value = "",
        hide_env_values = true
    )]
    pub ops_database_url: String,
    /// Nostr relays the hourly reliability record is posted to (comma
    /// separated). Empty: served only at /v1/hashu/reliability.
    #[arg(long, env = "HASHU_NOSTR_RELAYS", default_value = "")]
    pub nostr_relays: String,
    /// OpenTimestamps calendars that timestamp each hour (comma separated
    /// https URLs). Empty: hours are not timestamped.
    #[arg(long, env = "HASHU_OTS_CALENDARS", default_value = "")]
    pub ots_calendars: String,
    /// Tips on the mint's Nostr profile, to support Hashu's development
    /// (never a voucher sale). `on`: the mint's own tip jar at
    /// `tips@<mint host>` with NIP-57 zaps, when phoenixd is the Lightning
    /// backend and the mint has a public https URL. A Lightning address or
    /// LNURL-pay link: point the profile there instead. `off`: no tips.
    /// Supplier mints always use their own `lnurl` setting.
    #[arg(long, env = "HASHU_TIPS", default_value = "on")]
    pub tips: crate::tips::TipsSetting,
    /// The `about` of the operator mint's Nostr profile, exactly as
    /// written. Empty: the mint description, a link to its delivery
    /// record and where tips go.
    #[arg(
        long,
        env = "HASHU_NOSTR_ABOUT",
        default_value = "Hashu mints hash vouchers redeemable for hashrate directed at your pool."
    )]
    pub nostr_about: String,
    /// Date vouchers to Bitcoin difficulty epochs (2,016 blocks, #126):
    /// the mint sells only the current epoch, rotates its TiH keyset at
    /// each epoch, and converts expired vouchers (melt method `epoch`) to
    /// the current epoch, or the next one with room. Vouchers sold before
    /// this was turned on stay perpetual. Off: one perpetual keyset.
    #[arg(long, env = "HASHU_EPOCHS", default_value_t = false)]
    pub epochs: bool,
    /// Esplora API whose chain tip sets the epoch (Bitcoin mainnet, where
    /// redemptions mine, by default).
    #[arg(long, env = "HASHU_EPOCH_ESPLORA", default_value = esplora::DEFAULT_BASE_URL)]
    pub epoch_esplora: String,

    // ---- pricing ----------------------------------------------------
    /// Where the price of hashes comes from.
    #[arg(long, value_enum, default_value_t = PriceSource::Mrr)]
    pub price: PriceSource,
    /// Fixed price in msat per PiH (2^50 hashes = 1,024 TiH; ≈ 625 msat is
    /// ≈ 2 sats per THH). Required with `--price fixed`; with a live source
    /// it is used until the first sample arrives and whenever the source
    /// goes stale.
    #[arg(long, env = "HASHU_MSAT_PER_PIH")]
    pub msat_per_pih: Option<f64>,
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
    /// Fixed dollar price per PiH (2^50 hashes, e.g. 0.0006), used for both
    /// USDT and USDC. Takes precedence over `--usd-per-btc`.
    #[arg(long, env = "HASHU_USDT_PER_PIH")]
    pub usdt_per_pih: Option<f64>,
    /// BTC/USD rate, to price USDT quotes from the sats price so both rails
    /// sell TiH at the same price.
    #[arg(long, env = "HASHU_USD_PER_BTC")]
    pub usd_per_btc: Option<f64>,

    // ---- proxy ------------------------------------------------------
    /// Address the Stratum proxy binds for miners.
    #[arg(long, default_value = "0.0.0.0:3333")]
    pub stratum_bind: SocketAddr,
    /// Public Stratum address miners connect to, e.g.
    /// `stratum+tcp://mint.example.com:3335`: shown on the checkout info,
    /// the operator dashboard, the waitlist and supplier sign-up. Unset:
    /// the `--mint-url` host with the `--stratum-bind` port, which is
    /// wrong behind a port mapping (compose maps host 3335 to 3333).
    #[arg(long, env = "HASHU_STRATUM_URL")]
    pub stratum_url: Option<String>,
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
    #[command(flatten)]
    pub home: crate::proxy::HomePoolArgs,
    /// Control API (`GET /status`) the MDK app polls, and the operator GUI
    /// at `/`. Pass an empty value to turn it off.
    #[arg(long, default_value = "127.0.0.1:3334")]
    pub control_bind: Option<String>,
    /// Also serve `POST /redemptions` on the control API, bypassing the
    /// mint. Anyone who can reach the control port can then redirect the
    /// hashrate; keep it on loopback.
    #[arg(long)]
    pub control_allow_open: bool,
    /// Operator login for the GUI and control API: bcrypt hash of the
    /// password (`caddy hash-password` makes one). Required when
    /// `--control-bind` is not loopback, unless `--control-allow-open`.
    #[arg(long, env = "HASHU_OPERATOR_PASSWORD_HASH", hide_env_values = true)]
    pub operator_password_hash: Option<String>,
    /// User name for the operator login.
    #[arg(long, env = "HASHU_OPERATOR_USER", default_value = "operator")]
    pub operator_user: String,
    /// Periodic proxy metrics log interval (seconds). 0 disables.
    #[arg(long, default_value_t = 30)]
    pub metrics_interval: u64,
    /// Window the operator GUI's hashrate averages accepted shares over
    /// (seconds). Shares are sparse, so short windows are noisy.
    #[arg(long, default_value_t = 60)]
    pub hashrate_window: u64,
    /// The operator's Hashu wallet (the hashu.me PWA), e.g.
    /// `https://app.example.com`. The landing and checkout pages link to
    /// it and offer "Open in wallet" for tokens. Unset: no wallet links.
    #[arg(long, env = "HASHU_APP_URL")]
    pub app_url: Option<String>,
    /// Legal name of whoever sells this mint's vouchers, e.g.
    /// `Example Mining, LLC`. The checkout names them as the seller.
    /// Unset: no seller line.
    #[arg(long, env = "HASHU_SELLER_NAME")]
    pub seller_name: Option<String>,
    /// The seller's terms of sale (http(s) URL). With `--seller-name`, the
    /// checkout links them and asks buyers and redeemers to accept them.
    #[arg(long, env = "HASHU_TERMS_URL")]
    pub terms_url: Option<String>,

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

fn fixed_from_msat_per_pih(msat_per_pih: f64) -> Result<FixedHashprice> {
    if !msat_per_pih.is_finite() || msat_per_pih <= 0.0 {
        bail!("--msat-per-pih must be a positive number (got {msat_per_pih})");
    }
    Ok(FixedHashprice {
        sats_per_ths: msat_per_pih / msat_per_pih_from_sats_per_ths(1.0),
    })
}

/// Build the hashprice oracle and start its poller, if any.
async fn build_oracle(args: &RunArgs) -> Result<Arc<dyn HashpriceOracle + Send + Sync>> {
    let fallback = args.msat_per_pih.map(fixed_from_msat_per_pih).transpose()?;
    let every = Duration::from_secs(args.price_refresh_secs.max(10));
    // A sample stays usable for three refreshes, so one failed fetch
    // doesn't stop quoting.
    let stale = every * 3;
    match args.price {
        PriceSource::Fixed => {
            let f = fallback.context("--price fixed needs --msat-per-pih")?;
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
) -> MsatPerPihFromHashprice {
    let p = MsatPerPihFromHashprice::new(oracle.clone());
    match markup_bps {
        Some(bps) => p.with_markup_bps(bps),
        None => p,
    }
}

/// The mint's NUT-06 `icon_url`: the configured one (empty = none), else
/// Hashu's mark at `<mint url>/nostr/avatar.png` when the mint has a
/// public URL.
pub(crate) fn mint_icon_url(configured: Option<&str>, mint_url: Option<&str>) -> Option<String> {
    match configured.map(str::trim) {
        Some("") => None,
        Some(icon) => Some(icon.to_string()),
        None => mint_url
            .map(|u| u.trim().trim_end_matches('/'))
            .filter(|u| !u.is_empty())
            .map(|u| format!("{u}{}", hashu_mint::discovery::AVATAR_PATH)),
    }
}

/// cdk saves the mint info (NUT-06) in the database on first boot and
/// serves that copy from then on, so a later `--mint-url`, a newly enabled
/// payment rail or a new description would never show up in `/v1/info`.
/// Overwrite the stored copy with this boot's, keeping the pubkey cdk
/// derived from the keysets.
pub(crate) async fn refresh_stored_mint_info(
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
pub(crate) fn load_or_create_seed(dir: &Path) -> Result<Vec<u8>> {
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
) -> Result<Arc<dyn TihPricer>> {
    if let Some(usdt) = args.usdt_per_pih {
        let micro = (usdt * MICRO_PER_USDT as f64).round();
        if !micro.is_finite() || micro < 1.0 {
            bail!("--usdt-per-pih must be at least 0.000001 (got {usdt})");
        }
        return Ok(Arc::new(FixedUsdtPrice {
            micro_per_pih: micro as u64,
        }));
    }
    if let Some(usd_per_btc) = args.usd_per_btc {
        return Ok(Arc::new(SatsToUsdt {
            sats: Arc::new(sats_pricer(oracle, None)),
            usd_per_btc,
        }));
    }
    bail!("the stablecoin rails need a price: pass --usdt-per-pih or --usd-per-btc")
}

/// Poll MRR for the account's active rentals, if a key is configured.
/// MRR rental sync: env credentials unless the operator GUI saved a choice
/// in `mrr.json` (see [`crate::mrr_sync`]).
fn start_mrr_sync(args: &RunArgs, capacity: &Arc<CapacityBook>) -> Result<Arc<MrrSync>> {
    let env = match (&args.mrr_api_key, &args.mrr_api_secret) {
        (Some(k), Some(s)) => MrrCredentials::new(k, s, &args.mrr_rental_ids),
        _ => None,
    };
    MrrSync::start(
        capacity.clone(),
        args.mrr_api_base.clone(),
        Duration::from_secs(args.mrr_refresh_secs),
        Some(args.data_dir.join("mrr.json")),
        env,
    )
}

/// List the mint's receipt key as a NUT-06 `nostr` contact, unless it is
/// listed already.
pub(crate) fn add_nostr_contact(
    contacts: &mut Vec<cdk::nuts::ContactInfo>,
    keys: &nostr::Keys,
) -> Result<()> {
    use nostr::ToBech32;
    let npub = keys.public_key().to_bech32()?;
    if !contacts
        .iter()
        .any(|c| c.method == "nostr" && c.info == npub)
    {
        contacts.push(cdk::nuts::ContactInfo::new("nostr".into(), npub));
    }
    Ok(())
}

/// NIP-87 `n` tag for this mint: Hashu takes mainnet Lightning; a fake
/// Lightning backend is a test mint.
pub(crate) fn nostr_network(fake_lightning: bool) -> &'static str {
    if fake_lightning {
        "regtest"
    } else {
        "mainnet"
    }
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
            "HASHU_UPSTREAM must be stratum+tcp://host:port, stratum+ssl://host:port or stratum2+tcp://host[:port]/<authority key> (got {raw_upstream})"
        );
    };
    // Bind the Stratum port before anything slow (oracle, Lightning node,
    // mint database). After a deploy the rigs are already retrying: a
    // bound port queues their connections until the proxy starts
    // accepting, where a closed one refuses them, so they back off or
    // fail over to a backup pool.
    let stratum_listener = tokio::net::TcpListener::bind(args.stratum_bind)
        .await
        .with_context(|| format!("bind Stratum on {}", args.stratum_bind))?;
    let mut contacts = parse_contacts(&args.contact)?;
    // The mint's own Nostr identity: the receipt key that signs its
    // reliability record and NIP-87 announcement (#109, #116).
    add_nostr_contact(
        &mut contacts,
        &hashu_mint::reliability::receipt_keys(&args.data_dir.join("reliability"))
            .context("load reliability receipt key")?,
    )?;
    let stratum = crate::stratum_addr::resolve(
        args.stratum_url.as_deref(),
        args.mint_url.as_deref(),
        args.stratum_bind,
    )?;
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
        // Finished redemptions' trees are restored evicted (count only);
        // proofs for them are rebuilt from the journal on request.
        ShareAudit::persistent_evicting(&audit_path, |id| {
            ledger.get(id).is_some_and(|r| r.is_finished())
        })
        .with_context(|| format!("open share audit {audit_path:?}"))?,
    );
    let oracle = build_oracle(&args).await?;
    let limits = MintMeltLimits::new(MIN_PURCHASE_TIH, args.max_tih);
    let started_ms = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    std::fs::create_dir_all(&args.data_dir)
        .with_context(|| format!("create {:?}", args.data_dir))?;
    // Rented hashrate: what the mint can still sell (operator GUI / MRR).
    let capacity = Arc::new(CapacityBook::load(args.data_dir.join("capacity.json"))?);
    let mrr_sync = start_mrr_sync(&args, &capacity)?;
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
    if let Some(icon) = mint_icon_url(args.icon_url.as_deref(), args.mint_url.as_deref()) {
        builder = builder.with_icon_url(icon);
    }
    builder
        .configure_unit(
            tih_unit(),
            UnitConfig {
                amounts: DENOMS.to_vec(),
                input_fee_ppk: 0,
            },
        )
        .context("configure TiH unit")?;

    // Redemption: the only way out of the mint. Paused while the rigs
    // are down (the router fills `rigs` in once it exists).
    let rigs = hashu_core::availability::RigsDown::new();
    builder
        .add_payment_processor(
            tih_unit(),
            PaymentMethod::Custom(HASHRATE.into()),
            MintMeltLimits::new(args.min_redeem_tih.max(1), args.max_tih),
            Arc::new(
                HashrateMelter::new(ledger.clone())
                    .with_min_tih(args.min_redeem_tih)
                    .with_max_tih(args.max_tih)
                    .with_encrypted_pools_only(args.encrypted_pools_only)
                    .with_rigs(rigs.clone()),
            ),
        )
        .await
        .context("register hashrate melt")?;
    let mut methods = vec![HASHRATE.to_string()];
    let mut rails = Vec::new();

    // Epochs: one keyset per difficulty epoch, and the `epoch` melt that
    // converts vouchers between them (#126).
    let epochs = if args.epochs {
        let e = Arc::new(Epochs::open(
            &seed,
            DENOMS.to_vec(),
            Some(args.data_dir.join("epochs.json")),
        )?);
        builder
            .add_payment_processor(
                tih_unit(),
                PaymentMethod::Custom(EPOCH.into()),
                MintMeltLimits::new(1, args.max_tih),
                Arc::new(EpochConverter::new(e.clone())),
            )
            .await
            .context("register epoch conversion")?;
        Some(e)
    } else {
        None
    };

    // Lightning: mint-only.
    let mut fake_lightning = false;
    let mut phoenixd = false;
    if let Some((node, kind)) = lightning_node(&args).await? {
        fake_lightning = kind == "fake";
        phoenixd = kind == "phoenixd";
        let issuer = TihIssuer::new(sats_controlled.clone(), node, CurrencyUnit::Sat);
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
                tih_unit(),
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
    // mint sells TiH over its payment rails and redeems it only as
    // hashrate, so keep NUT-04 to the rails and NUT-05 to `hashrate`.
    let mut info = builder.current_mint_info();
    let hashrate = PaymentMethod::Custom(HASHRATE.into());
    let epoch_method = PaymentMethod::Custom(EPOCH.into());
    info.nuts
        .nut04
        .methods
        .retain(|m| m.method != hashrate && m.method != epoch_method);
    info.nuts
        .nut05
        .methods
        .retain(|m| m.method == hashrate || m.method == epoch_method);
    info.nuts.nut15.methods.clear();
    builder = builder.with_mint_info(info.clone());

    if rails.is_empty() {
        tracing::warn!(
            "no payment rail enabled: set PHOENIXD_URL/PHOENIXD_PASSWORD, --lightning fake, --usdt-deposit or --usdc-deposit"
        );
    }

    let mint = Arc::new(
        builder
            .build_with_seed(db.clone(), &seed)
            .await
            .context("build mint")?,
    );
    refresh_stored_mint_info(&mint, info).await?;
    mint.start().await.context("start mint")?;
    // Count what is owed before the first quote, then keep it current.
    match operator::obligations(&mint, &ledger).await {
        Ok(o) => {
            capacity.set_owed(o.total_owed(), hashu_mint::capacity::now_ms());
            capacity.set_sold(o.stock_sold as f64);
        }
        Err(e) => tracing::warn!(error = %e, "could not count obligations"),
    }
    let supply = Arc::new(crate::supply::SupplyHistory::load(
        args.data_dir.join("supply-history.jsonl"),
    ));
    tokio::spawn(operator::run_owed_refresher(
        mint.clone(),
        ledger.clone(),
        capacity.clone(),
        Duration::from_secs(5),
        Some(supply.clone()),
    ));
    let cap = capacity.snapshot(hashu_mint::capacity::now_ms());
    tracing::info!(
        limit = cap.limit,
        rented_ths = cap.rented_ths,
        capacity_tih = cap.capacity_tih,
        owed_tih = cap.owed_tih,
        available_tih = ?cap.available_tih,
        "capacity"
    );

    let price_now = crate::operator::msat_per_pih(&sats_controlled);
    tracing::info!(
        listen = %args.listen,
        rails = ?rails,
        msat_per_pih = ?price_now,
        "mint ready (unit TiH, melt method `hashrate`); buyer checkout at http://{}/checkout/",
        args.listen
    );

    // ---- hashrate meter + buyer checkout --------------------------------
    let metrics = Arc::new(ProxyMetrics::default());
    let meter = Arc::new(HashrateMeter::new(
        metrics.clone(),
        ledger.clone(),
        Duration::from_secs(args.hashrate_window.max(1)),
    ));
    let internal_url = if args.listen.ip().is_unspecified() {
        format!("http://127.0.0.1:{}", args.listen.port())
    } else {
        format!("http://{}", args.listen)
    };
    let checkout = Arc::new(
        CheckoutState::new(
            internal_url.clone(),
            args.mint_url.clone().unwrap_or(internal_url.clone()),
            controls.clone(),
            Some(sats_controlled.clone()),
            usdt_controlled.clone(),
            methods.iter().filter(|m| *m != HASHRATE).cloned().collect(),
            args.min_redeem_tih.max(1),
            args.max_tih,
            ledger.clone(),
            mint.clone(),
            meter.clone(),
            Arc::new(
                OrderBook::persistent(&args.data_dir.join("redemption-orders.json"))
                    .context("open redemption orders")?,
            ),
        )
        .with_app_url(app_url(args.app_url.as_deref())?)
        .with_seller(seller(
            args.seller_name.as_deref(),
            args.terms_url.as_deref(),
        )?)
        .with_encrypted_pools_only(args.encrypted_pools_only)
        .with_stratum(stratum.clone())
        .with_rigs(rigs.clone()),
    );

    let waitlist = if args.waitlist {
        let path = args.data_dir.join("waitlist.jsonl");
        Some(Arc::new(
            Waitlist::open(path.clone()).with_context(|| format!("open waitlist {path:?}"))?,
        ))
    } else {
        None
    };

    // The proxy router, built here because supplier mints sell against
    // their rigs' hashrate.
    let registry = args.home.registry();
    let router = Arc::new(
        Router::new(upstream.clone(), ledger.clone())
            .with_switch_mode(args.switch_mode)
            .with_home_pools(args.home.config()?)
            .with_suppliers(registry.clone())
            // A redeemer's pool sees the rig as <account>.<mint host>.
            .with_mint_hosts(hashu_proxy::router::MintHosts {
                operator: mint_host(&args),
                suppliers: supplier_base(&args, &registry)
                    .map(|base| supplier_domain(&args).unwrap_or(base)),
            }),
    );
    // The operator's own connected rigs, counted in capacity once the
    // operator sets a live horizon (operator GUI, "Live hashrate").
    {
        let r = router.clone();
        capacity.set_live_source(Arc::new(move || r.operator_hashrate() / 1e12));
    }
    // When waiting redemptions should get a rig, for the wallet.
    let _ = checkout.router.set(router.clone());
    // Whether each mint's rigs are down, for pausing its redemptions.
    {
        let r = router.clone();
        rigs.set(Arc::new(move |mint: Option<&str>| r.down_since(mint)));
    }
    // Each rig's work in the hashrate samples, for the dashboard's
    // stacked per-rig chart.
    {
        let r = router.clone();
        meter.set_rig_source(Arc::new(move || r.rig_work()));
    }
    // Restore the dashboard's live feed before the first sample, then
    // save it as it goes.
    let telemetry = match args.ops_database_url.trim() {
        "" => None,
        url => Some(
            crate::telemetry::Telemetry::open(
                url,
                meter.clone(),
                metrics.clone(),
                router.clone(),
                Duration::from_secs(30),
            )
            .await,
        ),
    };
    tokio::spawn(meter.clone().run(Duration::from_secs(1)));
    if let Some(t) = &telemetry {
        tokio::spawn(t.clone().run());
    }
    // The hourly reliability record of every mint served here (#109).
    let reliability = {
        let r = router.clone();
        let hashrate: crate::reliability::HashrateFn = Arc::new(move |mint: &str| {
            if mint.is_empty() {
                r.operator_hashrate()
            } else {
                r.supplier_hashrate(mint)
            }
        });
        let suppliers = supplier_base(&args, &registry).map(|base| {
            let reg = registry.clone();
            crate::reliability::SupplierMints {
                base: supplier_domain(&args).unwrap_or(base),
                dir: args.data_dir.join("mints"),
                names: Arc::new(move || reg.names()),
            }
        });
        let url = args
            .mint_url
            .clone()
            .unwrap_or_else(|| internal_url.clone());
        let rel = crate::reliability::Reliability::open(
            &args.data_dir,
            &url,
            hashu_mint::reliability::PublishConfig::parse(&args.nostr_relays, &args.ots_calendars),
            ledger.clone(),
            hashrate,
            suppliers,
        )
        .context("open reliability record")?;
        rel.set_keysets("", tih_keyset_ids(&mint));
        // Announce on Nostr only under a public URL (#116).
        if let Some(public) = args.mint_url.as_deref().filter(|u| !u.trim().is_empty()) {
            match mint.mint_info().await {
                Ok(info) => rel.set_announcement(
                    "",
                    hashu_mint::discovery::Announcement::from_info(
                        &info,
                        public.trim(),
                        nostr_network(fake_lightning),
                    ),
                ),
                Err(e) => tracing::warn!(error = %e, "no NIP-87 announcement: mint info"),
            }
        }
        rel
    };
    // With epochs on, one chain-tip reader dates every mint's issuance.
    let clock = args.epochs.then(|| {
        crate::epochs::chain_clock(esplora::EsploraClient::with_base(
            args.epoch_esplora.trim_end_matches('/').to_string(),
        ))
    });
    // Tips on the operator's Nostr profile (zaps support development).
    let tip_routes = {
        let (tip, routes) = crate::tips::setup(
            &args.tips,
            phoenixd,
            args.mint_url.as_deref(),
            &args.data_dir,
            hashu_mint::reliability::PublishConfig::parse(&args.nostr_relays, "").relays,
        )
        .context("tips")?;
        reliability.set_tip("", tip);
        reliability.set_profile_about("", Some(args.nostr_about.clone()));
        routes
    };
    // Registered suppliers' mints, on <name>.<supplier domain>.
    let tenants = supplier_mints(
        &args,
        &stratum,
        &registry,
        &ledger,
        fake_lightning,
        &router,
        &reliability,
        clock.clone(),
    );
    if let Some(t) = &tenants {
        t.start_all().await;
    }

    let mut router_methods = methods.clone();
    let mut planner: Option<Arc<crate::planner::Planner>> = None;
    if let Some(e) = &epochs {
        router_methods.push(EPOCH.to_string());
        e.set_plan_inputs(crate::epochs::plan_inputs(capacity.clone()));
        crate::epochs::set_operator(e.clone());
        let p = Arc::new(crate::planner::Planner {
            plan: Arc::new(hashu_mint::planner::HashratePlan::open(Some(
                args.data_dir.join("hashrate-plan.json"),
            ))?),
            epochs: e.clone(),
            measured: crate::planner::measured_from(capacity.clone()),
            record: Some(
                reliability
                    .store("")
                    .context("operator reliability record")?,
            ),
        });
        p.start();
        planner = Some(p);
        let on_rotate: Arc<dyn Fn() + Send + Sync> = {
            let (rel, mint) = (reliability.clone(), mint.clone());
            Arc::new(move || rel.set_keysets("", tih_keyset_ids(&mint)))
        };
        let on_tick: Arc<dyn Fn(hashu_core::epoch::EpochClock) + Send + Sync> = {
            let capacity = capacity.clone();
            Arc::new(move |c| capacity.set_epoch(c.current(), c.hours_left()))
        };
        tokio::spawn(crate::epochs::run(
            e.clone(),
            mint.clone(),
            db.clone(),
            clock.clone().expect("epochs on"),
            on_tick,
            on_rotate,
        ));
        tracing::info!(esplora = %args.epoch_esplora, "epochs on: vouchers dated to difficulty epochs");
    }
    let mut app = cdk_axum::create_mint_router(mint.clone(), router_methods)
        .await
        .context("build mint HTTP router")?
        .merge(checkout::router(checkout.clone()))
        .merge(crate::reliability::router(reliability.clone(), ""))
        .merge(tip_routes.unwrap_or_default())
        .merge(landing::router());
    if let Some(e) = &epochs {
        app = app
            .merge(crate::epochs::router(e.clone(), mint.clone()))
            .merge(
                planner
                    .clone()
                    .map(crate::planner::public_router)
                    .unwrap_or_default(),
            )
            .layer(axum::middleware::from_fn_with_state(
                e.clone(),
                crate::epochs::guard,
            ))
            .layer(axum::middleware::from_fn_with_state(
                e.clone(),
                crate::epochs::list_future_keysets,
            ));
    }
    if let Some(w) = &waitlist {
        let domain = supplier_domain(&args).or_else(|| mint_host(&args));
        app = app.merge(waitlist::router(w.clone(), domain.as_deref()));
    }
    if let Some(t) = &tenants {
        app = app.merge(tenants::operator_router(t.clone())).layer(
            axum::middleware::from_fn_with_state(t.clone(), tenants::dispatch),
        );
    }
    let http = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind mint API on {}", args.listen))?;
    let http_task = tokio::spawn(async move { axum::serve(http, app).await });

    // ---- proxy --------------------------------------------------------
    let reliability_recorder = reliability.recorder();
    let crediter = Arc::new(
        ShareCrediter::with_audit(ledger.clone(), audit.clone())
            .with_reliability(reliability_recorder.clone()),
    );
    tokio::spawn(reliability.clone().run());
    tokio::spawn(reliability.clone().run_inbox());
    // Free settled (stopped early) redemptions' share trees; completed
    // ones are freed as they complete.
    {
        let crediter = crediter.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(600));
            loop {
                tick.tick().await;
                let n = crediter.evict_finished();
                if n > 0 {
                    tracing::debug!(evicted = n, "freed finished share audit trees");
                }
            }
        });
    }
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
        let auth = match args
            .operator_password_hash
            .as_deref()
            .filter(|h| !h.trim().is_empty())
        {
            Some(hash) => {
                crate::operator_auth::OperatorAuth::with_password_hash(&args.operator_user, hash)
                    .context("--operator-password-hash")?
            }
            None => {
                anyhow::ensure!(
                    bind.ip().is_loopback() || args.control_allow_open,
                    "--control-bind {bind} is reachable from other machines but has no \
                     operator login: set HASHU_OPERATOR_PASSWORD_HASH (caddy hash-password)"
                );
                crate::operator_auth::OperatorAuth::open()
            }
        };
        let control = ControlState {
            ledger: ledger.clone(),
            metrics: metrics.clone(),
            default_upstream: upstream.clone(),
            audit,
            allow_open: args.control_allow_open,
            router: Some(router.clone()),
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
                stratum: Some(stratum.clone()),
                rails: rails.clone(),
                min_redeem_tih: args.min_redeem_tih,
                max_tih: args.max_tih,
            },
            started_ms,
            checkout: checkout.clone(),
            waitlist: waitlist.clone(),
            mrr: mrr_sync.clone(),
            auth: Arc::new(auth),
            planner: planner.clone(),
            reliability: Some(reliability.clone()),
            supply: Some(supply.clone()),
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
    let proxy = run_routed_listener_on(
        stratum_listener,
        router.clone(),
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
        _ = shutdown_signal() => {
            tracing::info!("shutting down");
            router.begin_shutdown();
            Ok(())
        }
    };
    // Connections dropped from here on are this daemon stopping, not rigs
    // disconnecting: behind the relay their miners stay connected.
    router.begin_shutdown();
    // Save what is otherwise written on a timer, so a deploy loses nothing.
    if let Some(t) = &telemetry {
        match tokio::time::timeout(Duration::from_secs(5), t.flush()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %format!("{e:#}"), "could not save the dashboard feed")
            }
            Err(_) => tracing::warn!("saving the dashboard feed timed out"),
        }
    }
    if let Err(e) = reliability_recorder.save() {
        tracing::warn!(error = %e, "could not save reliability buckets");
    }
    capacity.save_now();
    if let Err(e) = mint.stop().await {
        tracing::warn!(error = %e, "mint stop");
    }
    if let Some(t) = &tenants {
        t.stop_all().await;
    }
    result
}

/// Ctrl-C, or SIGTERM: what `docker stop` (and every deploy) sends. As
/// PID 1 in the container an unhandled SIGTERM is ignored, so Docker
/// waits 10 s and kills the daemon without saving.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
                return;
            }
            Err(e) => tracing::warn!(error = %e, "cannot listen for SIGTERM"),
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

/// Supplier mints, when a supplier registry is configured and this mint
/// has a public URL to name them under.
#[allow(clippy::too_many_arguments)]
fn supplier_mints(
    args: &RunArgs,
    stratum: &crate::stratum_addr::StratumAddr,
    registry: &Arc<hashu_proxy::suppliers::SupplierRegistry>,
    ledger: &Arc<RedemptionLedger>,
    fake_lightning: bool,
    router: &Arc<Router>,
    reliability: &Arc<crate::reliability::Reliability>,
    epochs: Option<crate::epochs::ClockFeed>,
) -> Option<Arc<Tenants>> {
    if !registry.path_configured() {
        return None;
    }
    let Some(base) = supplier_base(args, registry) else {
        tracing::warn!(
            "HASHU_SUPPLIERS_FILE is set but HASHU_MINT_URL is not a URL: supplier mints \
             (<name>.<supplier domain>) are off"
        );
        return None;
    };
    let domain = supplier_domain(args);
    tracing::info!(
        host = %format!("*.{}", domain.as_deref().unwrap_or(&base)),
        "serving supplier mints"
    );
    let r = router.clone();
    Some(Tenants::new(
        TenantConfig {
            base,
            domain,
            dir: args.data_dir.join("mints"),
            ledger: ledger.clone(),
            min_redeem_tih: args.min_redeem_tih,
            max_tih: args.max_tih,
            encrypted_pools_only: args.encrypted_pools_only,
            fake_lightning,
            hashrate: Arc::new(move |name: &str| r.supplier_hashrate(name)),
            stratum: stratum.clone(),
            probe: tenants::lnurl_probe(),
            reliability: Some(reliability.clone()),
            epochs,
            rigs: Some({
                let d = hashu_core::availability::RigsDown::new();
                let r = router.clone();
                d.set(Arc::new(move |mint: Option<&str>| r.down_since(mint)));
                d
            }),
        },
        registry.clone(),
    ))
}

/// Host supplier mints are named under (`<name>.<base>`), when a supplier
/// registry is configured and the mint has a public URL.
fn supplier_base(
    args: &RunArgs,
    registry: &Arc<hashu_proxy::suppliers::SupplierRegistry>,
) -> Option<String> {
    if !registry.path_configured() {
        return None;
    }
    mint_host(args)
}

/// Host of `--mint-url`, lowercased.
fn mint_host(args: &RunArgs) -> Option<String> {
    args.mint_url
        .as_deref()
        .and_then(|u| u.trim().parse::<axum::http::Uri>().ok())
        .and_then(|u| {
            u.host()
                .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
        })
}

/// `--supplier-domain`, lowercased, without a trailing dot; `None` when
/// unset or empty (compose passes an empty value).
fn supplier_domain(args: &RunArgs) -> Option<String> {
    args.supplier_domain
        .as_deref()
        .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
}

/// Ids of a mint's TiH keysets (active and retired), for the reliability
/// key document.
pub(crate) fn tih_keyset_ids(mint: &cdk::mint::Mint) -> Vec<String> {
    let unit = CurrencyUnit::Custom("tih".into());
    let mut ids: Vec<String> = mint
        .keysets()
        .keysets
        .into_iter()
        .filter(|k| k.unit == unit)
        .map(|k| k.id.to_string())
        .collect();
    ids.sort();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_price_round_trips_to_msat_per_pih() {
        let oracle: Arc<dyn HashpriceOracle + Send + Sync> =
            Arc::new(fixed_from_msat_per_pih(500.0).unwrap());
        let p = sats_pricer(&oracle, Some(0));
        // 10 PiH at 500 msat = 5 sats.
        assert_eq!(p.price(10 * 1024).unwrap(), 5);
        assert!((p.msat_per_pih_now().unwrap() - 500.0).abs() < 1e-9);
    }

    #[test]
    fn fallback_covers_an_empty_live_oracle() {
        let o = WithFallback {
            live: mrr::MrrOracle::new(4, Duration::from_secs(60)),
            fallback: Some(fixed_from_msat_per_pih(36.0).unwrap()),
        };
        let per_ths = o.sample_at(SystemTime::now()).unwrap();
        assert!((msat_per_pih_from_sats_per_ths(per_ths) - 36.0).abs() < 1e-9);
    }

    #[test]
    fn denominations_are_powers_of_two() {
        assert_eq!(DENOMS[0], 1);
        assert_eq!(DENOMS[40], 1 << 40);
    }

    #[test]
    fn nostr_contact_is_added_once() {
        use nostr::ToBech32;
        let keys = nostr::Keys::generate();
        let npub = keys.public_key().to_bech32().unwrap();
        let mut c = vec![cdk::nuts::ContactInfo::new("email".into(), "a@b.c".into())];
        add_nostr_contact(&mut c, &keys).unwrap();
        add_nostr_contact(&mut c, &keys).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].method, "nostr");
        assert_eq!(c[1].info, npub);
    }

    #[test]
    fn rejects_non_positive_fixed_price() {
        assert!(fixed_from_msat_per_pih(0.0).is_err());
        assert!(fixed_from_msat_per_pih(-1.0).is_err());
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

/// `--app-url`: an http(s) URL without a trailing slash, or none when unset
/// or empty (compose passes an empty value when there is no wallet).
fn app_url(raw: Option<&str>) -> Result<Option<String>> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let host = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or("");
    let host = host.split('/').next().unwrap_or("");
    if host.is_empty() || host.contains(char::is_whitespace) {
        bail!("--app-url must be an http(s) URL like https://app.example.com (got {raw})");
    }
    Ok(Some(raw.trim_end_matches('/').to_string()))
}

/// `--seller-name` and `--terms-url`: the seller shown at checkout. Empty
/// values count as unset; terms without a seller name are refused.
fn seller(name: Option<&str>, terms: Option<&str>) -> Result<Option<crate::checkout::Seller>> {
    let name = name.map(str::trim).filter(|s| !s.is_empty());
    let terms = terms.map(str::trim).filter(|s| !s.is_empty());
    let terms_url = match terms {
        None => None,
        Some(t) => {
            let rest = t
                .strip_prefix("https://")
                .or_else(|| t.strip_prefix("http://"))
                .unwrap_or("");
            if rest.split('/').next().unwrap_or("").is_empty() || t.contains(char::is_whitespace) {
                bail!(
                    "--terms-url must be an http(s) URL like https://example.com/terms (got {t})"
                );
            }
            Some(t.to_string())
        }
    };
    match (name, terms_url) {
        (None, None) => Ok(None),
        (None, Some(_)) => {
            bail!("--terms-url needs --seller-name: the checkout names who the terms are with")
        }
        (Some(n), terms_url) => Ok(Some(crate::checkout::Seller {
            name: n.to_string(),
            terms_url,
        })),
    }
}

#[cfg(test)]
mod seller_tests {
    use super::seller;

    #[test]
    fn seller_is_optional_and_checked() {
        assert_eq!(seller(None, None).unwrap(), None);
        assert_eq!(seller(Some(" "), Some("")).unwrap(), None);
        let s = seller(
            Some("Example Mining, LLC"),
            Some("https://example.com/terms"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(s.name, "Example Mining, LLC");
        assert_eq!(s.terms_url.as_deref(), Some("https://example.com/terms"));
        assert_eq!(
            seller(Some("Example"), None).unwrap().unwrap().terms_url,
            None
        );
        assert!(seller(None, Some("https://example.com/terms")).is_err());
        assert!(seller(Some("Example"), Some("example.com/terms")).is_err());
    }
}

#[cfg(test)]
mod app_url_tests {
    use super::app_url;

    #[test]
    fn app_url_is_optional_and_checked() {
        assert_eq!(app_url(None).unwrap(), None);
        assert_eq!(app_url(Some("")).unwrap(), None);
        assert_eq!(
            app_url(Some("https://app.example.com/"))
                .unwrap()
                .as_deref(),
            Some("https://app.example.com")
        );
        assert!(app_url(Some("app.example.com")).is_err());
        assert!(app_url(Some("https://")).is_err());
    }
}

#[cfg(test)]
mod icon_url_tests {
    use super::mint_icon_url;

    #[test]
    fn defaults_to_the_mark_at_the_mint_url() {
        assert_eq!(
            mint_icon_url(None, Some("https://mint.example/")).as_deref(),
            Some("https://mint.example/nostr/avatar.png")
        );
        assert_eq!(mint_icon_url(None, None), None);
        assert_eq!(mint_icon_url(None, Some(" ")), None);
    }

    #[test]
    fn configured_wins_and_empty_means_none() {
        assert_eq!(
            mint_icon_url(
                Some("https://cdn.example/logo.png"),
                Some("https://mint.example")
            )
            .as_deref(),
            Some("https://cdn.example/logo.png")
        );
        assert_eq!(mint_icon_url(Some(""), Some("https://mint.example")), None);
    }
}
