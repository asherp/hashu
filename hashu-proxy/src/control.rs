//! Local HTTP control surface for the proxy.
//!
//! Lets something outside the proxy process (the Hashu MDK app, a
//! dashboard, `curl`) see which redemptions are open and how far each has
//! been filled, so it can switch miners onto the proxy while one is open
//! and back off it when the last one completes.
//!
//! - `GET /status`: redemptions (oldest first, completed included), the
//!   default upstream and the proxy's share counters.
//! - `POST /redemptions`: open a redemption from a JSON body
//!   `{ "amount_thh": 1, "pool_url": "stratum+tcp://…", "worker_name": "…" }`.
//!   Only served when the listener is started with `allow_open`. It is a
//!   stand-in for the mint's `hashrate` melt until the mint daemon runs in
//!   this process (issue #3); anyone who can reach it can redirect the
//!   hashrate, so it binds to loopback by default.
//!
//! Plain HTTP/1.1 over tokio, one request per connection, no extra
//! dependencies.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hashu_core::redemption::{
    Redemption, RedemptionId, RedemptionLedger, RedemptionState, HASHES_PER_THH,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::router::upstream_for_pool_url;
use crate::ProxyMetrics;

/// Largest request body accepted.
const MAX_BODY: usize = 16 * 1024;

/// Shared state the control server reads.
#[derive(Clone)]
pub struct ControlState {
    pub ledger: Arc<RedemptionLedger>,
    pub metrics: Arc<ProxyMetrics>,
    pub default_upstream: String,
    /// Whether `POST /redemptions` is served.
    pub allow_open: bool,
}

/// Serve the control API on `bind` until the task is dropped.
pub async fn run_control_server(bind: SocketAddr, state: ControlState) -> std::io::Result<()> {
    let listener = TcpListener::bind(bind).await?;
    tracing::info!(%bind, allow_open = state.allow_open, "control API listening");
    serve(listener, state).await
}

/// Serve the control API on an already-bound listener.
pub async fn serve(listener: TcpListener, state: ControlState) -> std::io::Result<()> {
    loop {
        let (sock, _) = listener.accept().await?;
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(sock, &state).await {
                tracing::debug!(error = %e, "control request failed");
            }
        });
    }
}

async fn handle(sock: TcpStream, state: &ControlState) -> std::io::Result<()> {
    let (r, mut w) = sock.into_split();
    let mut br = BufReader::new(r);
    let mut request_line = String::new();
    br.read_line(&mut request_line).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if br.read_line(&mut line).await? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }

    let (code, body) = if content_length > MAX_BODY {
        (413, json!({ "error": "body too large" }))
    } else {
        let mut buf = vec![0u8; content_length];
        br.read_exact(&mut buf).await?;
        route(&method, &path, &buf, state)
    };

    let payload = body.to_string();
    let reason = match code {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    let resp = format!(
        "HTTP/1.1 {code} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    w.write_all(resp.as_bytes()).await?;
    w.shutdown().await
}

/// Dispatch one request. Split out from the socket code so it is unit
/// testable.
pub fn route(method: &str, path: &str, body: &[u8], state: &ControlState) -> (u16, Value) {
    let path = path.split('?').next().unwrap_or(path);
    match (method, path) {
        ("GET", "/status") => (200, status(state)),
        ("POST", "/redemptions") if state.allow_open => open(body, state),
        ("POST", "/redemptions") => (
            404,
            json!({ "error": "opening redemptions over HTTP is disabled (start the proxy with --control-allow-open)" }),
        ),
        _ => (404, json!({ "error": "not found" })),
    }
}

fn status(state: &ControlState) -> Value {
    let m = state.metrics.snapshot();
    let redemptions: Vec<Value> = state.ledger.all().iter().map(redemption_json).collect();
    let active = state.ledger.active().len();
    json!({
        "default_upstream": state.default_upstream,
        "active_redemptions": active,
        "redemptions": redemptions,
        "metrics": {
            "connections_accepted": m.connections_accepted,
            "shares_submitted": m.shares_submitted,
            "shares_accepted": m.shares_accepted,
            "shares_rejected": m.shares_rejected,
            "shares_committed": m.shares_committed,
            "shares_credited": m.shares_credited,
        },
    })
}

fn redemption_json(r: &Redemption) -> Value {
    let state = match r.state {
        RedemptionState::Pending => "pending",
        RedemptionState::Active => "active",
        RedemptionState::Completed => "completed",
    };
    let progress = if r.target_hashes == 0 {
        1.0
    } else {
        (r.accumulated_hashes as f64 / r.target_hashes as f64).min(1.0)
    };
    json!({
        "id": r.id.to_string(),
        "pool_url": r.pool_url,
        "worker_name": r.worker_name,
        "amount_thh": r.amount_thh,
        "filled_thh": r.accumulated_hashes as f64 / HASHES_PER_THH as f64,
        // u128 does not fit a JSON number; send exact values as strings.
        "accumulated_hashes": r.accumulated_hashes.to_string(),
        "target_hashes": r.target_hashes.to_string(),
        "progress": progress,
        "state": state,
    })
}

fn open(body: &[u8], state: &ControlState) -> (u16, Value) {
    let v: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => return (400, json!({ "error": format!("invalid JSON: {e}") })),
    };
    let Some(amount_thh) = v["amount_thh"].as_u64().filter(|a| *a > 0) else {
        return (
            400,
            json!({ "error": "amount_thh must be a positive integer" }),
        );
    };
    let Some(pool_url) = v["pool_url"].as_str().filter(|s| !s.is_empty()) else {
        return (400, json!({ "error": "pool_url is required" }));
    };
    if upstream_for_pool_url(pool_url).is_none() {
        return (
            400,
            json!({ "error": "pool_url must be host:port or stratum+tcp://host:port" }),
        );
    }
    let Some(worker_name) = v["worker_name"].as_str().filter(|s| !s.is_empty()) else {
        return (400, json!({ "error": "worker_name is required" }));
    };
    let r = Redemption::new(
        fresh_id(),
        amount_thh,
        pool_url.to_string(),
        worker_name.to_string(),
    );
    let out = redemption_json(&r);
    match state.ledger.open(r) {
        Ok(()) => {
            tracing::info!(id = %out["id"], amount_thh, pool_url, "redemption opened over control API");
            (201, out)
        }
        Err(e) => (400, json!({ "error": e.to_string() })),
    }
}

