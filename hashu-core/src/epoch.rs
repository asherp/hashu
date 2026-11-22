//! Difficulty epochs and what each one can still sell (#126).
//!
//! Epoch `N` is the 2,016 blocks at heights `N × 2016 .. (N + 1) × 2016`,
//! one Bitcoin difficulty period. A voucher dated to epoch `N` is work the
//! mint delivers while the network mines epoch `N`. Live issuance only
//! sells the current epoch; later epochs are sold separately, against
//! their own capacity.
//!
//! A dated voucher never lapses (paid value never expires, Cal. Civ. Code
//! 1749.5). Once its epoch ends it converts, TiH for TiH, to the current
//! epoch if that epoch still has room, and otherwise to the next epoch
//! that has. [`plan`] decides where that room is:
//!
//! ```text
//! capacity_E = hashrate × the hours of epoch E inside [now, now + horizon]
//! owed_C     = current epoch's vouchers + every expired or perpetual one
//! owed_E     = vouchers dated to E + whatever epoch E−1 could not hold
//! free_E     = max(0, capacity_E − owed_E)
//! ```
//!
//! Expired and perpetual vouchers are owed against the current epoch first
//! because they can be redeemed right now; what the current epoch cannot
//! hold rolls forward, epoch by epoch.

use serde::{Deserialize, Serialize};

use crate::denom::tih_from_ths_hours;

/// Blocks in one difficulty period.
pub const BLOCKS_PER_EPOCH: u64 = 2016;

/// Bitcoin's target block interval.
pub const TARGET_BLOCK_SECS: u64 = 600;

/// Hours one full epoch takes at the target block interval (336 h).
pub const EPOCH_HOURS: f64 = (BLOCKS_PER_EPOCH * TARGET_BLOCK_SECS) as f64 / 3600.0;

/// Epoch holding block `height`.
pub fn epoch_of(height: u64) -> u64 {
    height / BLOCKS_PER_EPOCH
}

/// First block height of epoch `epoch`.
pub fn start_height(epoch: u64) -> u64 {
    epoch * BLOCKS_PER_EPOCH
}

/// Where the chain stands, from the height of its tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpochClock {
    /// Height of the latest block.
    pub tip_height: u64,
}

impl EpochClock {
    pub fn new(tip_height: u64) -> Self {
        Self { tip_height }
    }

    /// The epoch being mined now: that of the next block, the height miners
    /// build templates for.
    pub fn current(&self) -> u64 {
        epoch_of(self.tip_height + 1)
    }

    /// Blocks of the current epoch still to be mined, the next one included.
    pub fn blocks_left(&self) -> u64 {
        start_height(self.current() + 1) - (self.tip_height + 1)
    }

    /// Expected hours until the current epoch ends.
    pub fn hours_left(&self) -> f64 {
        (self.blocks_left() * TARGET_BLOCK_SECS) as f64 / 3600.0
    }

    /// Expected hours from now until epoch `epoch` starts (0 for the
    /// current one or earlier).
    pub fn hours_until(&self, epoch: u64) -> f64 {
        if epoch <= self.current() {
            return 0.0;
        }
        self.hours_left() + (epoch - self.current() - 1) as f64 * EPOCH_HOURS
    }
}

/// A source of hashrate that stops at some point (a rental), or never.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Supply {
    pub ths: f64,
    /// Hours from now until it stops; `None`: it keeps going.
    pub hours_left: Option<f64>,
}

/// One epoch's line in a [`plan`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct PlanRow {
    pub epoch: u64,
    /// Hours of this epoch inside the sales window `[now, now + horizon]`.
    pub hours_in_window: f64,
    /// TiH the supply can deliver in that part of the epoch.
    pub capacity_tih: f64,
    /// TiH owed in this epoch, overflow from earlier epochs included.
    pub owed_tih: f64,
    /// TiH still free to sell or convert into this epoch.
    pub free_tih: f64,
}

/// Last epoch the sales window `[now, now + horizon_hours]` reaches.
pub fn last_epoch(clock: &EpochClock, horizon_hours: f64) -> u64 {
    let beyond = horizon_hours - clock.hours_left();
    if !(beyond > 0.0) {
        return clock.current();
    }
    clock.current() + (beyond / EPOCH_HOURS).ceil() as u64
}

