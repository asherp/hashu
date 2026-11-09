//! THH vouchers as plain English, with [Glossia](https://github.com/asherp/glossia).
//!
//! A `cashuB` token becomes a paragraph of ordinary sentences that decodes
//! back to the same voucher: Glossia hides BIP39 words, in order, among
//! cover words that are never BIP39 words, and its canonical form adds a
//! checksum and Reed–Solomon parity, so a few mistyped words are repaired.
//! The paragraph can be read aloud, written down or pasted into a chat, and
//! the mint is not readable in it and it does not look like a token to a scanner. It is still
//! a bearer voucher: whoever can read it can redeem it.
//!
//! Before Glossia sees it, the token is packed into a compact envelope,
//! which roughly halves the words. A V4 token stores each 32-byte secret
//! as 64 hex characters; the envelope stores the raw 32 bytes. It is the
//! same envelope Glossia uses for any Cashu token (`glossia::cashu`).
//! Envelope (all lengths one byte):
//!
//! ```text
//! 0x00 cbor..                       a V4 token's CBOR, unchanged (fallback)
//! 0x02 key[32] cbor..               the same, with a mint identity key
//! 0x01 flags len url [key[32]] len unit [len memo]
//!      nkeysets { len id  nproofs { exp secret[32] C[33] } }
//! ```
//!
//! `flags` bit 0: the URL is written without its `https://`; bit 1: a
//! memo follows; bit 2: the mint's 32-byte identity key follows the URL
//! (see [`MintKey`]). The mint URL and unit are always written: the format
//! assumes no mint and no unit. A proof's amount is `2^exp`. DLEQ
//! proofs are left out: the receiver checks the voucher with the mint
//! instead. A token the envelope can't hold exactly (a locked secret, a
//! witness, an amount that isn't a power of two) is packed as `0x00` so
//! nothing is ever lost. Any Cashu V4 token works, not only THH ones.

use std::str::FromStr;

use base64::Engine as _;
use ciborium::value::Value;

const RAW: u8 = 0x00;
const COMPACT: u8 = 0x01;
const RAW_WITH_MINT_KEY: u8 = 0x02;
const HTTPS_IMPLIED: u8 = 1;
const HAS_MEMO: u8 = 2;
const HAS_MINT_KEY: u8 = 4;

/// A mint's identity: the 32-byte x-only (BIP-340) public key of its
/// offline root key. Carried beside the mint URL so a wallet can tell when
/// that URL no longer points at the mint that issued the voucher. It is not
/// part of the Cashu token and is dropped when the words become `cashuB`.
pub type MintKey = [u8; 32];

const LANGUAGE: &str = "english";
const WORDLIST: &str = "bip39";

/// A voucher read back from words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromWords {
    /// The `cashuB` token.
    pub token: String,
    /// How many words parity repaired (0 when the text arrived intact).
    pub repaired: usize,
    /// The mint identity key the words carried, if any.
    pub mint_key: Option<MintKey>,
}

/// `cashuB…` → a paragraph of English. `None` if it isn't a V4 token.
pub fn to_words(token: &str) -> Option<String> {
    to_words_with_mint_key(token, None)
}

/// [`to_words`], carrying the mint's identity key beside its URL. It costs
/// about 27 words.
pub fn to_words_with_mint_key(token: &str, mint_key: Option<&MintKey>) -> Option<String> {
    let cbor = token_cbor(token.trim())?;
    let packed = pack(&cbor, mint_key);
    glossia::canonical_encode(&packed, LANGUAGE, WORDLIST).ok()
}

/// A paragraph of English → `cashuB…`. Text around the paragraph (a
/// greeting, a sign-off) is dropped by trying whole sentences off each
/// end, since ordinary words like "about" or "you" are BIP39 words too.
pub fn from_words(text: &str) -> Option<FromWords> {
    // Cheap rejection: the paragraph is dozens of sentences of words.
    if text.split_whitespace().count() < 20 {
        return None;
    }
    let cuts = sentence_starts(text);
    const TRIM: usize = 4;
    let starts = cuts.iter().take(TRIM + 1);
    for &a in starts {
        let ends = std::iter::once(text.len()).chain(cuts.iter().rev().copied().take(TRIM));
        for b in ends {
            if b <= a {
                continue;
            }
            if let Ok(d) = glossia::canonical_decode(&text[a..b], LANGUAGE, WORDLIST) {
                if let Some((cbor, mint_key)) = unpack(&d.payload) {
                    if let Some(token) = cbor_token(&cbor) {
                        return Some(FromWords {
                            token,
                            repaired: d.repaired.len(),
                            mint_key,
                        });
                    }
                }
            }
        }
    }
    None
}

