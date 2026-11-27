//! Hourly supply history for the mint's home page chart.
//!
//! Once an hour, right after the live stock tops up, the mint records how
//! its supply splits:
//!
//! ```text
//! redeeming + held + available = supply
//! ```
//!
//! - **redeeming**: TiH still to deliver on open redemptions;
//! - **held**: vouchers in holders' hands plus paid, unminted quotes;
//! - **available**: TiH still for sale (`capacity − owed`, 0 when oversold);
//! - **supply**: live hashrate average × the hours the stock covers, plus
//!   what running rentals can still deliver in that window.
//!
//! At the restock these add up exactly (the stock is sized to supply minus
//! owed); when hashrate falls the stack can sit above the supply line,
//! because a restock never takes back stock already listed.
//!
//! For now the rows are shown only on the operator dashboard
//! (`GET /api/supply`, unfiltered, plus a live row): Asher is trying the
//! charts there before any public page gets them (2026-11-27). [`router`]
//! is the public view for later and is not mounted yet.
//!
//! Privacy for that public view (Asher, 2026-11-27): only hourly rows are published, never a
//! live series, and while fewer than [`MIN_OPEN_REDEMPTIONS`] redemptions
//! are open the redeeming part is folded into "held". A lone redemption's
//! hourly drawdown would otherwise show its hashrate, which an observer
//! could match against a pool's public per-account hashrate.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use hashu_core::denom::TIH_PER_THH;
use hashu_mint::capacity::CapacitySnapshot;

const MS_PER_HOUR: u64 = 3_600_000;

/// Rows kept and served: two weeks of hours.
pub const KEEP_ROWS: usize = 14 * 24;

/// The redeeming band is shown only while at least this many redemptions
/// are open; below it, it is counted as held.
pub const MIN_OPEN_REDEMPTIONS: u64 = 3;

/// One hourly sample (private on disk; see [`Row::public`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub at_ms: u64,
    /// Hours the supply covers (the stock's window).
    pub window_hours: f64,
    pub supply_tih: f64,
    pub redeeming_tih: f64,
    pub held_tih: f64,
    pub available_tih: f64,
    pub open_redemptions: u64,
    /// Running totals for the cumulative chart: TiH issued over the
    /// payment rails (test mints left out) and TiH delivered to
    /// redeemers' pools, all time.
    #[serde(default)]
    pub sold_total_tih: f64,
    #[serde(default)]
    pub delivered_total_tih: f64,
}

/// What [`SupplyHistory::observe`] needs from the mint's obligations.
#[derive(Debug, Clone, Copy, Default)]
pub struct Counts {
    pub redeeming_tih: f64,
    pub open_redemptions: u64,
    pub sold_total_tih: f64,
    pub delivered_total_tih: f64,
}

