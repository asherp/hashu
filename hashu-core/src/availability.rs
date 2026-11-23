//! Whether a mint's rigs are up, for pausing redemptions while they are
//! down.
//!
//! A mint is down when every rig that fills its orders has been
//! disconnected for at least [`DOWN_AFTER`]. Hashrate plays no part: a
//! slow rig is still delivering (the redeem ETA says how slowly), and a
//! rate threshold over a few noisy shares would trip on bad luck. Daemon
//! restarts behind the Stratum relay are not downtime: the relay keeps the
//! rigs' sessions open.
//!
//! While a mint is down it opens no new redemptions, and a redeemer may
//! stop one in flight before the usual minimum is delivered.
//!
//! [`RigsDown`] is filled in once the proxy router exists (it is built
//! after the mints that consult it); until then nothing is down.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// How long all of a mint's rigs must be gone before the mint counts as
/// down.
pub const DOWN_AFTER: Duration = Duration::from_secs(120);

/// `Some(unix ms)` since when the mint's rigs have been down, for the
/// operator's mint (`None`) or supplier `Some(name)`'s.
pub type DownSinceFn = Arc<dyn Fn(Option<&str>) -> Option<u64> + Send + Sync>;

/// Late-bound [`DownSinceFn`]. See the module docs.
#[derive(Default)]
pub struct RigsDown(OnceLock<DownSinceFn>);

impl RigsDown {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Set the source. Later calls are ignored.
    pub fn set(&self, f: DownSinceFn) {
        let _ = self.0.set(f);
    }

    /// Since when (Unix ms) the rigs of `mint` (`None` = the operator's
    /// mint) have been down, or `None` while any is connected.
    pub fn down_since(&self, mint: Option<&str>) -> Option<u64> {
        self.0.get().and_then(|f| f(mint))
    }

    /// The message a refused redemption carries.
    pub fn message(since_ms: u64) -> String {
        let secs = since_ms / 1000;
        let (h, m) = ((secs / 3600) % 24, (secs / 60) % 60);
        format!(
            "this mint's rigs have been offline since {h:02}:{m:02} UTC, so it is not taking redemptions; try again once they reconnect"
        )
    }
}

impl std::fmt::Debug for RigsDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RigsDown")
            .field("set", &self.0.get().is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_down_until_set() {
        let d = RigsDown::new();
        assert_eq!(d.down_since(None), None);
        d.set(Arc::new(|m: Option<&str>| {
            (m == Some("alice")).then_some(7_200_000)
        }));
        assert_eq!(d.down_since(None), None);
        assert_eq!(d.down_since(Some("alice")), Some(7_200_000));
        assert!(RigsDown::message(7_200_000).contains("02:00 UTC"));
    }
}