/// Byte offsets where a sentence or line starts, the first being 0.
fn sentence_starts(text: &str) -> Vec<usize> {
    let mut out = vec![0];
    let mut after_stop = false;
    for (i, ch) in text.char_indices() {
        if matches!(ch, '.' | '!' | '?' | ':' | '\n') {
            after_stop = true;
        } else if after_stop && !ch.is_whitespace() {
            out.push(i);
            after_stop = false;
        }
    }
    out
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::URL_SAFE,
        base64::engine::GeneralPurposeConfig::new()
            .with_encode_padding(false)
            .with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
    )
}

fn token_cbor(token: &str) -> Option<Vec<u8>> {
    let body = token.strip_prefix("cashuB")?;
    // V3-era wallets sometimes emit standard base64; accept both.
    let body: String = body
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect();
    b64().decode(body).ok()
}

fn cbor_token(cbor: &[u8]) -> Option<String> {
    let s = format!("cashuB{}", b64().encode(cbor));
    // Only hand back what a Cashu wallet will accept.
    cdk::nuts::Token::from_str(&s).ok()?;
    Some(s)
}

fn pack(cbor: &[u8], mint_key: Option<&MintKey>) -> Vec<u8> {
    compact(cbor, mint_key).unwrap_or_else(|| {
        let mut out = match mint_key {
            Some(k) => {
                let mut v = vec![RAW_WITH_MINT_KEY];
                v.extend_from_slice(k);
                v
            }
            None => vec![RAW],
        };
        out.extend_from_slice(cbor);
        out
    })
}

/// The envelope → a V4 token's CBOR and the mint identity key, if any.
fn unpack(packed: &[u8]) -> Option<(Vec<u8>, Option<MintKey>)> {
    let (&tag, rest) = packed.split_first()?;
    match tag {
        RAW => Some((rest.to_vec(), None)),
        RAW_WITH_MINT_KEY => {
            let key: MintKey = rest.get(..32)?.try_into().ok()?;
            Some((rest[32..].to_vec(), Some(key)))
        }
        COMPACT => expand(rest),
        _ => None,
    }
}

fn get<'a>(map: &'a [(Value, Value)], key: &str) -> Option<&'a Value> {
    map.iter()
        .find(|(k, _)| k.as_text() == Some(key))
        .map(|(_, v)| v)
}

fn push_short(out: &mut Vec<u8>, bytes: &[u8]) -> Option<()> {
    out.push(u8::try_from(bytes.len()).ok()?);
    out.extend_from_slice(bytes);
    Some(())
}

/// A V4 token's CBOR → the compact envelope, or `None` if it would lose
/// anything other than DLEQ proofs.
fn compact(cbor: &[u8], mint_key: Option<&MintKey>) -> Option<Vec<u8>> {
    let v: Value = ciborium::de::from_reader(cbor).ok()?;
    let top = v.as_map()?;
    if top
        .iter()
        .any(|(k, _)| !matches!(k.as_text(), Some("m" | "u" | "d" | "t")))
    {
        return None;
    }
    let mint = get(top, "m")?.as_text()?;
    let unit = get(top, "u")?.as_text()?;
    let memo = match get(top, "d") {
        None => None,
        Some(d) => Some(d.as_text()?),
    };
    let mut flags = 0u8;
    let mut head = Vec::new();
    // The mint and unit are always written: the format favours no mint
    // and no unit.
    match mint.strip_prefix("https://") {
        Some(rest) => {
            flags |= HTTPS_IMPLIED;
            push_short(&mut head, rest.as_bytes())?;
        }
        None => push_short(&mut head, mint.as_bytes())?,
    }
    if let Some(k) = mint_key {
        flags |= HAS_MINT_KEY;
        head.extend_from_slice(k);
    }
    push_short(&mut head, unit.as_bytes())?;
    if let Some(memo) = memo {
        flags |= HAS_MEMO;
        push_short(&mut head, memo.as_bytes())?;
    }
    let mut out = vec![COMPACT, flags];
    out.extend(head);
    let keysets = get(top, "t")?.as_array()?;
    out.push(u8::try_from(keysets.len()).ok()?);
    for ks in keysets {
        let ks = ks.as_map()?;
        if ks
            .iter()
            .any(|(k, _)| !matches!(k.as_text(), Some("i" | "p")))
        {
            return None;
        }
        push_short(&mut out, get(ks, "i")?.as_bytes()?)?;
        let proofs = get(ks, "p")?.as_array()?;
        out.push(u8::try_from(proofs.len()).ok()?);
        for p in proofs {
            let p = p.as_map()?;
            if p.iter()
                .any(|(k, _)| !matches!(k.as_text(), Some("a" | "s" | "c" | "d")))
            {
                return None;
            }
            let a = u64::try_from(get(p, "a")?.as_integer()?).ok()?;
            if !a.is_power_of_two() {
                return None;
            }
            out.push(a.trailing_zeros() as u8);
            let s = get(p, "s")?.as_text()?;
            // Only cdk's default secret, 64 lowercase hex, round-trips.
            if s.len() != 64 || !s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return None;
            }
            out.extend(hex::decode(s).ok()?);
            let c = get(p, "c")?.as_bytes()?;
            if c.len() != 33 {
                return None;
            }
            out.extend_from_slice(c);
        }
    }
    Some(out)
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn short(&mut self) -> Option<&'a [u8]> {
        let n = self.byte()? as usize;
        self.take(n)
    }
    fn text(&mut self) -> Option<String> {
        String::from_utf8(self.short()?.to_vec()).ok()
    }
}

