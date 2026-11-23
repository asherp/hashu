//! The daemon restarts under a live miner: the relay keeps the miner's
//! connection, resumes it on the new daemon with the same extranonce, and
//! the miner's next share (on the new job) reaches the pool intact.
//!
//!     fake miner ⇄ hashu-relay ⇄ hashu-proxy (restarted) ⇄ fake pool

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hashu_core::redemption::RedemptionLedger;
use hashu_proxy::router::Router;
use hashu_proxy::stratum::run_routed_listener_on;
use hashu_proxy::ProxyMetrics;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const T: Duration = Duration::from_secs(10);
const H: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// What the pool saw in a submit: its session's en1, the job, the en2.
#[derive(Debug)]
struct Submit {
    session_en1: String,
    job: String,
    en2: String,
}

/// A pool that gives each session its own extranonce1 and job id.
async fn fake_pool(seen: mpsc::UnboundedSender<Submit>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let sessions = Arc::new(AtomicU32::new(0));
    tokio::spawn(async move {
        loop {
            let (sock, _) = l.accept().await.unwrap();
            let n = sessions.fetch_add(1, Ordering::SeqCst) + 1;
            let seen = seen.clone();
            tokio::spawn(async move {
                let en1 = format!("{n:08x}");
                let job = format!("job{n}");
                let (r, mut w) = sock.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let v: Value = serde_json::from_str(&line).unwrap();
                    let id = v["id"].clone();
                    let mut out = Vec::new();
                    match v["method"].as_str().unwrap_or_default() {
                        "mining.configure" => out.push(json!({"id": id, "result": {"version-rolling": true, "version-rolling.mask": "1fffe000"}, "error": null})),
                        "mining.subscribe" => out.push(json!({"id": id, "result": [[["mining.notify", "s"]], en1, 8], "error": null})),
                        "mining.authorize" => {
                            out.push(json!({"id": id, "result": true, "error": null}));
                            out.push(json!({"id": null, "method": "mining.set_difficulty", "params": [1.0]}));
                            out.push(json!({"id": null, "method": "mining.notify", "params": [job, H, "01aa", "02bb", [], "20000000", "1d00ffff", "60000000", true]}));
                        }
                        "mining.submit" => {
                            let _ = seen.send(Submit {
                                session_en1: en1.clone(),
                                job: v["params"][1].as_str().unwrap().into(),
                                en2: v["params"][2].as_str().unwrap().into(),
                            });
                            out.push(json!({"id": id, "result": true, "error": null}));
                        }
                        _ => out.push(json!({"id": id, "result": true, "error": null})),
                    }
                    for o in out {
                        w.write_all(format!("{o}\n").as_bytes()).await.unwrap();
                    }
                }
            });
        }
    });
    addr
}

