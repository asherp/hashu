//! `hashu mine` — single-threaded SHA-256d Stratum V1 client for live
//! testing of the proxy + share-commitment-tree pipeline.
//!
//! Topology when used as intended:
//!
//! ```text
//!   hashu mine ──TCP──▶ hashu proxy ──TCP──▶ public-pool.io
//! ```
//!
//! The miner connects, runs subscribe + authorize, consumes
//! `mining.set_difficulty` / `mining.notify` notifications, brute-forces
//! nonces in the current job until something hashes ≤ the share target,
//! then submits. Reuses [`hashu_proxy::stratum::header`] for header
//! reconstruction and [`hashu_proxy::stratum::compact`] for difficulty →
//! target conversion — same code paths the proxy uses to verify shares,
//! so any byte-order disagreement would surface immediately as 100%
//! "Low difficulty share" rejects from the pool.
//!
//! This is a *test* miner: one hashing thread, no extranonce-roll
//! randomization, no `mining.configure` (BIP310 version-rolling), no
//! reconnect logic. It exists to drive the proxy in anger; not to mine
//! competitively.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use hashu_proxy::stratum::compact::{target_bits_from_difficulty, target_from_compact};
use hashu_proxy::stratum::header::{
    coinbase_tx_hash, merkle_root_from_branch, parse_be_u32_hex, parse_branch_entry_hex,
    parse_prev_hash_hex, JobTemplate,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};

#[derive(Debug, Args)]
pub struct MineArgs {
    /// Upstream Stratum endpoint. Accepts `host:port` or
    /// `stratum+tcp://host:port`.
    #[arg(long)]
    pub upstream: String,
    /// Worker username (typically `<btc_address>.<worker_name>` for solo
    /// pools).
    #[arg(long)]
    pub user: String,
    /// Worker password. Some pools use this for vardiff hints, e.g.
    /// `d=1024`. Default `x`.
    #[arg(long, default_value = "x")]
    pub password: String,
    /// Periodic metrics log interval (seconds). 0 disables.
    #[arg(long, default_value_t = 10)]
    pub metrics_interval: u64,
}

#[derive(Default)]
struct MinerState {
    extranonce1: Vec<u8>,
    extranonce2_size: usize,
    current_difficulty: f64,
    current_job: Option<JobTemplate>,
    /// Bumped on every state change a miner needs to react to.
    generation: u64,
    /// Set when `mining.notify` arrives with `clean_jobs=true`. The miner
    /// uses this only to log; the generation counter is what drives the
    /// reset.
    last_clean_jobs: bool,
}

#[derive(Default)]
struct MineMetrics {
    hashes: AtomicU64,
    shares_submitted: AtomicU64,
    shares_accepted: AtomicU64,
    shares_rejected: AtomicU64,
}

pub async fn run(args: MineArgs) -> Result<()> {
    let upstream = strip_stratum_prefix(&args.upstream).to_string();
    if !upstream.contains(':') {
        anyhow::bail!("upstream must include a port; got {upstream}");
    }

    let stream = TcpStream::connect(&upstream)
        .await
        .with_context(|| format!("connect upstream {upstream}"))?;
    stream.set_nodelay(true).ok();
    let (r, w) = stream.into_split();

    let state = Arc::new(Mutex::new(MinerState::default()));
    let metrics = Arc::new(MineMetrics::default());
    let (tx, rx) = mpsc::channel::<String>(64);

    let writer_handle = tokio::spawn(writer_task(w, rx));
    let reader_handle = tokio::spawn(reader_task(
        BufReader::new(r),
        state.clone(),
        metrics.clone(),
    ));

    // Subscribe (id=1) + Authorize (id=2). The reader task will populate
    // state when responses come back; we wait briefly for extranonce1.
    tx.send(
        json!({"id": 1, "method": "mining.subscribe", "params": ["hashu-mine/0.1"]})
            .to_string(),
    )
    .await
    .map_err(|e| anyhow!("send subscribe: {e}"))?;
    tx.send(
        json!({"id": 2, "method": "mining.authorize", "params": [args.user.clone(), args.password.clone()]})
            .to_string(),
    )
    .await
    .map_err(|e| anyhow!("send authorize: {e}"))?;

    // Wait up to 5s for extranonce1 (i.e. for the subscribe response).
    let mut waited = Duration::ZERO;
    let step = Duration::from_millis(100);
    while waited < Duration::from_secs(5) {
        if !state.lock().await.extranonce1.is_empty() {
            break;
        }
        tokio::time::sleep(step).await;
        waited += step;
    }
    if state.lock().await.extranonce1.is_empty() {
        anyhow::bail!("no subscribe response within 5s");
    }

    // Spawn the mining loop on a blocking thread. spawn_blocking is the
    // right tool here — the inner loop never awaits.
    let submit_id = Arc::new(AtomicU64::new(100));
    let mine_handle = {
        let state = state.clone();
        let metrics = metrics.clone();
        let tx = tx.clone();
        let submit_id = submit_id.clone();
        let user = args.user.clone();
        tokio::task::spawn_blocking(move || mine_loop(state, metrics, tx, submit_id, user))
    };

    if args.metrics_interval > 0 {
        let metrics = metrics.clone();
        let interval = Duration::from_secs(args.metrics_interval);
        tokio::spawn(metrics_ticker(metrics, interval));
    }

    // Whichever side dies first ends the run.
    tokio::select! {
        r = reader_handle => { r?.context("reader")?; }
        r = writer_handle => { r?.context("writer")?; }
        r = mine_handle => { r?.context("miner")?; }
    }
    Ok(())
}

