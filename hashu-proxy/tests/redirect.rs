//! End-to-end redirect test: a melt opens a redemption, the proxy moves the
//! miner from the operator's default pool to the redeemer's pool (with the
//! redeemer's worker name), credits the shares accepted there, and moves
//! the miner back once the redemption is filled. These tests use
//! [`SwitchMode::Reconnect`]; `tests/seamless.rs` covers in-place switching.
//!
//! Topology (all on 127.0.0.1):
//!     fake miner ⇄ routed proxy ⇄ { default pool | redeemer pool }

use std::sync::Arc;
use std::time::Duration;

use hashu_core::redemption::{
    PoolTarget, Redemption, RedemptionId, RedemptionLedger, RedemptionState,
};
use hashu_proxy::credit::{CreditContext, ShareCrediter};
use hashu_proxy::router::{Router, SwitchMode};
use hashu_proxy::stratum::run_routed_listener;
use hashu_proxy::ProxyMetrics;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(5);
const HASH32_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// What a fake pool saw: `(pool, method, worker)` for authorize/submit.
type Seen = mpsc::UnboundedSender<(&'static str, String, String)>;

/// A pool that answers subscribe/authorize, then sends `difficulty` and
/// one job, and accepts every submit. Reports each authorize and submit.
async fn fake_pool(name: &'static str, difficulty: f64, seen: Seen) -> String {
    pool_with(name, difficulty, seen, true).await
}

/// Like [`fake_pool`], but `accept: false` refuses every authorize.
async fn pool_with(name: &'static str, difficulty: f64, seen: Seen, accept: bool) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (sock, _) = l.accept().await.unwrap();
            let seen = seen.clone();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut br = BufReader::new(r);
                let mut line = String::new();
                while br.read_line(&mut line).await.unwrap_or(0) > 0 {
                    let v: Value = serde_json::from_str(line.trim()).unwrap();
                    let id = v["id"].clone();
                    let method = v["method"].as_str().unwrap_or_default().to_string();
                    let mut out = Vec::new();
                    match method.as_str() {
                        "mining.subscribe" => {
                            out.push(serde_json::json!({"id": id, "result": [[], "abcdef01", 4], "error": null}));
                        }
                        "mining.authorize" | "mining.submit" => {
                            let worker = v["params"][0].as_str().unwrap().to_string();
                            let _ = seen.send((name, method.clone(), worker));
                            if !accept {
                                out.push(serde_json::json!({"id": id, "result": false, "error": [24, "unknown worker", null]}));
                                for o in out.drain(..) {
                                    w.write_all(format!("{o}\n").as_bytes()).await.unwrap();
                                }
                                line.clear();
                                continue;
                            }
                            out.push(serde_json::json!({"id": id, "result": true, "error": null}));
                            if method == "mining.authorize" {
                                out.push(serde_json::json!({"id": null, "method": "mining.set_difficulty", "params": [difficulty]}));
                                out.push(serde_json::json!({"id": null, "method": "mining.notify", "params": [
                                    "job1", HASH32_HEX, "aabbccdd", "11223344", [], "20000000", "1d00ffff", "60000000", true
                                ]}));
                            }
                        }
                        _ => {}
                    }
                    for o in out {
                        w.write_all(format!("{o}\n").as_bytes()).await.unwrap();
                    }
                    line.clear();
                }
            });
        }
    });
    addr
}

struct Miner {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
}

