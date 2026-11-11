//! Home pools: one public Stratum endpoint for small miners (Bitaxe,
//! NerdQaxe, other AxeOS / ESP-Miner devices).
//!
//! A home miner points at the proxy with the same settings it would use
//! for a solo pool, so the proxy can tell where its hashrate goes while
//! nobody is redeeming:
//!
//! | AxeOS field | Value                       | Home pool                     |
//! |-------------|-----------------------------|-------------------------------|
//! | User        | `bc1q….bitaxe1`             | the solo pool ([`HomeConfig::solo_upstream`], `solo.ckpool.org:3333` by default) |
//! | Password    | `x`                         |                               |
//! | Password    | `pool=public-pool.io:21496` | that pool, if its host is on [`HomeConfig::allowed_hosts`] |
//!
//! While a redemption needs hashrate, the router borrows the miner exactly
//! as it borrows the operator's rigs (worker name rewritten to the
//! redeemer's), then sends it back to its home pool, never to the
//! operator's default pool. A miner whose user is not a Bitcoin address
//! and gives no `pool=` has no home pool and is treated as before (the
//! operator's default pool, worker name passed through).
//!
//! Other password options (`d=512`, `x`) are kept and passed on; only the
//! `pool=` option is removed before the home pool sees the password.
//!
//! `pool=` only accepts hosts on an allow-list so a miner can't make the
//! proxy dial arbitrary addresses (such as services on its private
//! network).

use crate::upstream::normalize_upstream;

/// Hosts a miner may name with `pool=` by default: well-known public
/// pools that small miners use.
pub const DEFAULT_ALLOWED_HOSTS: &[&str] = &[
    "solo.ckpool.org",
    "eusolo.ckpool.org",
    "ausolo.ckpool.org",
    "public-pool.io",
    "stratum.braiins.com",
    "mine.ocean.xyz",
];

/// Default solo pool for miners whose user is a mainnet Bitcoin address.
pub const DEFAULT_SOLO_UPSTREAM: &str = "solo.ckpool.org:3333";

/// How home pools are chosen. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeConfig {
    /// Upstream (`host:port` or `tls://host:port`) for miners whose user
    /// is a mainnet Bitcoin address; `None` turns address detection off.
    pub solo_upstream: Option<String>,
    /// Hosts a miner may name with `pool=` (lowercase, no port). Empty
    /// turns `pool=` off.
    pub allowed_hosts: Vec<String>,
    /// Hashu's own worker account (`HASHU_OPERATOR_WORKER`, e.g. the
    /// operator's Braiins user). When set, only users equal to it or
    /// starting with `<it>.` are Hashu's rigs: they idle on the default
    /// pool and are the only rigs borrowed for redemptions. Every other
    /// miner idles on its home pool and is never borrowed, and a miner
    /// with no home pool is refused. `None` keeps the old behaviour:
    /// every rig may be borrowed and a miner with no home pool idles on
    /// the default pool.
    pub operator_worker: Option<String>,
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self {
            solo_upstream: Some(DEFAULT_SOLO_UPSTREAM.to_string()),
            allowed_hosts: DEFAULT_ALLOWED_HOSTS
                .iter()
                .map(|h| h.to_string())
                .collect(),
            operator_worker: None,
        }
    }
}

impl HomeConfig {
    /// No home pools: every miner goes to the operator's default pool.
    pub fn off() -> Self {
        Self {
            solo_upstream: None,
            allowed_hosts: Vec::new(),
            operator_worker: None,
        }
    }

    /// Whether `user` is one of Hashu's own rigs (see
    /// [`HomeConfig::operator_worker`]).
    pub fn is_operator(&self, user: &str) -> bool {
        let Some(op) = self.operator_worker.as_deref().filter(|o| !o.is_empty()) else {
            return false;
        };
        let user = user.trim();
        user == op
            || user
                .strip_prefix(op)
                .is_some_and(|rest| rest.starts_with('.'))
    }
}

/// Where a home miner's hashrate goes while nobody is redeeming.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomePool {
    /// Upstream to dial (see [`crate::upstream`]).
    pub upstream: String,
    /// The miner's own user, unchanged.
    pub worker_name: String,
    /// The miner's password without the `pool=` option (`x` if nothing
    /// is left).
    pub worker_password: String,
}

/// Who a miner is, from its `mining.authorize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinerIdentity {
    /// Supplier key: the user up to the first `.` (a Bitcoin address,
    /// or an account name). Work delivered to redemptions is tallied
    /// under this key.
    pub supplier: String,
    /// The miner's home pool, if it has one.
    pub home: Option<HomePool>,
    /// Why a `pool=` option was ignored, for the log.
    pub refused_pool: Option<String>,
    /// One of Hashu's own rigs ([`HomeConfig::operator_worker`]).
    pub operator: bool,
    /// The `pw=` option: the supplier secret this rig proves its name with
    /// (see [`crate::suppliers`]). Never passed to a pool.
    pub secret: Option<String>,
}

