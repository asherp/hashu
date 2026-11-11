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
//! The encoding is [`glossia::cashu`]: the token is packed into a compact
//! envelope (raw 32-byte secrets, no DLEQ, mint URL and unit always
//! written) before Glossia sees it, which roughly halves the words. A token
//! the envelope can't hold exactly (a locked secret, a witness, an amount
//! that isn't a power of two) is carried as raw CBOR, so nothing but DLEQ
//! is ever lost. Any Cashu V4 token works, not only THH ones.

use std::str::FromStr;

/// A mint's identity: the 32-byte x-only (BIP-340) public key of its
/// offline root key. Carried beside the mint URL so a wallet can tell when
/// that URL no longer points at the mint that issued the voucher. It is not
/// part of the Cashu token and is dropped when the words become `cashuB`.
pub use glossia::cashu::MintKey;

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
    glossia::cashu::cashu_to_words_with_mint_key(token, mint_key, LANGUAGE, WORDLIST).ok()
}

/// A paragraph of English → `cashuB…`. Text around the paragraph (a
/// greeting, a sign-off) is dropped by trying whole sentences off each
/// end, since ordinary words like "about" or "you" are BIP39 words too.
pub fn from_words(text: &str) -> Option<FromWords> {
    // Cheap rejection: the paragraph is dozens of sentences of words.
    if text.split_whitespace().count() < 20 {
        return None;
    }
    let d = glossia::cashu::cashu_from_words(text, LANGUAGE, WORDLIST).ok()?;
    // Only hand back what a Cashu wallet will accept.
    cdk::nuts::Token::from_str(&d.token).ok()?;
    Some(FromWords {
        token: d.token,
        repaired: d.repaired.len(),
        mint_key: d.mint_key,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ciborium::value::Value;

    fn b64() -> base64::engine::GeneralPurpose {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
    }

    fn token_cbor(token: &str) -> Vec<u8> {
        b64().decode(token.strip_prefix("cashuB").unwrap()).unwrap()
    }

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

    /// The same token with every proof's DLEQ removed.
    fn without_dleq(tok: &str) -> String {
        let mut v: Value = ciborium::de::from_reader(&token_cbor(tok)[..]).unwrap();
        let t = |s: &str| Value::Text(s.to_string());
        for (k, ks) in v.as_map_mut().unwrap() {
            if *k != t("t") {
                continue;
            }
            for ks in ks.as_array_mut().unwrap() {
                for (k, ps) in ks.as_map_mut().unwrap() {
                    if *k != t("p") {
                        continue;
                    }
                    for p in ps.as_array_mut().unwrap() {
                        p.as_map_mut().unwrap().retain(|(k, _)| *k != t("d"));
                    }
                }
            }
        }
        let mut cbor = Vec::new();
        ciborium::ser::into_writer(&v, &mut cbor).unwrap();
        format!("cashuB{}", b64().encode(cbor))
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
        let compact = to_words(&tok).unwrap().split_whitespace().count();
        // The same token as raw CBOR (envelope tag 0x00).
        let mut raw = vec![0u8];
        raw.extend(token_cbor(&tok));
        let raw = glossia::canonical_encode(&raw, LANGUAGE, WORDLIST)
            .unwrap()
            .split_whitespace()
            .count();
        assert!(compact * 100 < raw * 65, "{compact} vs {raw} words");
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
        for tok in [
            token("https://mint.hashu.io", "thh", None, &[2, 8], false),
            // An odd amount: carried as raw CBOR with the key.
            token("https://mint.hashu.io", "thh", None, &[3], false),
        ] {
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