impl Miner {
    /// Connect, subscribe and authorize as `miner.1`, and read through
    /// the pool's difficulty + job so the session is ready to submit.
    async fn connect(proxy: &str) -> Self {
        let sock = TcpStream::connect(proxy).await.unwrap();
        let (r, w) = sock.into_split();
        let mut m = Miner {
            r: BufReader::new(r),
            w,
        };
        m.send(r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#)
            .await;
        m.send(r#"{"id":2,"method":"mining.authorize","params":["miner.1","pw"]}"#)
            .await;
        // subscribe resp, authorize resp, set_difficulty, notify
        for _ in 0..4 {
            m.line().await.expect("pool reply");
        }
        m
    }

    async fn send(&mut self, s: &str) {
        self.w.write_all(format!("{s}\n").as_bytes()).await.unwrap();
    }

    async fn line(&mut self) -> Option<String> {
        let mut s = String::new();
        let n = timeout(T, self.r.read_line(&mut s))
            .await
            .expect("timed out reading from proxy")
            .unwrap_or(0);
        (n > 0).then_some(s)
    }

    async fn submit(&mut self, id: u32) {
        self.send(&format!(
            r#"{{"id":{id},"method":"mining.submit","params":["miner.1","job1","00000000","60000000","{id:08x}"]}}"#
        ))
        .await;
    }

    /// Read until the proxy closes the connection.
    async fn wait_closed(&mut self) {
        while self.line().await.is_some() {}
    }
}

async fn spawn_proxy(router: Arc<Router>, credit: Option<CreditContext>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    tokio::spawn(run_routed_listener(
        addr,
        router,
        Arc::new(ProxyMetrics::default()),
        credit,
        Duration::from_millis(50),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;
    addr.to_string()
}

fn seen_auth(pool: &'static str, worker: &str) -> (&'static str, String, String) {
    (pool, "mining.authorize".into(), worker.into())
}

#[tokio::test]
async fn melt_redirects_miner_to_redeemer_pool_and_back() {
    let (tx, mut seen) = mpsc::unbounded_channel();
    let default_pool = fake_pool("default", 1.0, tx.clone()).await;
    // The order is 3,274 TiH (≈ 1 THH). At difficulty 2^19 a share is worth
    // 2^51 hashes = 2,048 TiH, so the redemption fills on the second share.
    let redeemer_pool = fake_pool("redeemer", (1u64 << 19) as f64, tx).await;

    let ledger = Arc::new(RedemptionLedger::new());
    let router =
        Arc::new(Router::new(default_pool, ledger.clone()).with_switch_mode(SwitchMode::Reconnect));
    let credit = CreditContext::new(Arc::new(ShareCrediter::new(ledger.clone())), router.clone());
    let proxy = spawn_proxy(router, Some(credit)).await;

    // Idle: the miner mines for the operator, worker name untouched.
    let mut m = Miner::connect(&proxy).await;
    assert_eq!(seen.recv().await.unwrap(), seen_auth("default", "miner.1"));

    // A holder melts 3,274 TiH to their own pool.
    let id = RedemptionId::from_bytes([7; 16]);
    ledger
        .open(Redemption::new(
            id,
            3_274,
            format!("stratum+tcp://{redeemer_pool}"),
            "alice.rig".into(),
        ))
        .unwrap();

    // The proxy drops the miner; on reconnect it lands on the redeemer's
    // pool, authorized and submitting as the redeemer's worker.
    m.wait_closed().await;
    let mut m = Miner::connect(&proxy).await;
    assert_eq!(
        seen.recv().await.unwrap(),
        seen_auth("redeemer", "alice.rig")
    );

    m.submit(3).await;
    assert_eq!(
        seen.recv().await.unwrap(),
        ("redeemer", "mining.submit".into(), "alice.rig".into())
    );
    assert!(m.line().await.unwrap().contains("true"));
    assert_eq!(ledger.get(&id).unwrap().state, RedemptionState::Active);

    // The second share fills the redemption; the completion signal
    // releases the miner, which reconnects onto the default pool.
    m.submit(4).await;
    assert_eq!(seen.recv().await.unwrap().0, "redeemer");
    m.wait_closed().await;
    assert_eq!(ledger.get(&id).unwrap().state, RedemptionState::Completed);

    let _m = Miner::connect(&proxy).await;
    assert_eq!(seen.recv().await.unwrap(), seen_auth("default", "miner.1"));
}

#[tokio::test]
async fn unreachable_redeemer_pool_falls_back_to_default() {
    let (tx, mut seen) = mpsc::unbounded_channel();
    let default_pool = fake_pool("default", 1.0, tx).await;

    // A port nothing listens on.
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);

    let ledger = Arc::new(RedemptionLedger::new());
    let id = RedemptionId::from_bytes([1; 16]);
    ledger
        .open(Redemption::new(
            id,
            1,
            format!("stratum+tcp://{dead_addr}"),
            "alice.rig".into(),
        ))
        .unwrap();
    let router = Arc::new(Router::new(default_pool, ledger.clone()));
    let credit = CreditContext::new(Arc::new(ShareCrediter::new(ledger.clone())), router.clone());
    let proxy = spawn_proxy(router, Some(credit)).await;

    let mut m = Miner::connect(&proxy).await;
    assert_eq!(seen.recv().await.unwrap(), seen_auth("default", "miner.1"));

    // Shares on the fallback go to the operator and don't credit the
    // redemption.
    m.submit(3).await;
    assert!(m.line().await.unwrap().contains("true"));
    assert_eq!(ledger.get(&id).unwrap().accumulated_hashes, 0);
    assert_eq!(
        seen.recv().await.unwrap(),
        ("default", "mining.submit".into(), "miner.1".into())
    );

    // Held on the default pool rather than kicked straight back.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(seen.try_recv().is_err());
}

fn backup(addr: &str, worker: &str) -> PoolTarget {
    PoolTarget {
        pool_url: format!("stratum+tcp://{addr}"),
        worker_name: worker.into(),
        worker_password: None,
    }
}

#[tokio::test]
async fn unreachable_primary_fails_over_to_the_redeemers_backup_pool() {
    let (tx, mut seen) = mpsc::unbounded_channel();
    let default_pool = fake_pool("default", 1.0, tx.clone()).await;
    let backup_pool = fake_pool("backup", 256.0, tx).await;
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);

    let ledger = Arc::new(RedemptionLedger::new());
    let id = RedemptionId::from_bytes([2; 16]);
    ledger
        .open(
            Redemption::new(
                id,
                4,
                format!("stratum+tcp://{dead_addr}"),
                "alice.rig".into(),
            )
            .with_fallbacks(vec![backup(&backup_pool, "alice.backup")]),
        )
        .unwrap();
    let router = Arc::new(Router::new(default_pool, ledger.clone()));
    let credit = CreditContext::new(Arc::new(ShareCrediter::new(ledger.clone())), router.clone());
    let proxy = spawn_proxy(router, Some(credit)).await;

    // The primary can't be dialed: the miner lands on the backup pool as
    // the backup's worker, not on the operator's pool.
    let mut m = Miner::connect(&proxy).await;
    assert_eq!(
        seen.recv().await.unwrap(),
        seen_auth("backup", "alice.backup")
    );

    // Its shares fill the same redemption (difficulty 256 = 1 TiH).
    m.submit(3).await;
    assert!(m.line().await.unwrap().contains("true"));
    assert_eq!(
        seen.recv().await.unwrap(),
        ("backup", "mining.submit".into(), "alice.backup".into())
    );
    let r = ledger.get(&id).unwrap();
    assert_eq!(r.delivered_tih(), 1);
    // And count for the backup pool, not the dead primary.
    let per_pool: Vec<_> = r
        .pool_tih()
        .iter()
        .map(|(p, t)| (p.worker_name.clone(), *t))
        .collect();
    assert_eq!(per_pool, vec![("alice.backup".to_string(), 1)]);
}

#[tokio::test]
async fn refused_authorize_fails_over_to_the_backup_pool() {
    let (tx, mut seen) = mpsc::unbounded_channel();
    let default_pool = fake_pool("default", 1.0, tx.clone()).await;
    let refusing = pool_with("refusing", 256.0, tx.clone(), false).await;
    let backup_pool = fake_pool("backup", 256.0, tx).await;

    let ledger = Arc::new(RedemptionLedger::new());
    let id = RedemptionId::from_bytes([3; 16]);
    ledger
        .open(
            Redemption::new(
                id,
                4,
                format!("stratum+tcp://{refusing}"),
                "typo.rig".into(),
            )
            .with_fallbacks(vec![backup(&backup_pool, "alice.backup")]),
        )
        .unwrap();
    let router =
        Arc::new(Router::new(default_pool, ledger.clone()).with_switch_mode(SwitchMode::Reconnect));
    let credit = CreditContext::new(Arc::new(ShareCrediter::new(ledger.clone())), router.clone());
    let proxy = spawn_proxy(router, Some(credit)).await;

    // The primary refuses the worker; the proxy marks it down and drops
    // the miner, which reconnects onto the backup.
    let sock = TcpStream::connect(&proxy).await.unwrap();
    let (r, w) = sock.into_split();
    let mut m = Miner {
        r: BufReader::new(r),
        w,
    };
    m.send(r#"{"id":1,"method":"mining.subscribe","params":["test/1.0"]}"#)
        .await;
    m.send(r#"{"id":2,"method":"mining.authorize","params":["miner.1","pw"]}"#)
        .await;
    assert_eq!(
        seen.recv().await.unwrap(),
        seen_auth("refusing", "typo.rig")
    );
    m.wait_closed().await;

    let mut m = Miner::connect(&proxy).await;
    assert_eq!(
        seen.recv().await.unwrap(),
        seen_auth("backup", "alice.backup")
    );
    m.submit(3).await;
    assert!(m.line().await.unwrap().contains("true"));
    assert_eq!(ledger.get(&id).unwrap().delivered_tih(), 1);
}
