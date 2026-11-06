//! Re-send the current job right after a successful `mining.authorize`.
//!
//! ckpool (and some other pools) push `mining.set_difficulty` and
//! `mining.notify` as soon as `mining.subscribe` is answered, *before* the
//! `mining.authorize` response. Firmware talking to the pool directly
//! copes, but relays such as MiningRigRentals' treat the work they got
//! before authorize as unusable: they see authorize succeed, get no job
//! after it, decide the pool "does not send jobs", and drop the session
//! within milliseconds. Shares on the pre-authorize job then come back
//! stale.
//!
//! [`JobReplay`] watches both directions. When the pool's response to an
//! authorize the miner sent is `true`, it hands back the latest
//! difficulty and job seen so far (the job forced to `clean_jobs = true`)
//! so the proxy writes them to the miner straight after the authorize
//! result. It fires once per connection; later notifies pass through as
//! usual. If no job has arrived by then there is nothing to replay and
//! the pool's next notify reaches the miner normally.

use std::collections::HashSet;

use serde_json::Value;

#[derive(Debug, Default)]
pub struct JobReplay {
    /// Ids of `mining.authorize` requests sent by the miner and not yet
    /// answered.
    authorize_ids: HashSet<i64>,
    /// Latest `mining.set_difficulty` line from the pool, verbatim.
    difficulty: Option<String>,
    /// Latest `mining.notify` from the pool.
    notify: Option<Value>,
    /// Replay already happened (or authorize already succeeded).
    done: bool,
}

impl JobReplay {
    /// Inspect a miner→pool line.
    pub fn on_miner_line(&mut self, line: &str) {
        if self.done {
            return;
        }
        let Some(v) = parse(line) else { return };
        if v.get("method").and_then(Value::as_str) == Some("mining.authorize") {
            if let Some(id) = v.get("id").and_then(Value::as_i64) {
                self.authorize_ids.insert(id);
            }
        }
    }

    /// Inspect a pool→miner line. Returns lines (newline-terminated) to
    /// write to the miner *after* this one.
    pub fn on_pool_line(&mut self, line: &str) -> Vec<String> {
        if self.done {
            return Vec::new();
        }
        let Some(v) = parse(line) else {
            return Vec::new();
        };
        match v.get("method").and_then(Value::as_str) {
            Some("mining.set_difficulty") => {
                let mut l = line.trim_end().to_string();
                l.push('\n');
                self.difficulty = Some(l);
                return Vec::new();
            }
            Some("mining.notify") => {
                self.notify = Some(v);
                return Vec::new();
            }
            Some(_) => return Vec::new(),
            None => {}
        }
        let Some(id) = v.get("id").and_then(Value::as_i64) else {
            return Vec::new();
        };
        if !self.authorize_ids.remove(&id) {
            return Vec::new();
        }
        if v.get("result").and_then(Value::as_bool) != Some(true) {
            return Vec::new();
        }
        self.done = true;
        let Some(mut notify) = self.notify.take() else {
            return Vec::new();
        };
        if let Some(clean) = notify
            .get_mut("params")
            .and_then(Value::as_array_mut)
            .and_then(|p| p.get_mut(8))
        {
            *clean = Value::Bool(true);
        }
        let mut out = Vec::with_capacity(2);
        if let Some(d) = self.difficulty.take() {
            out.push(d);
        }
        if let Ok(mut s) = serde_json::to_string(&notify) {
            s.push('\n');
            out.push(s);
        }
        out
    }
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
    const DIFF: &str = r#"{"id":null,"method":"mining.set_difficulty","params":[2395328]}"#;

    #[test]
    fn replays_job_sent_before_authorize_result() {
        let mut r = JobReplay::default();
        r.on_miner_line(r#"{"id":4,"method":"mining.authorize","params":["w","x"]}"#);
        assert!(r.on_pool_line(DIFF).is_empty());
        assert!(r.on_pool_line(NOTIFY).is_empty());
        let out = r.on_pool_line(r#"{"id":4,"result":true,"error":null}"#);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0], format!("{DIFF}\n"));
        let n: Value = serde_json::from_str(&out[1]).unwrap();
        assert_eq!(n["method"], "mining.notify");
        assert_eq!(n["params"][0], "6a");
        assert_eq!(n["params"][8], true, "replayed job must be clean");
        assert!(out[1].ends_with('\n'));

        // Once only: later notifies and responses pass untouched.
        assert!(r.on_pool_line(NOTIFY).is_empty());
        assert!(r
            .on_pool_line(r#"{"id":4,"result":true,"error":null}"#)
            .is_empty());
    }

    #[test]
    fn nothing_to_replay_when_job_comes_after_authorize() {
        let mut r = JobReplay::default();
        r.on_miner_line(r#"{"id":2,"method":"mining.authorize","params":["w","x"]}"#);
        assert!(r
            .on_pool_line(r#"{"id":2,"result":true,"error":null}"#)
            .is_empty());
        assert!(r.on_pool_line(NOTIFY).is_empty());
    }

    #[test]
    fn failed_authorize_does_not_replay() {
        let mut r = JobReplay::default();
        r.on_miner_line(r#"{"id":2,"method":"mining.authorize","params":["w","x"]}"#);
        r.on_pool_line(NOTIFY);
        assert!(r
            .on_pool_line(r#"{"id":2,"result":false,"error":[24,"unauthorized",null]}"#)
            .is_empty());
    }

    #[test]
    fn other_responses_do_not_trigger() {
        let mut r = JobReplay::default();
        r.on_miner_line(r#"{"id":2,"method":"mining.authorize","params":["w","x"]}"#);
        r.on_pool_line(NOTIFY);
        // configure / subscribe responses with other ids.
        assert!(r
            .on_pool_line(r#"{"id":1,"result":{"version-rolling":true},"error":null}"#)
            .is_empty());
        assert!(r
            .on_pool_line(r#"{"id":3,"result":true,"error":null}"#)
            .is_empty());
    }
}
