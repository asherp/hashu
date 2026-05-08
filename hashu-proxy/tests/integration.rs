//! End-to-end loopback test for the Stratum V1 proxy.
//!
//! Topology:
//!     fake miner ⇄ proxy under test ⇄ fake pool
//!
//! All three live in the same tokio runtime on 127.0.0.1 with OS-assigned
//! ports. The fake pool replays a tiny scripted exchange (subscribe →
//! authorize → notify → accept-submit → reject-submit) and the test asserts
//! that the proxy's metrics see one connection, one submitted share, one
//! accepted, and one rejected.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use hashu_proxy::stratum::{run_listener, ListenerConfig};
use hashu_proxy::ProxyMetrics;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

async fn read_line<R: tokio::io::AsyncRead + Unpin>(r: &mut BufReader<R>) -> String {
    let mut s = String::new();
    let n = r.read_line(&mut s).await.unwrap();
    assert!(n > 0, "expected line, got EOF");
    s
}

#[tokio::test]
async fn proxy_passes_messages_and_counts_shares() {
    // ---- fake pool ----
    let pool_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pool_addr = pool_listener.local_addr().unwrap();
    let received_at_pool: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let pool_received = received_at_pool.clone();
    tokio::spawn(async move {
        let (sock, _) = pool_listener.accept().await.unwrap();
        let (r, mut w) = sock.into_split();
        let mut br = BufReader::new(r);

        // mining.subscribe → respond
        let sub = read_line(&mut br).await;
        pool_received.lock().await.push(sub);
        w.write_all(b"{\"id\":1,\"result\":[[],\"abcd\",4],\"error\":null}\n")
            .await.unwrap();

        // mining.authorize → respond true
        let auth = read_line(&mut br).await;
        pool_received.lock().await.push(auth);
        w.write_all(b"{\"id\":2,\"result\":true,\"error\":null}\n").await.unwrap();

        // pool-initiated mining.notify (no response expected)
        w.write_all(b"{\"id\":null,\"method\":\"mining.notify\",\"params\":[\"job1\",\"prevh\",\"cb1\",\"cb2\",[],\"v\",\"nb\",\"nt\",true]}\n")
            .await.unwrap();

        // mining.submit → accept
        let sub1 = read_line(&mut br).await;
        pool_received.lock().await.push(sub1);
        w.write_all(b"{\"id\":3,\"result\":true,\"error\":null}\n").await.unwrap();

        // mining.submit → reject
        let sub2 = read_line(&mut br).await;
        pool_received.lock().await.push(sub2);
        w.write_all(b"{\"id\":4,\"result\":false,\"error\":[23,\"Low difficulty share\",null]}\n")
            .await.unwrap();
    });

    // ---- proxy ----
    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener); // release port; proxy will rebind it

    let metrics = Arc::new(ProxyMetrics::default());
    let cfg = ListenerConfig {
        bind: proxy_addr,
        upstream: format!("{}:{}", pool_addr.ip(), pool_addr.port()),
    };
    let proxy_metrics = metrics.clone();
    let proxy_handle = tokio::spawn(async move { run_listener(cfg, proxy_metrics).await });

    // Wait for proxy to bind. ~50ms is plenty on a loopback.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // ---- fake miner ----
    let miner = TcpStream::connect(proxy_addr).await.unwrap();
    let (r, mut w) = miner.into_split();
    let mut br = BufReader::new(r);

    w.write_all(b"{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"hashu-test/0.1\"]}\n")
        .await.unwrap();
    let sub_resp = read_line(&mut br).await;
    assert!(sub_resp.contains("\"abcd\""), "got {sub_resp}");

    w.write_all(b"{\"id\":2,\"method\":\"mining.authorize\",\"params\":[\"worker.1\",\"x\"]}\n")
        .await.unwrap();
    let auth_resp = read_line(&mut br).await;
    assert!(auth_resp.contains("\"result\":true"), "got {auth_resp}");

    let notify = read_line(&mut br).await;
    assert!(notify.contains("mining.notify"), "got {notify}");

    w.write_all(
        b"{\"id\":3,\"method\":\"mining.submit\",\"params\":[\"worker.1\",\"job1\",\"e2\",\"nt\",\"nonce\"]}\n",
    )
    .await
    .unwrap();
    let r1 = read_line(&mut br).await;
    assert!(r1.contains("\"result\":true"), "got {r1}");

    w.write_all(
        b"{\"id\":4,\"method\":\"mining.submit\",\"params\":[\"worker.1\",\"job1\",\"e2\",\"nt\",\"badnonce\"]}\n",
    )
    .await
    .unwrap();
    let r2 = read_line(&mut br).await;
    assert!(r2.contains("\"result\":false"), "got {r2}");

    // Close the miner side and let the forwarders flush.
    drop(w);
    drop(br);
    tokio::time::sleep(Duration::from_millis(50)).await;

    let snap = metrics.snapshot();
    assert_eq!(snap.connections_accepted, 1, "{snap:?}");
    assert_eq!(snap.shares_submitted, 2, "{snap:?}");
    assert_eq!(snap.shares_accepted, 1, "{snap:?}");
    assert_eq!(snap.shares_rejected, 1, "{snap:?}");

    let pool_lines = received_at_pool.lock().await;
    assert!(pool_lines[0].contains("mining.subscribe"));
    assert!(pool_lines[1].contains("mining.authorize"));
    assert!(pool_lines[2].contains("mining.submit"));
    assert!(pool_lines[3].contains("mining.submit"));

    proxy_handle.abort();
    let _ = metrics.connections_accepted.load(Ordering::Relaxed);
}
