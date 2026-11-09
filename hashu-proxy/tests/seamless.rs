//! Seamless pool switching: a redemption moves a live miner connection
//! to the redeemer's pool (and back) without the miner reconnecting.
//!
//! Topology (all on 127.0.0.1):
//!     fake miner ⇄ routed proxy ⇄ { default pool | redeemer pool }
//!
//! The fake pools have different extranonce1s, so every share checks the
//! extranonce translation: the coinbase the miner builds from the job it
//! was sent must equal the one the pool rebuilds from the submit.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hashu_core::redemption::{Redemption, RedemptionId, RedemptionLedger, RedemptionState};
use hashu_proxy::credit::{CreditContext, ShareCrediter};
use hashu_proxy::router::Router;
use hashu_proxy::stratum::run_routed_listener;
use hashu_proxy::ProxyMetrics;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(5);
const H: &str = "1111111111111111111111111111111111111111111111111111111111111111";

#[derive(Clone)]
struct PoolCfg {
    name: &'static str,
    en1: &'static str,
    en2_size: usize,
    difficulty: f64,
    job: &'static str,
    coinb1: &'static str,
    coinb2: &'static str,
    /// Version-rolling mask granted on `mining.configure`, if any.
    mask: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    Auth {
        pool: &'static str,
        worker: String,
    },
    Submit {
        pool: &'static str,
        worker: String,
        job: String,
        /// Coinbase the pool rebuilt from the submit.
        coinbase: Vec<u8>,
    },
}

