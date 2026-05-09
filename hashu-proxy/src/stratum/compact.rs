//! Bitcoin-style compact difficulty encoding (`nBits`).
//!
//! Stratum V1 sets share difficulty as a positive number `D` over
//! `mining.set_difficulty`. The corresponding 256-bit share target under the
//! bdiff convention is
//!
//! ```text
//! target = bdiff_one / D = (0xffff << 208) / D
//! ```
//!
//! and we encode it back to a compact `u32` for the share-leaf record. The
//! compact form is `(size << 24) | mantissa` where the canonical mantissa
//! lives in `[0x008000, 0x7fffff]` (top bit clear → positive sign).
//!
//! We use `f64` arithmetic for the divide; the compact format only carries
//! a 23-bit mantissa, so the 52 bits of f64 precision are far in excess of
//! what the encoding can represent.

const BDIFF_ONE_MANTISSA: f64 = 65535.0;
const BDIFF_ONE_BITS: u32 = 0x1d00_ffff;
const MIN_CANONICAL_MANTISSA: f64 = 32768.0; // 0x008000
const MAX_CANONICAL_MANTISSA: f64 = 8_388_607.0; // 0x7fffff

/// Convert a Stratum V1 share difficulty to compact `nBits`.
///
/// Returns `BDIFF_ONE_BITS` for non-finite or non-positive inputs (Stratum
/// servers don't send these in practice; we still want a stable byte for
/// the share-leaf record rather than 0).
pub fn target_bits_from_difficulty(difficulty: f64) -> u32 {
    if !difficulty.is_finite() || difficulty <= 0.0 {
        return BDIFF_ONE_BITS;
    }

    let mut mantissa = BDIFF_ONE_MANTISSA / difficulty;
    let mut size: i32 = 29; // bdiff one's compact "size" byte

    while mantissa < MIN_CANONICAL_MANTISSA {
        mantissa *= 256.0;
        size -= 1;
        if size < 3 {
            // Underflow into sub-3-byte territory shouldn't happen for
            // positive D; fall back to the smallest representable non-zero
            // target rather than emitting 0.
            return 0x0300_8000;
        }
    }
    while mantissa > MAX_CANONICAL_MANTISSA {
        mantissa /= 256.0;
        size += 1;
        if size > 0xff {
            return 0xff7f_ffff;
        }
    }

    let mantissa = mantissa.round() as u32 & 0x007f_ffff;
    ((size as u32) << 24) | mantissa
}

/// Decode a compact `nBits` value back to a 256-bit big-endian target.
///
/// Used by the redeemer to test `sha256d(preimage) <= target`. We expose
/// this for tests; the proxy itself never compares targets.
pub fn target_from_compact(bits: u32) -> [u8; 32] {
    let size = (bits >> 24) as usize;
    let mantissa = bits & 0x007f_ffff;
    let negative = (bits & 0x0080_0000) != 0;
    let mut out = [0u8; 32];
    if mantissa == 0 || negative {
        return out;
    }
    if size <= 3 {
        let shift = 8 * (3 - size);
        let m = mantissa >> shift;
        out[29] = ((m >> 16) & 0xff) as u8;
        out[30] = ((m >> 8) & 0xff) as u8;
        out[31] = (m & 0xff) as u8;
    } else {
        let pos = 32 - size;
        if pos < 30 {
            out[pos] = ((mantissa >> 16) & 0xff) as u8;
            out[pos + 1] = ((mantissa >> 8) & 0xff) as u8;
            out[pos + 2] = (mantissa & 0xff) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn difficulty_one_is_bdiff_one() {
        assert_eq!(target_bits_from_difficulty(1.0), 0x1d00_ffff);
    }

    #[test]
    fn difficulty_2_pow_16_drops_two_size_bytes() {
        // bdiff_one / 2^16 = 65535 * 2^208 / 2^16 = 65535 * 2^192
        // → mantissa 0x00ffff at size 27.
        assert_eq!(target_bits_from_difficulty(65536.0), 0x1b00_ffff);
    }

    #[test]
    fn difficulty_2_pow_32_drops_four_size_bytes() {
        // 65535 * 2^176 → mantissa 0x00ffff at size 25.
        assert_eq!(target_bits_from_difficulty(4_294_967_296.0), 0x1900_ffff);
    }

    #[test]
    fn non_positive_or_nan_falls_back_to_bdiff_one() {
        assert_eq!(target_bits_from_difficulty(0.0), BDIFF_ONE_BITS);
        assert_eq!(target_bits_from_difficulty(-1.0), BDIFF_ONE_BITS);
        assert_eq!(target_bits_from_difficulty(f64::NAN), BDIFF_ONE_BITS);
        assert_eq!(target_bits_from_difficulty(f64::INFINITY), BDIFF_ONE_BITS);
    }

    #[test]
    fn target_from_compact_bdiff_one_matches_spec() {
        // bdiff one = 0x00000000FFFF0000_..._00000000 (BE)
        let t = target_from_compact(0x1d00_ffff);
        let mut expected = [0u8; 32];
        expected[4] = 0xff;
        expected[5] = 0xff;
        assert_eq!(t, expected);
    }

    #[test]
    fn target_decoded_decreases_monotonically_with_difficulty() {
        let t1 = target_from_compact(target_bits_from_difficulty(1.0));
        let t256 = target_from_compact(target_bits_from_difficulty(256.0));
        let t65k = target_from_compact(target_bits_from_difficulty(65536.0));
        assert!(t1 > t256);
        assert!(t256 > t65k);
    }

    #[test]
    fn negative_compact_decodes_to_zero() {
        // Top bit of mantissa set = "negative" flag in Bitcoin compact.
        // Such values aren't legal targets and should decode to 0.
        let t = target_from_compact(0x1d80_8000);
        assert_eq!(t, [0u8; 32]);
    }
}
