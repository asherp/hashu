//! Voucher denomination unit and conversions. ARCHITECTURE.md §4.1, #90.
//!
//! Vouchers count **hashes**, in units of `TiH` (tebihash, 2^40 hashes).
//! Hashrate is backed out as TiH/s (a Bitaxe Gamma is about 1.1 TiH/s).
//!
//! Binary fits pool accounting: a difficulty-1 share is 2^32 hashes, so
//! 1 TiH is exactly 256 difficulty-1 shares and a share at difficulty `D`
//! is `D / 256` TiH. Cashu's power-of-two denominations are then powers of
//! two of hashes too.
//!
//! Prices are quoted in **msat per PiH** (2^50 hashes = 1,024 TiH), since a
//! single TiH costs well under a millisat.
//!
//! `THH` (terahash-hour, 1 TH/s × 3,600 s = 3.6 × 10^15 hashes), the unit
//! before #90, is kept here only for display and for converting hashrate ×
//! time: 1 THH ≈ 3,274.2 TiH.

/// Cashu currency unit string for tebihash vouchers. cdk lowercases custom
/// units on the wire, so wallets see `tih`.
pub const UNIT_TIH: &str = "TiH";

/// Hashes in one `TiH`: 2^40.
pub const HASHES_PER_TIH: u128 = 1 << 40;

/// `TiH` in one `PiH` (2^50 hashes), the unit prices are quoted per.
pub const TIH_PER_PIH: u64 = 1024;

/// Hashes in one terahash-hour (1 TH/s for 3,600 s), for display only.
pub const HASHES_PER_THH: f64 = 3.6e15;

/// `TiH` in one terahash-hour: 3.6e15 / 2^40 ≈ 3,274.18.
pub const TIH_PER_THH: f64 = HASHES_PER_THH / HASHES_PER_TIH as f64;

/// `TiH` per second at 1 TH/s: 10^12 / 2^40 ≈ 0.9095.
pub const TIH_PER_TH_SECOND: f64 = 1e12 / HASHES_PER_TIH as f64;

/// `TiH` produced by `ths` TH/s sustained for `hours`.
pub fn tih_from_ths_hours(ths: f64, hours: f64) -> f64 {
    ths * hours * TIH_PER_THH
}

/// `TiH` as terahash-hours, for display.
pub fn tih_to_thh(tih: f64) -> f64 {
    tih / TIH_PER_THH
}

/// Millisats for `amount_tih` at `msat_per_pih`, rounded up.
/// `None` on overflow.
pub fn msat_for_tih(amount_tih: u64, msat_per_pih: u64) -> Option<u64> {
    let msat = (amount_tih as u128 * msat_per_pih as u128).div_ceil(TIH_PER_PIH as u128);
    u64::try_from(msat).ok()
}

/// Millisats per PiH from a hashprice in sats per TH·s.
pub fn msat_per_pih_from_sats_per_ths(sats_per_ths: f64) -> f64 {
    // 1 PiH = 2^50 hashes = 2^50 / 10^12 TH·s.
    sats_per_ths * 1000.0 * (HASHES_PER_TIH as f64 * TIH_PER_PIH as f64) / 1e12
}

/// Sats per TH/s per day equivalent to `msat_per_pih`: what a supplier
/// ask means in the units hashrate markets quote.
pub fn sats_per_th_day(msat_per_pih: f64) -> f64 {
    // One TH/s for a day = 86,400 × 10^12 hashes = 86,400e12 / 2^50 PiH.
    msat_per_pih / 1000.0 * 86_400e12 / (HASHES_PER_TIH as f64 * TIH_PER_PIH as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_tih_is_256_difficulty_one_shares() {
        assert_eq!(HASHES_PER_TIH, 256 * (1u128 << 32));
    }

    #[test]
    fn one_thh_is_about_3274_tih() {
        assert!((TIH_PER_THH - 3274.1809).abs() < 1e-3);
        assert!((tih_from_ths_hours(1.0, 1.0) - TIH_PER_THH).abs() < 1e-9);
        assert!((tih_to_thh(TIH_PER_THH) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn msat_rounds_up_per_pih() {
        assert_eq!(msat_for_tih(1024, 590), Some(590));
        assert_eq!(msat_for_tih(1, 590), Some(1));
        assert_eq!(msat_for_tih(2048, 590), Some(1180));
        assert_eq!(msat_for_tih(0, 590), Some(0));
        assert_eq!(msat_for_tih(u64::MAX, u64::MAX), None);
    }

    #[test]
    fn hashprice_converts_to_msat_per_pih_and_back() {
        // 48 sats/TH/day ≈ 2 sats per THH ≈ 0.6255 sat per PiH.
        let sats_per_ths = 48.0 / 86_400.0;
        let m = msat_per_pih_from_sats_per_ths(sats_per_ths);
        assert!((m - 625.5).abs() < 0.01, "got {m}");
        assert!((sats_per_th_day(m) - 48.0).abs() < 1e-9);
    }
}
