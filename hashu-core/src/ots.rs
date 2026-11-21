//! Just enough OpenTimestamps to stamp a digest, upgrade the proof once a
//! calendar has committed it to Bitcoin, and write a standard `.ots` file
//! that `ots verify` (or any OTS client) accepts.
//!
//! Format: <https://github.com/opentimestamps/python-opentimestamps>. A
//! timestamp is a tree: each node holds attestations and operations; an
//! operation maps the node's message to its child's. Calendars use
//! `sha256`, `append` and `prepend`; those plus `sha1`, `ripemd160` and
//! `reverse` (seen in older proofs) are supported here, anything else is
//! a parse error.

use sha2::{Digest, Sha256};

/// Magic bytes that open every `.ots` file.
pub const HEADER_MAGIC: &[u8] =
    b"\x00OpenTimestamps\x00\x00Proof\x00\xbf\x89\xe2\xe8\x84\xe8\x92\x94";
const MAJOR_VERSION: u64 = 1;

const TAG_PENDING: [u8; 8] = [0x83, 0xdf, 0xe3, 0x0d, 0x2e, 0xf9, 0x0c, 0x8e];
const TAG_BITCOIN: [u8; 8] = [0x05, 0x88, 0x96, 0x0d, 0x73, 0xd7, 0x19, 0x01];

const OP_SHA1: u8 = 0x02;
const OP_RIPEMD160: u8 = 0x03;
const OP_SHA256: u8 = 0x08;
const OP_APPEND: u8 = 0xf0;
const OP_PREPEND: u8 = 0xf1;
const OP_REVERSE: u8 = 0xf2;

const MAX_DEPTH: usize = 256;
const MAX_MSG: usize = 4096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum OtsError {
    #[error("truncated timestamp")]
    Truncated,
    #[error("unsupported operation 0x{0:02x}")]
    UnsupportedOp(u8),
    #[error("timestamp too deep")]
    TooDeep,
    #[error("not an .ots file")]
    BadHeader,
    #[error("file hashes with something other than sha256")]
    NotSha256,
    #[error("empty timestamp node")]
    Empty,
    #[error("message too long")]
    TooLong,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attestation {
    /// A calendar promises to commit the message to Bitcoin.
    Pending { uri: String },
    /// The message is the merkle root of the block at this height.
    Bitcoin { height: u64 },
    /// Some other kind, kept as is.
    Unknown { tag: [u8; 8], payload: Vec<u8> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Sha1,
    Ripemd160,
    Sha256,
    Append(Vec<u8>),
    Prepend(Vec<u8>),
    Reverse,
}

impl Op {
    pub fn apply(&self, msg: &[u8]) -> Vec<u8> {
        match self {
            Op::Sha1 => sha1::Sha1::digest(msg).to_vec(),
            Op::Ripemd160 => ripemd::Ripemd160::digest(msg).to_vec(),
            Op::Sha256 => Sha256::digest(msg).to_vec(),
            Op::Append(a) => [msg, a].concat(),
            Op::Prepend(p) => [p.as_slice(), msg].concat(),
            Op::Reverse => msg.iter().rev().copied().collect(),
        }
    }
}

/// One node: the message it commits and what hangs off it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Timestamp {
    pub msg: Vec<u8>,
    pub attestations: Vec<Attestation>,
    pub ops: Vec<(Op, Timestamp)>,
}

impl Timestamp {
    pub fn new(msg: Vec<u8>) -> Self {
        Self {
            msg,
            attestations: Vec::new(),
            ops: Vec::new(),
        }
    }

    /// Parse a serialized timestamp (a calendar response) for `msg`.
    pub fn parse(msg: &[u8], bytes: &[u8]) -> Result<Self, OtsError> {
        let mut r = Reader { b: bytes, i: 0 };
        let t = parse_node(&mut r, msg.to_vec(), 0)?;
        Ok(t)
    }

    pub fn serialize(&self) -> Result<Vec<u8>, OtsError> {
        let mut out = Vec::new();
        write_node(self, &mut out)?;
        Ok(out)
    }

    /// Fold `other` (a timestamp for the same message) into this one.
    pub fn merge(&mut self, other: Timestamp) {
        for a in other.attestations {
            if !self.attestations.contains(&a) {
                self.attestations.push(a);
            }
        }
        for (op, child) in other.ops {
            match self.ops.iter_mut().find(|(o, _)| *o == op) {
                Some((_, mine)) => mine.merge(child),
                None => self.ops.push((op, child)),
            }
        }
    }

    /// Every (message, calendar uri) still waiting on a calendar.
    pub fn pending(&self) -> Vec<(Vec<u8>, String)> {
        let mut out = Vec::new();
        self.walk(&mut |t| {
            for a in &t.attestations {
                if let Attestation::Pending { uri } = a {
                    out.push((t.msg.clone(), uri.clone()));
                }
            }
        });
        out
    }

    /// Every Bitcoin attestation: (block height, merkle root it claims,
    /// in internal byte order).
    pub fn bitcoin(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out = Vec::new();
        self.walk(&mut |t| {
            for a in &t.attestations {
                if let Attestation::Bitcoin { height } = a {
                    out.push((*height, t.msg.clone()));
                }
            }
        });
        out
    }

    /// True once at least one path ends in a Bitcoin block.
    pub fn is_complete(&self) -> bool {
        !self.bitcoin().is_empty()
    }

    /// Merge an upgrade for the node whose message is `msg`, dropping the
    /// pending attestation it replaces when it reaches Bitcoin. Returns
    /// whether a node matched.
    pub fn upgrade_at(&mut self, msg: &[u8], uri: &str, upgrade: Timestamp) -> bool {
        if self.msg == msg {
            let done = upgrade.is_complete();
            self.merge(upgrade);
            if done {
                self.attestations
                    .retain(|a| !matches!(a, Attestation::Pending { uri: u } if u == uri));
            }
            return true;
        }
        self.ops
            .iter_mut()
            .any(|(_, child)| child.upgrade_at(msg, uri, upgrade.clone()))
    }

    fn walk(&self, f: &mut dyn FnMut(&Timestamp)) {
        f(self);
        for (_, c) in &self.ops {
            c.walk(f);
        }
    }
}

/// A detached `.ots` proof for a SHA-256 digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DetachedTimestamp {
    pub timestamp: Timestamp,
}

