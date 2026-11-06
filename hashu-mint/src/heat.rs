//! Heat the rented hashrate turns into.
//!
//! An ASIC turns nearly all of its electricity into heat, so the heat behind
//! a quantity of work is that work times the miner's efficiency:
//!
//! ```text
//! heat (J)   = hashes / 10^12 × J/TH
//! heat (kWh) = THH × 3600 × J/TH / 3.6e6 = THH × J/TH / 1000
//! ```
//!
//! since 1 THH is 3,600 TH. The proxy knows the work (accepted share
//! difficulty) but not the machines, so J/TH is an estimate: matched from
//! the rental's rig name (MRR rig names and operator labels usually name the
//! model, e.g. "Antminer S19j Pro") or, failing that, a configurable default.

use serde::Serialize;

use crate::capacity::RentalView;

/// J/TH used when nothing names the rig: an S19-class machine.
pub const DEFAULT_J_PER_TH: f64 = 30.0;
/// BTU in one kWh.
pub const BTU_PER_KWH: f64 = 3_412.141_63;

/// Stock efficiencies of common SHA-256 ASICs, J/TH (manufacturer specs,
/// rounded). Matched against a normalised rig name; the longest match wins,
/// so "S19j Pro+" is not read as "S19".
const MODELS: &[(&str, &str, f64)] = &[
    // Bitmain Antminer
    ("s21xphyd", "Antminer S21 XP Hyd", 12.0),
    ("s21xp", "Antminer S21 XP", 13.5),
    ("s21prohyd", "Antminer S21 Pro Hyd", 14.5),
    ("s21pro", "Antminer S21 Pro", 15.0),
    ("s21+hyd", "Antminer S21+ Hyd", 15.0),
    ("s21+", "Antminer S21+", 16.5),
    ("s21hyd", "Antminer S21 Hyd", 16.0),
    ("s21", "Antminer S21", 17.5),
    ("t21", "Antminer T21", 19.0),
    ("s19xphyd", "Antminer S19 XP Hyd", 20.8),
    ("s19xp", "Antminer S19 XP", 21.5),
    ("s19kpro", "Antminer S19k Pro", 23.0),
    ("s19jxp", "Antminer S19j XP", 21.5),
    ("s19pro+hyd", "Antminer S19 Pro+ Hyd", 27.5),
    ("s19jpro+", "Antminer S19j Pro+", 27.5),
    ("s19jpro", "Antminer S19j Pro", 29.5),
    ("s19pro", "Antminer S19 Pro", 29.5),
    ("s19hyd", "Antminer S19 Hyd", 29.5),
    ("s19j", "Antminer S19j", 34.5),
    ("s19", "Antminer S19", 34.5),
    ("t19", "Antminer T19", 37.5),
    ("s17pro", "Antminer S17 Pro", 40.0),
    ("s17", "Antminer S17", 45.0),
    ("t17", "Antminer T17", 55.0),
    ("s9", "Antminer S9", 98.0),
    // MicroBT Whatsminer
    ("m66s", "Whatsminer M66S", 18.5),
    ("m60s", "Whatsminer M60S", 18.5),
    ("m60", "Whatsminer M60", 19.9),
    ("m56s", "Whatsminer M56S", 22.0),
    ("m53s", "Whatsminer M53S", 26.0),
    ("m50s", "Whatsminer M50S", 24.0),
    ("m50", "Whatsminer M50", 26.0),
    ("m30s++", "Whatsminer M30S++", 31.0),
    ("m30s+", "Whatsminer M30S+", 34.0),
    ("m30s", "Whatsminer M30S", 38.0),
    ("m20s", "Whatsminer M20S", 48.0),
    // Canaan Avalon
    ("a1466", "Avalon A1466", 21.5),
    ("a1366", "Avalon A1366", 25.0),
    ("a1346", "Avalon A1346", 30.0),
    ("a1246", "Avalon A1246", 38.0),
];

/// A known model and its J/TH.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Model {
    pub name: &'static str,
    pub j_per_th: f64,
}

/// The model a rig name refers to, if any.
pub fn model_for(name: &str) -> Option<Model> {
    // Lowercase with separators dropped, so "S19j Pro" matches "s19jpro";
    // `gap[i]` records that a separator came before byte `i`.
    let mut n = String::new();
    let mut gap = Vec::new();
    let mut sep = false;
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_whitespace() || matches!(c, '-' | '_' | '/' | ',' | '(' | ')') {
            sep = true;
            continue;
        }
        for _ in 0..c.len_utf8() {
            gap.push(sep);
        }
        sep = false;
        n.push(c);
    }
    gap.push(true);
    MODELS
        .iter()
        .filter(|(pat, ..)| contains_model(&n, &gap, pat))
        .max_by_key(|(pat, ..)| pat.len())
        .map(|&(_, name, j_per_th)| Model { name, j_per_th })
}

