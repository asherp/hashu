//! Per-connection Stratum V1 state, fed by the same lines the metrics
//! observer sees.
//!
//! Tracks just enough to reconstruct the share preimage on accepted
//! submits:
//!
//! - **`extranonce1`** + **`extranonce2_size`**: from the `mining.subscribe`
//!   response (server → miner). Pools may re-issue these mid-session via
//!   `mining.set_extranonce`; we honor that.
//! - **`current_difficulty`**: from `mining.set_difficulty` (server →
//!   miner). Used to encode the share leaf's `target_bits`.
//! - **`jobs[job_id]`**: from `mining.notify` (server → miner). When
//!   `clean_jobs` is true we drop prior jobs (the spec mandates miners
//!   abandon them). Capped at [`MAX_JOBS`] regardless to bound memory.
//!
//! Robust to malformed lines: parse functions return `Option`, and
//! [`SessionState::ingest`] silently skips anything it can't make sense of.
//! The metrics path in `observe.rs` keeps its own loose look at the same
//! traffic — they're independent.

use std::collections::HashMap;

use serde_json::Value;

use super::header::{parse_be_u32_hex, parse_branch_entry_hex, parse_prev_hash_hex, JobTemplate};
use super::observe::Direction;

/// Hard cap on retained jobs. Pools rarely keep more than a handful in
/// flight; this just prevents an unbounded grow on a misbehaving upstream.
pub const MAX_JOBS: usize = 32;

#[derive(Debug, Default)]
pub struct SessionState {
    pub extranonce1: Option<Vec<u8>>,
    pub extranonce2_size: Option<usize>,
    pub current_difficulty: Option<f64>,
    pub jobs: HashMap<String, JobTemplate>,
    /// Insertion order of `jobs`, used to evict the oldest when over cap.
    job_order: Vec<String>,
    /// `id → method` map for in-flight submits, so we can credit the
    /// matching response. Decoupled from the `observe` module's identical
    /// map (they don't share state — this one carries the parsed submit
    /// fields too).
    pending_submits: HashMap<i64, PendingSubmit>,
}

/// A `mining.submit` that's been parsed and is waiting on the pool's
/// accept/reject response so we can decide whether to commit a leaf.
#[derive(Debug, Clone)]
pub struct PendingSubmit {
    pub job_id: String,
    pub extranonce2: Vec<u8>,
    pub ntime: u32,
    pub nonce: u32,
    pub version_mask: Option<u32>,
    /// Difficulty in force at the moment of submit, captured to avoid
    /// races if `mining.set_difficulty` lands between submit and ack.
    pub difficulty_at_submit: Option<f64>,
}

/// What `ingest` decided to do with a line. Carries enough info for the
/// caller to act on *accepted* shares without having to re-parse anything.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    Nothing,
    SubscribeAck,
    SetDifficulty(f64),
    NotifyJob(String),
    SubmitOpened(i64),
    SubmitAccepted {
        id: i64,
        submit: PendingSubmit,
        // Boxed: the inline form makes `SessionEvent` ~256 bytes thanks to
        // `JobTemplate`'s prevhash + merkle_branch. The accepted-share path
        // is rare enough that one heap alloc is fine.
        job: Box<JobTemplate>,
        extranonce1: Vec<u8>,
    },
    SubmitRejected {
        id: i64,
    },
}