/// A Hashu Stratum proxy in its own runtime, so stopping it drops every
/// connection it holds, as a process exit does.
struct DaemonProcess {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DaemonProcess {
    fn start(addr: SocketAddr, pool: String) -> Self {
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let listener = loop {
                    match TcpListener::bind(addr).await {
                        Ok(l) => break l,
                        Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                    }
                };
                ready_tx.send(()).unwrap();
                let router = Arc::new(Router::new(pool, Arc::new(RedemptionLedger::new())));
                tokio::select! {
                    _ = run_routed_listener_on(listener, router, Arc::new(ProxyMetrics::default()), None, Duration::from_millis(20)) => {}
                    _ = rx => {}
                }
            });
            rt.shutdown_background();
        });
        ready_rx.recv_timeout(T).unwrap();
        Self {
            stop: Some(tx),
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

struct Miner {
    lines: Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
    next_id: i64,
}

impl Miner {
    async fn connect(addr: SocketAddr) -> Self {
        let (r, w) = TcpStream::connect(addr).await.unwrap().into_split();
        Self {
            lines: BufReader::new(r).lines(),
            w,
            next_id: 0,
        }
    }

    async fn send(&mut self, method: &str, params: Value) -> i64 {
        self.next_id += 1;
        let l = json!({"id": self.next_id, "method": method, "params": params});
        self.w.write_all(format!("{l}\n").as_bytes()).await.unwrap();
        self.next_id
    }

    /// Next line; panics if the connection closes or nothing arrives.
    async fn next(&mut self) -> Value {
        let l = timeout(T, self.lines.next_line())
            .await
            .expect("timed out")
            .unwrap()
            .expect("miner connection closed");
        serde_json::from_str(&l).unwrap()
    }

    async fn answer(&mut self, id: i64) -> Value {
        loop {
            let v = self.next().await;
            if v["id"] == id {
                return v;
            }
            assert_ne!(
                v["method"], "mining.set_extranonce",
                "miner was told to change extranonce"
            );
        }
    }

    async fn job(&mut self) -> String {
        loop {
            let v = self.next().await;
            assert_ne!(
                v["method"], "mining.set_extranonce",
                "miner was told to change extranonce"
            );
            if v["method"] == "mining.notify" {
                return v["params"][0].as_str().unwrap().into();
            }
        }
    }
}

async fn free_addr() -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    l.local_addr().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn miner_stays_connected_across_a_daemon_restart() {
    let (seen_tx, mut seen) = mpsc::unbounded_channel();
    let pool = fake_pool(seen_tx).await;
    let daemon_addr = free_addr().await;
    let daemon = DaemonProcess::start(daemon_addr, pool.clone());

    let relay = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let relay_addr = relay.local_addr().unwrap();
    tokio::spawn(hashu_relay::serve(
        relay,
        hashu_relay::Config::new(daemon_addr.to_string()),
    ));

    let mut m = Miner::connect(relay_addr).await;
    let c = m
        .send(
            "mining.configure",
            json!([["version-rolling"], {"version-rolling.mask": "ffffffff"}]),
        )
        .await;
    assert_eq!(
        m.answer(c).await["result"]["version-rolling.mask"],
        "1fffe000"
    );
    let s = m.send("mining.subscribe", json!(["test-miner/1.0"])).await;
    let sub = m.answer(s).await;
    let en1 = sub["result"][1].as_str().unwrap().to_string();
    let en2_size = sub["result"][2].as_u64().unwrap();
    assert_eq!(en2_size, 4, "proxy hands miners its own extranonce");
    let a = m.send("mining.authorize", json!(["alice.rig1", "x"])).await;
    assert_eq!(m.answer(a).await["result"], true);
    let job1 = m.job().await;
    assert_eq!(job1, "job1");
    let sid = m
        .send(
            "mining.submit",
            json!(["alice.rig1", job1, "00000001", "60000000", "00000000"]),
        )
        .await;
    assert_eq!(m.answer(sid).await["result"], true);
    let first = timeout(T, seen.recv()).await.unwrap().unwrap();
    assert_eq!(first.job, "job1");
    assert!(
        first.en2.starts_with(&en1),
        "pool en2 carries the miner's extranonce1"
    );

    // Deploy: the daemon process goes away...
    daemon.stop();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // ...a share sent meanwhile is answered as stale, not lost silently...
    let gap = m
        .send(
            "mining.submit",
            json!(["alice.rig1", job1, "00000002", "60000000", "00000000"]),
        )
        .await;
    let r = m.answer(gap).await;
    assert_eq!(r["error"][0], 21, "{r}");
    // ...and comes back on the same port.
    let daemon = DaemonProcess::start(daemon_addr, pool);

    // Same connection: a new job from the new pool session, no
    // set_extranonce, and the next share still fits.
    let job2 = m.job().await;
    assert_eq!(job2, "job2");
    let sid = m
        .send(
            "mining.submit",
            json!(["alice.rig1", job2, "00000003", "60000000", "00000000"]),
        )
        .await;
    assert_eq!(m.answer(sid).await["result"], true);
    let second = timeout(T, seen.recv()).await.unwrap().unwrap();
    assert_eq!(second.job, "job2");
    assert_eq!(second.session_en1, "00000002", "a new pool session");
    assert!(
        second.en2.starts_with(&en1),
        "resumed with the miner's extranonce1 {en1}, pool got en2 {}",
        second.en2
    );
    daemon.stop();
}
