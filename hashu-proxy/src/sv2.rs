//! Stratum V2 upstream: a per-connection SV1 ↔ SV2 translator.
//!
//! A redeemer can point their redemption at an SV2 pool
//! (`stratum2+tcp://host:port/<authority key>`, as Braiins Pool and
//! DEMAND publish them). Miners still speak Stratum V1 to Hashu.
//!
//! The rest of the proxy only knows pool legs that carry SV1 JSON lines
//! ([`crate::upstream::PoolStream`]). So [`dial`] connects to the SV2 pool,
//! does the Noise NX handshake (checking the pool's certificate against
//! the authority key in the URL) and `SetupConnection`, and then hands back
//! one end of an in-memory pipe. A task on the other end translates:
//!
//! | SV1 from the proxy        | SV2 to the pool                         |
//! |---------------------------|-----------------------------------------|
//! | `mining.configure`        | answered locally (BIP320 mask)          |
//! | `mining.subscribe`        | answered once the channel is open       |
//! | `mining.authorize`        | `OpenExtendedMiningChannel`             |
//! | `mining.suggest_difficulty` | `UpdateChannel` (nominal hashrate)    |
//! | `mining.submit`           | `SubmitSharesExtended`                  |
//!
//! | SV2 from the pool                  | SV1 to the proxy                 |
//! |------------------------------------|----------------------------------|
//! | `OpenExtendedMiningChannelSuccess` | subscribe + authorize results, difficulty |
//! | `NewExtendedMiningJob` + `SetNewPrevHash` | `mining.notify` (witness data stripped) |
//! | `SetTarget`                        | `mining.set_difficulty`          |
//! | `SetExtranoncePrefix`              | `mining.set_extranonce`          |
//! | `SubmitSharesSuccess` / `Error`    | one result per `mining.submit`   |
//!
//! One SV2 connection and one extended channel per leg, so each miner
//! connection still has its own upstream. The channel's extranonce prefix
//! is the leg's `extranonce1` and its extranonce size is `extranonce2_size`,
//! which leaves the proxy's virtual extranonce (see
//! [`crate::stratum::extranonce`]) working as it does for SV1 pools.
//!
//! SV2 acknowledges shares in batches: `SubmitSharesSuccess` names the
//! last sequence number it covers. Each `mining.submit` is held by its
//! sequence number and answered when a batch acknowledgement (or a
//! `SubmitSharesError` for it) arrives, so crediting downstream is
//! unchanged.
//!
//! The channel opens at `mining.authorize`, since SV2 carries the worker
//! name (`user_identity`) there. A miner that waits for its subscribe answer
//! before it authorizes can't use an SV2 pool as its first pool. Switches
//! in place always send both, so redemptions are not affected.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::str::FromStr;
use std::time::Duration;

use serde_json::{json, Value};
use stratum_apps::key_utils::Secp256k1PublicKey;
use stratum_apps::network_helpers::connect_with_noise;
use stratum_apps::network_helpers::noise_stream::{NoiseTcpReadHalf, NoiseTcpWriteHalf};
use stratum_core::binary_sv2::{GetSize, Serialize};
use stratum_core::bitcoin::Target;
use stratum_core::codec_sv2::MessageFrame;
use stratum_core::common_messages_sv2::{Protocol, SetupConnectionOwned};
use stratum_core::mining_sv2::{
    NewExtendedMiningJobOwned, SetNewPrevHashOwned, SubmitSharesExtendedOwned, UpdateChannelOwned,
};
use stratum_core::parsers_sv2::{
    AnyMessage, AnyMessageOwned, CommonMessagesOwned, IsSv2Message, MiningOwned,
};
use stratum_core::stratum_translation::sv1_to_sv2::build_sv2_open_extended_mining_channel;
use stratum_core::stratum_translation::sv2_to_sv1::build_sv1_notify_from_sv2;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::net::TcpStream;

use crate::upstream::PoolStream;

/// Prefix [`crate::upstream::normalize_upstream`] puts on an SV2 upstream:
/// `sv2://host:port/<authority key>`.
pub const SV2_PREFIX: &str = "sv2://";

/// Port used when an SV2 URL has none. Braiins Pool publishes its SV2
/// endpoints without a port.
pub const DEFAULT_PORT: u16 = 3336;

