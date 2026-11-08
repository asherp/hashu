//! Redemption orders made from the checkout page.
//!
//! The checkout redeems a pasted token from a throwaway cdk wallet, and that
//! wallet also makes the melt's blank outputs. When a redemption settles
//! early, the mint signs change on those blanks, and only that wallet's seed
//! can unblind it. So each checkout redemption is kept as an *order*: a
//! random order id (the private link the redeemer watches), the redemption
//! and melt quote it opened, and the wallet seed. When the redemption ends
//! with change, the checkout restores the wallet from the seed (NUT-09),
//! takes the unspent change proofs, and keeps them as a token on the order.
//!
//! The order id is a bearer secret, since the page behind it hands out the
//! change. It is never derived from the redemption id, which the public
//! status API lists. Holding the seed means the operator could take the
//! change: redeem from your own wallet if that matters.
//!
//! Orders live in one JSON file, rewritten atomically on each change and
//! readable by the owner only. A redemption opens a few orders a day, not
//! thousands a second.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use hashu_core::redemption::RedemptionId;
use serde::{Deserialize, Serialize};

/// One checkout redemption.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub order_id: String,
    pub redemption_id: RedemptionId,
    /// The cdk melt quote, used to make the mint finalize the melt.
    pub quote_id: String,
    /// Seed of the wallet that melted the token and made the blanks, hex.
    pub seed_hex: String,
    pub created_ms: u64,
    /// The change as a token, once claimed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_thh: Option<u64>,
}

impl Order {
    pub fn seed(&self) -> Option<[u8; 64]> {
        hex::decode(&self.seed_hex).ok()?.try_into().ok()
    }
}

/// A random order id in UUID v4 form.
pub fn new_order_id() -> String {
    let mut b: [u8; 16] = rand::random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// Whether `s` looks like an order id, before any lookup.
pub fn is_order_id(s: &str) -> bool {
    s.len() == 36
        && s.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit() && !c.is_ascii_uppercase(),
        })
}

#[derive(Debug, Default)]
pub struct OrderBook {
    path: Option<PathBuf>,
    orders: Mutex<HashMap<String, Order>>,
    /// Serializes change claims so one order is restored once at a time.
    pub claim: tokio::sync::Mutex<()>,
}

impl OrderBook {
    /// In memory only (tests, or a mint without a data dir).
    pub fn new() -> Self {
        Self::default()
    }

    /// Backed by the JSON file at `path`; missing file = no orders.
    pub fn persistent(path: &Path) -> std::io::Result<Self> {
        let orders = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Vec<Order>>(&text)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
                .into_iter()
                .map(|o| (o.order_id.clone(), o))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e),
        };
        Ok(Self {
            path: Some(path.to_path_buf()),
            orders: Mutex::new(orders),
            claim: Default::default(),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Order>> {
        self.orders.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, orders: &HashMap<String, Order>) {
        let Some(path) = &self.path else { return };
        let mut list: Vec<&Order> = orders.values().collect();
        list.sort_by(|a, b| (a.created_ms, &a.order_id).cmp(&(b.created_ms, &b.order_id)));
        if let Err(e) = write_private(path, &serde_json::to_vec_pretty(&list).unwrap_or_default()) {
            tracing::error!(error = %e, ?path, "saving redemption orders failed");
        }
    }

    pub fn insert(&self, order: Order) {
        let mut orders = self.lock();
        orders.insert(order.order_id.clone(), order);
        self.save(&orders);
    }

    pub fn get(&self, order_id: &str) -> Option<Order> {
        self.lock().get(order_id).cloned()
    }

    /// Record the claimed change on an order.
    pub fn set_change(&self, order_id: &str, change_thh: u64, token: Option<String>) {
        let mut orders = self.lock();
        if let Some(o) = orders.get_mut(order_id) {
            o.change_thh = Some(change_thh);
            o.change_token = token;
            self.save(&orders);
        }
    }
}

/// Write `bytes` to `path` via a temp file and rename, mode 0600 on Unix:
/// the file holds wallet seeds.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(id: &str) -> Order {
        Order {
            order_id: id.into(),
            redemption_id: RedemptionId::from_bytes([1; 16]),
            quote_id: "q".into(),
            seed_hex: hex::encode([9u8; 64]),
            created_ms: 5,
            change_token: None,
            change_thh: None,
        }
    }

    #[test]
    fn order_ids_are_random_v4_uuids() {
        let a = new_order_id();
        assert!(is_order_id(&a), "{a}");
        assert_eq!(&a[14..15], "4");
        assert_ne!(a, new_order_id());
        assert!(!is_order_id("not-an-id"));
        assert!(!is_order_id(&a.to_uppercase()));
        assert!(!is_order_id(&"0".repeat(36)));
    }

    #[test]
    fn orders_survive_reopen_and_keep_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orders.json");
        let id = new_order_id();
        {
            let book = OrderBook::persistent(&path).unwrap();
            book.insert(order(&id));
            book.set_change(&id, 6, Some("cashuBxyz".into()));
        }
        let book = OrderBook::persistent(&path).unwrap();
        let o = book.get(&id).unwrap();
        assert_eq!(o.change_thh, Some(6));
        assert_eq!(o.change_token.as_deref(), Some("cashuBxyz"));
        assert_eq!(o.seed(), Some([9u8; 64]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