/// Why the proxy refuses a miner, sent back as its authorize error.
pub const REFUSE_NO_HOME: &str =
    "no home pool: use a Bitcoin address as the user, or add pool=host:port to the password";

impl MinerIdentity {
    /// Whether this miner may be borrowed to fill redemptions.
    pub fn borrowable(&self, cfg: &HomeConfig) -> bool {
        cfg.operator_worker.is_none() || self.operator
    }

    /// `Some(reason)` if the proxy should refuse this miner: Hashu's own
    /// rigs are set, this isn't one, and it has no home pool to go to.
    pub fn refusal(&self, cfg: &HomeConfig) -> Option<&'static str> {
        (cfg.operator_worker.is_some() && !self.operator && self.home.is_none())
            .then_some(REFUSE_NO_HOME)
    }
}

impl MinerIdentity {
    /// Read a miner's identity from its authorize `user` and `password`.
    pub fn from_authorize(user: &str, password: &str, cfg: &HomeConfig) -> Self {
        let user = user.trim();
        let supplier = user.split('.').next().unwrap_or_default().to_string();

        let mut pool: Option<&str> = None;
        let mut secret: Option<String> = None;
        let mut rest: Vec<&str> = Vec::new();
        for opt in password.split(',').map(str::trim).filter(|o| !o.is_empty()) {
            if let Some(p) = opt.strip_prefix("pool=") {
                pool = pool.or(Some(p));
            } else if let Some(pw) = opt.strip_prefix("pw=") {
                secret = secret.or_else(|| Some(pw.to_string()));
            } else {
                rest.push(opt);
            }
        }
        let worker_password = if rest.is_empty() {
            "x".to_string()
        } else {
            rest.join(",")
        };

        let mut refused_pool = None;
        let upstream = match pool {
            Some(p) => match allowed_upstream(p, cfg) {
                Ok(u) => Some(u),
                Err(why) => {
                    refused_pool = Some(why);
                    None
                }
            },
            None => None,
        }
        .or_else(|| {
            cfg.solo_upstream
                .clone()
                .filter(|_| is_mainnet_address(&supplier))
        });

        let operator = cfg.is_operator(user);
        Self {
            supplier,
            secret,
            operator,
            home: upstream.filter(|_| !operator).map(|upstream| HomePool {
                upstream,
                worker_name: user.to_string(),
                worker_password,
            }),
            refused_pool,
        }
    }

    /// Read a miner's identity from the `params` of its
    /// `mining.authorize` (`[user, password]`).
    pub fn from_params(params: &serde_json::Value, cfg: &HomeConfig) -> Option<Self> {
        let user = params.get(0)?.as_str()?;
        let password = params.get(1).and_then(|p| p.as_str()).unwrap_or("");
        Some(Self::from_authorize(user, password, cfg))
    }
}

/// `pool=` value → upstream, if its host is allowed.
fn allowed_upstream(pool: &str, cfg: &HomeConfig) -> Result<String, String> {
    let upstream =
        normalize_upstream(pool).ok_or_else(|| format!("pool={pool} is not host:port"))?;
    let hostport = upstream
        .strip_prefix(crate::upstream::TLS_PREFIX)
        .unwrap_or(&upstream);
    let host = hostport
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(hostport)
        .to_ascii_lowercase();
    if cfg
        .allowed_hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(&host))
    {
        Ok(upstream)
    } else {
        Err(format!("pool host {host} is not on the allow-list"))
    }
}

