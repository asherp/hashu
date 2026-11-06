//! Animated QR codes for Cashu tokens, as Uniform Resources (BCR-2020-005).
//!
//! A token with a few proofs is too long for one QR code a phone can scan
//! off a screen. Cashu wallets (Cashu.me, nutstash, Minibits) scan an
//! animated QR instead: the token's bytes as a `ur:bytes` resource, split
//! into fountain-coded parts. This writes the simple parts (`seq` 1..=n,
//! each one fragment), which every UR decoder accepts; the page loops them
//! until the wallet has seen every frame.

const BYTEWORDS: &str = "ableacidalsoapexaquaarchatomauntawayaxisbackbaldbarnbeltbetabiasbluebodybragbrewbulbbuzzcalmcashcatschefcityclawcodecolacookcostcruxcurlcuspcyandarkdatadaysdelidicedietdoordowndrawdropdrumdulldutyeacheasyechoedgeepicevenexamexiteyesfactfairfernfigsfilmfishfizzflapflewfluxfoxyfreefrogfuelfundgalagamegeargemsgiftgirlglowgoodgraygrimgurugushgyrohalfhanghardhawkheathelphighhillholyhopehornhutsicedideaidleinchinkyintoirisironitemjadejazzjoinjoltjowljudojugsjumpjunkjurykeepkenokeptkeyskickkilnkingkitekiwiknoblamblavalazyleaflegsliarlimplionlistlogoloudloveluaulucklungmainmanymathmazememomenumeowmildmintmissmonknailnavyneednewsnextnoonnotenumbobeyoboeomitonyxopenovalowlspaidpartpeckplaypluspoempoolposepuffpumapurrquadquizraceramprealredorichroadrockroofrubyruinrunsrustsafesagascarsetssilkskewslotsoapsolosongstubsurfswantacotasktaxitenttiedtimetinytoiltombtoystriptunatwinuglyundouniturgeuservastveryvetovialvibeviewvisavoidvowswallwandwarmwaspwavewaxywebswhatwhenwhizwolfworkyankyawnyellyogayurtzapszerozestzinczonezoom";

/// Bytewords "minimal" style: each byte as the first and last letter of
/// its word, followed by the CRC-32 of the bytes.
fn bytewords_minimal(data: &[u8]) -> String {
    let words = BYTEWORDS.as_bytes();
    let crc = crc32fast::hash(data).to_be_bytes();
    let mut out = String::with_capacity((data.len() + 4) * 2);
    for &b in data.iter().chain(crc.iter()) {
        let w = &words[b as usize * 4..b as usize * 4 + 4];
        out.push(w[0] as char);
        out.push(w[3] as char);
    }
    out
}

fn cbor_head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xff => out.extend([m | 24, n as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend((n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend((n as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend(n.to_be_bytes());
        }
    }
}

fn cbor_bytes(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 9);
    cbor_head(&mut out, 2, data.len() as u64);
    out.extend_from_slice(data);
    out
}

/// The fragment length the reference encoder picks: the fewest equal
/// fragments that are each at most `max_len` bytes.
fn nominal_fragment_len(message_len: usize, min_len: usize, max_len: usize) -> usize {
    let max_count = message_len.div_ceil(min_len).max(1);
    let mut len = message_len;
    for count in 1..=max_count {
        len = message_len.div_ceil(count);
        if len <= max_len {
            break;
        }
    }
    len.max(1)
}

/// The frames of an animated QR carrying `payload` as `ur:bytes`, each
/// fragment at most `max_fragment` bytes. One frame means no animation.
pub fn ur_bytes_parts(payload: &[u8], max_fragment: usize) -> Vec<String> {
    let message = cbor_bytes(payload);
    let checksum = crc32fast::hash(&message);
    let frag_len = nominal_fragment_len(message.len(), 10, max_fragment);
    let seq_len = message.len().div_ceil(frag_len);
    if seq_len <= 1 {
        return vec![format!("ur:bytes/{}", bytewords_minimal(&message))];
    }
    (0..seq_len)
        .map(|i| {
            let mut fragment =
                message[i * frag_len..message.len().min((i + 1) * frag_len)].to_vec();
            fragment.resize(frag_len, 0);
            let mut part = vec![0x85];
            cbor_head(&mut part, 0, i as u64 + 1);
            cbor_head(&mut part, 0, seq_len as u64);
            cbor_head(&mut part, 0, message.len() as u64);
            cbor_head(&mut part, 0, checksum as u64);
            part.extend(cbor_bytes(&fragment));
            format!(
                "ur:bytes/{}-{}/{}",
                i + 1,
                seq_len,
                bytewords_minimal(&part)
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytewords_match_the_spec_vector() {
        // BCR-2020-012 test vector: [0, 1, 2, 128, 255] → minimal.
        assert_eq!(
            bytewords_minimal(&[0, 1, 2, 128, 255]),
            "aeadaolazmjendeoti"
        );
    }

    #[test]
    fn short_payloads_are_one_part() {
        let parts = ur_bytes_parts(b"cashuBshort", 200);
        assert_eq!(parts.len(), 1);
        assert!(parts[0].starts_with("ur:bytes/"));
        assert!(!parts[0][9..].contains('/'));
    }

    #[test]
    fn long_payloads_split_into_equal_numbered_parts() {
        let payload = vec![b'x'; 1000];
        let parts = ur_bytes_parts(&payload, 200);
        assert_eq!(parts.len(), 6); // 1003-byte message, six 168-byte fragments
        for (i, p) in parts.iter().enumerate() {
            assert!(p.starts_with(&format!("ur:bytes/{}-6/", i + 1)), "{p}");
        }
        let lens: Vec<usize> = parts
            .iter()
            .map(|p| p.rsplit('/').next().unwrap().len())
            .collect();
        assert!(lens.windows(2).all(|w| w[0] == w[1]));
    }
}
