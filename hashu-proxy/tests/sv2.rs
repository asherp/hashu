//! End-to-end test of the Stratum V2 upstream.
//!
//! Topology:
//!     SV1 miner ⇄ proxy under test ⇄ mock SV2 pool (Noise + SV2 frames)
//!
//! The mock pool is built from the same SRI crates as the translator. It
//! checks the Noise handshake against its authority key, opens one
//! extended channel, sends a future job plus `SetNewPrevHash`, and then
//! acknowledges one share and rejects another. The test checks what the
//! miner sees (extranonce, notify, results) and what the pool receives
//! (worker name, rolled version, extranonce).

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use hashu_core::redemption::{Redemption, RedemptionId, RedemptionLedger};
use hashu_proxy::credit::{CreditContext, ShareCrediter};
use hashu_proxy::router::Router;
use hashu_proxy::stratum::{run_listener, run_routed_listener, ListenerConfig};
use hashu_proxy::upstream::normalize_upstream;
use hashu_proxy::ProxyMetrics;
use serde_json::Value;
use stratum_apps::key_utils::{Secp256k1PublicKey, Secp256k1SecretKey};
use stratum_apps::network_helpers::accept_noise_connection;
use stratum_apps::network_helpers::noise_stream::{NoiseTcpReadHalf, NoiseTcpWriteHalf};
use stratum_core::binary_sv2::{
    B032Owned, B064KOwned, GetSize, Seq0255Owned, Serialize, Str0255Owned, Sv2OptionOwned,
    U256Owned,
};
use stratum_core::codec_sv2::MessageFrame;
use stratum_core::common_messages_sv2::SetupConnectionSuccessOwned;
use stratum_core::mining_sv2::{
    NewExtendedMiningJobOwned, OpenExtendedMiningChannelSuccessOwned, SetNewPrevHashOwned,
    SubmitSharesErrorOwned, SubmitSharesExtendedOwned, SubmitSharesSuccessOwned,
};
use stratum_core::parsers_sv2::{
    AnyMessage, AnyMessageOwned, CommonMessagesOwned, IsSv2Message, MiningOwned,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

const CHANNEL: u32 = 7;
const PREFIX: [u8; 4] = [0xaa, 0xbb, 0xcc, 0xdd];
const EXTRANONCE_SIZE: u16 = 8;
const JOB_VERSION: u32 = 0x2000_0000;

async fn send<M: IsSv2Message + Serialize + GetSize>(w: &mut NoiseTcpWriteHalf, m: M) {
    let (ty, ext, ch) = (m.message_type(), m.extension_type(), m.channel_bit());
    w.write_frame(MessageFrame::from_message(m, ty, ext, ch).unwrap())
        .await
        .unwrap();
}

async fn recv(r: &mut NoiseTcpReadHalf) -> AnyMessageOwned {
    let mut f = r.read_frame().await.unwrap();
    let h = f.header();
    AnyMessage::try_from((h, f.payload())).unwrap().into_owned()
}

fn keys() -> (Secp256k1PublicKey, Secp256k1SecretKey) {
    let secp = secp256k1::Secp256k1::new();
    let sk = secp256k1::SecretKey::from_slice(&[7u8; 32]).unwrap();
    let kp = secp256k1::Keypair::from_secret_key(&secp, &sk);
    let (xonly, _) = kp.x_only_public_key();
    (Secp256k1PublicKey(xonly), Secp256k1SecretKey(sk))
}

/// A plausible non-segwit coinbase split around a 12-byte extranonce
/// (4-byte prefix + 8 bytes the miner rolls).
fn coinbase() -> (Vec<u8>, Vec<u8>) {
    let mut prefix = hex::decode("01000000").unwrap(); // version
    prefix.push(1); // one input
    prefix.extend([0u8; 32]); // null outpoint
    prefix.extend([0xff; 4]);
    prefix.push(4 + 12); // script: BIP34 height + extranonce
    prefix.extend([0x03, 0x01, 0x02, 0x03]);
    let mut suffix = vec![0xff; 4]; // sequence
    suffix.push(1); // one output
    suffix.extend(hex::decode("00f2052a01000000").unwrap());
    suffix.push(0x19);
    suffix.extend(hex::decode("76a914").unwrap());
    suffix.extend([0x22; 20]);
    suffix.extend(hex::decode("88ac").unwrap());
    suffix.extend([0u8; 4]); // locktime
    (prefix, suffix)
}

/// What the mock pool saw.
#[derive(Debug)]
enum Seen {
    User(String),
    Share(SubmitSharesExtendedOwned),
}

/// Serve one connection; answer each share with the next of `results`
/// (true = accept in a batch ack, false = reject).
async fn mock_pool(listener: TcpListener, seen: mpsc::UnboundedSender<Seen>, results: Vec<bool>) {
    let (pk, sk) = keys();
    let (sock, _) = listener.accept().await.unwrap();
    let noise = accept_noise_connection(sock, pk, sk, 3600).await.unwrap();
    let (mut r, mut w) = noise.into_split();

    let AnyMessageOwned::Common(CommonMessagesOwned::SetupConnection(setup)) = recv(&mut r).await
    else {
        panic!("expected SetupConnection");
    };
    assert_eq!(setup.min_version, 2);
    send(
        &mut w,
        CommonMessagesOwned::SetupConnectionSuccess(SetupConnectionSuccessOwned {
            used_version: 2,
            flags: 0,
        }),
    )
    .await;

    let AnyMessageOwned::Mining(MiningOwned::OpenExtendedMiningChannel(open)) = recv(&mut r).await
    else {
        panic!("expected OpenExtendedMiningChannel");
    };
    seen.send(Seen::User(
        String::from_utf8(open.user_identity.as_bytes().to_vec()).unwrap(),
    ))
    .unwrap();
    assert!(open.min_extranonce_size >= 6);
    // Difficulty 1 target, little endian.
    let mut target = [0u8; 32];
    target[26] = 0xff;
    target[27] = 0xff;
    send(
        &mut w,
        MiningOwned::OpenExtendedMiningChannelSuccess(OpenExtendedMiningChannelSuccessOwned {
            request_id: open.request_id,
            channel_id: CHANNEL,
            target: U256Owned::from(target),
            extranonce_size: EXTRANONCE_SIZE,
            extranonce_prefix: B032Owned::new(PREFIX.to_vec()).unwrap(),
            group_channel_id: 0,
        }),
    )
    .await;

    let (prefix, suffix) = coinbase();
    send(
        &mut w,
        MiningOwned::NewExtendedMiningJob(NewExtendedMiningJobOwned {
            channel_id: CHANNEL,
            job_id: 1,
            ntime_start: Sv2OptionOwned::new(None),
            version: JOB_VERSION,
            version_rolling_allowed: true,
            merkle_path: Seq0255Owned::new(Vec::new()).unwrap(),
            coinbase_tx_prefix: B064KOwned::new(prefix).unwrap(),
            coinbase_tx_suffix: B064KOwned::new(suffix).unwrap(),
        }),
    )
    .await;
    send(
        &mut w,
        MiningOwned::SetNewPrevHash(SetNewPrevHashOwned {
            channel_id: CHANNEL,
            job_id: 1,
            prev_hash: U256Owned::from([0x11u8; 32]),
            ntime_start: 0x6553_f100,
            nbits: 0x1d00_ffff,
        }),
    )
    .await;

    // First share: accepted in a batch ack. Second: rejected.
    for accept in results {
        // Skip anything else, e.g. `UpdateChannel` after a suggested difficulty.
        let s = loop {
            if let AnyMessageOwned::Mining(MiningOwned::SubmitSharesExtended(s)) =
                recv(&mut r).await
            {
                break s;
            }
        };
        let seq = s.sequence_number;
        seen.send(Seen::Share(s)).unwrap();
        if accept {
            send(
                &mut w,
                MiningOwned::SubmitSharesSuccess(SubmitSharesSuccessOwned {
                    channel_id: CHANNEL,
                    last_sequence_number: seq,
                    new_submits_accepted_count: 1,
                    new_shares_sum: 1,
                }),
            )
            .await;
        } else {
            send(
                &mut w,
                MiningOwned::SubmitSharesError(SubmitSharesErrorOwned {
                    channel_id: CHANNEL,
                    sequence_number: seq,
                    error_code: Str0255Owned::new(b"difficulty-too-low".to_vec()).unwrap(),
                }),
            )
            .await;
        }
    }
    // Keep the connection open until the test is done.
    tokio::time::sleep(Duration::from_secs(5)).await;
}

async fn read_json<R: tokio::io::AsyncRead + Unpin>(r: &mut BufReader<R>) -> Value {
    let mut s = String::new();
    let n = tokio::time::timeout(Duration::from_secs(5), r.read_line(&mut s))
        .await
        .expect("timed out waiting for the proxy")
        .unwrap();
    assert!(n > 0, "proxy closed the miner connection");
    serde_json::from_str(&s).unwrap()
}

#[tokio::test]
async fn sv1_miner_mines_on_sv2_pool_through_proxy() {
    let pool = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let pool_addr = pool.local_addr().unwrap();
    let (tx, mut seen) = mpsc::unbounded_channel();
    tokio::spawn(mock_pool(pool, tx, vec![true, false]));

    let (pk, _) = keys();
    let url = format!("stratum2+tcp://127.0.0.1:{}/{pk}", pool_addr.port());
    let upstream = normalize_upstream(&url).expect("SV2 URL normalizes");
    assert!(upstream.starts_with("sv2://"));

    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = probe.local_addr().unwrap();
    drop(probe);
    let metrics = Arc::new(ProxyMetrics::default());
    let cfg = ListenerConfig {
        bind: proxy_addr,
        upstream,
    };
    tokio::spawn(run_listener(cfg, metrics));
    tokio::time::sleep(Duration::from_millis(50)).await;

    let miner = TcpStream::connect(proxy_addr).await.unwrap();
    let (r, mut w) = miner.into_split();
    let mut br = BufReader::new(r);
    // Like a Bitaxe: configure, subscribe, authorize without waiting.
    w.write_all(concat!(
        r#"{"id":1,"method":"mining.configure","params":[["version-rolling"],{"version-rolling.mask":"ffffffff","version-rolling.min-bit-count":16}]}"#, "\n",
        r#"{"id":2,"method":"mining.subscribe","params":["bitaxe/2.0"]}"#, "\n",
        r#"{"id":3,"method":"mining.authorize","params":["bc1qredeemer.rig1","x"]}"#, "\n",
    ).as_bytes())
    .await
    .unwrap();

    let configure = read_json(&mut br).await;
    assert_eq!(configure["id"], 1);
    assert_eq!(configure["result"]["version-rolling.mask"], "1fffe000");

    let subscribe = read_json(&mut br).await;
    assert_eq!(subscribe["id"], 2, "{subscribe}");
    assert_eq!(subscribe["result"][1], "aabbccdd");
    assert_eq!(subscribe["result"][2], EXTRANONCE_SIZE);

    let authorize = read_json(&mut br).await;
    assert_eq!(authorize["id"], 3);
    assert_eq!(authorize["result"], true);

    let diff = read_json(&mut br).await;
    assert_eq!(diff["method"], "mining.set_difficulty");
    assert!(
        (diff["params"][0].as_f64().unwrap() - 1.0).abs() < 1e-6,
        "{diff}"
    );

    let notify = read_json(&mut br).await;
    assert_eq!(notify["method"], "mining.notify", "{notify}");
    let p = &notify["params"];
    assert_eq!(p[0], "1");
    assert_eq!(p[5], "20000000");
    assert_eq!(p[6], "1d00ffff");
    assert_eq!(p[7], "6553f100");
    assert_eq!(p[8], true);
    assert_eq!(p[2], hex::encode(coinbase().0));

    match seen.recv().await.unwrap() {
        Seen::User(u) => assert_eq!(u, "bc1qredeemer.rig1"),
        other => panic!("expected the channel's user, got {other:?}"),
    }

    // Accepted share, with version rolling.
    w.write_all(
        b"{\"id\":4,\"method\":\"mining.submit\",\"params\":[\"bc1qredeemer.rig1\",\"1\",\"0102030405060708\",\"6553f101\",\"deadbeef\",\"00002000\"]}\n",
    )
    .await
    .unwrap();
    let ok = read_json(&mut br).await;
    assert_eq!(ok["id"], 4);
    assert_eq!(ok["result"], true, "{ok}");
    match seen.recv().await.unwrap() {
        Seen::Share(s) => {
            assert_eq!(s.channel_id, CHANNEL);
            assert_eq!(s.job_id, 1);
            assert_eq!(s.nonce, 0xdead_beef);
            assert_eq!(s.ntime, 0x6553_f101);
            assert_eq!(s.version, JOB_VERSION | 0x2000);
            assert_eq!(s.extranonce.as_bytes(), [1, 2, 3, 4, 5, 6, 7, 8]);
        }
        other => panic!("expected a share, got {other:?}"),
    }

    // Rejected share.
    w.write_all(
        b"{\"id\":5,\"method\":\"mining.submit\",\"params\":[\"bc1qredeemer.rig1\",\"1\",\"0102030405060709\",\"6553f101\",\"00000001\",\"00000000\"]}\n",
    )
    .await
    .unwrap();
    let rejected = read_json(&mut br).await;
    assert_eq!(rejected["id"], 5);
    assert!(rejected["result"] != true, "{rejected}");
    assert_eq!(rejected["error"][1], "difficulty-too-low");
}

#[tokio::test]
async fn wrong_authority_key_fails_the_dial() {
    let pool = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = pool.local_addr().unwrap().port();
    let (tx, _seen) = mpsc::unbounded_channel();
    tokio::spawn(mock_pool(pool, tx, vec![]));
    // Some other valid key: the pool's certificate won't verify.
    let other = Secp256k1PublicKey::from_str("9auqWEzQDVyd2oe1JVGFLMLHZtCo2FFqZwtKA5gd9xbuEu7PH72")
        .unwrap();
    let upstream = normalize_upstream(&format!("stratum2+tcp://127.0.0.1:{port}/{other}")).unwrap();
    let res = hashu_proxy::upstream::dial(&upstream).await;
    assert!(res.is_err(), "dial must fail with the wrong authority key");
}

/// An SV1 pool for idle hashrate: answers the handshake, sends one job,
/// accepts every share.
async fn sv1_default_pool() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let (sock, _) = l.accept().await.unwrap();
            tokio::spawn(async move {
                let (r, mut w) = sock.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let v: Value = serde_json::from_str(&line).unwrap();
                    let id = v["id"].clone();
                    let out = match v["method"].as_str().unwrap_or_default() {
                        "mining.configure" => vec![
                            serde_json::json!({"id": id, "result": {"version-rolling": true, "version-rolling.mask": "1fffe000"}, "error": null}),
                        ],
                        "mining.subscribe" => vec![
                            serde_json::json!({"id": id, "result": [[["mining.notify", "s"]], "d0d0d0d0", 8], "error": null}),
                        ],
                        "mining.authorize" => vec![
                            serde_json::json!({"id": id, "result": true, "error": null}),
                            serde_json::json!({"id": null, "method": "mining.set_difficulty", "params": [1.0]}),
                            serde_json::json!({"id": null, "method": "mining.notify", "params": ["dj1", "11".repeat(32), "01aa", "02bb", [], "20000000", "1d00ffff", "60000000", false]}),
                        ],
                        "mining.submit" => {
                            vec![serde_json::json!({"id": id, "result": true, "error": null})]
                        }
                        _ => vec![],
                    };
                    for o in out {
                        if w.write_all(format!("{o}\n").as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    addr
}

#[tokio::test]
async fn redemption_switches_live_miner_to_sv2_pool() {
    let (tx, mut seen) = mpsc::unbounded_channel();
    let pool = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = pool.local_addr().unwrap().port();
    tokio::spawn(mock_pool(pool, tx, vec![true]));
    let (pk, _) = keys();

    let ledger = Arc::new(RedemptionLedger::new());
    let router = Arc::new(Router::new(sv1_default_pool().await, ledger.clone()));
    let credit = CreditContext::new(Arc::new(ShareCrediter::new(ledger.clone())), router.clone());
    let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = probe.local_addr().unwrap();
    drop(probe);
    tokio::spawn(run_routed_listener(
        proxy,
        router,
        Arc::new(ProxyMetrics::default()),
        Some(credit),
        Duration::from_millis(20),
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;

    let miner = TcpStream::connect(proxy).await.unwrap();
    let (r, mut w) = miner.into_split();
    let mut br = BufReader::new(r);
    w.write_all(concat!(
        r#"{"id":1,"method":"mining.configure","params":[["version-rolling"],{"version-rolling.mask":"1fffe000"}]}"#, "\n",
        r#"{"id":2,"method":"mining.subscribe","params":["bitaxe/2.0"]}"#, "\n",
        r#"{"id":3,"method":"mining.authorize","params":["miner.1","x"]}"#, "\n",
    ).as_bytes())
    .await
    .unwrap();
    // The miner's extranonce, owned by the proxy.
    let (mut en1, mut en2_size) = (String::new(), 0);
    loop {
        let v = read_json(&mut br).await;
        if v["id"] == 2 {
            en1 = v["result"][1].as_str().unwrap().to_string();
            en2_size = v["result"][2].as_u64().unwrap() as usize;
        }
        if v["method"] == "mining.notify" {
            assert_eq!(v["params"][0], "dj1");
            break;
        }
    }
    assert_eq!((en1.len() / 2, en2_size), (2, 4));

    let id = RedemptionId::from_bytes([9; 16]);
    ledger
        .open(Redemption::new(
            id,
            3_274,
            format!("stratum2+tcp://127.0.0.1:{port}/{pk}"),
            "alice.rig".into(),
        ))
        .unwrap();

    // Same socket: the SV2 pool's job arrives, clean, with the SV2
    // prefix folded into coinb1.
    let notify = loop {
        let v = read_json(&mut br).await;
        if v["method"] == "mining.notify" && v["params"][0] == "1" {
            break v;
        }
    };
    assert_eq!(notify["params"][8], true);
    let coinb1 = notify["params"][2].as_str().unwrap();
    assert!(coinb1.ends_with("aabbccdd"), "{coinb1}");
    match seen.recv().await.unwrap() {
        Seen::User(u) => assert_eq!(u, "alice.rig"),
        other => panic!("{other:?}"),
    }

    w.write_all(
        b"{\"id\":10,\"method\":\"mining.submit\",\"params\":[\"miner.1\",\"1\",\"01020304\",\"6553f101\",\"deadbeef\",\"00002000\"]}\n",
    )
    .await
    .unwrap();
    let ok = loop {
        let v = read_json(&mut br).await;
        if v["id"] == 10 {
            break v;
        }
    };
    assert_eq!(ok["result"], true, "{ok}");
    match seen.recv().await.unwrap() {
        Seen::Share(s) => {
            // The pool sees the miner's extranonce mapped into its 8 bytes:
            // the proxy's 2, the miner's 4, then padding.
            let en = s.extranonce.as_bytes().to_vec();
            assert_eq!(en.len(), EXTRANONCE_SIZE as usize);
            assert_eq!(hex::encode(&en[..2]), en1);
            assert_eq!(en[2..6], [1, 2, 3, 4]);
            assert_eq!(s.version, JOB_VERSION | 0x2000);
        }
        other => panic!("{other:?}"),
    }
    // The accepted share was credited to the redemption.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if ledger.get(&id).unwrap().accumulated_hashes > 0 {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "share not credited");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
