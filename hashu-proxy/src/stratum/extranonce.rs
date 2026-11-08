//! Extranonce translation, so one miner connection can be moved between
//! upstream pools without reconnecting.
//!
//! A Stratum V1 coinbase is `coinb1 || extranonce1 || extranonce2 ||
//! coinb2`. The pool picks `extranonce1` (per session) and the size of
//! `extranonce2`; the miner fills `extranonce2`. A miner subscribed to
//! pool A keeps A's `extranonce1` for the life of its connection unless it
//! accepts `mining.set_extranonce`, which many miners do not.
//!
//! To hand the same miner to pool B, the proxy gives the miner a
//! *downstream* extranonce it owns and maps it into each upstream's
//! `extranonce2` space:
//!
//! ```text
//! upstream:  coinb1_U | en1_U | en2_U (size_U)                      | coinb2_U
//!                            = en1_D | en2_miner (size_D) | pad
//! miner sees: coinb1' = coinb1_U || en1_U,   en1 = en1_D,
//!             coinb2' = pad || coinb2_U,     en2 size = size_D
//! ```
//!
//! The miner builds exactly the upstream's coinbase, and the proxy
//! rewrites each `mining.submit`'s extranonce2 to `en1_D || en2_miner ||
//! pad`. This works for any upstream whose `extranonce2` is at least
//! `len(en1_D) + size_D` bytes. [`virtual_downstream`] keeps that small
//! (2 + 4 bytes) so most pools qualify.

use serde_json::Value;

/// Length of the proxy-owned extranonce1 handed to miners.
pub const VIRTUAL_EN1_LEN: usize = 2;
/// extranonce2 size handed to miners. Four bytes is plenty for an ASIC
/// that also rolls nonce, ntime and version bits.
pub const VIRTUAL_EN2_SIZE: usize = 4;

/// One side's extranonce parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extranonce {
    pub en1: Vec<u8>,
    pub en2_size: usize,
}

impl Extranonce {
    /// Bytes of the upstream coinbase this view occupies (`en1 + en2`).
    pub fn width(&self) -> usize {
        self.en1.len() + self.en2_size
    }
}

/// The extranonce a miner should see when the first upstream offers
/// `up`, or `None` if `up`'s extranonce2 is too small to carve a
/// downstream out of (the miner then sees `up` itself).
pub fn virtual_downstream(up: &Extranonce, tag: u16) -> Option<Extranonce> {
    if up.en2_size < VIRTUAL_EN1_LEN + VIRTUAL_EN2_SIZE {
        return None;
    }
    Some(Extranonce {
        en1: tag.to_be_bytes().to_vec(),
        en2_size: VIRTUAL_EN2_SIZE,
    })
}

/// How one upstream's extranonce maps onto the miner's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtranonceMap {
    up: Extranonce,
    down: Extranonce,
}

impl ExtranonceMap {
    /// The miner sees the upstream's own values: lines pass untouched.
    pub fn identity(up: Extranonce) -> Self {
        Self {
            down: up.clone(),
            up,
        }
    }

    /// Map `down` (what the miner was told) into `up`, or `None` if
    /// `up`'s extranonce2 cannot hold the miner's whole extranonce.
    pub fn new(up: Extranonce, down: Extranonce) -> Option<Self> {
        if up == down {
            return Some(Self::identity(up));
        }
        (up.en2_size >= down.width()).then_some(Self { up, down })
    }

    pub fn is_identity(&self) -> bool {
        self.up == self.down
    }

    pub fn upstream(&self) -> &Extranonce {
        &self.up
    }

    pub fn downstream(&self) -> &Extranonce {
        &self.down
    }

    fn pad(&self) -> usize {
        self.up.en2_size - self.down.width()
    }

    /// Rewrite a pool's `mining.notify` for the miner: `coinb1` gains the
    /// upstream extranonce1, `coinb2` gains the zero pad in front. `None`
    /// means "write the line unchanged" (identity map, or not a notify).
    pub fn patch_notify(&self, line: &str) -> Option<String> {
        if self.is_identity() {
            return None;
        }
        let mut v = parse(line)?;
        if v.get("method").and_then(Value::as_str) != Some("mining.notify") {
            return None;
        }
        let params = v.get_mut("params")?.as_array_mut()?;
        if params.len() < 4 {
            return None;
        }
        let coinb1 = params[2].as_str()?.to_string();
        let coinb2 = params[3].as_str()?.to_string();
        params[2] = Value::String(format!("{coinb1}{}", hex::encode(&self.up.en1)));
        params[3] = Value::String(format!("{}{coinb2}", "00".repeat(self.pad())));
        to_line(&v)
    }

    /// Rewrite a miner's `mining.submit` for the pool: extranonce2
    /// becomes `en1_D || en2_miner || pad`. `None` means "forward
    /// unchanged" (identity map, not a submit, or an extranonce2 of the
    /// wrong size, which the pool will reject on its own).
    pub fn patch_submit(&self, line: &str) -> Option<String> {
        if self.is_identity() {
            return None;
        }
        let mut v = parse(line)?;
        if v.get("method").and_then(Value::as_str) != Some("mining.submit") {
            return None;
        }
        let params = v.get_mut("params")?.as_array_mut()?;
        let en2 = hex::decode(params.get(2)?.as_str()?).ok()?;
        if en2.len() != self.down.en2_size {
            return None;
        }
        let mut full = self.down.en1.clone();
        full.extend_from_slice(&en2);
        full.resize(full.len() + self.pad(), 0);
        params[2] = Value::String(hex::encode(full));
        to_line(&v)
    }
}