/// BIP320 version bits, which SV2 extended channels may roll.
pub const VERSION_ROLLING_MASK: u32 = 0x1fff_e000;

/// Smallest extranonce the channel asks for. The proxy's virtual
/// extranonce needs 6 bytes; 8 leaves room.
pub const MIN_EXTRANONCE_SIZE: u16 = 8;

/// Hashrate assumed for a miner that gave no difficulty hint (H/s): about
/// one Bitaxe.
const DEFAULT_NOMINAL_HASHRATE: f32 = 1.0e12;

/// Seconds per share the nominal hashrate estimate assumes when it is
/// derived from a difficulty (`d=` or `mining.suggest_difficulty`).
const SECONDS_PER_SHARE: f64 = 10.0;

/// How long [`dial`] waits for `SetupConnection.Success`.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Bytes buffered in the in-memory pipe between the proxy and the
/// translator task.
const PIPE_BYTES: usize = 64 * 1024;

const SETUP_FLAG_REQUIRES_VERSION_ROLLING: u32 = 1 << 2;
const OPEN_CHANNEL_REQUEST_ID: u32 = 1;

/// Turn `stratum2+tcp://host[:port]/<key>` into `sv2://host:port/<key>`.
/// The key must parse as an SRI authority public key (base58check,
/// version 1). `None` for anything else, including a URL with no key:
/// without it the pool's identity can't be checked.
pub fn normalize(url: &str) -> Option<String> {
    let rest = url
        .trim()
        .strip_prefix("stratum2+tcp://")
        .or_else(|| url.trim().strip_prefix(SV2_PREFIX))?;
    let (hostport, key) = rest.split_once('/')?;
    let key = key.trim_end_matches('/');
    if key.contains('/') || Secp256k1PublicKey::from_str(key).is_err() {
        return None;
    }
    let (host, port) = match hostport.rsplit_once(':') {
        // An IPv6 literal without a port is `[::1]`, whose last `:` is
        // inside the brackets.
        Some((h, p)) if !p.contains(']') => (h, p.parse::<u16>().ok().filter(|p| *p != 0)?),
        _ => (hostport, DEFAULT_PORT),
    };
    if host.is_empty() {
        return None;
    }
    Some(format!("{SV2_PREFIX}{host}:{port}/{key}"))
}

/// Whether `upstream` (as [`normalize`] produced it) is an SV2 pool.
pub fn is_sv2(upstream: &str) -> bool {
    upstream.starts_with(SV2_PREFIX)
}

/// The `host:port` an SV2 upstream dials, for logs and pool identity.
pub fn address(upstream: &str) -> Option<&str> {
    upstream
        .strip_prefix(SV2_PREFIX)
        .and_then(|r| r.split_once('/'))
        .map(|(a, _)| a)
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

/// Connect to an SV2 pool, authenticate it and set up a mining
/// connection; returns a stream that speaks SV1 lines (see the module
/// docs). The caller's dial timeout covers all of it.
pub async fn dial(upstream: &str) -> io::Result<PoolStream> {
    let rest = upstream
        .strip_prefix(SV2_PREFIX)
        .ok_or_else(|| err(format!("not an SV2 upstream: {upstream}")))?;
    let (addr, key) = rest
        .split_once('/')
        .ok_or_else(|| err("SV2 upstream has no authority key"))?;
    let key = Secp256k1PublicKey::from_str(key)
        .map_err(|e| err(format!("bad SV2 authority key: {e}")))?;
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true).ok();
    let noise = connect_with_noise(tcp, Some(key))
        .await
        .map_err(|e| err(format!("SV2 Noise handshake with {addr}: {e}")))?;
    let (mut r, mut w) = noise.into_split();
    setup_connection(&mut r, &mut w, addr).await?;

    let (ours, theirs) = tokio::io::duplex(PIPE_BYTES);
    let (sv1_r, sv1_w) = tokio::io::split(ours);
    let addr = addr.to_string();
    tokio::spawn(async move {
        let mut t = Translator::new(w, sv1_w);
        let why = t.run(r, sv1_r).await;
        tracing::debug!(upstream = %addr, reason = %why, "SV2 translator stopped");
    });
    Ok(Box::new(theirs))
}