impl Row {
    /// The row as published: redeeming folded into held below the
    /// threshold, and no redemption count.
    pub fn public(&self) -> serde_json::Value {
        let split = self.open_redemptions >= MIN_OPEN_REDEMPTIONS;
        let (redeeming, held) = if split {
            (self.redeeming_tih, self.held_tih)
        } else {
            (0.0, self.held_tih + self.redeeming_tih)
        };
        json!({
            "at_ms": self.at_ms,
            "window_hours": round2(self.window_hours),
            "supply_tih": self.supply_tih.round() as u64,
            "redeeming_tih": split.then(|| redeeming.round() as u64),
            "held_tih": held.round() as u64,
            "available_tih": self.available_tih.round() as u64,
            "sold_total_tih": self.sold_total_tih.round() as u64,
        })
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// Split a capacity snapshot and the mint's obligations into a row.
/// `None` when the mint has no window to sell over (nothing live, no
/// running rental).
pub fn row_from(snap: &CapacitySnapshot, c: Counts, now_ms: u64) -> Option<Row> {
    let rentals_tih: f64 = snap.rentals.iter().map(|r| r.tih_left).sum();
    let (window_hours, live_tih) = match (snap.live.on, snap.live.batch_hours) {
        (true, Some(h)) if h > 0.0 => (h, snap.live.stock.ewma_ths * h * TIH_PER_THH),
        _ => {
            let h = snap
                .rentals
                .iter()
                .filter(|r| r.rental.end_ms > now_ms)
                .map(|r| r.hours_left)
                .fold(0.0, f64::max);
            (h, 0.0)
        }
    };
    if window_hours <= 0.0 {
        return None;
    }
    let redeeming = c.redeeming_tih.clamp(0.0, snap.owed_tih.max(0.0));
    Some(Row {
        at_ms: now_ms,
        window_hours,
        supply_tih: live_tih + rentals_tih,
        redeeming_tih: redeeming,
        held_tih: (snap.owed_tih - redeeming).max(0.0),
        available_tih: snap.headroom_tih.max(0.0),
        open_redemptions: c.open_redemptions,
        sold_total_tih: c.sold_total_tih,
        delivered_total_tih: c.delivered_total_tih,
    })
}

#[derive(Debug, Default)]
struct Inner {
    rows: VecDeque<Row>,
    /// The live stock's last restock seen.
    restocked_ms: Option<u64>,
}

/// Hourly rows, appended to `supply-history.jsonl` in the data dir.
#[derive(Debug, Default)]
pub struct SupplyHistory {
    inner: Mutex<Inner>,
    path: Option<PathBuf>,
}

impl SupplyHistory {
    pub fn in_memory() -> Self {
        Self::default()
    }

    /// Load the last [`KEEP_ROWS`] rows from `path` (missing file: empty).
    pub fn load(path: PathBuf) -> Self {
        let mut rows = VecDeque::new();
        if let Ok(text) = std::fs::read_to_string(&path) {
            for line in text.lines() {
                match serde_json::from_str::<Row>(line) {
                    Ok(r) => {
                        rows.push_back(r);
                        if rows.len() > KEEP_ROWS {
                            rows.pop_front();
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "skipping bad supply history line"),
                }
            }
        }
        Self {
            inner: Mutex::new(Inner {
                rows,
                restocked_ms: None,
            }),
            path: Some(path),
        }
    }

    /// Fold in the latest reading. A row is recorded right after the live
    /// stock restocks (so the bands add up to supply), or, with no live
    /// stock, once per clock hour; never twice in the same hour.
    /// Returns whether a row was recorded.
    pub fn observe(&self, snap: &CapacitySnapshot, c: Counts, now_ms: u64) -> bool {
        let mut g = self.inner.lock().unwrap();
        let restocked = snap
            .live
            .on
            .then_some(snap.live.stock.restocked_ms)
            .flatten();
        let seen = std::mem::replace(&mut g.restocked_ms, restocked);
        let hour = now_ms / MS_PER_HOUR;
        let have_hour = g.rows.back().is_some_and(|r| r.at_ms / MS_PER_HOUR == hour);
        if have_hour {
            return false;
        }
        let due = match restocked {
            // After a restart the first reading only primes `seen`,
            // unless the restock it shows happened in this hour.
            Some(at) => {
                seen.is_some_and(|s| s != at) || (seen.is_none() && at / MS_PER_HOUR == hour)
            }
            None => true,
        };
        if !due {
            return false;
        }
        let Some(row) = row_from(snap, c, now_ms) else {
            return false;
        };
        if let Some(path) = &self.path {
            let line = serde_json::to_string(&row).expect("row serializes");
            let res = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .and_then(|mut f| writeln!(f, "{line}"));
            if let Err(e) = res {
                tracing::warn!(error = %e, path = %path.display(), "could not save supply history");
            }
        }
        g.rows.push_back(row);
        while g.rows.len() > KEEP_ROWS {
            g.rows.pop_front();
        }
        true
    }

    pub fn rows(&self) -> Vec<Row> {
        self.inner.lock().unwrap().rows.iter().cloned().collect()
    }
}

/// `GET /v1/hashu/supply`: the hourly rows, oldest first.
pub fn router(h: Arc<SupplyHistory>) -> Router {
    Router::new()
        .route("/v1/hashu/supply", get(index))
        .with_state(h)
}

async fn index(State(h): State<Arc<SupplyHistory>>) -> Response {
    Json(json!({
        "tih_per_thh": TIH_PER_THH,
        "every_hours": 1,
        "min_open_redemptions": MIN_OPEN_REDEMPTIONS,
        "rows": public_rows(&h.rows()),
    }))
    .into_response()
}

/// Rows as published. `delivered_total_tih` moves only at rows with at
/// least [`MIN_OPEN_REDEMPTIONS`] open, or at the first row of each UTC
/// day; otherwise it repeats the last value shown. A lone redemption's
/// delivery then shows up at most once a day, as a day's total, not as an
/// hourly rate an observer could match to a pool account.
pub fn public_rows(rows: &[Row]) -> Vec<serde_json::Value> {
    const DAY_MS: u64 = 24 * MS_PER_HOUR;
    let mut shown: Option<f64> = None;
    let mut day = None;
    rows.iter()
        .map(|r| {
            let d = r.at_ms / DAY_MS;
            if shown.is_none() || day != Some(d) || r.open_redemptions >= MIN_OPEN_REDEMPTIONS {
                shown = Some(r.delivered_total_tih);
            }
            day = Some(d);
            let mut v = r.public();
            v["delivered_total_tih"] = json!(shown.unwrap_or(0.0).round() as u64);
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hashu_mint::capacity::CapacityBook;

    fn snap(book: &CapacityBook, now: u64) -> CapacitySnapshot {
        book.snapshot(now)
    }

    #[test]
    fn public_row_folds_redeeming_below_threshold() {
        let r = Row {
            at_ms: 0,
            window_hours: 24.0,
            supply_tih: 1000.0,
            redeeming_tih: 100.0,
            held_tih: 200.0,
            available_tih: 700.0,
            open_redemptions: 2,
            sold_total_tih: 0.0,
            delivered_total_tih: 0.0,
        };
        let p = r.public();
        assert!(p["redeeming_tih"].is_null());
        assert_eq!(p["held_tih"], 300);
        assert!(p.get("open_redemptions").is_none());
        let p = Row {
            open_redemptions: 3,
            ..r
        }
        .public();
        assert_eq!(p["redeeming_tih"], 100);
        assert_eq!(p["held_tih"], 200);
    }

    #[test]
    fn no_window_records_nothing() {
        let book = CapacityBook::new();
        let h = SupplyHistory::in_memory();
        assert!(!h.observe(&snap(&book, 0), Counts::default(), 0));
        assert!(h.rows().is_empty());
    }

    #[test]
    fn rentals_record_once_per_hour() {
        let book = CapacityBook::new();
        let hour = MS_PER_HOUR;
        let rental = hashu_mint::capacity::rental_from_json(
            &json!({ "label": "r", "ths": 10.0, "end_ms": 48 * hour }),
            0,
        )
        .unwrap();
        book.set_manual_at(vec![rental], 0);
        book.set_owed(1000.0, 0);
        let h = SupplyHistory::in_memory();
        let c = Counts {
            redeeming_tih: 400.0,
            open_redemptions: 1,
            ..Default::default()
        };
        assert!(h.observe(&snap(&book, 10), c, 10));
        assert!(!h.observe(&snap(&book, 20), c, 20));
        assert!(h.observe(&snap(&book, hour + 5), c, hour + 5));
        let rows = h.rows();
        assert_eq!(rows.len(), 2);
        let r = &rows[0];
        assert!((r.window_hours - 48.0).abs() < 0.01);
        assert!((r.redeeming_tih - 400.0).abs() < 1e-9);
        assert!((r.held_tih - 600.0).abs() < 1e-9);
        // Rentals only: owed + available = what the rental can deliver.
        let sum = r.redeeming_tih + r.held_tih + r.available_tih;
        assert!(
            (sum - r.supply_tih).abs() < 1.0,
            "{sum} vs {}",
            r.supply_tih
        );
    }

    #[test]
    fn delivered_moves_with_three_open_or_once_a_day() {
        let row = |h: u64, open: u64, delivered: f64| Row {
            at_ms: h * MS_PER_HOUR,
            window_hours: 24.0,
            supply_tih: 0.0,
            redeeming_tih: 0.0,
            held_tih: 0.0,
            available_tih: 0.0,
            open_redemptions: open,
            sold_total_tih: 0.0,
            delivered_total_tih: delivered,
        };
        let rows = [
            row(10, 0, 5.0),  // first row: shown
            row(11, 1, 8.0),  // one open: held at 5
            row(12, 3, 20.0), // three open: shown
            row(13, 1, 25.0), // held at 20
            row(24, 0, 30.0), // new UTC day: shown
        ];
        let got: Vec<u64> = public_rows(&rows)
            .iter()
            .map(|v| v["delivered_total_tih"].as_u64().unwrap())
            .collect();
        assert_eq!(got, vec![5, 5, 20, 20, 30]);
    }
}
