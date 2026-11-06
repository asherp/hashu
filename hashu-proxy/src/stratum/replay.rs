//! Hold the pool's work until the miner is authorized.
//!
//! ckpool (and some other pools) push `mining.set_difficulty` and
//! `mining.notify` as soon as `mining.subscribe` is answered, *before* the
//! `mining.authorize` response. Firmware talking to the pool directly
//! copes, but relays such as MiningRigRentals' do not: work that arrives
//! before authorize is ignored, and a session with no job after authorize
//! is dropped as a pool that "does not send jobs". Re-sending the job
//! after authorize (while also letting the early copy through) was not
//! enough; the relay needs to see no work at all before authorize.
//!
//! [`JobGate`] therefore *holds* every pool notification (anything with a
//! `method`: difficulty, jobs, extranonce changes, …) from the start of
//! the connection until the pool answers the miner's `mining.authorize`.
//! Responses (configure, subscribe, …) pass straight through. When the
//! authorize response arrives it is forwarded first, then the held
//! notifications in order, collapsed so the miner gets only the latest
//! difficulty and the latest job, with the job forced to
//! `clean_jobs = true`. After that the gate is open and every line
//! passes untouched. A failed authorize also opens the gate (flushing
//! what was held), so behaviour is plain passthrough from then on.
//!
//! A cap bounds what is held, in case a miner never authorizes: past
//! [`MAX_HELD`] notifications the gate opens and flushes.

use std::collections::HashSet;

use serde_json::Value;

/// Upper bound on held notifications before the gate gives up and opens.
pub const MAX_HELD: usize = 64;

/// What to do with one pool→miner line.
#[derive(Debug, PartialEq, Eq)]
pub enum PoolLine {
    /// Write these lines to the miner, in order (the line itself is
    /// included where it should be written).
    Write(Vec<String>),
    /// Hold the line; write nothing now.
    Held,
}

#[derive(Debug, Default)]
pub struct JobGate {
    /// Ids of `mining.authorize` requests sent by the miner and not yet
    /// answered.
    authorize_ids: HashSet<i64>,
    /// Notifications held so far, verbatim (newline-terminated).
    held: Vec<String>,
    /// Gate is open: everything passes through.
    open: bool,
}

impl JobGate {
    /// Inspect a miner→pool line.
    pub fn on_miner_line(&mut self, line: &str) {
        if self.open {
            return;
        }
        let Some(v) = parse(line) else { return };
        if v.get("method").and_then(Value::as_str) == Some("mining.authorize") {
            if let Some(id) = v.get("id").and_then(Value::as_i64) {
                self.authorize_ids.insert(id);
            }
        }
    }

    /// Decide what to write to the miner for one pool→miner line.
    pub fn on_pool_line(&mut self, line: &str) -> PoolLine {
        // Lines that pass through are written byte-for-byte; only held
        // lines are normalised (they are written after other lines, so
        // they must end in a newline).
        let raw = line.to_string();
        if self.open {
            return PoolLine::Write(vec![raw]);
        }
        let Some(v) = parse(line) else {
            return PoolLine::Write(vec![raw]);
        };
        let is_notification = v.get("method").is_some_and(|m| !m.is_null());
        if is_notification {
            self.held.push(terminated(line));
            if self.held.len() > MAX_HELD {
                tracing::warn!(
                    held = self.held.len(),
                    "no authorize result yet; releasing held pool messages"
                );
                return PoolLine::Write(self.open_and_flush(Vec::new()));
            }
            return PoolLine::Held;
        }
        let is_authorize_response = v
            .get("id")
            .and_then(Value::as_i64)
            .is_some_and(|id| self.authorize_ids.remove(&id));
        if !is_authorize_response {
            return PoolLine::Write(vec![raw]);
        }
        let ok = v.get("result").and_then(Value::as_bool) == Some(true);
        if ok && !self.held.is_empty() {
            tracing::info!(
                held = self.held.len(),
                "authorized; releasing held difficulty and job"
            );
        }
        PoolLine::Write(self.open_and_flush(vec![terminated(line)]))
    }

    /// Open the gate and return `first` followed by the held messages,
    /// collapsed to the latest difficulty and latest job (made clean).
    fn open_and_flush(&mut self, mut out: Vec<String>) -> Vec<String> {
        self.open = true;
        let held = std::mem::take(&mut self.held);
        let mut difficulty = None;
        let mut notify = None;
        for l in held {
            let method =
                parse(&l).and_then(|v| v.get("method").and_then(Value::as_str).map(str::to_string));
            match method.as_deref() {
                Some("mining.set_difficulty") => difficulty = Some(l),
                Some("mining.notify") => notify = Some(l),
                _ => out.push(l),
            }
        }
        out.extend(difficulty);
        if let Some(n) = notify {
            out.push(force_clean(&n).unwrap_or(n));
        }
        out
    }
}

/// `mining.notify` line with `params[8]` (clean_jobs) set to true.
fn force_clean(line: &str) -> Option<String> {
    let mut v = parse(line)?;
    let clean = v.get_mut("params")?.as_array_mut()?.get_mut(8)?;
    if *clean == Value::Bool(true) {
        return None; // already clean; keep the pool's bytes
    }
    *clean = Value::Bool(true);
    let mut s = serde_json::to_string(&v).ok()?;
    s.push('\n');
    Some(s)
}

