//! When each rig was connected: one [`Session`] per miner TCP session,
//! from connect to disconnect, so the operator's hashrate chart can tell a
//! rig that is down (no session) from one that is slow (a session with
//! few shares).
//!
//! A session is the miner's own connection, not the daemon's side of it.
//! Behind `hashu-relay` the miner stays connected while the daemon
//! restarts: the relay dials the daemon again and sends a
//! [`crate::stratum::RESUME_METHOD`] line carrying the time the miner first
//! connected, and the daemon picks the session it already had for that
//! rig back up ([`SessionLog::open`] with `resumed`). So:
//!
//! - a daemon restart behind the relay is not a disconnect: connections
//!   the daemon drops while it shuts down ([`SessionLog::begin_shutdown`])
//!   leave their sessions open, they are saved open, and a resume within
//!   [`RESUME_GRACE`] of the next start continues them;
//! - a session saved open that no resume claims in time ended when the
//!   daemon stopped (the relay gave up, or there is no relay and the miner
//!   reconnected as a new session), at the last time the daemon was known
//!   to be up;
//! - a connection the daemon closes on purpose (to move a miner) that the
//!   relay resumes straight away reopens the same session.
//!
//! Sessions are keyed by rig label (the miner's authorize user) and start
//! time. Nothing here feeds redemption metering or the reliability record.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::credit::ConnectionId;

/// How long after a start a session saved open may still be resumed.
/// Longer than the relay's default hold (300 s).
pub const RESUME_GRACE: Duration = Duration::from_secs(360);
/// How far apart the relay's and the daemon's view of when a miner
/// connected may be and still name the same session.
pub const MATCH_WINDOW: Duration = Duration::from_secs(120);
/// Closed sessions are kept in memory this long after they end (the
/// chart shows at most an hour; the ops database keeps them for longer).
pub const KEEP_CLOSED: Duration = Duration::from_secs(2 * 3600);

/// One miner connection, connect to disconnect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// The miner's authorize user, e.g. `alice.rig1`.
    pub rig: String,
    /// Unix ms the miner connected.
    pub start_ms: u64,
    /// Unix ms it disconnected; `None` while connected.
    pub end_ms: Option<u64>,
}

/// Every rig's sessions. See the module docs.
#[derive(Debug)]
pub struct SessionLog {
    sessions: BTreeMap<u64, Session>,
    next: u64,
    /// The session each live connection belongs to.
    open: HashMap<ConnectionId, u64>,
    /// Sessions saved open by an earlier run that no connection has
    /// claimed yet, with when they stop waiting (Unix ms).
    held: HashMap<u64, u64>,
    /// When the earlier run was last known to be up: held sessions that
    /// are never resumed end here.
    alive_ms: u64,
    /// Sessions are recorded from this time on (Unix ms); before it the
    /// log knows nothing.
    since_ms: u64,
    shutting_down: bool,
}

impl SessionLog {
    pub fn new(now_ms: u64) -> Self {
        Self {
            sessions: BTreeMap::new(),
            next: 0,
            open: HashMap::new(),
            held: HashMap::new(),
            alive_ms: now_ms,
            since_ms: now_ms,
            shutting_down: false,
        }
    }

    /// Since when sessions are recorded (Unix ms).
    pub fn since_ms(&self) -> u64 {
        self.since_ms
    }

    fn insert(&mut self, s: Session) -> u64 {
        let id = self.next;
        self.next += 1;
        self.sessions.insert(id, s);
        id
    }

    fn claimable(&self, id: u64) -> bool {
        !self.open.values().any(|&o| o == id)
    }

    /// Connection `conn` of rig `rig` is set up. `resumed` is `None` for a
    /// fresh miner connection; `Some(since)` when a relay resumed it, with
    /// the time the miner connected if the relay sent one.
    pub fn open(
        &mut self,
        conn: ConnectionId,
        rig: &str,
        now_ms: u64,
        resumed: Option<Option<u64>>,
    ) {
        self.expire(now_ms);
        if let Some(id) = self.open.remove(&conn) {
            // Re-identified under another name: start over under it.
            self.end(id, now_ms);
        }
        let found = match resumed {
            // The relay says when the miner connected: that session, open
            // (held from an earlier run) or just closed by us.
            // The daemon saw the miner's first connection a moment after
            // the relay did (it waits for the authorize), so the closest
            // start within [`MATCH_WINDOW`].
            Some(Some(since)) => self
                .sessions
                .iter()
                .filter(|(id, s)| {
                    s.rig == rig
                        && s.start_ms.abs_diff(since) <= MATCH_WINDOW.as_millis() as u64
                        && self.claimable(**id)
                })
                .min_by_key(|(_, s)| s.start_ms.abs_diff(since))
                .map(|(id, _)| *id),
            // An older relay: the oldest held session of this rig.
            Some(None) => self
                .held
                .keys()
                .filter(|id| self.sessions.get(id).is_some_and(|s| s.rig == rig))
                .min_by_key(|id| self.sessions[id].start_ms)
                .copied(),
            None => None,
        };
        let id = match found {
            Some(id) => {
                self.held.remove(&id);
                if let Some(s) = self.sessions.get_mut(&id) {
                    s.end_ms = None;
                }
                id
            }
            None => {
                let start_ms = match resumed {
                    Some(Some(since)) if since <= now_ms => since,
                    _ => now_ms,
                };
                self.insert(Session {
                    rig: rig.to_string(),
                    start_ms,
                    end_ms: None,
                })
            }
        };
        self.open.insert(conn, id);
    }

