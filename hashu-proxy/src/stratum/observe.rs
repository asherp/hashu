//! Loose JSON inspection of Stratum V1 lines.
//!
//! We don't need to fully parse messages — for forwarding we only peek at
//! `id` and `method` (and `result` on responses). Anything that can't be
//! classified is logged and forwarded as-is.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use serde_json::Value;

use crate::ProxyMetrics;

/// Stratum methods we track requests for so we can credit responses to a
/// concrete operation. Currently only `mining.submit`; expand as needed.
const TRACKED_METHODS: &[&str] = &["mining.submit"];

/// Per-connection: in-flight request `id → method` map. Reset on connection
/// close.
#[derive(Debug, Default)]
pub struct PendingSubmits {
    inner: Mutex<HashMap<i64, String>>,
}

impl PendingSubmits {
    pub fn record(&self, id: i64, method: String) {
        if let Ok(mut g) = self.inner.lock() {
            g.insert(id, method);
        }
    }

    pub fn take(&self, id: i64) -> Option<String> {
        self.inner.lock().ok().and_then(|mut g| g.remove(&id))
    }

    pub fn pending(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Miner → upstream pool (request side).
    M2P,
    /// Upstream pool → miner (notify or response side).
    P2M,
}

/// Inspect one line of stratum traffic. Updates pending-submit map and
/// metrics; never mutates the line. Malformed lines are logged at warn.
pub fn observe_line(
    line: &str,
    direction: Direction,
    pending: &PendingSubmits,
    metrics: &ProxyMetrics,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let v: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, line = %trimmed, "stratum line not valid JSON");
            return;
        }
    };

    let id = v.get("id").and_then(|x| x.as_i64());
    let method = v.get("method").and_then(|x| x.as_str());

    match direction {
        Direction::M2P => {
            tracing::debug!(?direction, ?id, ?method, "stratum");
            if let (Some(id), Some(method)) = (id, method) {
                if TRACKED_METHODS.contains(&method) {
                    pending.record(id, method.to_string());
                    if method == "mining.submit" {
                        metrics.shares_submitted.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        Direction::P2M => {
            tracing::debug!(?direction, ?id, ?method, "stratum");
            // Pool→miner messages are either notifications (id=null,
            // method=mining.notify/set_difficulty) or responses (id set,
            // method=null, result/error present).
            if method.is_some() {
                return;
            }
            if let Some(id) = id {
                if let Some(method) = pending.take(id) {
                    if method == "mining.submit" {
                        let accepted = matches!(v.get("result").and_then(|r| r.as_bool()), Some(true));
                        if accepted {
                            metrics.shares_accepted.fetch_add(1, Ordering::Relaxed);
                            tracing::info!(id, "share accepted");
                        } else {
                            metrics.shares_rejected.fetch_add(1, Ordering::Relaxed);
                            let err = v.get("error").cloned().unwrap_or(Value::Null);
                            tracing::info!(id, ?err, "share rejected");
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> ProxyMetrics {
        ProxyMetrics::default()
    }

    #[test]
    fn empty_line_is_noop() {
        let p = PendingSubmits::default();
        let m = metrics();
        observe_line("\n", Direction::M2P, &p, &m);
        assert_eq!(m.snapshot().shares_submitted, 0);
    }

    #[test]
    fn submit_request_increments_submitted_and_records_pending() {
        let p = PendingSubmits::default();
        let m = metrics();
        let line = r#"{"id":42,"method":"mining.submit","params":["worker.1","jobid","extranonce2","ntime","nonce"]}"#;
        observe_line(line, Direction::M2P, &p, &m);
        assert_eq!(m.snapshot().shares_submitted, 1);
        assert_eq!(p.pending(), 1);
    }

    #[test]
    fn accepted_response_increments_accepted_and_clears_pending() {
        let p = PendingSubmits::default();
        let m = metrics();
        let req = r#"{"id":42,"method":"mining.submit","params":[]}"#;
        observe_line(req, Direction::M2P, &p, &m);
        let resp = r#"{"id":42,"result":true,"error":null}"#;
        observe_line(resp, Direction::P2M, &p, &m);
        let snap = m.snapshot();
        assert_eq!(snap.shares_submitted, 1);
        assert_eq!(snap.shares_accepted, 1);
        assert_eq!(snap.shares_rejected, 0);
        assert_eq!(p.pending(), 0);
    }

    #[test]
    fn rejected_response_increments_rejected_and_clears_pending() {
        let p = PendingSubmits::default();
        let m = metrics();
        let req = r#"{"id":7,"method":"mining.submit","params":[]}"#;
        observe_line(req, Direction::M2P, &p, &m);
        let resp = r#"{"id":7,"result":false,"error":[23,"Invalid share","trace"]}"#;
        observe_line(resp, Direction::P2M, &p, &m);
        let snap = m.snapshot();
        assert_eq!(snap.shares_submitted, 1);
        assert_eq!(snap.shares_accepted, 0);
        assert_eq!(snap.shares_rejected, 1);
        assert_eq!(p.pending(), 0);
    }

    #[test]
    fn unrelated_methods_dont_affect_pending() {
        let p = PendingSubmits::default();
        let m = metrics();
        let sub = r#"{"id":1,"method":"mining.subscribe","params":["bfgminer/5.5.0"]}"#;
        observe_line(sub, Direction::M2P, &p, &m);
        let auth = r#"{"id":2,"method":"mining.authorize","params":["worker","x"]}"#;
        observe_line(auth, Direction::M2P, &p, &m);
        assert_eq!(p.pending(), 0);
        assert_eq!(m.snapshot().shares_submitted, 0);
    }

    #[test]
    fn pool_notify_is_ignored_for_metrics() {
        let p = PendingSubmits::default();
        let m = metrics();
        let notify = r#"{"id":null,"method":"mining.notify","params":["job","prevhash","cb1","cb2",[],"ver","nbits","ntime",true]}"#;
        observe_line(notify, Direction::P2M, &p, &m);
        let snap = m.snapshot();
        assert_eq!(snap.shares_accepted, 0);
        assert_eq!(snap.shares_rejected, 0);
    }

    #[test]
    fn malformed_line_does_not_panic() {
        let p = PendingSubmits::default();
        let m = metrics();
        observe_line("not json", Direction::M2P, &p, &m);
        observe_line("{not even close", Direction::P2M, &p, &m);
        assert_eq!(m.snapshot().shares_submitted, 0);
    }

    #[test]
    fn response_without_matching_request_is_ignored() {
        let p = PendingSubmits::default();
        let m = metrics();
        let resp = r#"{"id":999,"result":true,"error":null}"#;
        observe_line(resp, Direction::P2M, &p, &m);
        assert_eq!(m.snapshot().shares_accepted, 0);
    }
}