/// `pat` occurs in `s` without running into adjacent digits, so "s9" does
/// not match "s90" or "s19", while "S19 120T" still matches "s19".
fn contains_model(s: &str, gap: &[bool], pat: &str) -> bool {
    let digit_at = |i: usize| s[i..].chars().next().is_some_and(|c| c.is_ascii_digit());
    s.match_indices(pat).any(|(i, _)| {
        let end = i + pat.len();
        let before_ok = i == 0
            || gap[i]
            || !s[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_digit());
        let after_ok = !pat.ends_with(|c: char| c.is_ascii_digit()) || gap[end] || !digit_at(end);
        before_ok && after_ok
    })
}

/// kWh of heat behind `thh` at `j_per_th`.
pub fn kwh(thh: f64, j_per_th: f64) -> f64 {
    thh.max(0.0) * j_per_th / 1000.0
}

/// kW of heat from `ths` of hashrate at `j_per_th`.
pub fn kw(ths: f64, j_per_th: f64) -> f64 {
    ths.max(0.0) * j_per_th / 1000.0
}

/// How the efficiency was chosen, per rental.
#[derive(Debug, Clone, Serialize)]
pub struct RigEfficiency {
    pub id: String,
    pub label: String,
    pub ths: f64,
    /// Model matched from the label, if any.
    pub model: Option<&'static str>,
    pub j_per_th: f64,
}

/// The fleet's efficiency: hashrate-weighted over running rentals.
#[derive(Debug, Clone, Serialize)]
pub struct Efficiency {
    pub j_per_th: f64,
    /// `"rigs"`: every running rental named a known model; `"mixed"`: some
    /// did; `"default"`: none did (or no rentals).
    pub source: &'static str,
    pub default_j_per_th: f64,
    pub rigs: Vec<RigEfficiency>,
}

/// Efficiency of the running rentals, falling back to `default_j_per_th`
/// for rigs whose name does not name a model.
pub fn fleet_efficiency(rentals: &[RentalView], default_j_per_th: f64, now_ms: u64) -> Efficiency {
    let rigs: Vec<RigEfficiency> = rentals
        .iter()
        .filter(|r| r.rental.end_ms > now_ms && r.rental.ths > 0.0)
        .map(|r| {
            let m = model_for(&r.rental.label);
            RigEfficiency {
                id: r.rental.id.clone(),
                label: r.rental.label.clone(),
                ths: r.rental.ths,
                model: m.map(|m| m.name),
                j_per_th: m.map_or(default_j_per_th, |m| m.j_per_th),
            }
        })
        .collect();
    let ths: f64 = rigs.iter().map(|r| r.ths).sum();
    let matched = rigs.iter().filter(|r| r.model.is_some()).count();
    let j_per_th = if ths > 0.0 {
        rigs.iter().map(|r| r.ths * r.j_per_th).sum::<f64>() / ths
    } else {
        default_j_per_th
    };
    let source = match matched {
        0 => "default",
        n if n == rigs.len() => "rigs",
        _ => "mixed",
    };
    Efficiency {
        j_per_th,
        source,
        default_j_per_th,
        rigs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capacity::Rental;

    fn view(label: &str, ths: f64) -> RentalView {
        RentalView {
            source: "mrr",
            rental: Rental {
                id: label.into(),
                label: label.into(),
                ths,
                end_ms: 10_000,
            },
            hours_left: 1.0,
            thh_left: ths,
        }
    }

    #[test]
    fn matches_the_most_specific_model() {
        assert_eq!(model_for("Antminer S19j Pro+ 120T").unwrap().j_per_th, 27.5);
        assert_eq!(model_for("S19 Pro").unwrap().name, "Antminer S19 Pro");
        assert_eq!(model_for("  S19 ").unwrap().name, "Antminer S19");
        assert_eq!(model_for("bitmain-s21-xp #4").unwrap().j_per_th, 13.5);
        assert_eq!(model_for("WhatsMiner M30S++").unwrap().j_per_th, 31.0);
        assert_eq!(model_for("Antminer S9").unwrap().j_per_th, 98.0);
        assert_eq!(model_for("S19 110T").unwrap().name, "Antminer S19");
        assert!(model_for("S90 rig").is_none());
        assert!(model_for("my rig").is_none());
        assert!(model_for("").is_none());
    }

    #[test]
    fn heat_units() {
        // 1 THH at 30 J/TH: 3600 TH × 30 J = 108 kJ = 0.03 kWh.
        assert!((kwh(1.0, 30.0) - 0.03).abs() < 1e-12);
        // 413 TH/s of S21s: about 7.2 kW.
        assert!((kw(413.0, 17.5) - 7.2275).abs() < 1e-9);
    }

    #[test]
    fn fleet_is_hashrate_weighted() {
        let e = fleet_efficiency(&[view("S21", 100.0), view("rig", 100.0)], 30.0, 0);
        assert_eq!(e.source, "mixed");
        assert!((e.j_per_th - 23.75).abs() < 1e-9);
        let e = fleet_efficiency(&[view("S21 Pro", 50.0)], 30.0, 0);
        assert_eq!(e.source, "rigs");
        assert_eq!(e.j_per_th, 15.0);
        // Ended rentals and no rentals fall back to the default.
        let e = fleet_efficiency(&[view("S21", 100.0)], 30.0, 20_000);
        assert_eq!((e.source, e.j_per_th), ("default", 30.0));
    }
}