/// A pool that answers configure/subscribe/authorize, sends its
/// difficulty and one job after authorize, and accepts every submit whose
/// extranonce2 is the size it asked for.
async fn fake_pool(cfg: PoolCfg, seen: mpsc::UnboundedSender<Seen>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (sock, _) = l.accept().await.unwrap();
            let seen = seen.clone();
            let cfg = cfg.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut br = BufReader::new(r);
                let mut line = String::new();
                while br.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let v: Value = serde_json::from_str(line.trim()).unwrap();
                    line.clear();
                    let id = v["id"].clone();
                    let method = v["method"].as_str().unwrap_or_default();
                    let mut out = Vec::new();
                    match method {
                        "mining.configure" => out.push(match cfg.mask {
                            Some(m) => json!({"id": id, "result": {"version-rolling": true, "version-rolling.mask": m}, "error": null}),
                            None => json!({"id": id, "result": {"version-rolling": false}, "error": null}),
                        }),
                        "mining.subscribe" => out.push(json!({"id": id, "result": [[["mining.notify", "s"]], cfg.en1, cfg.en2_size], "error": null})),
                        "mining.extranonce.subscribe" => out.push(json!({"id": id, "result": true, "error": null})),
                        "mining.authorize" => {
                            let worker = v["params"][0].as_str().unwrap().to_string();
                            let _ = seen.send(Seen::Auth { pool: cfg.name, worker });
                            out.push(json!({"id": id, "result": true, "error": null}));
                            out.push(json!({"id": null, "method": "mining.set_difficulty", "params": [cfg.difficulty]}));
                            out.push(json!({"id": null, "method": "mining.notify", "params": [
                                cfg.job, H, cfg.coinb1, cfg.coinb2, [], "20000000", "1d00ffff", "60000000", false
                            ]}));
                        }
                        "mining.submit" => {
                            let p = &v["params"];
                            let en2 = hex::decode(p[2].as_str().unwrap()).unwrap();
                            let ok = en2.len() == cfg.en2_size;
                            let coinbase = if ok {
                                [
                                    hex::decode(cfg.coinb1).unwrap(),
                                    hex::decode(cfg.en1).unwrap(),
                                    en2,
                                    hex::decode(cfg.coinb2).unwrap(),
                                ]
                                .concat()
                            } else {
                                Vec::new()
                            };
                            let _ = seen.send(Seen::Submit {
                                pool: cfg.name,
                                worker: p[0].as_str().unwrap().to_string(),
                                job: p[1].as_str().unwrap().to_string(),
                                coinbase,
                            });
                            out.push(if ok {
                                json!({"id": id, "result": true, "error": null})
                            } else {
                                json!({"id": id, "result": false, "error": [20, "bad extranonce2 size", null]})
                            });
                        }
                        _ => {}
                    }
                    for o in out {
                        if w.write_all(format!("{o}\n").as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

struct Miner {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
    en1: Vec<u8>,
    en2_size: usize,
    job: String,
    coinb1: Vec<u8>,
    coinb2: Vec<u8>,
    difficulty: f64,
    next_id: u64,
}

impl Miner {
    /// Connect, (configure,) subscribe, (extranonce.subscribe,) authorize
    /// as `miner.1`, and read until the first job.
    async fn connect(proxy: &str, mask: Option<&str>, extranonce_subscribe: bool) -> Self {
        Self::connect_as(proxy, mask, extranonce_subscribe, "miner.1", "pw").await
    }

    /// Like [`Miner::connect`], authorizing as `user` / `password`.
    async fn connect_as(
        proxy: &str,
        mask: Option<&str>,
        extranonce_subscribe: bool,
        user: &str,
        password: &str,
    ) -> Self {
        let sock = TcpStream::connect(proxy).await.unwrap();
        let (r, w) = sock.into_split();
        let mut m = Miner {
            r: BufReader::new(r),
            w,
            en1: vec![],
            en2_size: 0,
            job: String::new(),
            coinb1: vec![],
            coinb2: vec![],
            difficulty: 1.0,
            next_id: 100,
        };
        if let Some(mask) = mask {
            m.send(json!({"id": 1, "method": "mining.configure", "params": [["version-rolling"], {"version-rolling.mask": mask}]}))
                .await;
        }
        m.send(json!({"id": 2, "method": "mining.subscribe", "params": ["test/1.0"]}))
            .await;
        if extranonce_subscribe {
            m.send(json!({"id": 3, "method": "mining.extranonce.subscribe", "params": []}))
                .await;
        }
        m.send(json!({"id": 4, "method": "mining.authorize", "params": [user, password]}))
            .await;
        while m.job.is_empty() {
            m.next().await.expect("pool reply");
        }
        m
    }

    async fn send(&mut self, v: Value) {
        self.w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
    }

    /// Read one line, apply it to the miner's state, and return it.
    async fn next(&mut self) -> Option<Value> {
        let mut s = String::new();
        let n = timeout(T, self.r.read_line(&mut s))
            .await
            .expect("timed out reading from proxy")
            .unwrap_or(0);
        if n == 0 {
            return None;
        }
        let v: Value = serde_json::from_str(s.trim()).unwrap();
        match v["method"].as_str() {
            Some("mining.notify") => {
                let p = &v["params"];
                self.job = p[0].as_str().unwrap().into();
                self.coinb1 = hex::decode(p[2].as_str().unwrap()).unwrap();
                self.coinb2 = hex::decode(p[3].as_str().unwrap()).unwrap();
            }
            Some("mining.set_difficulty") => self.difficulty = v["params"][0].as_f64().unwrap(),
            Some("mining.set_extranonce") => {
                self.en1 = hex::decode(v["params"][0].as_str().unwrap()).unwrap();
                self.en2_size = v["params"][1].as_u64().unwrap() as usize;
            }
            Some(_) => {}
            None => {
                if v["id"] == 2 {
                    self.en1 = hex::decode(v["result"][1].as_str().unwrap()).unwrap();
                    self.en2_size = v["result"][2].as_u64().unwrap() as usize;
                }
            }
        }
        Some(v)
    }

    /// Read until a job with `job_id` arrives; returns that notify.
    async fn until_job(&mut self, job_id: &str) -> Value {
        loop {
            let v = self.next().await.expect("connection closed");
            if v["method"] == "mining.notify" && v["params"][0] == job_id {
                return v;
            }
        }
    }

    /// Submit a share on `job` (default: the current one). Returns the
    /// coinbase this miner built for it.
    async fn submit_on(&mut self, job: Option<&str>) -> Vec<u8> {
        self.next_id += 1;
        let id = self.next_id;
        let en2 = vec![0x5a; self.en2_size];
        let job = job.map(str::to_string).unwrap_or_else(|| self.job.clone());
        self.send(json!({"id": id, "method": "mining.submit", "params": [
            "miner.1", job, hex::encode(&en2), "60000000", format!("{id:08x}")
        ]}))
        .await;
        [
            self.coinb1.clone(),
            self.en1.clone(),
            en2,
            self.coinb2.clone(),
        ]
        .concat()
    }

    /// Read until the response to the last submit.
    async fn submit_result(&mut self) -> Value {
        loop {
            let v = self.next().await.expect("connection closed");
            if v["id"] == self.next_id {
                return v["result"].clone();
            }
        }
    }

    async fn wait_closed(&mut self) {
        while self.next().await.is_some() {}
    }
}

async fn spawn_proxy(
    router: Arc<Router>,
    credit: Option<CreditContext>,
) -> (String, Arc<ProxyMetrics>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let metrics = Arc::new(ProxyMetrics::default());
    tokio::spawn(run_routed_listener(
        addr,
        router,
        metrics.clone(),
        credit,
        Duration::from_millis(20),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    (addr.to_string(), metrics)
}

fn default_cfg() -> PoolCfg {
    PoolCfg {
        name: "default",
        en1: "d0d0d0d0",
        en2_size: 8,
        difficulty: 1.0,
        job: "dj1",
        coinb1: "01aa",
        coinb2: "02bb",
        mask: Some("1fffe000"),
    }
}

fn redeemer_cfg() -> PoolCfg {
    PoolCfg {
        name: "redeemer",
        en1: "e1e2e3e4e5",
        en2_size: 8,
        // 1 THH is 3.6e15 hashes; at 2^19 a share is 2^51 ≈ 2.25e15, so
        // the redemption fills on the second share.
        difficulty: (1u64 << 19) as f64,
        job: "rj1",
        coinb1: "03cc",
        coinb2: "04dd",
        mask: Some("1fffe000"),
    }
}

struct Setup {
    proxy: String,
    metrics: Arc<ProxyMetrics>,
    ledger: Arc<RedemptionLedger>,
    redeemer: String,
    seen: mpsc::UnboundedReceiver<Seen>,
}

async fn setup(redeemer: PoolCfg) -> Setup {
    let (tx, seen) = mpsc::unbounded_channel();
    let default_pool = fake_pool(default_cfg(), tx.clone()).await;
    let redeemer = fake_pool(redeemer, tx).await;
    let ledger = Arc::new(RedemptionLedger::new());
    let router = Arc::new(Router::new(default_pool, ledger.clone()));
    let credit = CreditContext::new(Arc::new(ShareCrediter::new(ledger.clone())), router.clone());
    let (proxy, metrics) = spawn_proxy(router, Some(credit)).await;
    Setup {
        proxy,
        metrics,
        ledger,
        redeemer,
        seen,
    }
}

fn open_redemption(s: &Setup, pool: &str) -> RedemptionId {
    let id = RedemptionId::from_bytes([7; 16]);
    s.ledger
        .open(Redemption::new(
            id,
            1,
            format!("stratum+tcp://{pool}"),
            "alice.rig".into(),
        ))
        .unwrap();
    id
}

fn auth(pool: &'static str, worker: &str) -> Seen {
    Seen::Auth {
        pool,
        worker: worker.into(),
    }
}

async fn recv(seen: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
    timeout(T, seen.recv()).await.expect("pool event").unwrap()
}

/// Expect the pool to see a submit from `worker` on `pool` whose
/// coinbase is `coinbase`.
async fn expect_submit(
    seen: &mut mpsc::UnboundedReceiver<Seen>,
    pool: &'static str,
    worker: &str,
    coinbase: &[u8],
) {
    match recv(seen).await {
        Seen::Submit {
            pool: p,
            worker: w,
            coinbase: c,
            ..
        } => {
            assert_eq!((p, w.as_str()), (pool, worker));
            assert_eq!(hex::encode(c), hex::encode(coinbase), "coinbase mismatch");
        }
        other => panic!("expected a submit, got {other:?}"),
    }
}

#[tokio::test]
async fn redemption_switches_pool_without_reconnecting() {
    let mut s = setup(redeemer_cfg()).await;

    let mut m = Miner::connect(&s.proxy, Some("1fffe000"), false).await;
    assert_eq!(recv(&mut s.seen).await, auth("default", "miner.1"));
    // The miner gets a small proxy-owned extranonce, not the pool's.
    assert_eq!((m.en1.len(), m.en2_size), (2, 4));

    let cb = m.submit_on(None).await;
    expect_submit(&mut s.seen, "default", "miner.1", &cb).await;
    assert_eq!(m.submit_result().await, true);

    // A melt opens a redemption. The same connection gets the redeemer's
    // difficulty and a clean job.
    let t0 = Instant::now();
    let id = open_redemption(&s, &s.redeemer.clone());
    let notify = m.until_job("rj1").await;
    let switch_ms = t0.elapsed().as_millis();
    eprintln!("switch to redeemer took {switch_ms} ms");
    assert!(switch_ms < 1000, "switch took {switch_ms} ms");
    assert_eq!(
        notify["params"][8], true,
        "first job on the new pool is clean"
    );
    assert_eq!(m.difficulty, (1u64 << 19) as f64);
    assert_eq!(recv(&mut s.seen).await, auth("redeemer", "alice.rig"));

    let cb = m.submit_on(None).await;
    expect_submit(&mut s.seen, "redeemer", "alice.rig", &cb).await;
    assert_eq!(m.submit_result().await, true);
    assert_eq!(s.ledger.get(&id).unwrap().state, RedemptionState::Active);

    // The second share fills it; the miner is switched back in place.
    let cb = m.submit_on(None).await;
    expect_submit(&mut s.seen, "redeemer", "alice.rig", &cb).await;
    assert_eq!(m.submit_result().await, true);
    m.until_job("dj1").await;
    assert_eq!(s.ledger.get(&id).unwrap().state, RedemptionState::Completed);
    assert_eq!(recv(&mut s.seen).await, auth("default", "miner.1"));
    assert_eq!(m.difficulty, 1.0);

    let cb = m.submit_on(None).await;
    expect_submit(&mut s.seen, "default", "miner.1", &cb).await;
    assert_eq!(m.submit_result().await, true);

    // One miner connection the whole time.
    assert_eq!(s.metrics.connections_accepted.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn late_share_on_old_job_goes_to_old_pool() {
    let mut s = setup(redeemer_cfg()).await;
    let mut m = Miner::connect(&s.proxy, None, false).await;
    assert_eq!(recv(&mut s.seen).await, auth("default", "miner.1"));
    // Coinbase the miner would build for the default pool's job.
    let old = (m.coinb1.clone(), m.coinb2.clone());

    let id = open_redemption(&s, &s.redeemer.clone());
    m.until_job("rj1").await;
    assert_eq!(recv(&mut s.seen).await, auth("redeemer", "alice.rig"));

    // A share the miner found on the default pool's job just before the
    // switch still reaches the default pool, under the miner's worker.
    let new = (m.coinb1.clone(), m.coinb2.clone());
    (m.coinb1, m.coinb2) = old;
    let cb = m.submit_on(Some("dj1")).await;
    (m.coinb1, m.coinb2) = new;
    expect_submit(&mut s.seen, "default", "miner.1", &cb).await;
    assert_eq!(m.submit_result().await, true);
    // ...and credits nothing to the redemption.
    assert_eq!(s.ledger.get(&id).unwrap().accumulated_hashes, 0);
}

#[tokio::test]
async fn small_extranonce_pool_falls_back_to_reconnect() {
    let mut cfg = redeemer_cfg();
    cfg.en2_size = 4; // can't hold the miner's 2 + 4 bytes
    let mut s = setup(cfg).await;
    let mut m = Miner::connect(&s.proxy, None, false).await;
    assert_eq!(recv(&mut s.seen).await, auth("default", "miner.1"));

    open_redemption(&s, &s.redeemer.clone());
    // The proxy tried in place (the redeemer saw an authorize), found it
    // can't, and closed the miner so it reconnects.
    assert_eq!(recv(&mut s.seen).await, auth("redeemer", "alice.rig"));
    m.wait_closed().await;
    let mut m = Miner::connect(&s.proxy, None, false).await;
    assert_eq!(recv(&mut s.seen).await, auth("redeemer", "alice.rig"));
    let cb = m.submit_on(None).await;
    expect_submit(&mut s.seen, "redeemer", "alice.rig", &cb).await;
}

#[tokio::test]
async fn small_extranonce_pool_uses_set_extranonce_when_miner_supports_it() {
    let mut cfg = redeemer_cfg();
    cfg.en2_size = 4;
    let mut s = setup(cfg).await;
    let mut m = Miner::connect(&s.proxy, None, true).await;
    assert_eq!(recv(&mut s.seen).await, auth("default", "miner.1"));

    open_redemption(&s, &s.redeemer.clone());
    m.until_job("rj1").await;
    // The miner was told the redeemer's own extranonce.
    assert_eq!((hex::encode(&m.en1), m.en2_size), ("e1e2e3e4e5".into(), 4));
    assert_eq!(recv(&mut s.seen).await, auth("redeemer", "alice.rig"));
    let cb = m.submit_on(None).await;
    expect_submit(&mut s.seen, "redeemer", "alice.rig", &cb).await;
    assert_eq!(m.submit_result().await, true);
    assert_eq!(s.metrics.connections_accepted.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn ungranted_version_rolling_falls_back_to_reconnect() {
    let mut cfg = redeemer_cfg();
    cfg.mask = None;
    let mut s = setup(cfg).await;
    let mut m = Miner::connect(&s.proxy, Some("1fffe000"), false).await;
    assert_eq!(recv(&mut s.seen).await, auth("default", "miner.1"));
    open_redemption(&s, &s.redeemer.clone());
    assert_eq!(recv(&mut s.seen).await, auth("redeemer", "alice.rig"));
    m.wait_closed().await;
}

#[tokio::test]
async fn unreachable_redeemer_pool_keeps_miner_on_default() {
    let mut s = setup(redeemer_cfg()).await;
    let mut m = Miner::connect(&s.proxy, None, false).await;
    assert_eq!(recv(&mut s.seen).await, auth("default", "miner.1"));

    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap().to_string();
    drop(dead);
    let id = open_redemption(&s, &dead_addr);

    // Nothing reaches the miner and the connection stays up.
    let mut line = String::new();
    assert!(
        timeout(Duration::from_millis(300), m.r.read_line(&mut line))
            .await
            .is_err(),
        "unexpected line {line:?}"
    );
    let cb = m.submit_on(None).await;
    expect_submit(&mut s.seen, "default", "miner.1", &cb).await;
    assert_eq!(m.submit_result().await, true);
    assert_eq!(s.ledger.get(&id).unwrap().accumulated_hashes, 0);
}

/// A Bitaxe-style miner on the one public endpoint: its user is a Bitcoin
/// address and its password names its home pool. It idles there (never
/// on the operator's default pool), is borrowed for a redemption, goes
/// back home when the redemption fills, and its supplier is credited.
#[tokio::test]
async fn home_pool_miner_idles_at_home_and_is_borrowed_for_redemptions() {
    use hashu_proxy::home::HomeConfig;

    const ADDR: &str = "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq";
    let user = format!("{ADDR}.bitaxe");

    let (tx, mut seen) = mpsc::unbounded_channel();
    let default_pool = fake_pool(default_cfg(), tx.clone()).await;
    let home_pool = fake_pool(
        PoolCfg {
            name: "home",
            en1: "a0a1a2a3",
            job: "hj1",
            ..default_cfg()
        },
        tx.clone(),
    )
    .await;
    let redeemer = fake_pool(redeemer_cfg(), tx).await;
    let ledger = Arc::new(RedemptionLedger::new());
    let router = Arc::new(
        Router::new(default_pool, ledger.clone()).with_home_pools(HomeConfig {
            solo_upstream: None,
            allowed_hosts: vec!["127.0.0.1".into()],
        }),
    );
    let credit = CreditContext::new(Arc::new(ShareCrediter::new(ledger.clone())), router.clone());
    let (proxy, _metrics) = spawn_proxy(router.clone(), Some(credit)).await;

    let mut m = Miner::connect_as(
        &proxy,
        Some("1fffe000"),
        false,
        &user,
        &format!("d=4,pool=stratum+tcp://{home_pool}"),
    )
    .await;
    assert_eq!(recv(&mut seen).await, auth("home", &user));
    let cb = m.submit_on(None).await;
    expect_submit(&mut seen, "home", &user, &cb).await;
    assert_eq!(m.submit_result().await, true);

    let s = Setup {
        proxy,
        metrics: Arc::new(ProxyMetrics::default()),
        ledger: ledger.clone(),
        redeemer: redeemer.clone(),
        seen,
    };
    let id = open_redemption(&s, &redeemer);
    let mut seen = s.seen;
    m.until_job("rj1").await;
    assert_eq!(recv(&mut seen).await, auth("redeemer", "alice.rig"));
    for _ in 0..2 {
        let cb = m.submit_on(None).await;
        expect_submit(&mut seen, "redeemer", "alice.rig", &cb).await;
        assert_eq!(m.submit_result().await, true);
    }
    assert_eq!(ledger.get(&id).unwrap().state, RedemptionState::Completed);

    // Filled: back to the home pool, not the operator's default pool.
    m.until_job("hj1").await;
    assert_eq!(recv(&mut seen).await, auth("home", &user));
    let cb = m.submit_on(None).await;
    expect_submit(&mut seen, "home", &user, &cb).await;
    assert_eq!(m.submit_result().await, true);

    let sup = router.suppliers();
    assert_eq!(sup.len(), 1);
    assert_eq!(sup[0].supplier, ADDR);
    assert_eq!(sup[0].connections, 1);
    assert_eq!(sup[0].home_upstream.as_deref(), Some(home_pool.as_str()));
    // Two redeemer shares at 2^19 difficulty; home shares don't count.
    assert_eq!(sup[0].delivered_hashes, 2.0 * (1u64 << 51) as f64);
}

/// A miner that isn't a home miner (an account name, no `pool=`) still
/// goes to the operator's default pool when home pools are on.
#[tokio::test]
async fn account_miner_still_uses_the_default_pool_with_home_pools_on() {
    use hashu_proxy::home::HomeConfig;
    let (tx, mut seen) = mpsc::unbounded_channel();
    let default_pool = fake_pool(default_cfg(), tx).await;
    let ledger = Arc::new(RedemptionLedger::new());
    let router =
        Arc::new(Router::new(default_pool, ledger.clone()).with_home_pools(HomeConfig::default()));
    let (proxy, _metrics) = spawn_proxy(router, None).await;
    let mut m = Miner::connect(&proxy, None, false).await;
    assert_eq!(recv(&mut seen).await, auth("default", "miner.1"));
    let cb = m.submit_on(None).await;
    expect_submit(&mut seen, "default", "miner.1", &cb).await;
}
