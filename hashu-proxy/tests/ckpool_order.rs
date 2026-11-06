//! ckpool sends `set_difficulty` + `mining.notify` before answering
//! `mining.authorize`. A rental relay (MiningRigRentals) that sends
//! `mining.configure` first and only takes work after authorize must still
//! get a job right after the authorize result, or it drops the session.

use std::sync::Arc;
use std::time::Duration;

use hashu_core::redemption::RedemptionLedger;
use hashu_proxy::router::Router;
use hashu_proxy::stratum::run_routed_listener;
use hashu_proxy::ProxyMetrics;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

const H: &str = "1111111111111111111111111111111111111111111111111111111111111111";

/// ckpool-like pool: job is pushed right after the subscribe response,
/// authorize is answered afterwards with no job following it.
async fn ckpool_like() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let (sock, _) = l.accept().await.unwrap();
        let (r, mut w) = sock.into_split();
        let mut br = BufReader::new(r);
        let mut line = String::new();
        while br.read_line(&mut line).await.unwrap_or(0) > 0 {
            let v: Value = serde_json::from_str(line.trim()).unwrap();
            let id = v["id"].clone();
            let mut out = vec![];
            match v["method"].as_str() {
                Some("mining.configure") => out.push(json!({"id": id, "result": {"version-rolling": true, "version-rolling.mask": "1fffe000"}, "error": null})),
                Some("mining.subscribe") => {
                    out.push(json!({"id": id, "result": [[["mining.notify", "a"]], "abcdef01", 8], "error": null}));
                    out.push(json!({"id": null, "method": "mining.set_difficulty", "params": [2395328]}));
                    out.push(json!({"id": null, "method": "mining.notify", "params": ["6a", H, "aa", "bb", [], "20000000", "1d00ffff", "60000000", false]}));
                }
                Some("mining.authorize") => out.push(json!({"id": id, "result": true, "error": null})),
                _ => {}
            }
            for o in out {
                w.write_all(format!("{o}\n").as_bytes()).await.unwrap();
            }
            line.clear();
        }
    });
    addr
}

async fn read(br: &mut BufReader<tokio::net::tcp::OwnedReadHalf>) -> Value {
    let mut s = String::new();
    timeout(Duration::from_secs(3), br.read_line(&mut s))
        .await
        .expect("line from proxy")
        .unwrap();
    serde_json::from_str(s.trim()).unwrap()
}

#[tokio::test]
async fn job_is_resent_after_authorize_result() {
    let pool = ckpool_like().await;
    let router = Arc::new(Router::new(pool, Arc::new(RedemptionLedger::new())));
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = l.local_addr().unwrap();
    drop(l);
    tokio::spawn(run_routed_listener(
        proxy,
        router,
        Arc::new(ProxyMetrics::default()),
        None,
        Duration::from_secs(1),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let sock = TcpStream::connect(proxy).await.unwrap();
    let (r, mut w) = sock.into_split();
    let mut br = BufReader::new(r);
    for m in [
        json!({"id": 1, "method": "mining.configure", "params": [["version-rolling"], {"version-rolling.mask": "1fffe000", "version-rolling.min-bit-count": 2}]}),
        json!({"id": 2, "method": "mining.subscribe", "params": ["bmminer/2.0.0"]}),
    ] {
        w.write_all(format!("{m}\n").as_bytes()).await.unwrap();
    }
    assert_eq!(read(&mut br).await["id"], 1); // configure
    assert_eq!(read(&mut br).await["id"], 2); // subscribe
    assert_eq!(read(&mut br).await["method"], "mining.set_difficulty");
    assert_eq!(read(&mut br).await["method"], "mining.notify");

    w.write_all(b"{\"id\":3,\"method\":\"mining.authorize\",\"params\":[\"asher.rig\",\"x\"]}\n")
        .await
        .unwrap();
    let auth = read(&mut br).await;
    assert_eq!(
        (auth["id"].clone(), auth["result"].clone()),
        (json!(3), json!(true))
    );
    // The job follows the authorize result, now with clean_jobs = true.
    let d = read(&mut br).await;
    assert_eq!(d["method"], "mining.set_difficulty");
    assert_eq!(d["params"][0], 2395328);
    let n = read(&mut br).await;
    assert_eq!(n["method"], "mining.notify");
    assert_eq!(n["params"][0], "6a");
    assert_eq!(n["params"][8], true);
}