    /// A relay said connection `conn` resumes a miner that connected at
    /// `since` (if it said). Moves the connection onto that session if it
    /// was opened as a new one.
    pub fn resumed(&mut self, conn: ConnectionId, now_ms: u64, since: Option<u64>) {
        let Some(&id) = self.open.get(&conn) else {
            return;
        };
        let Some(s) = self.sessions.get(&id) else {
            return;
        };
        let window = MATCH_WINDOW.as_millis() as u64;
        if let Some(t) = since {
            let mine = t.abs_diff(s.start_ms);
            let closer = self.sessions.iter().any(|(i, x)| {
                *i != id && x.rig == s.rig && self.claimable(*i) && x.start_ms.abs_diff(t) < mine
            });
            if mine <= window && !closer {
                // Already the session it resumes.
                return;
            }
        }
        let rig = s.rig.clone();
        // Drop the session this connection started as, then claim the one
        // it resumes.
        self.open.remove(&conn);
        self.sessions.remove(&id);
        self.open(conn, &rig, now_ms, Some(since));
    }

    fn end(&mut self, id: u64, now_ms: u64) {
        if let Some(s) = self.sessions.get_mut(&id) {
            if s.end_ms.is_none() {
                s.end_ms = Some(now_ms.max(s.start_ms));
            }
        }
    }

    /// Connection `conn` closed. While the daemon shuts down its sessions
    /// stay open (the miner may well still be connected to a relay).
    pub fn close(&mut self, conn: ConnectionId, now_ms: u64) {
        if let Some(id) = self.open.remove(&conn) {
            if !self.shutting_down {
                self.end(id, now_ms);
            }
        }
    }

    /// The daemon is stopping: connections that close from now on are not
    /// disconnects.
    pub fn begin_shutdown(&mut self) {
        self.shutting_down = true;
    }

    /// End held sessions whose wait ran out, at the last time the earlier
    /// run was up; drop sessions that ended long ago.
    pub fn expire(&mut self, now_ms: u64) {
        let due: Vec<u64> = self
            .held
            .iter()
            .filter(|(_, &until)| now_ms >= until)
            .map(|(id, _)| *id)
            .collect();
        for id in due {
            self.held.remove(&id);
            let alive = self.alive_ms;
            self.end(id, alive);
        }
        let cut = now_ms.saturating_sub(KEEP_CLOSED.as_millis() as u64);
        self.sessions
            .retain(|_, s| s.end_ms.is_none_or(|e| e >= cut));
    }

    /// Sessions open at or after `from_ms`, oldest first.
    pub fn list(&mut self, now_ms: u64, from_ms: u64) -> Vec<Session> {
        self.expire(now_ms);
        let mut v: Vec<Session> = self
            .sessions
            .values()
            .filter(|s| s.end_ms.is_none_or(|e| e >= from_ms))
            .cloned()
            .collect();
        v.sort_by(|a, b| (a.start_ms, &a.rig).cmp(&(b.start_ms, &b.rig)));
        v
    }