/// Rewrite a `mining.subscribe` response so the miner sees `down` as its
/// extranonce1 / extranonce2 size. `None` if the line is not a subscribe
/// result.
pub fn rewrite_subscribe_result(line: &str, down: &Extranonce) -> Option<String> {
    let mut v = parse(line)?;
    let result = v.get_mut("result")?.as_array_mut()?;
    if result.len() != 3 || !result[1].is_string() {
        return None;
    }
    result[1] = Value::String(hex::encode(&down.en1));
    result[2] = Value::from(down.en2_size as u64);
    to_line(&v)
}

/// `mining.set_extranonce` notification telling the miner to use `x`.
pub fn set_extranonce_line(x: &Extranonce) -> String {
    let v = serde_json::json!({
        "id": null,
        "method": "mining.set_extranonce",
        "params": [hex::encode(&x.en1), x.en2_size],
    });
    format!("{v}\n")
}

fn parse(line: &str) -> Option<Value> {
    serde_json::from_str(line.trim()).ok()
}

fn to_line(v: &Value) -> Option<String> {
    let mut s = serde_json::to_string(v).ok()?;
    s.push('\n');
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn x(en1: &str, en2_size: usize) -> Extranonce {
        Extranonce {
            en1: hex::decode(en1).unwrap(),
            en2_size,
        }
    }

    fn sha256d(b: &[u8]) -> [u8; 32] {
        Sha256::digest(Sha256::digest(b)).into()
    }

    const NOTIFY: &str = r#"{"id":null,"method":"mining.notify","params":["j1","00","01000000aa","bbcc",[],"20000000","1d00ffff","60000000",false]}"#;

    #[test]
    fn virtual_downstream_needs_room() {
        assert_eq!(virtual_downstream(&x("abcdef01", 4), 7), None);
        assert_eq!(virtual_downstream(&x("abcdef01", 8), 7), Some(x("0007", 4)));
        assert_eq!(virtual_downstream(&x("ab", 6), 0x0102), Some(x("0102", 4)));
    }

    #[test]
    fn map_requires_upstream_en2_to_hold_downstream() {
        let down = x("0007", 4);
        assert!(ExtranonceMap::new(x("aabbccdd", 8), down.clone()).is_some());
        assert!(ExtranonceMap::new(x("aabbccdd", 6), down.clone()).is_some());
        assert!(ExtranonceMap::new(x("aabbccdd", 4), down.clone()).is_none());
        // Same values both sides: identity even though 4 < 6.
        let id = ExtranonceMap::new(x("aabbccdd", 4), x("aabbccdd", 4)).unwrap();
        assert!(id.is_identity());
        assert_eq!(id.patch_notify(NOTIFY), None);
    }

    /// The coinbase the miner builds from the patched notify is
    /// byte-for-byte the one the pool builds from the patched submit.
    #[test]
    fn miner_and_pool_build_the_same_coinbase() {
        let up = x("f00dbabe", 8);
        let down = x("0007", 4);
        let m = ExtranonceMap::new(up.clone(), down.clone()).unwrap();

        let n: Value = serde_json::from_str(&m.patch_notify(NOTIFY).unwrap()).unwrap();
        let miner_en2 = hex::decode("11223344").unwrap();
        let miner_cb = [
            hex::decode(n["params"][2].as_str().unwrap()).unwrap(),
            down.en1.clone(),
            miner_en2.clone(),
            hex::decode(n["params"][3].as_str().unwrap()).unwrap(),
        ]
        .concat();

        let submit = r#"{"id":9,"method":"mining.submit","params":["w","j1","11223344","60000000","00000001"]}"#;
        let s: Value = serde_json::from_str(&m.patch_submit(submit).unwrap()).unwrap();
        let pool_en2 = hex::decode(s["params"][2].as_str().unwrap()).unwrap();
        assert_eq!(pool_en2.len(), up.en2_size);
        let pool_cb = [
            hex::decode("01000000aa").unwrap(),
            up.en1.clone(),
            pool_en2,
            hex::decode("bbcc").unwrap(),
        ]
        .concat();

        assert_eq!(miner_cb, pool_cb);
        assert_eq!(sha256d(&miner_cb), sha256d(&pool_cb));
        // Other fields survive.
        assert_eq!(s["params"][1], "j1");
        assert_eq!(s["id"], 9);
        assert_eq!(n["params"][0], "j1");
        assert_eq!(n["params"][8], false);
    }

    #[test]
    fn wrong_size_submit_is_left_alone() {
        let m = ExtranonceMap::new(x("f00dbabe", 8), x("0007", 4)).unwrap();
        let submit =
            r#"{"id":9,"method":"mining.submit","params":["w","j1","1122","60000000","00000001"]}"#;
        assert_eq!(m.patch_submit(submit), None);
        let auth = r#"{"id":2,"method":"mining.authorize","params":["w","x"]}"#;
        assert_eq!(m.patch_submit(auth), None);
        assert_eq!(m.patch_notify(auth), None);
    }

    #[test]
    fn subscribe_result_is_rewritten() {
        let line = r#"{"id":1,"result":[[["mining.notify","s"]],"f00dbabe",8],"error":null}"#;
        let out: Value =
            serde_json::from_str(&rewrite_subscribe_result(line, &x("0007", 4)).unwrap()).unwrap();
        assert_eq!(out["result"][1], "0007");
        assert_eq!(out["result"][2], 4);
        assert_eq!(out["result"][0][0][0], "mining.notify");
        assert_eq!(
            rewrite_subscribe_result(r#"{"id":2,"result":true,"error":null}"#, &x("00", 4)),
            None
        );
    }

    #[test]
    fn set_extranonce_line_shape() {
        let v: Value = serde_json::from_str(&set_extranonce_line(&x("abcd", 6))).unwrap();
        assert_eq!(v["method"], "mining.set_extranonce");
        assert_eq!(v["params"], serde_json::json!(["abcd", 6]));
    }
}