impl DetachedTimestamp {
    /// A proof for `digest` with no attestations yet.
    pub fn for_digest(digest: [u8; 32]) -> Self {
        Self {
            timestamp: Timestamp::new(digest.to_vec()),
        }
    }

    pub fn digest(&self) -> &[u8] {
        &self.timestamp.msg
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, OtsError> {
        let mut out = HEADER_MAGIC.to_vec();
        write_varuint(MAJOR_VERSION, &mut out);
        out.push(OP_SHA256);
        out.extend_from_slice(&self.timestamp.msg);
        write_node(&self.timestamp, &mut out)?;
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, OtsError> {
        let rest = bytes
            .strip_prefix(HEADER_MAGIC)
            .ok_or(OtsError::BadHeader)?;
        let mut r = Reader { b: rest, i: 0 };
        if r.varuint()? != MAJOR_VERSION {
            return Err(OtsError::BadHeader);
        }
        if r.byte()? != OP_SHA256 {
            return Err(OtsError::NotSha256);
        }
        let digest = r.take(32)?.to_vec();
        let timestamp = parse_node(&mut r, digest, 0)?;
        Ok(Self { timestamp })
    }
}

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, OtsError> {
        let v = *self.b.get(self.i).ok_or(OtsError::Truncated)?;
        self.i += 1;
        Ok(v)
    }
    fn take(&mut self, n: usize) -> Result<&[u8], OtsError> {
        let end = self.i.checked_add(n).ok_or(OtsError::Truncated)?;
        let s = self.b.get(self.i..end).ok_or(OtsError::Truncated)?;
        self.i = end;
        Ok(s)
    }
    fn varuint(&mut self) -> Result<u64, OtsError> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            if shift < 64 {
                v |= ((b & 0x7f) as u64) << shift;
            }
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(OtsError::Truncated);
            }
        }
    }
    fn varbytes(&mut self) -> Result<Vec<u8>, OtsError> {
        let n = self.varuint()? as usize;
        if n > MAX_MSG {
            return Err(OtsError::TooLong);
        }
        Ok(self.take(n)?.to_vec())
    }
}

fn parse_node(r: &mut Reader, msg: Vec<u8>, depth: usize) -> Result<Timestamp, OtsError> {
    if depth > MAX_DEPTH {
        return Err(OtsError::TooDeep);
    }
    let mut t = Timestamp::new(msg);
    let mut tag = r.byte()?;
    loop {
        let more = tag == 0xff;
        let this = if more { r.byte()? } else { tag };
        if this == 0x00 {
            let atag: [u8; 8] = r.take(8)?.try_into().map_err(|_| OtsError::Truncated)?;
            let payload = r.varbytes()?;
            let mut p = Reader { b: &payload, i: 0 };
            let a = match atag {
                TAG_PENDING => Attestation::Pending {
                    uri: String::from_utf8_lossy(&p.varbytes()?).into_owned(),
                },
                TAG_BITCOIN => Attestation::Bitcoin {
                    height: p.varuint()?,
                },
                _ => Attestation::Unknown { tag: atag, payload },
            };
            t.attestations.push(a);
        } else {
            let op = match this {
                OP_SHA1 => Op::Sha1,
                OP_RIPEMD160 => Op::Ripemd160,
                OP_SHA256 => Op::Sha256,
                OP_APPEND => Op::Append(r.varbytes()?),
                OP_PREPEND => Op::Prepend(r.varbytes()?),
                OP_REVERSE => Op::Reverse,
                other => return Err(OtsError::UnsupportedOp(other)),
            };
            let next = op.apply(&t.msg);
            if next.len() > MAX_MSG {
                return Err(OtsError::TooLong);
            }
            let child = parse_node(r, next, depth + 1)?;
            t.ops.push((op, child));
        }
        if !more {
            return Ok(t);
        }
        tag = r.byte()?;
    }
}