impl SessionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspect a stratum line and update state. Returns a [`SessionEvent`]
    /// describing the parse result so the caller can act (most useful on
    /// `SubmitAccepted`, which carries everything needed to build a share
    /// leaf without further state lookups).
    pub fn ingest(&mut self, line: &str, direction: Direction) -> SessionEvent {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return SessionEvent::Nothing;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return SessionEvent::Nothing,
        };

        match direction {
            Direction::M2P => self.ingest_m2p(&v),
            Direction::P2M => self.ingest_p2m(&v),
        }
    }

    fn ingest_m2p(&mut self, v: &Value) -> SessionEvent {
        let method = v.get("method").and_then(|m| m.as_str());
        let id = v.get("id").and_then(|x| x.as_i64());
        if method != Some("mining.submit") {
            return SessionEvent::Nothing;
        }
        let id = match id {
            Some(i) => i,
            None => return SessionEvent::Nothing,
        };
        let params = match v.get("params").and_then(|p| p.as_array()) {
            Some(p) => p,
            None => return SessionEvent::Nothing,
        };
        let pending = match parse_submit_params(params, self.current_difficulty) {
            Some(p) => p,
            None => return SessionEvent::Nothing,
        };
        self.pending_submits.insert(id, pending);
        SessionEvent::SubmitOpened(id)
    }

    fn ingest_p2m(&mut self, v: &Value) -> SessionEvent {
        // Notifications carry a method and id=null; responses carry an id
        // and no method. Treat each separately.
        let method = v.get("method").and_then(|m| m.as_str());
        let id = v.get("id").and_then(|x| x.as_i64());

        if let Some(method) = method {
            return self.ingest_p2m_notification(method, v);
        }

        let id = match id {
            Some(i) => i,
            None => return SessionEvent::Nothing,
        };
        let pending = match self.pending_submits.remove(&id) {
            Some(p) => p,
            None => return SessionEvent::Nothing,
        };
        let accepted = matches!(v.get("result").and_then(|r| r.as_bool()), Some(true));
        if !accepted {
            return SessionEvent::SubmitRejected { id };
        }

        let job = match self.jobs.get(&pending.job_id) {
            Some(j) => j.clone(),
            None => return SessionEvent::Nothing,
        };
        let en1 = match self.extranonce1.clone() {
            Some(e) => e,
            None => return SessionEvent::Nothing,
        };
        SessionEvent::SubmitAccepted {
            id,
            submit: pending,
            job: Box::new(job),
            extranonce1: en1,
        }
    }

    fn ingest_p2m_notification(&mut self, method: &str, v: &Value) -> SessionEvent {
        let params = v.get("params").and_then(|p| p.as_array());
        match method {
            "mining.set_difficulty" => match params.and_then(|p| p.first()).and_then(|x| x.as_f64()) {
                Some(d) => {
                    self.current_difficulty = Some(d);
                    SessionEvent::SetDifficulty(d)
                }
                None => SessionEvent::Nothing,
            },
            "mining.set_extranonce" => {
                if let Some(p) = params {
                    self.absorb_extranonce(p);
                }
                SessionEvent::Nothing
            }
            "mining.notify" => match params.and_then(|p| parse_notify_params(p.as_slice())) {
                Some(job) => {
                    let id = job.job_id.clone();
                    let clean_jobs = v
                        .get("params")
                        .and_then(|p| p.as_array())
                        .and_then(|p| p.get(8))
                        .and_then(|c| c.as_bool())
                        .unwrap_or(false);
                    if clean_jobs {
                        self.jobs.clear();
                        self.job_order.clear();
                    }
                    self.insert_job(id.clone(), job);
                    SessionEvent::NotifyJob(id)
                }
                None => SessionEvent::Nothing,
            },
            _ => SessionEvent::Nothing,
        }
    }

    fn insert_job(&mut self, id: String, job: JobTemplate) {
        if !self.jobs.contains_key(&id) {
            self.job_order.push(id.clone());
            while self.job_order.len() > MAX_JOBS {
                let evict = self.job_order.remove(0);
                self.jobs.remove(&evict);
            }
        }
        self.jobs.insert(id, job);
    }

    /// Subscribe responses are id-correlated to the miner's request. We
    /// don't track the request id, so we accept any response whose
    /// `result` shape matches `[subscriptions, extranonce1_hex,
    /// extranonce2_size_int]`. This is the canonical Stratum V1 mining
    /// subscribe response and isn't used by other methods.
    pub fn try_absorb_subscribe_response(&mut self, line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return false;
        }
        let v: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => return false,
        };
        if v.get("method").and_then(|m| m.as_str()).is_some() {
            return false; // notifications never carry a result tuple
        }
        let result = match v.get("result").and_then(|r| r.as_array()) {
            Some(r) => r,
            None => return false,
        };
        if result.len() != 3 {
            return false;
        }
        let en1_hex = match result[1].as_str() {
            Some(s) => s,
            None => return false,
        };
        let en2_size = match result[2].as_u64() {
            Some(n) => n as usize,
            None => return false,
        };
        let en1 = match hex::decode(en1_hex) {
            Ok(b) => b,
            Err(_) => return false,
        };
        self.extranonce1 = Some(en1);
        self.extranonce2_size = Some(en2_size);
        true
    }

    fn absorb_extranonce(&mut self, params: &[Value]) {
        let en1_hex = params.first().and_then(|x| x.as_str());
        let en2_size = params.get(1).and_then(|x| x.as_u64());
        if let (Some(hex_s), Some(sz)) = (en1_hex, en2_size) {
            if let Ok(b) = hex::decode(hex_s) {
                self.extranonce1 = Some(b);
                self.extranonce2_size = Some(sz as usize);
            }
        }
    }
}