async fn setup_connection(
    r: &mut NoiseTcpReadHalf,
    w: &mut NoiseTcpWriteHalf,
    addr: &str,
) -> io::Result<()> {
    let (host, port) = addr.rsplit_once(':').unwrap_or((addr, "0"));
    let setup = SetupConnectionOwned {
        protocol: Protocol::MiningProtocol,
        min_version: 2,
        max_version: 2,
        flags: SETUP_FLAG_REQUIRES_VERSION_ROLLING,
        endpoint_host: str255(host)?,
        endpoint_port: port.parse().unwrap_or(0),
        vendor: str255("hashu")?,
        hardware_version: str255("")?,
        firmware: str255(env!("CARGO_PKG_VERSION"))?,
        device_id: str255("")?,
    };
    send(w, CommonMessagesOwned::SetupConnection(setup)).await?;
    let reply = tokio::time::timeout(SETUP_TIMEOUT, recv(r))
        .await
        .map_err(|_| err("SV2 SetupConnection timed out"))??;
    match reply {
        AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionSuccess(_)) => Ok(()),
        AnyMessageOwned::Common(CommonMessagesOwned::SetupConnectionError(e)) => Err(err(format!(
            "SV2 pool refused SetupConnection: {}",
            text(e.error_code.as_bytes())
        ))),
        other => Err(err(format!(
            "SV2 pool sent {other} before SetupConnection.Success"
        ))),
    }
}

type Str255 = stratum_core::binary_sv2::Str0255Owned;
type B032 = stratum_core::binary_sv2::B032Owned;
type U256 = stratum_core::binary_sv2::U256Owned;

fn str255(s: &str) -> io::Result<Str255> {
    Str255::new(s.as_bytes().to_vec()).map_err(|_| err(format!("string too long for SV2: {s:?}")))
}

/// An SV2 string or error code, for logs and SV1 error messages.
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

async fn send<M>(w: &mut NoiseTcpWriteHalf, msg: M) -> io::Result<()>
where
    M: IsSv2Message + Serialize + GetSize,
{
    let (ty, ext, ch) = (msg.message_type(), msg.extension_type(), msg.channel_bit());
    let frame = MessageFrame::from_message(msg, ty, ext, ch)
        .map_err(|e| err(format!("SV2 frame: {e:?}")))?;
    w.write_frame(frame)
        .await
        .map_err(|e| err(format!("SV2 write: {e}")))
}

async fn recv(r: &mut NoiseTcpReadHalf) -> io::Result<AnyMessageOwned> {
    let mut frame = r
        .read_frame()
        .await
        .map_err(|e| err(format!("SV2 read: {e}")))?;
    let header = frame.header();
    let msg = AnyMessage::try_from((header, frame.payload()))
        .map_err(|e| err(format!("SV2 parse: {e:?}")))?;
    Ok(msg.into_owned())
}

/// The open extended channel.
struct Channel {
    id: u32,
    extranonce_size: u16,
}

/// Job data a submit needs.
#[derive(Clone, Copy)]
struct JobInfo {
    version: u32,
    version_rolling_allowed: bool,
}

/// One leg's translation state.
struct Translator {
    w: NoiseTcpWriteHalf,
    out: WriteHalf<DuplexStream>,
    /// Version-rolling mask granted at `mining.configure`, if asked.
    mask: Option<u32>,
    /// `mining.subscribe` id still waiting for the channel.
    subscribe_id: Option<Value>,
    /// `mining.authorize` id waiting for the channel.
    authorize_id: Option<Value>,
    /// Set once `OpenExtendedMiningChannel` was sent.
    opening: bool,
    channel: Option<Channel>,
    /// The channel's extranonce prefix (the leg's `extranonce1`).
    prefix: Vec<u8>,
    prev_hash: Option<SetNewPrevHashOwned>,
    /// Future jobs, waiting for their `SetNewPrevHash`.
    future: HashMap<u32, NewExtendedMiningJobOwned>,
    /// Jobs submits may name.
    jobs: HashMap<u32, JobInfo>,
    nominal_hash_rate: f32,
    seq: u32,
    /// SV1 submit ids by SV2 sequence number, oldest first.
    pending: BTreeMap<u32, Value>,
}

