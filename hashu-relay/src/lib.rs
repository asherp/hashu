//! A Stratum V1 relay that keeps miners connected while the Hashu daemon
//! behind it restarts.
//!
//! ```text
//! rigs ⇄ hashu-relay (rarely restarts) ⇄ hashu (Stratum proxy + mint; every deploy)
//! ```
//!
//! The relay forwards lines both ways untouched and remembers each miner's
//! handshake: `mining.configure`, `mining.subscribe`,
//! `mining.extranonce.subscribe`, `mining.suggest_difficulty`,
//! `mining.authorize`, and the extranonce and version-rolling mask the
//! miner was given. When the daemon's side of a session closes (a deploy,
//! a crash, or the daemon moving the miner to another pool by closing its
//! connection) the relay keeps the miner's socket open, dials the daemon
//! again and replays that handshake under its own request ids, after a
//! [`RESUME_METHOD`] line asking for the extranonce the miner already works
//! with. The daemon answers with a fresh difficulty and job, which the
//! miner takes like any job change: no reconnect, so no failing over to a
//! backup pool it might never leave.
//!
//! While the daemon is away the miner's submits are answered as stale
//! (that work is lost either way: its jobs belong to a pool session that
//! ended). The miner is closed, as it would be without the relay, when:
//! the daemon closes it before its handshake finished (e.g. a refused
//! worker), the daemon stays away longer than [`Config::hold`], the
//! replayed authorize is refused, the new session's extranonce differs and
//! the miner can't take `mining.set_extranonce`, the version-rolling mask
//! changed, or sessions keep ending right after resuming.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;

/// Line the relay sends before replaying a miner's handshake:
/// `{"id":null,"method":"hashu.resume","params":["<extranonce1 hex>",
/// <extranonce2 size>, <unix ms the miner connected>]}`. The time lets the
/// daemon carry on the miner's connect/disconnect record instead of
/// counting a restart as a disconnect. Must match `hashu_proxy::stratum::RESUME_METHOD`.
pub const RESUME_METHOD: &str = "hashu.resume";

/// Request ids the relay uses for a replayed handshake. Within 32 bits
/// (some pools read ids as `u32`) and apart from the daemon's own
/// (734_000_001..=734_000_004).
const R_CONFIGURE: i64 = 734_100_001;
const R_SUBSCRIBE: i64 = 734_100_002;
const R_XSUB: i64 = 734_100_003;
const R_SUGGEST: i64 = 734_100_004;
const R_AUTHORIZE: i64 = 734_100_010;
/// Most authorizes (workers) one miner connection may replay.
const MAX_AUTHORIZES: usize = 8;
const RELAY_IDS: std::ops::Range<i64> = R_CONFIGURE..R_AUTHORIZE + MAX_AUTHORIZES as i64;

/// Sessions that end this soon after resuming count as flapping.
const FLAP_WINDOW: Duration = Duration::from_secs(60);
/// Resumes within [`FLAP_WINDOW`] before the relay gives up on a miner.
const FLAP_LIMIT: usize = 5;
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_MIN: Duration = Duration::from_millis(250);
const BACKOFF_MAX: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct Config {
    /// The daemon's Stratum address (`host:port`), resolved on every dial.
    pub upstream: String,
    /// How long a miner is held while the daemon is unreachable.
    pub hold: Duration,
    /// How long a replayed handshake may take (covers the daemon's
    /// startup: it binds its port before it starts accepting).
    pub handshake_timeout: Duration,
}

impl Config {
    pub fn new(upstream: impl Into<String>) -> Self {
        Self {
            upstream: upstream.into(),
            hold: Duration::from_secs(300),
            handshake_timeout: Duration::from_secs(30),
        }
    }
}

/// Bind `listen` and relay every miner that connects. Returns only on a
/// listener error.
pub async fn run(listen: SocketAddr, cfg: Config) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    serve(listener, cfg).await
}

/// [`run`] on a bound listener.
pub async fn serve(listener: TcpListener, cfg: Config) -> Result<()> {
    let cfg = Arc::new(cfg);
    tracing::info!(
        bind = %listener.local_addr()?,
        upstream = %cfg.upstream,
        "stratum relay listening"
    );
    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        let cfg = cfg.clone();
        tokio::spawn(async move {
            tracing::info!(%peer, "miner connected");
            match session(sock, &cfg).await {
                Ok(why) => tracing::info!(%peer, reason = %why, "miner connection closed"),
                Err(e) => tracing::warn!(%peer, error = %e, "miner connection ended with error"),
            }
        });
    }
}