/// Whether `s` looks like a mainnet Bitcoin address: bech32 / bech32m
/// (`bc1…`) or base58 P2PKH / P2SH (`1…`, `3…`). Shape only, no checksum:
/// the solo pool checks it properly and refuses the authorize otherwise.
pub fn is_mainnet_address(s: &str) -> bool {
    const BECH32: &str = "qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    const BASE58: &str = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let lower = s.to_ascii_lowercase();
    if let Some(data) = lower.strip_prefix("bc1") {
        // Mixed case is invalid bech32.
        let one_case = s == lower || s == s.to_ascii_uppercase();
        return one_case
            && (39..=59).contains(&data.len())
            && data.chars().all(|c| BECH32.contains(c));
    }
    (s.starts_with('1') || s.starts_with('3'))
        && (26..=35).contains(&s.len())
        && s.chars().all(|c| BASE58.contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";

    #[test]
    fn recognises_mainnet_addresses() {
        assert!(is_mainnet_address(ADDR));
        assert!(is_mainnet_address(&ADDR.to_ascii_uppercase()));
        assert!(is_mainnet_address(
            "bc1p5d7rjq7g6rdk2yhzks9smlaqtedr4dekq08ge8ztwac72sfr9rusxg3297"
        ));
        assert!(is_mainnet_address("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN2"));
        assert!(is_mainnet_address("3J98t1WpEZ73CNmQviecrnyiWrnqRhWNLy"));
        assert!(!is_mainnet_address(
            "tb1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
        ));
        assert!(!is_mainnet_address("asher"));
        assert!(!is_mainnet_address("bc1qshort"));
        assert!(!is_mainnet_address(
            "bc1qAr0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq"
        ));
        assert!(!is_mainnet_address("1BvBMSEYstWetqTFn5Au4m4GFg7xJaNVN0"));
    }

    #[test]
    fn address_user_gets_the_solo_pool() {
        let id =
            MinerIdentity::from_authorize(&format!("{ADDR}.bitaxe1"), "x", &HomeConfig::default());
        assert_eq!(id.supplier, ADDR);
        let home = id.home.unwrap();
        assert_eq!(home.upstream, DEFAULT_SOLO_UPSTREAM);
        assert_eq!(home.worker_name, format!("{ADDR}.bitaxe1"));
        assert_eq!(home.worker_password, "x");
    }

    #[test]
    fn pool_option_picks_an_allowed_pool_and_is_stripped() {
        let id = MinerIdentity::from_authorize(
            &format!("{ADDR}.gamma"),
            "d=1024, pool=stratum+tcp://public-pool.io:21496",
            &HomeConfig::default(),
        );
        let home = id.home.unwrap();
        assert_eq!(home.upstream, "public-pool.io:21496");
        assert_eq!(home.worker_password, "d=1024");
        assert_eq!(id.refused_pool, None);
    }

    #[test]
    fn pool_option_on_an_account_name() {
        let id = MinerIdentity::from_authorize(
            "asher.bitaxe",
            "pool=stratum.braiins.com:3333",
            &HomeConfig::default(),
        );
        assert_eq!(id.supplier, "asher");
        assert_eq!(id.home.unwrap().upstream, "stratum.braiins.com:3333");
    }

    #[test]
    fn unknown_pool_host_is_refused() {
        for pool in ["pool=phoenixd:9740", "pool=10.0.0.5:3333", "pool=nope"] {
            let id = MinerIdentity::from_authorize(ADDR, pool, &HomeConfig::default());
            assert!(id.refused_pool.is_some(), "{pool}");
            // Still falls back to the solo pool for an address user.
            assert_eq!(id.home.unwrap().upstream, DEFAULT_SOLO_UPSTREAM);
        }
        let id =
            MinerIdentity::from_authorize("asher.1", "pool=10.0.0.5:3333", &HomeConfig::default());
        assert_eq!(id.home, None);
    }

    #[test]
    fn plain_account_has_no_home_pool() {
        let id = MinerIdentity::from_authorize("asherp.rig1", "x", &HomeConfig::default());
        assert_eq!(id.supplier, "asherp");
        assert_eq!(id.home, None);
    }

    #[test]
    fn off_config_never_assigns_a_home() {
        let id =
            MinerIdentity::from_authorize(ADDR, "pool=solo.ckpool.org:3333", &HomeConfig::off());
        assert_eq!(id.home, None);
    }

    #[test]
    fn operator_worker_marks_hashus_rigs() {
        let cfg = HomeConfig {
            operator_worker: Some("hashu".into()),
            ..HomeConfig::default()
        };
        for u in ["hashu", "hashu.mrr1"] {
            let id = MinerIdentity::from_authorize(u, "pool=public-pool.io:21496", &cfg);
            assert!(id.operator, "{u}");
            assert_eq!(id.home, None, "operator rigs idle on the default pool");
            assert!(id.borrowable(&cfg));
            assert_eq!(id.refusal(&cfg), None);
        }
        // Prefix must end at a dot.
        let id = MinerIdentity::from_authorize("hashuevil.1", "x", &cfg);
        assert!(!id.operator);
        assert_eq!(id.refusal(&cfg), Some(REFUSE_NO_HOME));
        // Home miners: kept at home, never borrowed, not refused.
        let id = MinerIdentity::from_authorize(ADDR, "x", &cfg);
        assert!(!id.borrowable(&cfg));
        assert_eq!(id.refusal(&cfg), None);
        // Without the setting nothing changes.
        let id = MinerIdentity::from_authorize("bob.rig1", "x", &HomeConfig::default());
        assert!(id.borrowable(&HomeConfig::default()));
        assert_eq!(id.refusal(&HomeConfig::default()), None);
    }

    #[test]
    fn pw_option_is_kept_out_of_the_pool_password() {
        let id = MinerIdentity::from_authorize(
            "alice.rig1",
            "pw=s3cret,d=512,pool=public-pool.io:21496",
            &HomeConfig::default(),
        );
        assert_eq!(id.secret.as_deref(), Some("s3cret"));
        assert_eq!(id.home.unwrap().worker_password, "d=512");
    }

    #[test]
    fn from_params_reads_authorize() {
        let p = serde_json::json!([ADDR, "x"]);
        let id = MinerIdentity::from_params(&p, &HomeConfig::default()).unwrap();
        assert!(id.home.is_some());
        assert!(
            MinerIdentity::from_params(&serde_json::json!([]), &HomeConfig::default()).is_none()
        );
    }
}