impl Translator {
    fn new(w: NoiseTcpWriteHalf, out: WriteHalf<DuplexStream>) -> Self {
        Self {
            w,
            out,
            mask: None,
            subscribe_id: None,
            authorize_id: None,
            opening: false,
            channel: None,
            prefix: Vec::new(),
            prev_hash: None,
            future: HashMap::new(),
            jobs: HashMap::new(),
            nominal_hash_rate: DEFAULT_NOMINAL_HASHRATE,
            seq: 0,
            pending: BTreeMap::new(),
        }
    }

    /// Translate until either side closes; returns why it stopped.
    async fn run(&mut self, mut r: NoiseTcpReadHalf, sv1: ReadHalf<DuplexStream>) -> String {
        let mut lines = BufReader::new(sv1).lines();
        loop {
            // `read_frame` is cancellation-safe; `next_line` is too.
            let step = tokio::select! {
                frame = recv(&mut r) => match frame {
                    Ok(m) => self.on_sv2(m).await,
                    Err(e) => return format!("pool: {e}"),
                },
                line = lines.next_line() => match line {
                    Ok(Some(l)) => self.on_sv1(&l).await,
                    Ok(None) => return "proxy closed the leg".into(),
                    Err(e) => return format!("proxy side: {e}"),
                },
            };
            if let Err(e) = step {
                return e.to_string();
            }
        }
    }

    async fn write(&mut self, v: Value) -> io::Result<()> {
        let mut line = v.to_string();
        line.push('\n');
        self.out.write_all(line.as_bytes()).await?;
        self.out.flush().await
    }

    async fn reply(&mut self, id: Value, result: Value) -> io::Result<()> {
        self.write(json!({"id": id, "result": result, "error": null}))
            .await
    }

    async fn reply_err(&mut self, id: Value, code: i64, msg: &str) -> io::Result<()> {
        self.write(json!({"id": id, "result": null, "error": [code, msg, null]}))
            .await
    }