type MinerLines = Lines<BufReader<OwnedReadHalf>>;

struct Daemon {
    lines: Lines<BufReader<OwnedReadHalf>>,
    w: OwnedWriteHalf,
}

impl Daemon {
    async fn dial(addr: &str) -> std::io::Result<Self> {
        let s = tokio::time::timeout(DIAL_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| std::io::Error::other("connect timed out"))??;
        s.set_nodelay(true).ok();
        let (r, w) = s.into_split();
        Ok(Self {
            lines: BufReader::new(r).lines(),
            w,
        })
    }

    async fn send(&mut self, line: &str) -> std::io::Result<()> {
        write_line(&mut self.w, line).await
    }
}

async fn write_line(w: &mut OwnedWriteHalf, line: &str) -> std::io::Result<()> {
    w.write_all(line.trim_end().as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// What the relay knows about one miner's session.
#[derive(Default, Debug)]
struct Facts {
    configure: Option<(Value, Value)>,
    subscribe: Option<(Value, Value)>,
    extranonce_subscribed: bool,
    suggest: Option<Value>,
    /// `(id, params)` of each distinct authorize.
    authorizes: Vec<(Value, Value)>,
    /// Extranonce1 (hex) and extranonce2 size the miner works with.
    extranonce: Option<(String, u64)>,
    mask: Option<String>,
    authorized: bool,
    /// The miner's requests the daemon hasn't answered: id key -> (id, method).
    pending: HashMap<String, (Value, String)>,
    /// When the miner connected to the relay (Unix ms).
    connected_ms: u64,
}

impl Facts {
    /// Subscribed and authorized: a session worth resuming.
    fn established(&self) -> bool {
        self.extranonce.is_some() && self.authorized
    }

    fn on_miner(&mut self, line: &str) {
        let Some(v) = parse(line) else { return };
        let Some(method) = v.get("method").and_then(Value::as_str) else {
            return;
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let params = v.get("params").cloned().unwrap_or(json!([]));
        if !id.is_null() {
            self.pending
                .insert(id.to_string(), (id.clone(), method.to_string()));
        }
        match method {
            "mining.configure" if self.configure.is_none() => self.configure = Some((id, params)),
            "mining.subscribe" if self.subscribe.is_none() => self.subscribe = Some((id, params)),
            "mining.extranonce.subscribe" => self.extranonce_subscribed = true,
            "mining.suggest_difficulty" => self.suggest = Some(params),
            "mining.authorize"
                if self.authorizes.len() < MAX_AUTHORIZES
                    && !self.authorizes.iter().any(|(_, p)| *p == params) =>
            {
                self.authorizes.push((id, params));
            }
            _ => {}
        }
    }

    /// Note a line from the daemon; false if it is an answer to the
    /// relay's own replayed handshake (not for the miner).
    fn on_daemon(&mut self, line: &str) -> bool {
        let Some(v) = parse(line) else { return true };
        if let Some(method) = v.get("method").and_then(Value::as_str) {
            if method == "mining.set_extranonce" {
                if let Some(x) = extranonce_of(v.get("params")) {
                    self.extranonce = Some(x);
                }
            }
            return true;
        }
        let Some(id) = v.get("id").filter(|i| !i.is_null()) else {
            return true;
        };
        if id.as_i64().is_some_and(|i| RELAY_IDS.contains(&i)) {
            return false;
        }
        self.pending.remove(&id.to_string());
        let result = v.get("result");
        if self.subscribe.as_ref().is_some_and(|(i, _)| i == id) {
            if let Some(x) = subscribe_extranonce(result) {
                self.extranonce = Some(x);
            }
        }
        if self.configure.as_ref().is_some_and(|(i, _)| i == id) {
            self.mask = mask_of(result);
        }
        if self.authorizes.iter().any(|(i, _)| i == id) && result == Some(&Value::Bool(true)) {
            self.authorized = true;
        }
        true
    }
}

/// `[en1 hex, en2 size, ..]` -> `(en1, en2 size)`.
fn extranonce_of(params: Option<&Value>) -> Option<(String, u64)> {
    let p = params?.as_array()?;
    let en1 = p.first()?.as_str()?.to_ascii_lowercase();
    let en2 = p.get(1)?.as_u64()?;
    Some((en1, en2))
}

/// A subscribe result `[subscriptions, en1 hex, en2 size]` -> `(en1, en2 size)`.
fn subscribe_extranonce(result: Option<&Value>) -> Option<(String, u64)> {
    let a = result?.as_array()?;
    let en1 = a.get(1)?.as_str()?.to_ascii_lowercase();
    let en2 = a.get(2)?.as_u64()?;
    Some((en1, en2))
}

fn mask_of(result: Option<&Value>) -> Option<String> {
    result?
        .get("version-rolling.mask")?
        .as_str()
        .map(str::to_ascii_lowercase)
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn parse(line: &str) -> Option<Value> {
    serde_json::from_str(line.trim()).ok()
}

enum Lost {
    Miner(String),
    Daemon(String),
}

/// One miner, for as long as it stays connected. Returns why it closed.
async fn session(sock: TcpStream, cfg: &Config) -> Result<String> {
    sock.set_nodelay(true).ok();
    let (r, mut mw) = sock.into_split();
    let mut miner: MinerLines = BufReader::new(r).lines();
    let mut f = Facts {
        connected_ms: unix_ms(),
        ..Facts::default()
    };

    // First connection: the miner's lines wait in its socket until the
    // daemon is reachable.
    let deadline = Instant::now() + cfg.hold;
    let mut backoff = BACKOFF_MIN;
    let mut daemon = loop {
        match Daemon::dial(&cfg.upstream).await {
            Ok(d) => break d,
            Err(e) if Instant::now() + backoff < deadline => {
                tracing::debug!(error = %e, "daemon unreachable; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(BACKOFF_MAX);
            }
            Err(e) => return Ok(format!("daemon unreachable: {e}")),
        }
    };

    let mut resumes: VecDeque<Instant> = VecDeque::new();
    loop {
        let why = match forward(&mut miner, &mut mw, &mut daemon, &mut f).await {
            Lost::Miner(why) => return Ok(why),
            Lost::Daemon(why) => why,
        };
        if !f.established() {
            return Ok(format!(
                "daemon closed the session before the miner was set up ({why})"
            ));
        }
        let now = Instant::now();
        while resumes
            .front()
            .is_some_and(|t| now.duration_since(*t) > FLAP_WINDOW)
        {
            resumes.pop_front();
        }
        if resumes.len() >= FLAP_LIMIT {
            return Ok(format!("daemon keeps closing the session ({why})"));
        }
        tracing::info!(reason = %why, "daemon session ended; holding the miner and resuming");
        let away = Instant::now();
        answer_pending(&mut mw, &mut f).await?;
        match resume(cfg, &mut miner, &mut mw, &mut f).await? {
            Ok(d) => {
                tracing::info!(
                    after_ms = away.elapsed().as_millis() as u64,
                    "miner resumed on the daemon without reconnecting"
                );
                daemon = d;
                resumes.push_back(Instant::now());
            }
            Err(why) => return Ok(why),
        }
    }
}

/// Forward lines both ways until one side goes away.
async fn forward(
    miner: &mut MinerLines,
    mw: &mut OwnedWriteHalf,
    daemon: &mut Daemon,
    f: &mut Facts,
) -> Lost {
    loop {
        tokio::select! {
            l = miner.next_line() => match l {
                Ok(Some(l)) => {
                    f.on_miner(&l);
                    if let Err(e) = daemon.send(&l).await {
                        return Lost::Daemon(format!("write: {e}"));
                    }
                }
                Ok(None) => return Lost::Miner("miner closed".into()),
                Err(e) => return Lost::Miner(format!("miner read: {e}")),
            },
            l = daemon.lines.next_line() => match l {
                Ok(Some(l)) => {
                    if f.on_daemon(&l) {
                        if let Err(e) = write_line(mw, &l).await {
                            return Lost::Miner(format!("miner write: {e}"));
                        }
                    }
                }
                Ok(None) => return Lost::Daemon("daemon closed".into()),
                Err(e) => return Lost::Daemon(format!("read: {e}")),
            },
        }
    }
}

/// Answer the miner's unanswered requests: the session they went to
/// is gone.
async fn answer_pending(mw: &mut OwnedWriteHalf, f: &mut Facts) -> std::io::Result<()> {
    for (_, (id, method)) in f.pending.drain() {
        write_line(mw, &refusal(&id, &method)).await?;
    }
    Ok(())
}

fn refusal(id: &Value, method: &str) -> String {
    let error = if method == "mining.submit" {
        json!([21, "Stale share: pool connection restarting", null])
    } else {
        json!([20, "Pool connection restarting", null])
    };
    json!({"id": id, "result": null, "error": error}).to_string()
}

/// Run `fut` while answering the miner's requests as refused. `None`: the
/// miner went away.
async fn while_away<F: Future>(
    miner: &mut MinerLines,
    mw: &mut OwnedWriteHalf,
    fut: F,
) -> Option<F::Output> {
    tokio::pin!(fut);
    loop {
        tokio::select! {
            out = &mut fut => return Some(out),
            l = miner.next_line() => match l {
                Ok(Some(l)) => {
                    let Some(v) = parse(&l) else { continue };
                    let (Some(id), Some(method)) = (
                        v.get("id").filter(|i| !i.is_null()),
                        v.get("method").and_then(Value::as_str),
                    ) else {
                        continue;
                    };
                    if write_line(mw, &refusal(id, method)).await.is_err() {
                        return None;
                    }
                }
                _ => return None,
            },
        }
    }
}

/// The daemon's answers to a replayed handshake.
struct Replayed {
    daemon: Daemon,
    extranonce: Option<(String, u64)>,
    mask: Option<String>,
    authorized: bool,
    /// Notifications (difficulty, jobs) that arrived during the handshake.
    held: Vec<String>,
}

/// Dial the daemon again and replay the miner's handshake, until it works
/// or [`Config::hold`] runs out. The outer error is an I/O error on the
/// miner; the inner one says why the miner should be closed.
async fn resume(
    cfg: &Config,
    miner: &mut MinerLines,
    mw: &mut OwnedWriteHalf,
    f: &mut Facts,
) -> Result<Result<Daemon, String>> {
    let deadline = Instant::now() + cfg.hold;
    let mut backoff = BACKOFF_MIN;
    let r = loop {
        let attempt = tokio::time::timeout_at(
            deadline.min(Instant::now() + cfg.handshake_timeout),
            replay(&cfg.upstream, f),
        );
        match while_away(miner, mw, attempt).await {
            None => return Ok(Err("miner closed while the daemon was away".into())),
            Some(Ok(Ok(r))) => break r,
            Some(Ok(Err(e))) => tracing::debug!(error = %e, "resume attempt failed"),
            Some(Err(_)) => tracing::debug!("resume attempt timed out"),
        }
        if Instant::now() + backoff >= deadline {
            return Ok(Err(format!(
                "daemon unreachable for {} s",
                cfg.hold.as_secs()
            )));
        }
        if while_away(miner, mw, tokio::time::sleep(backoff))
            .await
            .is_none()
        {
            return Ok(Err("miner closed while the daemon was away".into()));
        }
        backoff = (backoff * 2).min(BACKOFF_MAX);
    };

    if !r.authorized {
        return Ok(Err("daemon refused the miner's authorize on resume".into()));
    }
    if f.configure.is_some() && r.mask != f.mask {
        return Ok(Err(format!(
            "version-rolling mask changed on resume ({:?} -> {:?})",
            f.mask, r.mask
        )));
    }
    if r.extranonce != f.extranonce {
        let Some((en1, en2)) = r.extranonce.clone() else {
            return Ok(Err("daemon sent no extranonce on resume".into()));
        };
        if !f.extranonce_subscribed {
            return Ok(Err(format!(
                "extranonce changed on resume ({:?} -> {en1}/{en2}) and the miner can't take set_extranonce",
                f.extranonce
            )));
        }
        tracing::info!(%en1, en2, "extranonce changed on resume; sending set_extranonce");
        let l = json!({"id": null, "method": "mining.set_extranonce", "params": [en1, en2]});
        write_line(mw, &l.to_string()).await?;
        f.extranonce = r.extranonce.clone();
    }
    for l in &r.held {
        f.on_daemon(l);
        write_line(mw, l).await?;
    }
    Ok(Ok(r.daemon))
}

/// One attempt: dial, send the miner's handshake under relay ids, and
/// read until the subscribe and every authorize are answered.
async fn replay(upstream: &str, f: &Facts) -> std::io::Result<Replayed> {
    let mut d = Daemon::dial(upstream).await?;
    if let Some((en1, en2)) = &f.extranonce {
        d.send(
            &json!({"id": null, "method": RESUME_METHOD, "params": [en1, en2, f.connected_ms]})
                .to_string(),
        )
        .await?;
    }
    if let Some((_, p)) = &f.configure {
        d.send(&json!({"id": R_CONFIGURE, "method": "mining.configure", "params": p}).to_string())
            .await?;
    }
    let sub = f
        .subscribe
        .as_ref()
        .map(|(_, p)| p.clone())
        .unwrap_or(json!([]));
    d.send(&json!({"id": R_SUBSCRIBE, "method": "mining.subscribe", "params": sub}).to_string())
        .await?;
    if f.extranonce_subscribed {
        d.send(
            &json!({"id": R_XSUB, "method": "mining.extranonce.subscribe", "params": []})
                .to_string(),
        )
        .await?;
    }
    if let Some(p) = &f.suggest {
        d.send(
            &json!({"id": R_SUGGEST, "method": "mining.suggest_difficulty", "params": p})
                .to_string(),
        )
        .await?;
    }
    for (i, (_, p)) in f.authorizes.iter().enumerate() {
        let id = R_AUTHORIZE + i as i64;
        d.send(&json!({"id": id, "method": "mining.authorize", "params": p}).to_string())
            .await?;
    }

    let mut extranonce = None;
    let mut mask = None;
    let mut subscribed = false;
    let mut configured = f.configure.is_none();
    let mut auth_left = f.authorizes.len();
    let mut authorized = false;
    let mut held = Vec::new();
    while !(subscribed && configured && auth_left == 0) {
        let Some(l) = d.lines.next_line().await? else {
            return Err(std::io::Error::other("daemon closed during the handshake"));
        };
        let Some(v) = parse(&l) else { continue };
        if v.get("method").and_then(Value::as_str).is_some() {
            held.push(l);
            continue;
        }
        let result = v.get("result");
        match v.get("id").and_then(Value::as_i64) {
            Some(R_SUBSCRIBE) => {
                subscribed = true;
                extranonce = subscribe_extranonce(result);
            }
            Some(R_CONFIGURE) => {
                configured = true;
                mask = mask_of(result);
            }
            Some(i) if (R_AUTHORIZE..R_AUTHORIZE + MAX_AUTHORIZES as i64).contains(&i) => {
                auth_left = auth_left.saturating_sub(1);
                authorized |= result == Some(&Value::Bool(true));
            }
            _ => {}
        }
    }
    Ok(Replayed {
        daemon: d,
        extranonce,
        mask,
        authorized,
        held,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learns_the_handshake() {
        let mut f = Facts::default();
        f.on_miner(r#"{"id":1,"method":"mining.configure","params":[["version-rolling"],{"version-rolling.mask":"ffffffff"}]}"#);
        f.on_miner(r#"{"id":2,"method":"mining.subscribe","params":["bitaxe/2.4"]}"#);
        f.on_miner(r#"{"id":3,"method":"mining.authorize","params":["alice.rig1","x"]}"#);
        assert!(f.on_daemon(r#"{"id":1,"result":{"version-rolling":true,"version-rolling.mask":"1FFFE000"},"error":null}"#));
        assert!(f.on_daemon(r#"{"id":2,"result":[[["mining.notify","s"]],"00AB",4],"error":null}"#));
        assert!(!f.established());
        assert!(f.on_daemon(r#"{"id":3,"result":true,"error":null}"#));
        assert!(f.established());
        assert_eq!(f.extranonce, Some(("00ab".into(), 4)));
        assert_eq!(f.mask.as_deref(), Some("1fffe000"));
        assert!(f.pending.is_empty());
    }

    #[test]
    fn swallows_answers_to_its_own_replay() {
        let mut f = Facts::default();
        assert!(!f.on_daemon(r#"{"id":734100002,"result":[[],"00ab",4],"error":null}"#));
        assert!(f.on_daemon(r#"{"id":null,"method":"mining.notify","params":[]}"#));
    }

    #[test]
    fn tracks_unanswered_submits() {
        let mut f = Facts::default();
        f.on_miner(r#"{"id":7,"method":"mining.submit","params":["a","j","0","0","0"]}"#);
        f.on_miner(r#"{"id":8,"method":"mining.submit","params":["a","j","0","0","1"]}"#);
        f.on_daemon(r#"{"id":7,"result":true,"error":null}"#);
        assert_eq!(f.pending.len(), 1);
        let (id, m) = f.pending.values().next().unwrap();
        assert_eq!(id, &json!(8));
        let r: Value = serde_json::from_str(&refusal(id, m)).unwrap();
        assert_eq!(r["error"][0], 21);
    }

    #[test]
    fn set_extranonce_from_daemon_updates_what_the_miner_has() {
        let mut f = Facts::default();
        f.on_daemon(r#"{"id":null,"method":"mining.set_extranonce","params":["beef",4]}"#);
        assert_eq!(f.extranonce, Some(("beef".into(), 4)));
    }
}