    /// Carry on sessions saved by an earlier run that was last up at
    /// `alive_ms`, recording since `since_ms`. Open ones wait
    /// [`RESUME_GRACE`] for their miner's relay to resume them.
    pub fn restore(&mut self, saved: Vec<Session>, alive_ms: u64, since_ms: u64, now_ms: u64) {
        self.alive_ms = alive_ms.min(now_ms);
        self.since_ms = self.since_ms.min(since_ms);
        let until = now_ms + RESUME_GRACE.as_millis() as u64;
        for s in saved {
            if s.start_ms > now_ms
                || self
                    .sessions
                    .values()
                    .any(|x| x.rig == s.rig && x.start_ms == s.start_ms)
            {
                continue;
            }
            let open = s.end_ms.is_none();
            let id = self.insert(s);
            if open {
                self.held.insert(id, until);
            }
        }
        self.expire(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(n: u64) -> ConnectionId {
        ConnectionId(n)
    }

    #[test]
    fn connect_and_disconnect() {
        let mut log = SessionLog::new(0);
        log.open(c(1), "a.rig1", 1_000, None);
        log.close(c(1), 5_000);
        log.open(c(2), "a.rig1", 9_000, None);
        let v = log.list(10_000, 0);
        assert_eq!(
            v,
            vec![
                Session {
                    rig: "a.rig1".into(),
                    start_ms: 1_000,
                    end_ms: Some(5_000)
                },
                Session {
                    rig: "a.rig1".into(),
                    start_ms: 9_000,
                    end_ms: None
                },
            ]
        );
    }

    #[test]
    fn a_restart_behind_the_relay_is_not_a_disconnect() {
        // First run: the miner connects at 1 s; the daemon stops at 50 s.
        let mut first = SessionLog::new(0);
        first.open(c(1), "a.rig1", 1_000, None);
        first.begin_shutdown();
        first.close(c(1), 50_000);
        let saved = first.list(50_000, 0);
        assert_eq!(saved[0].end_ms, None);

        // Second run: the relay resumes it, saying it connected at 1 s.
        let mut second = SessionLog::new(55_000);
        second.restore(saved, 50_000, 0, 55_000);
        second.open(c(7), "a.rig1", 56_000, Some(Some(1_000)));
        // Long after the grace it is still the one open session.
        let v = second.list(1_000_000, 0);
        assert_eq!(
            v,
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None
            }]
        );
        assert_eq!(second.since_ms(), 0);
    }

    #[test]
    fn resume_line_after_the_session_opened_moves_it() {
        // The connection is placed (authorize in the prelude) before the
        // resume line is handled.
        let mut log = SessionLog::new(0);
        log.restore(
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None,
            }],
            50_000,
            0,
            55_000,
        );
        log.open(c(7), "a.rig1", 56_000, None);
        log.resumed(c(7), 56_000, Some(1_000));
        assert_eq!(
            log.list(60_000, 0),
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None
            }]
        );
    }

    #[test]
    fn unclaimed_saved_sessions_end_when_the_daemon_stopped() {
        // No relay: the miner reconnects as a new session.
        let mut log = SessionLog::new(55_000);
        log.restore(
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None,
            }],
            50_000,
            0,
            55_000,
        );
        log.open(c(2), "a.rig1", 58_000, None);
        let later = 55_000 + RESUME_GRACE.as_millis() as u64;
        assert_eq!(
            log.list(later, 0),
            vec![
                Session {
                    rig: "a.rig1".into(),
                    start_ms: 1_000,
                    end_ms: Some(50_000)
                },
                Session {
                    rig: "a.rig1".into(),
                    start_ms: 58_000,
                    end_ms: None
                },
            ]
        );
    }

    #[test]
    fn an_older_relay_claims_the_held_session_by_rig() {
        let mut log = SessionLog::new(55_000);
        log.restore(
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None,
            }],
            50_000,
            0,
            55_000,
        );
        // The connection opened before its resume line was handled.
        log.open(c(2), "a.rig1", 58_000, None);
        log.resumed(c(2), 58_000, None);
        assert_eq!(
            log.list(60_000, 0),
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None
            }]
        );
    }

    #[test]
    fn a_deliberate_close_resumed_by_the_relay_reopens_the_session() {
        let mut log = SessionLog::new(0);
        log.open(c(1), "a.rig1", 1_000, None);
        log.close(c(1), 30_000);
        log.open(c(2), "a.rig1", 30_100, Some(Some(1_000)));
        assert_eq!(
            log.list(31_000, 0),
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None
            }]
        );
    }

    #[test]
    fn a_deliberate_close_then_a_late_resume_line_reopens_the_session() {
        let mut log = SessionLog::new(0);
        log.open(c(1), "a.rig1", 1_000, None);
        log.close(c(1), 30_000);
        // Placed from the prelude first, resume line handled after.
        log.open(c(2), "a.rig1", 30_100, None);
        log.resumed(c(2), 30_100, Some(900));
        assert_eq!(
            log.list(31_000, 0),
            vec![Session {
                rig: "a.rig1".into(),
                start_ms: 1_000,
                end_ms: None
            }]
        );
        log.close(c(2), 40_000);
        assert_eq!(log.list(41_000, 0)[0].end_ms, Some(40_000));
    }
}