    async fn on_sv1(&mut self, line: &str) -> io::Result<()> {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            return Ok(());
        };
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        match v.get("method").and_then(Value::as_str) {
            Some("mining.configure") => {
                let result = configure_result(&params, &mut self.mask);
                self.reply(id, result).await
            }
            Some("mining.subscribe") => {
                if self.channel.is_some() {
                    let r = self.subscribe_result();
                    self.reply(id, r).await
                } else {
                    self.subscribe_id = Some(id);
                    Ok(())
                }
            }
            Some("mining.authorize") => self.on_authorize(id, &params).await,
            Some("mining.suggest_difficulty") => {
                if let Some(d) = params.get(0).and_then(Value::as_f64) {
                    self.nominal_hash_rate = hashrate_for_difficulty(d);
                    if let Some(ch) = &self.channel {
                        let update = UpdateChannelOwned {
                            channel_id: ch.id,
                            nominal_hash_rate: self.nominal_hash_rate,
                            max_target: max_target(),
                        };
                        send(&mut self.w, MiningOwned::UpdateChannel(update)).await?;
                    }
                }
                self.reply(id, json!(true)).await
            }
            Some("mining.extranonce.subscribe") => self.reply(id, json!(true)).await,
            Some("mining.submit") => self.on_submit(id, &params).await,
            Some(m) if !id.is_null() => {
                let m = m.to_string();
                self.reply_err(id, 20, &format!("{m} is not supported on an SV2 pool"))
                    .await
            }
            _ => Ok(()),
        }
    }

    async fn on_authorize(&mut self, id: Value, params: &Value) -> io::Result<()> {
        if self.channel.is_some() {
            return self.reply(id, json!(true)).await;
        }
        if self.opening {
            // A second authorize while the first is pending.
            return self
                .reply_err(id, 20, "authorize already in progress")
                .await;
        }
        let user = params.get(0).and_then(Value::as_str).unwrap_or("");
        let password = params.get(1).and_then(Value::as_str).unwrap_or("");
        if let Some(d) = difficulty_hint(password) {
            self.nominal_hash_rate = hashrate_for_difficulty(d);
        }
        let open = match build_sv2_open_extended_mining_channel(
            OPEN_CHANNEL_REQUEST_ID,
            user.to_string(),
            self.nominal_hash_rate,
            Target::MAX,
            MIN_EXTRANONCE_SIZE,
        ) {
            Ok(o) => o,
            Err(e) => {
                return self
                    .reply_err(id, 24, &format!("bad worker name: {e:?}"))
                    .await
            }
        };
        self.opening = true;
        self.authorize_id = Some(id);
        send(&mut self.w, MiningOwned::OpenExtendedMiningChannel(open)).await
    }

    async fn on_submit(&mut self, id: Value, params: &Value) -> io::Result<()> {
        let Some(ch) = &self.channel else {
            return self.reply_err(id, 24, "not authorized").await;
        };
        let channel_id = ch.id;
        let want = ch.extranonce_size as usize;
        let Some(s) = SubmitParams::parse(params) else {
            return self.reply_err(id, 20, "malformed mining.submit").await;
        };
        let Some(job) = self.jobs.get(&s.job_id).copied() else {
            return self.reply_err(id, 21, "job not found").await;
        };
        if s.extranonce2.len() != want {
            return self
                .reply_err(
                    id,
                    20,
                    &format!("extranonce2 is {} bytes, want {want}", s.extranonce2.len()),
                )
                .await;
        }
        let version = match (s.version_bits, self.mask) {
            (Some(bits), Some(mask)) if job.version_rolling_allowed => {
                (job.version & !mask) | (bits & mask)
            }
            (Some(_), _) => {
                return self.reply_err(id, 20, "version rolling not allowed").await;
            }
            (None, _) => job.version,
        };
        self.seq = self.seq.wrapping_add(1);
        let share = SubmitSharesExtendedOwned {
            channel_id,
            sequence_number: self.seq,
            job_id: s.job_id,
            nonce: s.nonce,
            ntime: s.ntime,
            version,
            extranonce: B032::new(s.extranonce2).map_err(|_| err("extranonce2 too long"))?,
        };
        self.pending.insert(self.seq, id);
        send(&mut self.w, MiningOwned::SubmitSharesExtended(share)).await
    }

    fn subscribe_result(&self) -> Value {
        // Only called with an open channel; the prefix is kept in the
        // last `SetExtranoncePrefix` or the channel's success.
        let (prefix, size) = self.extranonce();
        json!([
            [["mining.set_difficulty", "1"], ["mining.notify", "1"]],
            hex::encode(prefix),
            size
        ])
    }

    fn extranonce(&self) -> (Vec<u8>, u16) {
        let ch = self.channel.as_ref().expect("channel open");
        (self.prefix.clone(), ch.extranonce_size)
    }

    async fn on_sv2(&mut self, msg: AnyMessageOwned) -> io::Result<()> {
        let AnyMessageOwned::Mining(m) = msg else {
            return Ok(());
        };
        match m {
            MiningOwned::OpenExtendedMiningChannelSuccess(ok) => {
                self.prefix = ok.extranonce_prefix.to_owned_bytes();
                self.channel = Some(Channel {
                    id: ok.channel_id,
                    extranonce_size: ok.extranonce_size,
                });
                if let Some(id) = self.subscribe_id.take() {
                    let r = self.subscribe_result();
                    self.reply(id, r).await?;
                }
                if let Some(id) = self.authorize_id.take() {
                    self.reply(id, json!(true)).await?;
                }
                let target = ok.target.to_array();
                self.set_difficulty(target).await
            }
            MiningOwned::OpenMiningChannelError(e) => {
                let why = format!(
                    "SV2 pool refused the channel: {}",
                    text(e.error_code.as_bytes())
                );
                if let Some(id) = self.subscribe_id.take() {
                    self.reply_err(id, 20, &why).await?;
                }
                if let Some(id) = self.authorize_id.take() {
                    self.write(json!({"id": id, "result": false, "error": [24, why, null]}))
                        .await?;
                }
                self.opening = false;
                Ok(())
            }
            MiningOwned::NewExtendedMiningJob(job) => {
                if Some(job.channel_id) != self.channel.as_ref().map(|c| c.id) {
                    return Ok(());
                }
                self.jobs.insert(
                    job.job_id,
                    JobInfo {
                        version: job.version,
                        version_rolling_allowed: job.version_rolling_allowed,
                    },
                );
                let is_future = job.ntime_start.clone().into_inner().is_none();
                match (&self.prev_hash, is_future) {
                    (Some(ph), false) => {
                        let ph = ph.clone();
                        self.notify(ph, job, false).await
                    }
                    _ => {
                        self.future.insert(job.job_id, job);
                        Ok(())
                    }
                }
            }
            MiningOwned::SetNewPrevHash(ph) => {
                if Some(ph.channel_id) != self.channel.as_ref().map(|c| c.id) {
                    return Ok(());
                }
                self.prev_hash = Some(ph.clone());
                // Older jobs build on a stale tip: forget them.
                let job = self.future.remove(&ph.job_id);
                self.future.clear();
                self.jobs.retain(|id, _| *id == ph.job_id);
                match job {
                    Some(job) => self.notify(ph, job, true).await,
                    None => Ok(()),
                }
            }
            MiningOwned::SetTarget(t) => {
                let target = t.target.to_array();
                self.set_difficulty(target).await
            }
            MiningOwned::SetExtranoncePrefix(p) => {
                self.prefix = p.extranonce_prefix.to_owned_bytes();
                let size = self.channel.as_ref().map_or(0, |c| c.extranonce_size);
                let en1 = hex::encode(&self.prefix);
                self.write(
                    json!({"id": null, "method": "mining.set_extranonce", "params": [en1, size]}),
                )
                .await
            }
            MiningOwned::SubmitSharesSuccess(ok) => {
                let done: Vec<u32> = self
                    .pending
                    .range(..=ok.last_sequence_number)
                    .map(|(s, _)| *s)
                    .collect();
                for s in done {
                    if let Some(id) = self.pending.remove(&s) {
                        self.reply(id, json!(true)).await?;
                    }
                }
                Ok(())
            }
            MiningOwned::SubmitSharesError(e) => {
                if let Some(id) = self.pending.remove(&e.sequence_number) {
                    let code = text(e.error_code.as_bytes());
                    self.reply_err(id, 23, &code).await?;
                }
                Ok(())
            }
            MiningOwned::CloseChannel(c) => Err(err(format!(
                "SV2 pool closed the channel: {}",
                text(c.reason_code.as_bytes())
            ))),
            _ => Ok(()),
        }
    }

    async fn notify(
        &mut self,
        ph: SetNewPrevHashOwned,
        job: NewExtendedMiningJobOwned,
        clean: bool,
    ) -> io::Result<()> {
        let n = build_sv1_notify_from_sv2(ph, job, clean)
            .map_err(|e| err(format!("translating SV2 job: {e:?}")))?;
        let msg: stratum_core::sv1_api::json_rpc::Message = n.into();
        let v = serde_json::to_value(&msg).map_err(|e| err(e.to_string()))?;
        self.write(v).await
    }

    async fn set_difficulty(&mut self, target_le: [u8; 32]) -> io::Result<()> {
        let d = Target::from_le_bytes(target_le).difficulty_float();
        self.write(json!({"id": null, "method": "mining.set_difficulty", "params": [d]}))
            .await
    }
}