fn write_varuint(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn write_varbytes(b: &[u8], out: &mut Vec<u8>) {
    write_varuint(b.len() as u64, out);
    out.extend_from_slice(b);
}

fn write_attestation(a: &Attestation, out: &mut Vec<u8>) {
    out.push(0x00);
    let (tag, payload) = match a {
        Attestation::Pending { uri } => {
            let mut p = Vec::new();
            write_varbytes(uri.as_bytes(), &mut p);
            (TAG_PENDING, p)
        }
        Attestation::Bitcoin { height } => {
            let mut p = Vec::new();
            write_varuint(*height, &mut p);
            (TAG_BITCOIN, p)
        }
        Attestation::Unknown { tag, payload } => (*tag, payload.clone()),
    };
    out.extend_from_slice(&tag);
    write_varbytes(&payload, out);
}

fn write_op(op: &Op, out: &mut Vec<u8>) {
    match op {
        Op::Sha1 => out.push(OP_SHA1),
        Op::Ripemd160 => out.push(OP_RIPEMD160),
        Op::Sha256 => out.push(OP_SHA256),
        Op::Append(a) => {
            out.push(OP_APPEND);
            write_varbytes(a, out);
        }
        Op::Prepend(p) => {
            out.push(OP_PREPEND);
            write_varbytes(p, out);
        }
        Op::Reverse => out.push(OP_REVERSE),
    }
}

fn write_node(t: &Timestamp, out: &mut Vec<u8>) -> Result<(), OtsError> {
    let n = t.attestations.len() + t.ops.len();
    if n == 0 {
        return Err(OtsError::Empty);
    }
    let mut i = 0;
    for a in &t.attestations {
        i += 1;
        if i < n {
            out.push(0xff);
        }
        write_attestation(a, out);
    }
    for (op, child) in &t.ops {
        i += 1;
        if i < n {
            out.push(0xff);
        }
        write_op(op, out);
        write_node(child, out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A calendar-style response: append a nonce, sha256, then pending.
    fn calendar_response(uri: &str) -> Vec<u8> {
        let mut out = Vec::new();
        write_op(&Op::Append(vec![7; 16]), &mut out);
        write_op(&Op::Sha256, &mut out);
        write_attestation(
            &Attestation::Pending {
                uri: uri.to_string(),
            },
            &mut out,
        );
        out
    }

    #[test]
    fn round_trips_a_two_calendar_proof() {
        let digest = [1u8; 32];
        let mut d = DetachedTimestamp::for_digest(digest);
        for uri in ["https://a.example", "https://b.example"] {
            let t = Timestamp::parse(&digest, &calendar_response(uri)).unwrap();
            d.timestamp.merge(t);
        }
        // Both calendars appended the same nonce, so their ops merged.
        assert_eq!(d.timestamp.ops.len(), 1);
        assert_eq!(d.timestamp.pending().len(), 2);
        let bytes = d.to_bytes().unwrap();
        let back = DetachedTimestamp::from_bytes(&bytes).unwrap();
        assert_eq!(back, d);
        assert!(!back.timestamp.is_complete());
    }

    #[test]
    fn upgrade_replaces_pending_with_bitcoin() {
        let digest = [2u8; 32];
        let mut d = DetachedTimestamp::for_digest(digest);
        d.timestamp
            .merge(Timestamp::parse(&digest, &calendar_response("https://a.example")).unwrap());
        let (msg, uri) = d.timestamp.pending().remove(0);
        // Upgrade: prepend, sha256, bitcoin attestation at height 900000.
        let mut up = Vec::new();
        write_op(&Op::Prepend(vec![9; 32]), &mut up);
        write_op(&Op::Sha256, &mut up);
        write_attestation(&Attestation::Bitcoin { height: 900_000 }, &mut up);
        let upgrade = Timestamp::parse(&msg, &up).unwrap();
        assert!(d.timestamp.upgrade_at(&msg, &uri, upgrade));
        assert!(d.timestamp.is_complete());
        assert!(d.timestamp.pending().is_empty());
        let (height, root) = d.timestamp.bitcoin().remove(0);
        assert_eq!(height, 900_000);
        assert_eq!(root, Sha256::digest([vec![9; 32], msg].concat()).to_vec());
        let back = DetachedTimestamp::from_bytes(&d.to_bytes().unwrap()).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(
            DetachedTimestamp::from_bytes(b"nope").unwrap_err(),
            OtsError::BadHeader
        );
        assert!(Timestamp::parse(&[0; 32], &[0x67]).is_err());
        assert!(Timestamp::parse(&[0; 32], &[]).is_err());
    }

    #[test]
    fn varuint_round_trip() {
        for v in [0u64, 1, 127, 128, 300, 900_000, u32::MAX as u64] {
            let mut out = Vec::new();
            write_varuint(v, &mut out);
            assert_eq!(Reader { b: &out, i: 0 }.varuint().unwrap(), v);
        }
    }
}
