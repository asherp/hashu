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
use hashu_proxy::sessions::Session;
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
    router: Arc<Router>,
}

impl DaemonProcess {
    /// Start on `addr`, carrying on rig sessions saved by an earlier run
    /// (as the ops database does).
    fn start(addr: SocketAddr, pool: String, saved: Vec<Session>) -> Self {
        let router = Arc::new(Router::new(pool, Arc::new(RedemptionLedger::new())));
        if !saved.is_empty() {
            router.restore_sessions(saved, now_ms(), 0);
        }
        let r = router.clone();
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
                let router = r;
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
            router,
        }
    }

    /// Stop as the daemon does on SIGTERM; returns its rig sessions as
    /// saved on the way out.
    fn stop(mut self) -> Vec<Session> {
        self.router.begin_shutdown();
        let _ = self.stop.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
        self.router.rig_sessions(0).0
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
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
    let daemon = DaemonProcess::start(daemon_addr, pool.clone(), Vec::new());

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

    // The rig's session started when it connected.
    let (before, _) = daemon.router.rig_sessions(0);
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!(before[0].rig, "alice.rig1");
    assert_eq!(before[0].end_ms, None);

    // Deploy: the daemon process goes away...
    let saved = daemon.stop();
    assert_eq!(saved, before, "a restart is not a disconnect");
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
    let daemon = DaemonProcess::start(daemon_addr, pool, saved);

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
    // The new daemon carries on the same session: the relay told it when
    // the miner connected.
    let (after, _) = daemon.router.rig_sessions(0);
    assert_eq!(after, before, "one unbroken session across the restart");

    // The miner goes away: now the session ends.
    drop(m);
    let mut ended = None;
    for _ in 0..100 {
        let (v, _) = daemon.router.rig_sessions(0);
        if v.len() == 1 && v[0].end_ms.is_some() {
            ended = Some(v[0].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let ended = ended.expect("session ended after the miner left");
    assert_eq!(ended.start_ms, before[0].start_ms);
    daemon.stop();
}
