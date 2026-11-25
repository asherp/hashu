//! Test a redeemer's pool before a redemption opens: dial it the way the
//! router would (plain TCP, TLS with the certificate checked, or Stratum
//! V2), subscribe, and authorize the worker. The wallet shows the result
//! before it pays, so a typo in the URL or worker name, an unreachable
//! pool or an unencrypted link is seen up front instead of as a
//! redemption that never fills.
//!
//! A pool that accepts any worker name (F2Pool, ViaBTC) passes here even
//! with a misspelt one: only the pool knows its accounts.

use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::upstream::{self, is_tls, normalize_upstream};

/// How long the whole check may take: dial, handshake, subscribe and
/// authorize.
pub const CHECK_TIMEOUT: Duration = Duration::from_secs(15);

/// How the pool link is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// Plain Stratum V1 over TCP: readable and redirectable in transit.
    Plain,
    /// Stratum V1 over TLS, certificate checked against the public roots.
    Tls,
    /// Stratum V2 (Noise), pinned to the authority key in the URL.
    Sv2,
}

impl Transport {
    pub fn encrypted(self) -> bool {
        self != Transport::Plain
    }
}

/// Where a failed check stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Not a pool URL the proxy can dial.
    Url,
    /// TCP connect, TLS or Noise handshake.
    Connect,
    /// `mining.subscribe`.
    Subscribe,
    /// `mining.authorize`: the pool refused the worker.
    Authorize,
}

/// Result of [`check_pool`], as `POST /checkout/api/pool-check` returns it.
#[derive(Debug, Clone, Serialize)]
pub struct PoolCheck {
    /// The pool answered and accepted the worker.
    pub ok: bool,
    pub transport: Transport,
    /// `transport` is TLS or Stratum V2.
    pub encrypted: bool,
    /// Where it failed, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<Stage>,
    /// What went wrong, when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Shown even when `ok`: the link is plain Stratum V1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Milliseconds from dialing to the authorize answer (or the failure).
    pub elapsed_ms: u64,
}

/// What a plain pool link risks, in words a redeemer can act on.
pub const PLAIN_WARNING: &str = "This pool connection is not encrypted (plain stratum+tcp). \
Anyone on the network path can pose as the pool and take your shares. \
Use the pool's stratum+ssl:// or Stratum V2 address if it has one.";

/// The transport `pool_url` would use, or `None` if it isn't routable.
pub fn transport_of(pool_url: &str) -> Option<Transport> {
    let up = normalize_upstream(pool_url)?;
    Some(if crate::sv2::is_sv2(&up) {
        Transport::Sv2
    } else if is_tls(&up) {
        Transport::Tls
    } else {
        Transport::Plain
    })
}

/// Dial `pool_url`, subscribe and authorize `worker`. Never errors: a
/// failure is a [`PoolCheck`] with `ok: false`.
pub async fn check_pool(pool_url: &str, worker: &str, password: Option<&str>) -> PoolCheck {
    let start = Instant::now();
    let Some(transport) = transport_of(pool_url) else {
        return PoolCheck {
            ok: false,
            transport: Transport::Plain,
            encrypted: false,
            stage: Some(Stage::Url),
            error: Some("not a pool URL the proxy can dial".into()),
            warning: None,
            elapsed_ms: 0,
        };
    };
    let outcome = tokio::time::timeout(CHECK_TIMEOUT, handshake(pool_url, worker, password))
        .await
        .unwrap_or_else(|_| {
            Err((
                Stage::Connect,
                format!("no answer within {} s", CHECK_TIMEOUT.as_secs()),
            ))
        });
    let (stage, error) = match outcome {
        Ok(()) => (None, None),
        Err((s, e)) => (Some(s), Some(e)),
    };
    PoolCheck {
        ok: error.is_none(),
        transport,
        encrypted: transport.encrypted(),
        stage,
        error,
        warning: (!transport.encrypted()).then(|| PLAIN_WARNING.to_string()),
        elapsed_ms: start.elapsed().as_millis() as u64,
    }
}