/// What each epoch in the sales window can deliver and already owes.
///
/// The window is `[now, now + horizon_hours]`, the seller's horizon: an
/// epoch sells only the part of it inside the window, so later epochs come
/// into view as the window slides and grow hour by hour.
///
/// - `carried_tih`: TiH owed now that is not dated to a later epoch: the
///   current epoch's vouchers plus every expired or perpetual one.
/// - `dated_tih(e)`: TiH dated to future epoch `e`.
/// - `planned_ths(e)`: TH/s the seller plans for future epoch `e` (a
///   hashrate planner); `None` sizes it from `supply` like the current one.
pub fn plan(
    clock: &EpochClock,
    supply: &[Supply],
    carried_tih: f64,
    dated_tih: impl Fn(u64) -> f64,
    planned_ths: impl Fn(u64) -> Option<f64>,
    horizon_hours: f64,
) -> Vec<PlanRow> {
    let current = clock.current();
    let horizon = horizon_hours.max(0.0);
    let last = last_epoch(clock, horizon);
    let mut rows = Vec::with_capacity((last - current) as usize + 1);
    let mut overflow = carried_tih.max(0.0);
    for epoch in current..=last {
        let from = clock.hours_until(epoch);
        let to = if epoch == current {
            clock.hours_left()
        } else {
            from + EPOCH_HOURS
        }
        .min(horizon);
        let hours_in_window = (to - from).max(0.0);
        let capacity_tih: f64 = match planned_ths(epoch).filter(|_| epoch > current) {
            Some(ths) => tih_from_ths_hours(ths.max(0.0), hours_in_window),
            None => supply
                .iter()
                .map(|s| {
                    let end = s.hours_left.map_or(to, |h| h.min(to));
                    tih_from_ths_hours(s.ths.max(0.0), (end - from).max(0.0))
                })
                .sum(),
        };
        let dated = if epoch == current {
            0.0
        } else {
            dated_tih(epoch).max(0.0)
        };
        let owed_tih = overflow + dated;
        overflow = (owed_tih - capacity_tih).max(0.0);
        rows.push(PlanRow {
            epoch,
            hours_in_window,
            capacity_tih,
            owed_tih,
            free_tih: (capacity_tih - owed_tih).max(0.0),
        });
    }
    rows
}

