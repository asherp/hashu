//! End-to-end: accepted shares fill a redemption in the ledger.
//!
//! Topology: fake miner ⇄ proxy (with a [`CreditContext`]) ⇄ fake pool.
//! The pool sets difficulty 500_000 and accepts every submit. A 3,274 TiH
//! (≈ 1 THH) redemption needs 3,274 × 2^40 hashes = 838,144 × 2^32, so:
//!   - a share submitted while unbound (default pool) credits nothing;
//!   - the first bound share leaves it Active at 500_000 × 2^32 hashes;
//!   - the second bound share completes it and fires the completion signal.

use std::sync::Arc;
use std::time::Duration;

use hashu_core::redemption::{
    Redemption, RedemptionId, RedemptionLedger, RedemptionState, HASHES_PER_DIFF_ONE,
};
use hashu_proxy::credit::{CreditContext, ShareCrediter, StaticBinding};
use hashu_proxy::stratum::{run_listener_with, ListenerConfig};
use hashu_proxy::ProxyMetrics;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

const HASH32_HEX: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const DIFFICULTY: f64 = 500_000.0;

async fn read_line<R: tokio::io::AsyncRead + Unpin>(r: &mut BufReader<R>) -> String {
    let mut s = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), r.read_line(&mut s))
        .await
        .expect("timed out reading line")
        .unwrap();
    assert!(n > 0, "expected line, got EOF");
    s
}

async fn submit<W: tokio::io::AsyncWrite + Unpin>(w: &mut W, id: i64, nonce: &str) {
    let line = format!(
        "{{\"id\":{id},\"method\":\"mining.submit\",\"params\":[\"worker.1\",\"job1\",\"c0c1c2c3\",\"60000001\",\"{nonce}\"]}}\n"
    );
    w.write_all(line.as_bytes()).await.unwrap();
}

fn request_id(line: &str) -> i64 {
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    v["id"].as_i64().unwrap()
}

/// Fake pool: answers subscribe and authorize, pushes difficulty and one
/// job, then accepts every `mining.submit`.
async fn fake_pool(listener: TcpListener) {
    let (sock, _) = listener.accept().await.unwrap();
    let (r, mut w) = sock.into_split();
    let mut br = BufReader::new(r);

    let _sub = read_line(&mut br).await;
    w.write_all(b"{\"id\":1,\"result\":[[[\"mining.set_difficulty\",\"s1\"],[\"mining.notify\",\"s2\"]],\"abcdef01\",4],\"error\":null}\n")
        .await
        .unwrap();
    let _auth = read_line(&mut br).await;
    w.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n")
        .await
        .unwrap();
    w.write_all(
        format!("{{\"id\":null,\"method\":\"mining.set_difficulty\",\"params\":[{DIFFICULTY}]}}\n")
            .as_bytes(),
    )
    .await
    .unwrap();
    let notify = format!(
        "{{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"job1\",\"{HASH32_HEX}\",\"aabbccdd\",\"11223344\",[],\"20000000\",\"1d00ffff\",\"60000000\",true]}}\n"
    );
    w.write_all(notify.as_bytes()).await.unwrap();

    loop {
        let mut line = String::new();
        if br.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let id = request_id(&line);
        w.write_all(format!("{{\"id\":{id},\"result\":true,\"error\":null}}\n").as_bytes())
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn accepted_shares_fill_bound_redemption() {
    let pool_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pool_addr = pool_listener.local_addr().unwrap();
    tokio::spawn(fake_pool(pool_listener));

    // Ledger with one open 3,274 TiH redemption; the binding starts unbound.
    let rid = RedemptionId::from_bytes([0x42; 16]);
    let ledger = Arc::new(RedemptionLedger::new());
    ledger
        .open(Redemption::new(
            rid,
            3_274,
            "stratum+tcp://redeemer:3333".into(),
            "w".into(),
        ))
        .unwrap();
    let crediter = Arc::new(ShareCrediter::new(ledger.clone()));
    let mut completions = crediter.subscribe_completions();
    let binding = Arc::new(StaticBinding::default());
    let crediter_audit = crediter.audit().clone();
    let credit = CreditContext::new(crediter, binding.clone());

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener);
    let metrics = Arc::new(ProxyMetrics::default());
    let cfg = ListenerConfig {
        bind: proxy_addr,
        upstream: pool_addr.to_string(),
    };
    let proxy_metrics = metrics.clone();
    let proxy =
        tokio::spawn(async move { run_listener_with(cfg, proxy_metrics, Some(credit)).await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let miner = TcpStream::connect(proxy_addr).await.unwrap();
    let (r, mut w) = miner.into_split();
    let mut br = BufReader::new(r);
    w.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"hashu-test/0.1\"]}\n")
        .await
        .unwrap();
    read_line(&mut br).await;
    w.write_all(b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"worker.1\",\"x\"]}\n")
        .await
        .unwrap();
    read_line(&mut br).await;
    assert!(read_line(&mut br).await.contains("set_difficulty"));
    assert!(read_line(&mut br).await.contains("mining.notify"));

    // Unbound: goes to the default pool, accepted but not credited.
    submit(&mut w, 3, "00000001").await;
    assert!(read_line(&mut br).await.contains("\"result\":true"));
    assert_eq!(ledger.get(&rid).unwrap().accumulated_hashes, 0);

    // Bound: each accepted share credits DIFFICULTY × 2^32 hashes.
    binding.set(Some(rid));
    submit(&mut w, 4, "00000002").await;
    assert!(read_line(&mut br).await.contains("\"result\":true"));
    let r = ledger.get(&rid).unwrap();
    assert_eq!(r.state, RedemptionState::Active);
    assert_eq!(r.accumulated_hashes, 500_000 * HASHES_PER_DIFF_ONE);
    assert!(completions.try_recv().is_err());

    submit(&mut w, 5, "00000003").await;
    assert!(read_line(&mut br).await.contains("\"result\":true"));
    let r = ledger.get(&rid).unwrap();
    assert_eq!(r.state, RedemptionState::Completed);
    assert!(r.accumulated_hashes >= r.target_hashes);

    let done = tokio::time::timeout(Duration::from_secs(1), completions.recv())
        .await
        .expect("completion not signalled")
        .unwrap();
    assert_eq!(done, rid);

    // The two credited shares, and only those, are in the redemption's
    // commitment tree, and each proves against its root.
    let audit = crediter_audit;
    let head = audit.head(&rid);
    assert_eq!(head.leaf_count, 2);
    let root = head.root.unwrap();
    for i in 0..2 {
        let p = audit.proof(&rid, i).unwrap();
        assert_eq!(p.leaf.preimage.len(), 80);
        assert!(hashu_core::share::verify_inclusion(
            &p.leaf_hash,
            &p.proof,
            &root
        ));
    }

    let snap = metrics.snapshot();
    assert_eq!(snap.shares_accepted, 3, "{snap:?}");
    assert_eq!(snap.shares_committed, 3, "{snap:?}");
    assert_eq!(snap.shares_credited, 2, "{snap:?}");

    proxy.abort();
}