fn parse_submit_params(params: &[Value], current_difficulty: Option<f64>) -> Option<PendingSubmit> {
    // Standard form:
    //   [worker, job_id, extranonce2_hex, ntime_hex, nonce_hex,
    //    (optional) version_mask_hex]
    if params.len() < 5 {
        return None;
    }
    let job_id = params[1].as_str()?.to_string();
    let extranonce2 = hex::decode(params[2].as_str()?).ok()?;
    let ntime = parse_be_u32_hex(params[3].as_str()?, "submit.ntime").ok()?;
    let nonce = parse_be_u32_hex(params[4].as_str()?, "submit.nonce").ok()?;
    let version_mask = match params.get(5).and_then(|x| x.as_str()) {
        Some(s) => Some(parse_be_u32_hex(s, "submit.version_mask").ok()?),
        None => None,
    };
    Some(PendingSubmit {
        job_id,
        extranonce2,
        ntime,
        nonce,
        version_mask,
        difficulty_at_submit: current_difficulty,
    })
}

fn parse_notify_params(params: &[Value]) -> Option<JobTemplate> {
    // [job_id, prevhash, coinb1, coinb2, merkle_branch, version, nbits,
    //  ntime, clean_jobs]
    if params.len() < 9 {
        return None;
    }
    let job_id = params[0].as_str()?.to_string();
    let prev_hash = parse_prev_hash_hex(params[1].as_str()?).ok()?;
    let coinb1 = hex::decode(params[2].as_str()?).ok()?;
    let coinb2 = hex::decode(params[3].as_str()?).ok()?;
    let branch_arr = params[4].as_array()?;
    let mut merkle_branch = Vec::with_capacity(branch_arr.len());
    for entry in branch_arr {
        let s = entry.as_str()?;
        merkle_branch.push(parse_branch_entry_hex(s).ok()?);
    }
    let version = parse_be_u32_hex(params[5].as_str()?, "notify.version").ok()?;
    let nbits = parse_be_u32_hex(params[6].as_str()?, "notify.nbits").ok()?;
    let ntime = parse_be_u32_hex(params[7].as_str()?, "notify.ntime").ok()?;
    Some(JobTemplate {
        job_id,
        prev_hash,
        coinb1,
        coinb2,
        merkle_branch,
        version,
        nbits,
        ntime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify_line(job_id: &str) -> String {
        // 32-byte prevhash hex (all 0x11 — wire = natural for this constant
        // pattern), coinb1/coinb2 of 4 bytes each, no merkle branch, version
        // 0x20000000, nbits 0x1d00ffff, ntime 0x60000000, clean_jobs=false.
        format!(
            r#"{{"id":null,"method":"mining.notify","params":["{}","{}","aabbccdd","11223344",[],"20000000","1d00ffff","60000000",false]}}"#,
            job_id,
            "11".repeat(32),
        )
    }

    fn subscribe_response_line() -> String {
        r#"{"id":1,"result":[[["mining.set_difficulty","sub1"],["mining.notify","sub2"]],"abcdef01",4],"error":null}"#
            .to_string()
    }

    #[test]
    fn subscribe_response_populates_extranonces() {
        let mut s = SessionState::new();
        assert!(s.try_absorb_subscribe_response(&subscribe_response_line()));
        assert_eq!(s.extranonce1.as_deref(), Some(&[0xab, 0xcd, 0xef, 0x01][..]));
        assert_eq!(s.extranonce2_size, Some(4));
    }

    #[test]
    fn malformed_subscribe_response_is_no_op() {
        let mut s = SessionState::new();
        assert!(!s.try_absorb_subscribe_response("not json"));
        assert!(!s.try_absorb_subscribe_response(r#"{"id":1,"result":[1,2,3,4]}"#));
        assert!(s.extranonce1.is_none());
    }

    #[test]
    fn set_difficulty_records_value() {
        let mut s = SessionState::new();
        let line = r#"{"id":null,"method":"mining.set_difficulty","params":[1024]}"#;
        let ev = s.ingest(line, Direction::P2M);
        matches!(ev, SessionEvent::SetDifficulty(_));
        assert_eq!(s.current_difficulty, Some(1024.0));
    }

    #[test]
    fn notify_records_a_job() {
        let mut s = SessionState::new();
        let ev = s.ingest(&notify_line("J1"), Direction::P2M);
        assert!(matches!(ev, SessionEvent::NotifyJob(ref id) if id == "J1"));
        assert!(s.jobs.contains_key("J1"));
    }

    #[test]
    fn clean_jobs_clears_prior_notify() {
        let mut s = SessionState::new();
        s.ingest(&notify_line("J1"), Direction::P2M);
        // Build a clean_jobs=true variant
        let line = format!(
            r#"{{"id":null,"method":"mining.notify","params":["J2","{}","aabbccdd","11223344",[],"20000000","1d00ffff","60000000",true]}}"#,
            "11".repeat(32),
        );
        s.ingest(&line, Direction::P2M);
        assert!(!s.jobs.contains_key("J1"));
        assert!(s.jobs.contains_key("J2"));
    }

    #[test]
    fn job_cap_evicts_oldest() {
        let mut s = SessionState::new();
        for i in 0..(MAX_JOBS + 5) {
            s.ingest(&notify_line(&format!("J{i}")), Direction::P2M);
        }
        assert_eq!(s.jobs.len(), MAX_JOBS);
        // Earliest insertions are gone
        for i in 0..5 {
            assert!(!s.jobs.contains_key(&format!("J{i}")));
        }
        // Latest is retained
        assert!(s.jobs.contains_key(&format!("J{}", MAX_JOBS + 4)));
    }

    #[test]
    fn submit_then_accepted_yields_full_event() {
        let mut s = SessionState::new();
        assert!(s.try_absorb_subscribe_response(&subscribe_response_line()));
        s.ingest(
            r#"{"id":null,"method":"mining.set_difficulty","params":[1.0]}"#,
            Direction::P2M,
        );
        s.ingest(&notify_line("J1"), Direction::P2M);
        let submit = r#"{"id":42,"method":"mining.submit","params":["w.1","J1","11223344","60000001","deadbeef"]}"#;
        assert!(matches!(
            s.ingest(submit, Direction::M2P),
            SessionEvent::SubmitOpened(42)
        ));

        let resp = r#"{"id":42,"result":true,"error":null}"#;
        let ev = s.ingest(resp, Direction::P2M);
        match ev {
            SessionEvent::SubmitAccepted {
                id,
                submit,
                job,
                extranonce1,
            } => {
                assert_eq!(id, 42);
                assert_eq!(submit.job_id, "J1");
                assert_eq!(submit.nonce, 0xdead_beef);
                assert_eq!(submit.ntime, 0x6000_0001);
                assert_eq!(submit.difficulty_at_submit, Some(1.0));
                assert_eq!(job.job_id, "J1");
                assert_eq!(extranonce1, vec![0xab, 0xcd, 0xef, 0x01]);
            }
            other => panic!("expected SubmitAccepted, got {other:?}"),
        }
    }

    #[test]
    fn submit_then_rejected_emits_rejected_event() {
        let mut s = SessionState::new();
        s.try_absorb_subscribe_response(&subscribe_response_line());
        s.ingest(&notify_line("J1"), Direction::P2M);
        let submit = r#"{"id":7,"method":"mining.submit","params":["w","J1","11223344","60000000","deadbeef"]}"#;
        s.ingest(submit, Direction::M2P);
        let resp = r#"{"id":7,"result":false,"error":[23,"Invalid","trace"]}"#;
        assert!(matches!(
            s.ingest(resp, Direction::P2M),
            SessionEvent::SubmitRejected { id: 7 }
        ));
    }

    #[test]
    fn accepted_response_without_job_or_extranonce_is_dropped() {
        let mut s = SessionState::new();
        // No subscribe → no extranonce1; no notify → no job.
        let submit = r#"{"id":1,"method":"mining.submit","params":["w","JX","11223344","60000000","deadbeef"]}"#;
        s.ingest(submit, Direction::M2P);
        let resp = r#"{"id":1,"result":true,"error":null}"#;
        assert!(matches!(s.ingest(resp, Direction::P2M), SessionEvent::Nothing));
    }

    #[test]
    fn malformed_lines_are_no_op() {
        let mut s = SessionState::new();
        assert!(matches!(
            s.ingest("not json", Direction::M2P),
            SessionEvent::Nothing
        ));
        assert!(matches!(
            s.ingest("{}", Direction::P2M),
            SessionEvent::Nothing
        ));
        assert!(matches!(
            s.ingest("\n", Direction::M2P),
            SessionEvent::Nothing
        ));
    }

    #[test]
    fn version_mask_in_submit_is_captured() {
        let mut s = SessionState::new();
        s.try_absorb_subscribe_response(&subscribe_response_line());
        s.ingest(&notify_line("J1"), Direction::P2M);
        let submit = r#"{"id":9,"method":"mining.submit","params":["w","J1","11223344","60000000","deadbeef","00000004"]}"#;
        s.ingest(submit, Direction::M2P);
        let resp = r#"{"id":9,"result":true,"error":null}"#;
        match s.ingest(resp, Direction::P2M) {
            SessionEvent::SubmitAccepted { submit, .. } => {
                assert_eq!(submit.version_mask, Some(0x0000_0004));
            }
            other => panic!("expected SubmitAccepted, got {other:?}"),
        }
    }
}