/// `mining.submit` params `[worker, job_id, extranonce2, ntime, nonce,
/// version_bits?]`.
struct SubmitParams {
    job_id: u32,
    extranonce2: Vec<u8>,
    ntime: u32,
    nonce: u32,
    version_bits: Option<u32>,
}

impl SubmitParams {
    fn parse(p: &Value) -> Option<Self> {
        let s = |i: usize| p.get(i).and_then(Value::as_str);
        let hex32 = |i: usize| s(i).and_then(|h| u32::from_str_radix(h, 16).ok());
        Some(Self {
            job_id: s(1)?.parse().ok()?,
            extranonce2: hex::decode(s(2)?).ok()?,
            ntime: hex32(3)?,
            nonce: hex32(4)?,
            version_bits: match s(5) {
                Some(h) => Some(u32::from_str_radix(h, 16).ok()?),
                None => None,
            },
        })
    }
}

/// Answer a `mining.configure` locally: grant version rolling within
/// BIP320's bits, and nothing else.
fn configure_result(params: &Value, mask: &mut Option<u32>) -> Value {
    let exts = params.get(0).and_then(Value::as_array);
    let wants = exts.is_some_and(|e| e.iter().any(|x| x.as_str() == Some("version-rolling")));
    if !wants {
        return json!({});
    }
    let asked = params
        .get(1)
        .and_then(|o| o.get("version-rolling.mask"))
        .and_then(Value::as_str)
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .unwrap_or(VERSION_ROLLING_MASK);
    let granted = asked & VERSION_ROLLING_MASK;
    *mask = Some(granted);
    json!({"version-rolling": true, "version-rolling.mask": format!("{granted:08x}")})
}