/// The compact envelope (after its tag byte) → a V4 token's CBOR and the
/// mint identity key, if any.
fn expand(body: &[u8]) -> Option<(Vec<u8>, Option<MintKey>)> {
    let mut r = Reader(body);
    let flags = r.byte()?;
    if flags & !(HTTPS_IMPLIED | HAS_MEMO | HAS_MINT_KEY) != 0 {
        return None;
    }
    let m = r.text()?;
    let mint = if flags & HTTPS_IMPLIED != 0 {
        format!("https://{m}")
    } else {
        m
    };
    let mint_key = if flags & HAS_MINT_KEY != 0 {
        Some(MintKey::try_from(r.take(32)?).ok()?)
    } else {
        None
    };
    let unit = r.text()?;
    let memo = if flags & HAS_MEMO != 0 {
        Some(r.text()?)
    } else {
        None
    };
    let t = |s: &str| Value::Text(s.to_string());
    let mut keysets = Vec::new();
    for _ in 0..r.byte()? {
        let id = r.short()?.to_vec();
        let mut proofs = Vec::new();
        for _ in 0..r.byte()? {
            let exp = r.byte()?;
            if exp > 63 {
                return None;
            }
            let secret = hex::encode(r.take(32)?);
            let c = r.take(33)?.to_vec();
            proofs.push(Value::Map(vec![
                (t("a"), Value::Integer((1u64 << exp).into())),
                (t("s"), Value::Text(secret)),
                (t("c"), Value::Bytes(c)),
            ]));
        }
        keysets.push(Value::Map(vec![
            (t("i"), Value::Bytes(id)),
            (t("p"), Value::Array(proofs)),
        ]));
    }
    if !r.0.is_empty() {
        return None;
    }
    let mut top = vec![(t("m"), Value::Text(mint)), (t("u"), Value::Text(unit))];
    if let Some(memo) = memo {
        top.push((t("d"), Value::Text(memo)));
    }
    top.push((t("t"), Value::Array(keysets)));
    let mut out = Vec::new();
    ciborium::ser::into_writer(&Value::Map(top), &mut out).ok()?;
    Some((out, mint_key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A V4 token built by hand: `amounts` proofs at `mint`, with DLEQ
    /// proofs on each if `dleq`.
    fn token(mint: &str, unit: &str, memo: Option<&str>, amounts: &[u64], dleq: bool) -> String {
        let t = |s: &str| Value::Text(s.to_string());
        let mut seed = 7u8;
        let mut bytes = |n: usize| -> Vec<u8> {
            (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(31).wrapping_add(17);
                    seed
                })
                .collect()
        };
        let proofs = amounts
            .iter()
            .map(|&a| {
                let mut c = vec![2u8];
                c.extend(bytes(32));
                let mut p = vec![
                    (t("a"), Value::Integer(a.into())),
                    (t("s"), Value::Text(hex::encode(bytes(32)))),
                    (t("c"), Value::Bytes(c)),
                ];
                if dleq {
                    p.push((
                        t("d"),
                        Value::Map(vec![
                            (t("e"), Value::Bytes(bytes(32))),
                            (t("s"), Value::Bytes(bytes(32))),
                            (t("r"), Value::Bytes(bytes(32))),
                        ]),
                    ));
                }
                Value::Map(p)
            })
            .collect();
        let mut id = vec![0u8];
        id.extend(bytes(7));
        let mut top = vec![(t("m"), t(mint)), (t("u"), t(unit))];
        if let Some(m) = memo {
            top.push((t("d"), t(m)));
        }
        top.push((
            t("t"),
            Value::Array(vec![Value::Map(vec![
                (t("i"), Value::Bytes(id)),
                (t("p"), Value::Array(proofs)),
            ])]),
        ));
        let mut cbor = Vec::new();
        ciborium::ser::into_writer(&Value::Map(top), &mut cbor).unwrap();
        format!("cashuB{}", b64().encode(cbor))
    }

    fn without_dleq(tok: &str) -> String {
        cbor_token(
            &expand(&compact(&token_cbor(tok).unwrap(), None).unwrap()[1..])
                .unwrap()
                .0,
        )
        .unwrap()
    }

    #[test]
    fn hashu_voucher_round_trips_through_words() {
        let tok = token(
            "https://mint.hashu.io",
            "thh",
            Some("Hashu THH"),
            &[2, 8],
            false,
        );
        let words = to_words(&tok).unwrap();
        assert!(!words.contains("cashu"));
        let back = from_words(&words).unwrap();
        assert_eq!(
            back,
            FromWords {
                token: tok,
                repaired: 0,
                mint_key: None,
            }
        );
    }

    #[test]
    fn compact_shrinks_the_payload() {
        let tok = token("https://mint.hashu.io", "thh", None, &[2, 8], false);
        let cbor = token_cbor(&tok).unwrap();
        // Two proofs: 267 bytes of CBOR pack into 163.
        assert!(
            pack(&cbor, None).len() * 100 < cbor.len() * 65,
            "{} vs {}",
            pack(&cbor, None).len(),
            cbor.len()
        );
    }

    #[test]
    fn other_mints_and_units_round_trip() {
        for mint in [
            "https://mint.minibits.cash/Bitcoin",
            "http://127.0.0.1:8085",
        ] {
            let tok = token(mint, "sat", None, &[1, 4, 64], false);
            assert_eq!(from_words(&to_words(&tok).unwrap()).unwrap().token, tok);
        }
    }

    #[test]
    fn dleq_is_dropped_but_the_proofs_survive() {
        let tok = token("https://mint.hashu.io", "thh", None, &[16], true);
        let back = from_words(&to_words(&tok).unwrap()).unwrap().token;
        assert_eq!(back, without_dleq(&tok));
        assert!(back.len() < tok.len());
    }

    #[test]
    fn locked_or_odd_tokens_fall_back_to_raw() {
        let tok = token("https://mint.hashu.io", "thh", None, &[3], false);
        let cbor = token_cbor(&tok).unwrap();
        assert_eq!(pack(&cbor, None)[0], RAW);
        assert_eq!(from_words(&to_words(&tok).unwrap()).unwrap().token, tok);
    }

    #[test]
    fn mistyped_words_are_repaired() {
        let tok = token("https://mint.hashu.io", "thh", None, &[2, 8], false);
        let words = to_words(&tok).unwrap();
        let bip39: std::collections::HashSet<String> =
            glossia::load_payload_words_for_wordlist(LANGUAGE, WORDLIST)
                .unwrap()
                .into_iter()
                .collect();
        // Swap two payload words for another BIP39 word, as a mistyped copy would.
        let mut n = 0;
        let damaged: Vec<String> = words
            .split(' ')
            .map(|w| {
                let bare = w.trim_matches(|c: char| !c.is_alphabetic()).to_lowercase();
                if bip39.contains(&bare) && bare != "zoo" {
                    n += 1;
                    if n == 5 || n == 40 {
                        return w.to_lowercase().replace(&bare, "zoo");
                    }
                }
                w.to_string()
            })
            .collect();
        let back = from_words(&damaged.join(" ")).unwrap();
        assert_eq!(back.token, tok);
        assert_eq!(back.repaired, 2);
    }

    #[test]
    fn mint_key_rides_beside_the_token() {
        let key: MintKey = std::array::from_fn(|i| i as u8 + 1);
        for (tok, tag) in [
            (
                token("https://mint.hashu.io", "thh", None, &[2, 8], false),
                COMPACT,
            ),
            (
                token("https://mint.hashu.io", "thh", None, &[3], false),
                RAW_WITH_MINT_KEY,
            ),
        ] {
            let cbor = token_cbor(&tok).unwrap();
            assert_eq!(pack(&cbor, Some(&key))[0], tag);
            let words = to_words_with_mint_key(&tok, Some(&key)).unwrap();
            let back = from_words(&words).unwrap();
            // The token is a standard Cashu token; the key comes back beside it.
            assert_eq!(back.token, tok);
            assert_eq!(back.mint_key, Some(key));
        }
    }

    #[test]
    fn surrounding_chat_is_ignored() {
        let tok = token("https://mint.hashu.io", "thh", None, &[2, 8], false);
        let words = to_words(&tok).unwrap();
        let msg = format!("Hey, this is about the hashrate I promised you:\n\n{words}\n\nThanks again, see you soon!");
        assert_eq!(from_words(&msg).unwrap().token, tok);
    }

    #[test]
    fn ordinary_text_is_not_a_voucher() {
        assert_eq!(from_words("hello"), None);
        assert_eq!(
            from_words("The quick brown fox jumps over the lazy dog, again and again, because it can and nobody stops it from doing so today."),
            None
        );
    }
}