/// A random-enough 16-byte id: SHA-256 over the clock, the pid and a
/// process counter. The mint generates real ids; this only has to be
/// unique and not derived from the pool or worker.
fn fresh_id() -> RedemptionId {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let digest = h.finalize();
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    RedemptionId::from_bytes(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(allow_open: bool) -> ControlState {
        ControlState {
            ledger: Arc::new(RedemptionLedger::new()),
            metrics: Arc::new(ProxyMetrics::default()),
            default_upstream: "home:3333".into(),
            allow_open,
        }
    }

    #[test]
    fn status_lists_redemptions_with_progress() {
        let s = state(false);
        let id = RedemptionId::from_bytes([7; 16]);
        s.ledger
            .open(Redemption::new(id, 2, "pool:3333".into(), "bob.1".into()))
            .unwrap();
        // One share worth half a THH: 1.8e15 hashes / 2^32.
        s.ledger
            .credit_share(&id, 1.8e15 / (1u64 << 32) as f64)
            .unwrap();
        let (code, v) = route("GET", "/status", b"", &s);
        assert_eq!(code, 200);
        assert_eq!(v["active_redemptions"], 1);
        assert_eq!(v["default_upstream"], "home:3333");
        let r = &v["redemptions"][0];
        assert_eq!(r["state"], "active");
        assert_eq!(r["worker_name"], "bob.1");
        assert!((r["progress"].as_f64().unwrap() - 0.25).abs() < 1e-6);
        assert_eq!(r["target_hashes"], "7200000000000000");
    }

    #[test]
    fn completed_redemptions_stay_in_status() {
        let s = state(false);
        let id = RedemptionId::from_bytes([1; 16]);
        s.ledger
            .open(Redemption::new(id, 1, "pool:3333".into(), "w".into()))
            .unwrap();
        s.ledger.credit_share(&id, 1e9).unwrap();
        let (_, v) = route("GET", "/status", b"", &s);
        assert_eq!(v["active_redemptions"], 0);
        assert_eq!(v["redemptions"][0]["state"], "completed");
        assert_eq!(v["redemptions"][0]["progress"], 1.0);
    }

    #[test]
    fn open_is_refused_unless_allowed() {
        let s = state(false);
        let body = br#"{"amount_thh":1,"pool_url":"stratum+tcp://p:3333","worker_name":"w"}"#;
        assert_eq!(route("POST", "/redemptions", body, &s).0, 404);
        assert!(s.ledger.all().is_empty());
    }

    #[test]
    fn open_creates_a_pending_redemption() {
        let s = state(true);
        let body = br#"{"amount_thh":3,"pool_url":"stratum+tcp://p:3333","worker_name":"w.1"}"#;
        let (code, v) = route("POST", "/redemptions", body, &s);
        assert_eq!(code, 201, "{v}");
        assert_eq!(v["state"], "pending");
        assert_eq!(v["amount_thh"], 3);
        assert_eq!(s.ledger.active().len(), 1);
        // A second open gets a different id.
        let (_, v2) = route("POST", "/redemptions", body, &s);
        assert_ne!(v["id"], v2["id"]);
    }

    #[test]
    fn open_validates_the_body() {
        let s = state(true);
        for body in [
            &b"nope"[..],
            br#"{"amount_thh":0,"pool_url":"p:1","worker_name":"w"}"#,
            br#"{"amount_thh":1,"pool_url":"","worker_name":"w"}"#,
            br#"{"amount_thh":1,"pool_url":"no-port","worker_name":"w"}"#,
            br#"{"amount_thh":1,"pool_url":"p:1"}"#,
        ] {
            assert_eq!(route("POST", "/redemptions", body, &s).0, 400);
        }
        assert!(s.ledger.all().is_empty());
    }

    #[test]
    fn unknown_paths_are_404() {
        assert_eq!(route("GET", "/nope", b"", &state(true)).0, 404);
    }

    #[tokio::test]
    async fn serves_over_tcp() {
        let s = state(true);
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(serve(l, s.clone()));
        let mut c = TcpStream::connect(addr).await.unwrap();
        let body = r#"{"amount_thh":1,"pool_url":"p:3333","worker_name":"w"}"#;
        c.write_all(
            format!(
                "POST /redemptions HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 201"), "{out}");
        assert_eq!(s.ledger.active().len(), 1);
    }
}