/// `d=<difficulty>` from a password like `x,d=512`.
fn difficulty_hint(password: &str) -> Option<f64> {
    password
        .split(',')
        .map(str::trim)
        .find_map(|o| o.strip_prefix("d="))
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0)
}

/// Hashrate (H/s) that finds a share of difficulty `d` every
/// [`SECONDS_PER_SHARE`].
fn hashrate_for_difficulty(d: f64) -> f32 {
    (d * 4_294_967_296.0 / SECONDS_PER_SHARE) as f32
}

fn max_target() -> U256 {
    U256::from(Target::MAX.to_le_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72";

    #[test]
    fn normalizes_sv2_urls() {
        let n = normalize;
        assert_eq!(
            n(&format!("stratum2+tcp://pool.example:3336/{KEY}")).as_deref(),
            Some(format!("sv2://pool.example:3336/{KEY}").as_str())
        );
        // No port: Braiins' published form.
        assert_eq!(
            n(&format!("stratum2+tcp://v2.stratum.braiins.com/{KEY}")).as_deref(),
            Some(format!("sv2://v2.stratum.braiins.com:3336/{KEY}").as_str())
        );
        assert_eq!(
            n(&format!("sv2://p:1/{KEY}")).as_deref(),
            Some(format!("sv2://p:1/{KEY}").as_str())
        );
        assert_eq!(n("stratum2+tcp://p:3336"), None, "no key");
        assert_eq!(n("stratum2+tcp://p:3336/notakey"), None);
        assert_eq!(n(&format!("stratum2+tcp://p:0/{KEY}")), None);
        assert_eq!(n(&format!("stratum2+tcp://:3336/{KEY}")), None);
        assert_eq!(n(&format!("stratum+tcp://p:3333/{KEY}")), None);
        assert_eq!(address(&format!("sv2://p:9/{KEY}")), Some("p:9"));
    }

    #[test]
    fn configure_grants_bip320_bits_only() {
        let mut mask = None;
        let r = configure_result(
            &json!([["version-rolling"], {"version-rolling.mask": "ffffffff"}]),
            &mut mask,
        );
        assert_eq!(r["version-rolling.mask"], "1fffe000");
        assert_eq!(mask, Some(VERSION_ROLLING_MASK));
        let mut mask = None;
        assert_eq!(
            configure_result(&json!([["minimum-difficulty"], {}]), &mut mask),
            json!({})
        );
        assert_eq!(mask, None);
    }

    #[test]
    fn difficulty_hint_and_hashrate() {
        assert_eq!(difficulty_hint("x,d=512"), Some(512.0));
        assert_eq!(difficulty_hint("x"), None);
        assert_eq!(difficulty_hint("d=-1"), None);
        // 1000 difficulty every 10 s is ~429 GH/s.
        assert!((hashrate_for_difficulty(1000.0) - 4.294_967e11).abs() < 1e6);
    }

    #[test]
    fn parses_submit() {
        let s = SubmitParams::parse(&json!([
            "w",
            "7",
            "00000000000000ab",
            "6553f100",
            "deadbeef",
            "00002000"
        ]))
        .unwrap();
        assert_eq!(s.job_id, 7);
        assert_eq!(s.extranonce2.len(), 8);
        assert_eq!(s.ntime, 0x6553f100);
        assert_eq!(s.nonce, 0xdeadbeef);
        assert_eq!(s.version_bits, Some(0x2000));
        assert!(SubmitParams::parse(&json!(["w", "x", "00", "0", "0"])).is_none());
    }
}