/// Where an expired (or perpetual) voucher converts to: the current epoch
/// if it is not oversold, otherwise the first later epoch that is not.
/// The voucher is already counted in the plan's carried TiH, so an epoch
/// that is not oversold has room for it. `None`: no epoch in the plan can
/// deliver anything (no hashrate), so the voucher stays as it is.
pub fn next_available(rows: &[PlanRow]) -> Option<u64> {
    rows.iter()
        .find(|r| r.capacity_tih > 0.0 && r.owed_tih <= r.capacity_tih)
        .map(|r| r.epoch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::denom::TIH_PER_THH;

    #[test]
    fn epochs_follow_the_next_block() {
        // Tip 2015: the next block (2016) opens epoch 1.
        let c = EpochClock::new(2015);
        assert_eq!(c.current(), 1);
        assert_eq!(c.blocks_left(), 2016);
        assert_eq!(c.hours_left(), EPOCH_HOURS);
        // Tip 2014: one block of epoch 0 is left.
        let c = EpochClock::new(2014);
        assert_eq!(c.current(), 0);
        assert_eq!(c.blocks_left(), 1);
        assert!((c.hours_left() - 1.0 / 6.0).abs() < 1e-12);
        assert!((c.hours_until(1) - 1.0 / 6.0).abs() < 1e-12);
        assert!((c.hours_until(2) - (1.0 / 6.0 + EPOCH_HOURS)).abs() < 1e-9);
        assert_eq!(c.hours_until(0), 0.0);
    }

    fn rate(ths: f64) -> Vec<Supply> {
        vec![Supply {
            ths,
            hours_left: None,
        }]
    }

    #[test]
    fn plan_spreads_capacity_over_epochs() {
        // 1,008 blocks (168 h) left in epoch 10.
        let c = EpochClock::new(10 * 2016 + 1007);
        assert_eq!(c.blocks_left(), 1008);
        let rows = plan(
            &c,
            &rate(1.0),
            0.0,
            |_| 0.0,
            |_| None,
            168.0 + 2.0 * EPOCH_HOURS,
        );
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].epoch, 10);
        assert!((rows[0].capacity_tih - 168.0 * TIH_PER_THH).abs() < 1e-6);
        assert!((rows[1].capacity_tih - 336.0 * TIH_PER_THH).abs() < 1e-6);
        assert_eq!(rows[1].free_tih, rows[1].capacity_tih);
    }

    #[test]
    fn overflow_rolls_to_the_next_epoch_with_room() {
        let c = EpochClock::new(10 * 2016 + 1007);
        let cap0 = 168.0 * TIH_PER_THH;
        let cap1 = 336.0 * TIH_PER_THH;
        // Owed now: all of epoch 10 plus half of epoch 11; epoch 11 also
        // has its own dated vouchers filling the other half and a bit more.
        let rows = plan(
            &c,
            &rate(1.0),
            cap0 + cap1 / 2.0,
            |e| if e == 11 { cap1 / 2.0 + 10.0 } else { 0.0 },
            |_| None,
            168.0 + 3.0 * EPOCH_HOURS,
        );
        assert_eq!(rows[0].free_tih, 0.0);
        assert!((rows[1].owed_tih - (cap1 + 10.0)).abs() < 1e-6);
        assert_eq!(rows[1].free_tih, 0.0);
        assert!((rows[2].owed_tih - 10.0).abs() < 1e-6);
        assert_eq!(next_available(&rows), Some(12));
    }

    #[test]
    fn current_epoch_takes_expired_vouchers_when_it_has_room() {
        let c = EpochClock::new(10 * 2016 + 1007);
        let rows = plan(&c, &rate(1.0), 4096.0, |_| 0.0, |_| None, 1000.0);
        assert_eq!(next_available(&rows), Some(10));
    }

    #[test]
    fn no_hashrate_means_nowhere_to_convert() {
        let c = EpochClock::new(10 * 2016 + 1007);
        let rows = plan(&c, &rate(0.0), 4096.0, |_| 0.0, |_| None, 1000.0);
        assert_eq!(next_available(&rows), None);
    }

    #[test]
    fn rentals_count_only_until_they_end() {
        let c = EpochClock::new(10 * 2016 + 1007);
        let supply = [Supply {
            ths: 1.0,
            hours_left: Some(200.0),
        }];
        let rows = plan(
            &c,
            &supply,
            0.0,
            |_| 0.0,
            |_| None,
            168.0 + 2.0 * EPOCH_HOURS,
        );
        assert!((rows[0].capacity_tih - 168.0 * TIH_PER_THH).abs() < 1e-6);
        assert!((rows[1].capacity_tih - 32.0 * TIH_PER_THH).abs() < 1e-6);
        assert_eq!(rows[2].capacity_tih, 0.0);
    }

    #[test]
    fn the_horizon_brings_epochs_into_view() {
        // 168 h left in epoch 10; a two-week horizon reaches half of 11.
        let c = EpochClock::new(10 * 2016 + 1007);
        let rows = plan(&c, &rate(1.0), 0.0, |_| 0.0, |_| None, 336.0);
        assert_eq!(
            rows.iter().map(|r| r.epoch).collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert!((rows[1].hours_in_window - 168.0).abs() < 1e-9);
        assert!((rows[1].capacity_tih - 168.0 * TIH_PER_THH).abs() < 1e-6);
        // A shorter horizon sells only part of the current epoch.
        let rows = plan(&c, &rate(1.0), 0.0, |_| 0.0, |_| None, 24.0);
        assert_eq!(rows.len(), 1);
        assert!((rows[0].capacity_tih - 24.0 * TIH_PER_THH).abs() < 1e-6);
        // An hour later the window has slid an hour into epoch 11... (one
        // block = 10 min, so six blocks later)
        let c = EpochClock::new(10 * 2016 + 1013);
        let rows = plan(&c, &rate(1.0), 0.0, |_| 0.0, |_| None, 336.0);
        assert!((rows[1].hours_in_window - 169.0).abs() < 1e-9);
        assert_eq!(last_epoch(&c, 167.0), 10);
    }

    #[test]
    fn planned_hashrate_sizes_future_epochs() {
        let c = EpochClock::new(10 * 2016 + 1007);
        let rows = plan(
            &c,
            &rate(1.0),
            0.0,
            |_| 0.0,
            |e| (e == 11).then_some(3.0),
            168.0 + 2.0 * EPOCH_HOURS,
        );
        // The current epoch keeps the measured rate; 11 uses the plan.
        assert!((rows[0].capacity_tih - 168.0 * TIH_PER_THH).abs() < 1e-6);
        assert!((rows[1].capacity_tih - 3.0 * 336.0 * TIH_PER_THH).abs() < 1e-6);
        assert!((rows[2].capacity_tih - 336.0 * TIH_PER_THH).abs() < 1e-6);
    }
}