async fn handshake(
    pool_url: &str,
    worker: &str,
    password: Option<&str>,
) -> Result<(), (Stage, String)> {
    let up = normalize_upstream(pool_url).ok_or((Stage::Url, "bad pool URL".to_string()))?;
    let stream = upstream::dial(&up)
        .await
        .map_err(|e| (Stage::Connect, format!("could not connect: {e}")))?;
    let (r, mut w) = tokio::io::split(stream);
    let mut lines = BufReader::new(r).lines();
    let sub = json!({"id": 1, "method": "mining.subscribe", "params": ["hashu-pool-check/1.0"]});
    let auth =
        json!({"id": 2, "method": "mining.authorize", "params": [worker, password.unwrap_or("x")]});
    let out = format!("{sub}\n{auth}\n").into_bytes();
    w.write_all(&out)
        .await
        .map_err(|e| (Stage::Subscribe, format!("pool closed the connection: {e}")))?;
    w.flush().await.ok();
    let mut subscribed = false;
    loop {
        let stage = if subscribed {
            Stage::Authorize
        } else {
            Stage::Subscribe
        };
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) => return Err((stage, "pool closed the connection".into())),
            Err(e) => return Err((stage, format!("pool connection failed: {e}"))),
        };
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        match v.get("id").and_then(Value::as_u64) {
            Some(1) => {
                if let Some(e) = refusal(&v) {
                    return Err((Stage::Subscribe, format!("pool refused subscribe: {e}")));
                }
                subscribed = true;
            }
            Some(2) => {
                if let Some(e) = refusal(&v) {
                    return Err((
                        Stage::Authorize,
                        format!("pool refused worker {worker:?}: {e}"),
                    ));
                }
                return if v["result"] == Value::Bool(true) {
                    Ok(())
                } else {
                    Err((Stage::Authorize, format!("pool refused worker {worker:?}")))
                };
            }
            // Notifications (set_difficulty, notify, …) before the answers.
            _ => {}
        }
    }
}

/// The error in a JSON-RPC answer, if it has one.
fn refusal(v: &Value) -> Option<String> {
    match v.get("error") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(
            a.get(1)
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| Value::Array(a.clone()).to_string()),
        ),
        Some(Value::Object(o)) => Some(
            o.get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| Value::Object(o.clone()).to_string()),
        ),
        Some(e) => Some(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// A pool on localhost that answers subscribe and authorizes only
    /// `good`. Returns its `stratum+tcp://` URL.
    async fn mock_pool() -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                tokio::spawn(async move {
                    let (r, mut w) = s.into_split();
                    let mut lines = BufReader::new(r).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let v: Value = serde_json::from_str(&line).unwrap();
                        let id = v["id"].clone();
                        let reply = match v["method"].as_str() {
                            Some("mining.subscribe") => {
                                json!({"id": id, "result": [[], "00", 4], "error": null})
                            }
                            Some("mining.authorize") if v["params"][0] == "good" => {
                                json!({"id": id, "result": true, "error": null})
                            }
                            _ => {
                                json!({"id": id, "result": null, "error": [24, "Unauthorized worker", null]})
                            }
                        };
                        let note =
                            json!({"id": null, "method": "mining.set_difficulty", "params": [1]});
                        w.write_all(format!("{note}\n{reply}\n").as_bytes())
                            .await
                            .unwrap();
                    }
                });
            }
        });
        format!("stratum+tcp://{addr}")
    }

    #[tokio::test]
    async fn accepted_worker_on_plain_pool_passes_with_a_warning() {
        let url = mock_pool().await;
        let c = check_pool(&url, "good", None).await;
        assert!(c.ok, "{c:?}");
        assert_eq!(c.transport, Transport::Plain);
        assert!(!c.encrypted);
        assert!(c.warning.is_some());
    }

    #[tokio::test]
    async fn refused_worker_fails_at_authorize() {
        let url = mock_pool().await;
        let c = check_pool(&url, "typo", None).await;
        assert!(!c.ok);
        assert_eq!(c.stage, Some(Stage::Authorize));
        assert!(c.error.unwrap().contains("Unauthorized worker"));
    }

    #[tokio::test]
    async fn unreachable_pool_fails_at_connect() {
        // Bind then drop: nothing listens on that port.
        let port = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let c = check_pool(&format!("stratum+tcp://127.0.0.1:{port}"), "good", None).await;
        assert!(!c.ok);
        assert_eq!(c.stage, Some(Stage::Connect));
    }

    #[test]
    fn transports() {
        assert_eq!(transport_of("stratum+tcp://p:1"), Some(Transport::Plain));
        assert_eq!(transport_of("stratum+ssl://p:443"), Some(Transport::Tls));
        assert_eq!(
            transport_of("stratum2+tcp://stratum.braiins.com:3333/9awtMD5KQgvRUh2yFbjVeT7b6hjipWcAsQHd6wEhgtDT9soosna"),
            Some(Transport::Sv2)
        );
        assert_eq!(transport_of("http://p:1"), None);
    }
}