fn terminated(line: &str) -> String {
    let mut l = line.trim_end_matches(['\r', '\n']).to_string();
    l.push('\n');
    l
}

fn parse(line: &str) -> Option<Value> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    serde_json::from_str(t).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTIFY: &str = r#"{"id":null,"method":"mining.notify","params":["6a","11","aa","bb",[],"20000000","1d00ffff","60000000",false]}"#;
    const NOTIFY2: &str = r#"{"id":null,"method":"mining.notify","params":["6b","11","aa","bb",[],"20000000","1d00ffff","60000001",false]}"#;
    const DIFF: &str = r#"{"id":null,"method":"mining.set_difficulty","params":[2395328]}"#;
    const AUTH: &str = r#"{"id":4,"method":"mining.authorize","params":["w","x"]}"#;
    const AUTH_OK: &str = r#"{"id":4,"result":true,"error":null}"#;

    fn nl(s: &str) -> String {
        format!("{s}\n")
    }

    fn writes(p: PoolLine) -> Vec<String> {
        match p {
            PoolLine::Write(v) => v,
            PoolLine::Held => panic!("expected write, got held"),
        }
    }

    #[test]
    fn holds_work_until_authorize_result_then_releases_latest() {
        let mut g = JobGate::default();
        // Responses before authorize pass through.
        let sub = r#"{"id":2,"result":[[],"abcdef01",8],"error":null}"#;
        assert_eq!(
            writes(g.on_pool_line(&format!("{sub}\n"))),
            vec![format!("{sub}\n")]
        );
        // ckpool pushes work right after subscribe: held.
        assert_eq!(g.on_pool_line(&nl(DIFF)), PoolLine::Held);
        assert_eq!(g.on_pool_line(&nl(NOTIFY)), PoolLine::Held);
        assert_eq!(g.on_pool_line(&nl(NOTIFY2)), PoolLine::Held);
        g.on_miner_line(AUTH);
        let out = writes(g.on_pool_line(&nl(AUTH_OK)));
        assert_eq!(out.len(), 3, "{out:?}");
        assert_eq!(out[0], format!("{AUTH_OK}\n"));
        assert_eq!(out[1], format!("{DIFF}\n"));
        let n: Value = serde_json::from_str(&out[2]).unwrap();
        assert_eq!(n["params"][0], "6b", "only the latest job");
        assert_eq!(n["params"][8], true, "released job must be clean");

        // Gate is open: later work passes untouched.
        assert_eq!(
            writes(g.on_pool_line(&nl(NOTIFY))),
            vec![format!("{NOTIFY}\n")]
        );
    }

    #[test]
    fn nothing_held_when_job_comes_after_authorize() {
        let mut g = JobGate::default();
        g.on_miner_line(AUTH);
        assert_eq!(
            writes(g.on_pool_line(&nl(AUTH_OK))),
            vec![format!("{AUTH_OK}\n")]
        );
        assert_eq!(
            writes(g.on_pool_line(&nl(NOTIFY))),
            vec![format!("{NOTIFY}\n")]
        );
    }

    #[test]
    fn failed_authorize_releases_and_opens() {
        let mut g = JobGate::default();
        assert_eq!(g.on_pool_line(&nl(NOTIFY)), PoolLine::Held);
        g.on_miner_line(AUTH);
        let fail = r#"{"id":4,"result":false,"error":[24,"unauthorized",null]}"#;
        let out = writes(g.on_pool_line(&nl(fail)));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], format!("{fail}\n"));
        assert_eq!(writes(g.on_pool_line(&nl(DIFF))), vec![format!("{DIFF}\n")]);
    }

    #[test]
    fn other_notifications_are_released_in_order_before_work() {
        let mut g = JobGate::default();
        let en = r#"{"id":null,"method":"mining.set_extranonce","params":["aa",8]}"#;
        assert_eq!(g.on_pool_line(&nl(DIFF)), PoolLine::Held);
        assert_eq!(g.on_pool_line(&nl(en)), PoolLine::Held);
        assert_eq!(g.on_pool_line(&nl(NOTIFY)), PoolLine::Held);
        g.on_miner_line(AUTH);
        let out = writes(g.on_pool_line(&nl(AUTH_OK)));
        assert_eq!(out[1], format!("{en}\n"));
        assert_eq!(out[2], format!("{DIFF}\n"));
        assert!(out[3].contains("mining.notify"));
    }

    #[test]
    fn cap_releases_if_miner_never_authorizes() {
        let mut g = JobGate::default();
        for _ in 0..MAX_HELD {
            assert_eq!(g.on_pool_line(&nl(NOTIFY)), PoolLine::Held);
        }
        let out = writes(g.on_pool_line(&nl(DIFF)));
        assert_eq!(out.len(), 2); // latest difficulty + latest job
        assert_eq!(writes(g.on_pool_line(&nl(NOTIFY))).len(), 1);
    }

    #[test]
    fn non_authorize_responses_do_not_open() {
        let mut g = JobGate::default();
        g.on_miner_line(AUTH);
        assert_eq!(g.on_pool_line(&nl(NOTIFY)), PoolLine::Held);
        let cfg = r#"{"id":1,"result":{"version-rolling":true},"error":null}"#;
        assert_eq!(writes(g.on_pool_line(&nl(cfg))).len(), 1);
        assert_eq!(g.on_pool_line(&nl(DIFF)), PoolLine::Held);
    }
}