async fn writer_task(mut writer: OwnedWriteHalf, mut rx: mpsc::Receiver<String>) -> Result<()> {
    while let Some(line) = rx.recv().await {
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
    let _ = writer.shutdown().await;
    Ok(())
}

async fn reader_task(
    mut reader: BufReader<OwnedReadHalf>,
    state: Arc<Mutex<MinerState>>,
    metrics: Arc<MineMetrics>,
) -> Result<()> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Err(anyhow!("upstream closed connection"));
        }
        let v: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, line = %line.trim(), "non-json from upstream");
                continue;
            }
        };
        handle_pool_message(&v, &state, &metrics).await;
    }
}

async fn handle_pool_message(v: &Value, state: &Mutex<MinerState>, metrics: &MineMetrics) {
    let method = v.get("method").and_then(|m| m.as_str());
    let id = v.get("id").and_then(|x| x.as_i64());

    match method {
        Some("mining.set_difficulty") => {
            if let Some(d) = v
                .get("params")
                .and_then(|p| p.as_array())
                .and_then(|p| p.first())
                .and_then(|x| x.as_f64())
            {
                let mut s = state.lock().await;
                s.current_difficulty = d;
                s.generation += 1;
                tracing::info!(difficulty = d, "set_difficulty");
            }
        }
        Some("mining.notify") => {
            let params_arr = v.get("params").and_then(|p| p.as_array()).cloned();
            if let Some(params) = params_arr {
                if let Some(job) = parse_notify(&params) {
                    let clean_jobs = params.get(8).and_then(|c| c.as_bool()).unwrap_or(false);
                    let mut s = state.lock().await;
                    let job_id = job.job_id.clone();
                    s.current_job = Some(job);
                    s.generation += 1;
                    s.last_clean_jobs = clean_jobs;
                    tracing::info!(job_id = %job_id, clean_jobs, "new job");
                }
            }
        }
        Some(other) => {
            tracing::debug!(method = other, "unhandled pool notification");
        }
        None => {
            // It's a response (id-correlated). Routing by id:
            //   1 → subscribe response (extranonce1 / extranonce2_size)
            //   2 → authorize response (true/false)
            //  ≥100 → submit response (accepted / rejected)
            let id = match id {
                Some(i) => i,
                None => return,
            };
            match id {
                1 => absorb_subscribe(v, state).await,
                2 => {
                    let ok = matches!(v.get("result").and_then(|r| r.as_bool()), Some(true));
                    if !ok {
                        let err = v.get("error").cloned().unwrap_or(Value::Null);
                        tracing::warn!(?err, "authorize rejected");
                    } else {
                        tracing::info!("authorized");
                    }
                }
                _ => {
                    let accepted =
                        matches!(v.get("result").and_then(|r| r.as_bool()), Some(true));
                    if accepted {
                        metrics.shares_accepted.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(id, "share accepted by pool");
                    } else {
                        metrics.shares_rejected.fetch_add(1, Ordering::Relaxed);
                        let err = v.get("error").cloned().unwrap_or(Value::Null);
                        tracing::warn!(id, ?err, "share rejected by pool");
                    }
                }
            }
        }
    }
}

async fn absorb_subscribe(v: &Value, state: &Mutex<MinerState>) {
    let result = match v.get("result").and_then(|r| r.as_array()) {
        Some(r) => r,
        None => {
            tracing::warn!(?v, "subscribe response missing result array");
            return;
        }
    };
    if result.len() != 3 {
        tracing::warn!(len = result.len(), "subscribe response result has unexpected length");
        return;
    }
    let en1 = match result[1].as_str().and_then(|s| hex::decode(s).ok()) {
        Some(e) => e,
        None => {
            tracing::warn!("subscribe response: extranonce1 not a hex string");
            return;
        }
    };
    let en2_size = match result[2].as_u64() {
        Some(n) => n as usize,
        None => {
            tracing::warn!("subscribe response: extranonce2_size not an integer");
            return;
        }
    };
    let mut s = state.lock().await;
    s.extranonce1 = en1.clone();
    s.extranonce2_size = en2_size;
    s.generation += 1;
    tracing::info!(
        extranonce1 = %hex::encode(&en1),
        extranonce2_size = en2_size,
        "subscribed",
    );
}

fn parse_notify(params: &[Value]) -> Option<JobTemplate> {
    if params.len() < 9 {
        return None;
    }
    let job_id = params[0].as_str()?.to_string();
    let prev_hash = parse_prev_hash_hex(params[1].as_str()?).ok()?;
    let coinb1 = hex::decode(params[2].as_str()?).ok()?;
    let coinb2 = hex::decode(params[3].as_str()?).ok()?;
    let branch = params[4].as_array()?;
    let mut merkle_branch = Vec::with_capacity(branch.len());
    for entry in branch {
        merkle_branch.push(parse_branch_entry_hex(entry.as_str()?).ok()?);
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

/// Pure synchronous mining loop. Polls the shared state for the latest
/// job + difficulty by comparing the `generation` counter; a single
/// reload per generation is enough — between job changes we just iterate
/// nonces.
fn mine_loop(
    state: Arc<Mutex<MinerState>>,
    metrics: Arc<MineMetrics>,
    tx: mpsc::Sender<String>,
    submit_id: Arc<AtomicU64>,
    user: String,
) -> Result<()> {
    /// Hashes per inner batch before we re-check the generation counter.
    /// 1M ≈ 10–50 ms on commodity x86; keeps wakeup latency on a job
    /// change well under one Stratum interval.
    const BATCH: u32 = 1_048_576;

    let mut current_gen: u64 = 0;
    let mut job: Option<JobTemplate> = None;
    let mut en1: Vec<u8> = Vec::new();
    let mut target_be: [u8; 32] = [0xff; 32];
    let mut extranonce2: Vec<u8> = Vec::new();
    let mut nonce_base: u32 = 0;

    loop {
        // Refresh state on generation change.
        let observed_gen = state.blocking_lock().generation;
        if observed_gen != current_gen {
            let s = state.blocking_lock();
            current_gen = s.generation;
            job = s.current_job.clone();
            en1 = s.extranonce1.clone();
            let en2_size = s.extranonce2_size;
            let difficulty = s.current_difficulty;
            drop(s);

            target_be = if difficulty > 0.0 {
                target_from_compact(target_bits_from_difficulty(difficulty))
            } else {
                // Pool hasn't sent set_difficulty yet — start permissive.
                target_from_compact(0x1d00_ffff)
            };
            extranonce2 = vec![0u8; en2_size.max(1)];
            nonce_base = 0;
        }

        let job_ref = match job.as_ref() {
            Some(j) => j,
            None => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        if en1.is_empty() {
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }

        // One batch at the current (job, extranonce2): build coinbase +
        // merkle root once, then iterate nonces.
        let coinbase_hash = coinbase_tx_hash(&job_ref.coinb1, &en1, &extranonce2, &job_ref.coinb2);
        let merkle_root = merkle_root_from_branch(&coinbase_hash, &job_ref.merkle_branch);

        let mut header = [0u8; 80];
        header[0..4].copy_from_slice(&job_ref.version.to_le_bytes());
        header[4..36].copy_from_slice(&job_ref.prev_hash);
        header[36..68].copy_from_slice(&merkle_root);
        header[68..72].copy_from_slice(&job_ref.ntime.to_le_bytes());
        header[72..76].copy_from_slice(&job_ref.nbits.to_le_bytes());

        let end = nonce_base.saturating_add(BATCH);
        for nonce in nonce_base..end {
            header[76..80].copy_from_slice(&nonce.to_le_bytes());
            if header_meets_target(&header, &target_be) {
                let id = submit_id.fetch_add(1, Ordering::Relaxed) as i64;
                let msg = json!({
                    "id": id,
                    "method": "mining.submit",
                    "params": [
                        user,
                        job_ref.job_id,
                        hex::encode(&extranonce2),
                        format!("{:08x}", job_ref.ntime),
                        format!("{:08x}", nonce),
                    ]
                })
                .to_string();
                metrics.shares_submitted.fetch_add(1, Ordering::Relaxed);
                if tx.blocking_send(msg).is_err() {
                    tracing::warn!("submit channel closed; mining stops");
                    return Ok(());
                }
                tracing::info!(job_id = %job_ref.job_id, nonce, "submitting share");
            }
        }
        metrics.hashes.fetch_add(BATCH as u64, Ordering::Relaxed);

        nonce_base = end;
        if nonce_base == 0 {
            // wrapped: roll extranonce2
            increment_le(&mut extranonce2);
        }
    }
}

fn header_meets_target(header: &[u8; 80], target_be: &[u8; 32]) -> bool {
    let h1 = Sha256::digest(header);
    let h2: [u8; 32] = Sha256::digest(h1).into();
    // h2 is the natural sha256d output. Per Bitcoin convention it's
    // interpreted as a little-endian integer; reverse to big-endian and
    // compare to the BE-stored target.
    let mut h_be = [0u8; 32];
    for i in 0..32 {
        h_be[i] = h2[31 - i];
    }
    h_be <= *target_be
}

fn increment_le(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        *b = b.wrapping_add(1);
        if *b != 0 {
            return;
        }
    }
}

async fn metrics_ticker(metrics: Arc<MineMetrics>, interval: Duration) {
    let mut tick = tokio::time::interval(interval);
    tick.tick().await;
    let mut last_h = 0u64;
    let mut last_t = Instant::now();
    loop {
        tick.tick().await;
        let h = metrics.hashes.load(Ordering::Relaxed);
        let dt = last_t.elapsed().as_secs_f64();
        let rate = if dt > 0.0 {
            (h - last_h) as f64 / dt
        } else {
            0.0
        };
        tracing::info!(
            hashes = h,
            rate_hps = format!("{:.0}", rate),
            submitted = metrics.shares_submitted.load(Ordering::Relaxed),
            accepted = metrics.shares_accepted.load(Ordering::Relaxed),
            rejected = metrics.shares_rejected.load(Ordering::Relaxed),
            "mining metrics",
        );
        last_h = h;
        last_t = Instant::now();
    }
}

fn strip_stratum_prefix(s: &str) -> &str {
    s.strip_prefix("stratum+tcp://")
        .or_else(|| s.strip_prefix("stratum+tcps://"))
        .or_else(|| s.strip_prefix("stratum://"))
        .unwrap_or(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_meets_loose_target_for_any_input() {
        // Target = 0xffff…ff covers every possible hash → always meets.
        let header = [0u8; 80];
        let target = [0xff; 32];
        assert!(header_meets_target(&header, &target));
    }

    #[test]
    fn header_misses_zero_target() {
        let header = [0u8; 80];
        let target = [0u8; 32];
        // Hash of an all-zero header is essentially never zero.
        assert!(!header_meets_target(&header, &target));
    }

    #[test]
    fn increment_le_basic() {
        let mut b = vec![0u8, 0, 0, 0];
        increment_le(&mut b);
        assert_eq!(b, vec![1, 0, 0, 0]);
        b = vec![0xff, 0, 0, 0];
        increment_le(&mut b);
        assert_eq!(b, vec![0, 1, 0, 0]);
        b = vec![0xff, 0xff, 0xff, 0xff];
        increment_le(&mut b);
        assert_eq!(b, vec![0, 0, 0, 0]);
    }

    #[test]
    fn strip_prefix_handles_common_forms() {
        assert_eq!(strip_stratum_prefix("stratum+tcp://x:1"), "x:1");
        assert_eq!(strip_stratum_prefix("stratum+tcps://x:1"), "x:1");
        assert_eq!(strip_stratum_prefix("stratum://x:1"), "x:1");
        assert_eq!(strip_stratum_prefix("x:1"), "x:1");
    }

    #[test]
    fn parse_notify_accepts_minimal_valid_input() {
        let params: Vec<Value> = serde_json::from_str(&format!(
            r#"["job1","{prev}","aabbccdd","11223344",[],"20000000","1d00ffff","60000000",false]"#,
            prev = "11".repeat(32),
        ))
        .unwrap();
        let job = parse_notify(&params).unwrap();
        assert_eq!(job.job_id, "job1");
        assert_eq!(job.version, 0x2000_0000);
        assert_eq!(job.nbits, 0x1d00_ffff);
        assert_eq!(job.ntime, 0x6000_0000);
        assert_eq!(job.merkle_branch.len(), 0);
    }
}
