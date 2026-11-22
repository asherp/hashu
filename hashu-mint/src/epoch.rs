//! One TiH keyset per difficulty epoch (#126).
//!
//! With epochs on, the mint's active TiH keyset is the current epoch's:
//! every rail (Lightning, USDT, USDC) issues vouchers dated to the epoch
//! the network is mining. When the chain enters a new epoch the mint
//! rotates to a new keyset, and the old one stays valid for swaps and
//! redemptions: an expired voucher never lapses.
//!
//! - **Keys.** cdk derives keyset `i` of a unit at `m/129372'/<unit>'/i'`
//!   and each rotation takes the next index. Epoch `N`'s keyset is the one
//!   at `anchor.index + (N − anchor.epoch)`, so the keysets of epochs that
//!   have not started yet are known now: [`Epochs::keyset_for`] derives
//!   them from the seed the same way cdk will when the epoch arrives.
//!   Vouchers dated to a future epoch are signed with that key here, and
//!   cdk accepts them once it rotates to it: not before the epoch starts.
//! - **Conversion.** Melt method [`METHOD`] spends vouchers (cdk checks and
//!   marks the proofs) and reserves the same TiH in a target epoch:
//!   `auto` is the current epoch if it has room and otherwise the next
//!   epoch with room ([`hashu_core::epoch::next_available`]); an explicit
//!   epoch, up to [`LOOKAHEAD`] ahead, is how later epochs are sold
//!   separately. The wallet then has the outputs signed with that epoch's
//!   key ([`Epochs::sign`]). TiH for TiH: vouchers count work.
//! - **Perpetual vouchers.** Keysets from before epochs were turned on
//!   are perpetual. They stay redeemable as they are, and can convert like
//!   an expired voucher.
//! - **Rotation waits for open redemptions.** cdk signs a redemption's
//!   change when it settles, with the keyset the outputs name, and refuses
//!   an inactive one. So the mint rotates only while no `hashrate` melt is
//!   pending; vouchers sold in that gap carry the previous epoch's keyset
//!   and convert like any expired voucher.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bitcoin::bip32::{ChildNumber, DerivationPath, Xpriv};
use cdk::cdk_database::MintKeysDatabase;
use cdk::cdk_payment::{
    CreateIncomingPaymentResponse, Error, Event, IncomingPaymentOptions, MakePaymentResponse,
    MintPayment, OutgoingPaymentOptions, PaymentIdentifier, PaymentQuoteResponse, SettingsResponse,
    WaitPaymentResponse,
};
use cdk::nuts::nut02::KeySetVersion;
use cdk::nuts::{
    BlindSignature, BlindedMessage, CurrencyUnit, Id, KeySet, MeltQuoteState, MintKeySet,
};
use cdk::Amount;
use futures::stream::{self, BoxStream};
use hashu_core::epoch::{next_available, plan, EpochClock, PlanRow, Supply};
use serde::{Deserialize, Serialize};

use crate::capacity::now_ms;
use crate::payment::tih_unit;

/// Melt method that converts vouchers to another epoch.
pub const METHOD: &str = "epoch";

/// Epochs ahead of the current one that can be sold or converted into.
pub const LOOKAHEAD: u64 = 3;

/// How far the planner looks for room when converting `auto`: two years.
/// An expired voucher with nowhere to go stays as it is (still valid).
pub const SEARCH_EPOCHS: u64 = 52;

/// Where the planner's supply and carried TiH come from (the capacity
/// book and the mint's obligations): `(supply, carried_tih)`.
pub type PlanInputs = Arc<dyn Fn() -> (Vec<Supply>, f64) + Send + Sync>;

/// Epoch `epoch`'s keyset sits at derivation index `index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    pub epoch: u64,
    pub index: u32,
}

/// TiH reserved in an epoch by a conversion melt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversion {
    pub amount_tih: u64,
    pub epoch: u64,
    pub keyset_id: String,
    pub created_ms: u64,
    /// Signatures handed out, so a retry gets the same ones back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Vec<BlindSignature>>,
    /// Blinded secrets (hex) the signatures are for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outputs: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    anchor: Option<Anchor>,
    /// Epoch keysets cdk has rotated to: id → epoch.
    #[serde(default)]
    keysets: BTreeMap<String, u64>,
    /// Conversion melts by id (hex of the request hash).
    #[serde(default)]
    conversions: BTreeMap<String, Conversion>,
}

/// Target of a conversion melt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// Current epoch if it has room, else the next one that has.
    Auto,
    Epoch(u64),
}

/// Request string of an `epoch` melt (JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvertRequest {
    /// `"auto"` or an epoch number.
    pub epoch: serde_json::Value,
    pub amount_tih: u64,
    /// Makes two otherwise identical requests distinct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
}

impl ConvertRequest {
    pub fn parse(request: &str) -> Result<(Self, Target), Error> {
        let req: Self = serde_json::from_str(request)
            .map_err(|e| Error::Custom(format!("bad epoch request: {e}")))?;
        let target = match &req.epoch {
            serde_json::Value::String(s) if s == "auto" => Target::Auto,
            serde_json::Value::Number(n) => Target::Epoch(
                n.as_u64()
                    .ok_or_else(|| Error::Custom("epoch must be a whole number".into()))?,
            ),
            _ => return Err(Error::Custom("epoch must be \"auto\" or a number".into())),
        };
        if req.amount_tih == 0 {
            return Err(Error::Custom("amount_tih must be positive".into()));
        }
        Ok((req, target))
    }
}

/// Conversion id for a request string: hex of its SHA-256.
pub fn conversion_id(request: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(request.as_bytes()))
}

/// The mint's epochs: clock, keysets and conversions.
pub struct Epochs {
    inner: Mutex<Persisted>,
    clock: Mutex<Option<EpochClock>>,
    inputs: Mutex<Option<PlanInputs>>,
    version: Mutex<KeySetVersion>,
    path: Option<PathBuf>,
    xpriv: Xpriv,
    amounts: Vec<u64>,
}

impl std::fmt::Debug for Epochs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Epochs").finish_non_exhaustive()
    }
}

fn derivation_path(unit: &CurrencyUnit, index: u32) -> DerivationPath {
    DerivationPath::from(vec![
        ChildNumber::from_hardened_idx(129372).expect("valid index"),
        ChildNumber::from_hardened_idx(unit.hashed_derivation_index()).expect("valid index"),
        ChildNumber::from_hardened_idx(index).expect("valid index"),
    ])
}

impl Epochs {
    /// Epochs for a mint built from `seed` signing `amounts`, kept in
    /// `path` (in memory with `None`).
    pub fn open(seed: &[u8], amounts: Vec<u64>, path: Option<PathBuf>) -> anyhow::Result<Self> {
        let p = match &path {
            Some(path) => match std::fs::read(path) {
                Ok(b) => serde_json::from_slice(&b)
                    .map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
                Err(e) => return Err(anyhow::anyhow!("read {}: {e}", path.display())),
            },
            None => Persisted::default(),
        };
        // cdk's signatory: `Xpriv::new_master(Bitcoin, seed)`.
        let xpriv = Xpriv::new_master(bitcoin::Network::Bitcoin, seed)
            .map_err(|e| anyhow::anyhow!("mint seed: {e}"))?;
        Ok(Self {
            inner: Mutex::new(p),
            clock: Mutex::new(None),
            inputs: Mutex::new(None),
            version: Mutex::new(KeySetVersion::Version01),
            path,
            xpriv,
            amounts,
        })
    }

    fn save(&self, p: &Persisted) {
        let Some(path) = &self.path else { return };
        let tmp = path.with_extension("json.tmp");
        let r = serde_json::to_vec_pretty(p)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            tracing::error!(path = %path.display(), error = %e, "could not save epochs");
        }
    }

    pub fn set_clock(&self, clock: EpochClock) {
        *self.clock.lock().unwrap() = Some(clock);
    }

    pub fn clock(&self) -> Option<EpochClock> {
        *self.clock.lock().unwrap()
    }

    pub fn set_plan_inputs(&self, f: PlanInputs) {
        *self.inputs.lock().unwrap() = Some(f);
    }

    pub fn anchor(&self) -> Option<Anchor> {
        self.inner.lock().unwrap().anchor
    }

    /// Keyset of epoch `epoch` derived from the seed (`None` before the
    /// anchor, i.e. before epochs were turned on).
    pub fn keyset_for(&self, epoch: u64) -> Option<MintKeySet> {
        let anchor = self.anchor()?;
        let index = anchor
            .index
            .checked_add(u32::try_from(epoch.checked_sub(anchor.epoch)?).ok()?)?;
        let unit = tih_unit();
        Some(MintKeySet::generate_from_xpriv(
            &bitcoin::secp256k1::Secp256k1::new(),
            self.xpriv,
            &self.amounts,
            unit.clone(),
            derivation_path(&unit, index),
            0,
            None,
            *self.version.lock().unwrap(),
        ))
    }

    /// Public keys of epoch `epoch`'s keyset.
    pub fn keys_for(&self, epoch: u64) -> Option<KeySet> {
        self.keyset_for(epoch).map(|k| public_keyset(&k, None))
    }

    /// Epoch a keyset is dated to; `None` for a perpetual keyset (or one
    /// this mint does not know). Future epochs within [`LOOKAHEAD`] are
    /// recognised too.
    pub fn epoch_of_keyset(&self, id: &str) -> Option<u64> {
        if let Some(e) = self.inner.lock().unwrap().keysets.get(id) {
            return Some(*e);
        }
        let current = self.clock()?.current();
        (current + 1..=current + LOOKAHEAD)
            .find(|e| self.keyset_for(*e).is_some_and(|k| k.id.to_string() == id))
    }

    /// Epoch keysets cdk has rotated to: id → epoch.
    pub fn keysets(&self) -> BTreeMap<String, u64> {
        self.inner.lock().unwrap().keysets.clone()
    }

    /// Bring the mint's active TiH keyset up to the current epoch: on the
    /// first run, rotate once so vouchers already sold stay perpetual; then
    /// once per epoch boundary. `can_rotate` is false while a redemption is
    /// open (see the module docs). Returns the keysets rotated to.
    pub async fn sync(
        &self,
        mint: &cdk::Mint,
        db: &(dyn MintKeysDatabase<Err = cdk::cdk_database::Error> + Send + Sync),
        can_rotate: bool,
    ) -> anyhow::Result<Vec<(u64, Id)>> {
        let Some(clock) = self.clock() else {
            return Ok(Vec::new());
        };
        let current = clock.current();
        let unit = tih_unit();
        let mut rotated = Vec::new();
        loop {
            let active = db
                .get_active_keyset_id(&unit)
                .await
                .map_err(|e| anyhow::anyhow!("active keyset: {e}"))?
                .ok_or_else(|| anyhow::anyhow!("no active TiH keyset"))?;
            let info = db
                .get_keyset_info(&active)
                .await
                .map_err(|e| anyhow::anyhow!("keyset info: {e}"))?
                .ok_or_else(|| anyhow::anyhow!("no info for keyset {active}"))?;
            let index = info.derivation_path_index.unwrap_or(1);
            *self.version.lock().unwrap() = active.get_version();
            let want = {
                let mut g = self.inner.lock().unwrap();
                match g.anchor {
                    Some(a) => a.index as u64 + current.saturating_sub(a.epoch),
                    None => {
                        // First run: the active keyset holds perpetual
                        // vouchers; the current epoch gets the next index.
                        g.anchor = Some(Anchor {
                            epoch: current,
                            index: index + 1,
                        });
                        self.save(&g);
                        tracing::info!(epoch = current, index = index + 1, "epochs on");
                        index as u64 + 1
                    }
                }
            };
            if index as u64 >= want {
                if index as u64 > want {
                    tracing::error!(
                        index,
                        want,
                        "TiH keyset is ahead of the epoch schedule (rotated outside epochs?)"
                    );
                }
                let mut g = self.inner.lock().unwrap();
                let anchor = g.anchor.expect("set above");
                if index >= anchor.index {
                    let epoch = anchor.epoch + (index - anchor.index) as u64;
                    if g.keysets.insert(active.to_string(), epoch).is_none() {
                        self.save(&g);
                    }
                }
                return Ok(rotated);
            }
            if !can_rotate {
                tracing::info!(
                    epoch = current,
                    "new epoch: keyset rotation waits for open redemptions"
                );
                return Ok(rotated);
            }
            let epoch = {
                let a = self.anchor().expect("set above");
                a.epoch + (index + 1 - a.index) as u64
            };
            let expected = self.keyset_for(epoch).map(|k| k.id);
            let new = mint
                .rotate_keyset(
                    unit.clone(),
                    self.amounts.clone(),
                    0,
                    active.get_version() == KeySetVersion::Version01,
                    None,
                )
                .await
                .map_err(|e| anyhow::anyhow!("rotate keyset: {e}"))?;
            if expected != Some(new.id) {
                tracing::error!(epoch, got = %new.id, expected = ?expected, "epoch keyset differs from the derived one");
            }
            tracing::info!(epoch, keyset = %new.id, "rotated TiH keyset to new epoch");
            {
                let mut g = self.inner.lock().unwrap();
                g.keysets.insert(new.id.to_string(), epoch);
                self.save(&g);
            }
            rotated.push((epoch, new.id));
        }
    }

    /// TiH reserved by conversions in each epoch.
    pub fn dated(&self) -> BTreeMap<u64, u64> {
        let g = self.inner.lock().unwrap();
        let mut m = BTreeMap::new();
        for c in g.conversions.values() {
            *m.entry(c.epoch).or_insert(0) += c.amount_tih;
        }
        m
    }

    /// TiH signed here rather than by cdk for epochs up to `through`
    /// (keysets cdk now knows): owed on top of what cdk counts.
    pub fn signed_outside_cdk(&self, through: u64) -> u64 {
        self.dated().range(..=through).map(|(_, v)| *v).sum()
    }

    /// The plan for the current epoch and `lookahead` more.
    pub fn plan(&self, lookahead: u64) -> Option<Vec<PlanRow>> {
        let clock = self.clock()?;
        let inputs = self.inputs.lock().unwrap().clone()?;
        let (supply, carried) = inputs();
        let dated = self.dated();
        Some(plan(
            &clock,
            &supply,
            carried,
            |e| dated.get(&e).copied().unwrap_or(0) as f64,
            lookahead,
        ))
    }

    /// Where `amount_tih` would go for `target`, or why it can't.
    pub fn resolve(&self, target: Target, amount_tih: u64) -> Result<u64, String> {
        let clock = self
            .clock()
            .ok_or("the epoch clock has not read the chain yet")?;
        let current = clock.current();
        match target {
            Target::Auto => {
                let rows = self.plan(SEARCH_EPOCHS).ok_or("capacity not counted yet")?;
                next_available(&rows).ok_or_else(|| {
                    "no epoch has capacity; the vouchers stay valid as they are".into()
                })
            }
            Target::Epoch(e) if e < current => Err(format!("epoch {e} has ended")),
            Target::Epoch(e) if e > current + LOOKAHEAD => Err(format!(
                "epoch {e} is too far ahead (up to {})",
                current + LOOKAHEAD
            )),
            Target::Epoch(e) => {
                let rows = self.plan(LOOKAHEAD).ok_or("capacity not counted yet")?;
                let row = rows
                    .iter()
                    .find(|r| r.epoch == e)
                    .ok_or("epoch not in plan")?;
                let room = if e == current {
                    // The current epoch's own and expired vouchers are
                    // already counted in it.
                    row.capacity_tih > 0.0 && row.owed_tih <= row.capacity_tih
                } else {
                    row.free_tih >= amount_tih as f64
                };
                if room {
                    Ok(e)
                } else {
                    Err(format!(
                        "epoch {e} has room for {} TiH",
                        row.free_tih.floor() as u64
                    ))
                }
            }
        }
    }

    /// Reserve a conversion (idempotent by id).
    pub fn record(&self, id: &str, target: Target, amount_tih: u64) -> Result<Conversion, String> {
        if let Some(c) = self.conversion(id) {
            return Ok(c);
        }
        let epoch = self.resolve(target, amount_tih)?;
        let keyset = self
            .keyset_for(epoch)
            .ok_or("epochs are not anchored yet")?;
        let c = Conversion {
            amount_tih,
            epoch,
            keyset_id: keyset.id.to_string(),
            created_ms: now_ms(),
            signatures: None,
            outputs: Vec::new(),
        };
        let mut g = self.inner.lock().unwrap();
        let c = g.conversions.entry(id.to_string()).or_insert(c).clone();
        self.save(&g);
        Ok(c)
    }

    pub fn conversion(&self, id: &str) -> Option<Conversion> {
        self.inner.lock().unwrap().conversions.get(id).cloned()
    }

    /// Sign a conversion's outputs with its epoch's key. They must name
    /// that keyset and add up to the converted TiH. Signs once: the same
    /// outputs get the same signatures again, others are refused.
    pub fn sign(
        &self,
        id: &str,
        outputs: &[BlindedMessage],
    ) -> Result<Vec<BlindSignature>, String> {
        let c = self.conversion(id).ok_or("unknown conversion")?;
        let secrets: Vec<String> = outputs.iter().map(|o| o.blinded_secret.to_hex()).collect();
        if let Some(sigs) = &c.signatures {
            return if c.outputs == secrets {
                Ok(sigs.clone())
            } else {
                Err("this conversion is already signed for other outputs".into())
            };
        }
        let total: u64 = outputs.iter().map(|o| u64::from(o.amount)).sum();
        if total != c.amount_tih {
            return Err(format!(
                "outputs add up to {total} TiH; the conversion is {} TiH",
                c.amount_tih
            ));
        }
        let mut seen = std::collections::HashSet::new();
        if !secrets.iter().all(|s| seen.insert(s)) {
            return Err("duplicate outputs".into());
        }
        let keyset = self
            .keyset_for(c.epoch)
            .ok_or("epochs are not anchored yet")?;
        if keyset.id.to_string() != c.keyset_id {
            return Err("epoch keyset changed; contact the operator".into());
        }
        let mut sigs = Vec::with_capacity(outputs.len());
        for o in outputs {
            if o.keyset_id != keyset.id {
                return Err(format!(
                    "outputs must use epoch {}'s keyset {}",
                    c.epoch, keyset.id
                ));
            }
            let pair = keyset
                .keys
                .get(&o.amount)
                .ok_or_else(|| format!("no key for amount {}", o.amount))?;
            let c_ = cdk::dhke::sign_message(&pair.secret_key, &o.blinded_secret)
                .map_err(|e| e.to_string())?;
            sigs.push(
                BlindSignature::new(
                    o.amount,
                    c_,
                    keyset.id,
                    &o.blinded_secret,
                    pair.secret_key.clone(),
                )
                .map_err(|e| e.to_string())?,
            );
        }
        let mut g = self.inner.lock().unwrap();
        let entry = g.conversions.get_mut(id).ok_or("unknown conversion")?;
        if let Some(sigs) = &entry.signatures {
            // Raced with another call: answer as above.
            return if entry.outputs == secrets {
                Ok(sigs.clone())
            } else {
                Err("this conversion is already signed for other outputs".into())
            };
        }
        entry.signatures = Some(sigs.clone());
        entry.outputs = secrets;
        self.save(&g);
        Ok(sigs)
    }
}

/// The public half of a keyset, as `/v1/keys` serves it.
pub fn public_keyset(k: &MintKeySet, active: Option<bool>) -> KeySet {
    KeySet {
        id: k.id,
        unit: k.unit.clone(),
        active,
        keys: k.keys.clone().into(),
        input_fee_ppk: k.input_fee_ppk,
        final_expiry: k.final_expiry,
    }
}

/// Melt backend for method [`METHOD`]: spends vouchers into a conversion.
#[derive(Debug, Clone)]
pub struct EpochConverter {
    epochs: Arc<Epochs>,
}

impl EpochConverter {
    pub fn new(epochs: Arc<Epochs>) -> Self {
        Self { epochs }
    }

    fn request(unit: &CurrencyUnit, options: OutgoingPaymentOptions) -> Result<String, Error> {
        if *unit != tih_unit() {
            return Err(Error::UnsupportedUnit);
        }
        let OutgoingPaymentOptions::Custom(custom) = options else {
            return Err(Error::UnsupportedPaymentOption);
        };
        if custom.method != METHOD {
            return Err(Error::UnsupportedPaymentOption);
        }
        Ok(custom.request)
    }
}

fn lookup(id: &str) -> PaymentIdentifier {
    PaymentIdentifier::CustomId(id.to_string())
}

#[async_trait]
impl MintPayment for EpochConverter {
    type Err = Error;

    async fn get_settings(&self) -> Result<SettingsResponse, Self::Err> {
        let mut custom = std::collections::HashMap::new();
        custom.insert(
            METHOD.to_string(),
            serde_json::json!({ "lookahead": LOOKAHEAD }).to_string(),
        );
        Ok(SettingsResponse {
            unit: tih_unit().to_string(),
            bolt11: None,
            bolt12: None,
            custom,
        })
    }

    async fn create_incoming_payment_request(
        &self,
        _options: IncomingPaymentOptions,
    ) -> Result<CreateIncomingPaymentResponse, Self::Err> {
        Err(Error::UnsupportedPaymentOption)
    }

    async fn get_payment_quote(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<PaymentQuoteResponse, Self::Err> {
        let request = Self::request(unit, options)?;
        let (req, target) = ConvertRequest::parse(&request)?;
        // Refuse early when there is nowhere to go; checked again on melt.
        self.epochs
            .resolve(target, req.amount_tih)
            .map_err(Error::Custom)?;
        Ok(PaymentQuoteResponse {
            request_lookup_id: Some(lookup(&conversion_id(&request))),
            amount: Amount::new(req.amount_tih, unit.clone()),
            fee: Amount::new(0, unit.clone()),
            state: MeltQuoteState::Unpaid,
        })
    }

    async fn make_payment(
        &self,
        unit: &CurrencyUnit,
        options: OutgoingPaymentOptions,
    ) -> Result<MakePaymentResponse, Self::Err> {
        let request = Self::request(unit, options)?;
        let (req, target) = ConvertRequest::parse(&request)?;
        let id = conversion_id(&request);
        let c = self
            .epochs
            .record(&id, target, req.amount_tih)
            .map_err(Error::Custom)?;
        tracing::info!(conversion = %id, epoch = c.epoch, amount_tih = c.amount_tih, "vouchers converted");
        Ok(MakePaymentResponse {
            payment_lookup_id: lookup(&id),
            payment_proof: None,
            status: MeltQuoteState::Paid,
            total_spent: Amount::new(c.amount_tih, unit.clone()),
        })
    }

    async fn wait_payment_event(&self) -> Result<BoxStream<'static, Event>, Self::Err> {
        // Must stay open: see `HashrateMelter::wait_payment_event`.
        Ok(Box::pin(stream::pending()))
    }

    fn is_wait_invoice_active(&self) -> bool {
        false
    }

    fn cancel_wait_invoice(&self) {}

    async fn check_incoming_payment_status(
        &self,
        _payment_identifier: &PaymentIdentifier,
    ) -> Result<Vec<WaitPaymentResponse>, Self::Err> {
        Ok(Vec::new())
    }

    async fn check_outgoing_payment(
        &self,
        payment_identifier: &PaymentIdentifier,
    ) -> Result<MakePaymentResponse, Self::Err> {
        let PaymentIdentifier::CustomId(id) = payment_identifier else {
            return Err(Error::Custom("epoch expects a custom lookup id".into()));
        };
        let (status, amount) = match self.epochs.conversion(id) {
            Some(c) => (MeltQuoteState::Paid, c.amount_tih),
            None => (MeltQuoteState::Unpaid, 0),
        };
        Ok(MakePaymentResponse {
            payment_lookup_id: payment_identifier.clone(),
            payment_proof: None,
            status,
            total_spent: Amount::new(amount, tih_unit()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epochs() -> Epochs {
        let e = Epochs::open(&[7u8; 32], (0..41).map(|i| 1u64 << i).collect(), None).unwrap();
        e.inner.lock().unwrap().anchor = Some(Anchor {
            epoch: 10,
            index: 2,
        });
        // 1,008 blocks left in epoch 10.
        e.set_clock(EpochClock::new(10 * 2016 + 1007));
        e.set_plan_inputs(Arc::new(|| {
            (
                vec![Supply {
                    ths: 1.0,
                    hours_left: None,
                }],
                0.0,
            )
        }));
        e
    }

    #[test]
    fn parses_requests() {
        let (_, t) = ConvertRequest::parse(r#"{"epoch":"auto","amount_tih":5}"#).unwrap();
        assert_eq!(t, Target::Auto);
        let (r, t) = ConvertRequest::parse(r#"{"epoch":12,"amount_tih":5,"nonce":"a"}"#).unwrap();
        assert_eq!((t, r.amount_tih), (Target::Epoch(12), 5));
        assert!(ConvertRequest::parse(r#"{"epoch":"soon","amount_tih":5}"#).is_err());
        assert!(ConvertRequest::parse(r#"{"epoch":12,"amount_tih":0}"#).is_err());
        assert!(ConvertRequest::parse(r#"{"epoch":12,"amount_tih":1,"x":1}"#).is_err());
    }

    #[test]
    fn epoch_keysets_are_distinct_and_recognised() {
        let e = epochs();
        let k10 = e.keyset_for(10).unwrap().id;
        let k11 = e.keyset_for(11).unwrap().id;
        assert_ne!(k10, k11);
        assert_eq!(e.epoch_of_keyset(&k11.to_string()), Some(11));
        assert!(e.keyset_for(9).is_none());
    }

    #[test]
    fn auto_goes_to_the_current_epoch_with_room() {
        let e = epochs();
        assert_eq!(e.resolve(Target::Auto, 4096), Ok(10));
        assert!(e.resolve(Target::Epoch(9), 1).is_err());
        assert!(e.resolve(Target::Epoch(10 + LOOKAHEAD + 1), 1).is_err());
        assert_eq!(e.resolve(Target::Epoch(12), 4096), Ok(12));
    }

    #[test]
    fn auto_rolls_to_the_next_epoch_when_the_current_is_full() {
        let e = epochs();
        let cap0 = hashu_core::denom::tih_from_ths_hours(1.0, 168.0);
        e.set_plan_inputs(Arc::new(move || {
            (
                vec![Supply {
                    ths: 1.0,
                    hours_left: None,
                }],
                cap0 + 1.0,
            )
        }));
        assert_eq!(e.resolve(Target::Auto, 4096), Ok(11));
        // The current epoch is oversold, so an explicit request for it fails.
        assert!(e.resolve(Target::Epoch(10), 1).is_err());
    }

    #[test]
    fn future_epochs_fill_up() {
        let e = epochs();
        let cap = hashu_core::denom::tih_from_ths_hours(1.0, 336.0).floor() as u64;
        e.record("a", Target::Epoch(11), cap - 10).unwrap();
        assert!(e.resolve(Target::Epoch(11), 11).is_err());
        assert_eq!(e.resolve(Target::Epoch(11), 10), Ok(11));
        assert_eq!(e.signed_outside_cdk(10), 0);
        assert_eq!(e.signed_outside_cdk(11), cap - 10);
    }

    #[test]
    fn signs_once_with_the_epoch_key() {
        use cdk::nuts::PreMintSecrets;
        let e = epochs();
        e.record("c", Target::Epoch(11), 5).unwrap();
        let ks = e.keyset_for(11).unwrap();
        let premint = PreMintSecrets::random(
            ks.id,
            Amount::from(5),
            &cdk::amount::SplitTarget::default(),
            &(0, (0..41).map(|i| 1u64 << i).collect::<Vec<_>>()).into(),
        )
        .unwrap();
        let outputs = premint.blinded_messages();
        let sigs = e.sign("c", &outputs).unwrap();
        assert_eq!(sigs.len(), outputs.len());
        // Unblind and check against the epoch's public keys.
        let keys = public_keyset(&ks, None);
        let proofs =
            cdk::dhke::construct_proofs(sigs.clone(), premint.rs(), premint.secrets(), &keys.keys)
                .unwrap();
        for p in &proofs {
            let k = ks.keys.get(&p.amount).unwrap();
            cdk::dhke::verify_message(&k.secret_key, p.c, p.secret.as_bytes()).unwrap();
        }
        // Same outputs: same signatures; others refused.
        assert_eq!(e.sign("c", &outputs).unwrap(), sigs);
        let other = PreMintSecrets::random(
            ks.id,
            Amount::from(5),
            &cdk::amount::SplitTarget::default(),
            &(0, vec![1, 2, 4]).into(),
        )
        .unwrap();
        assert!(e.sign("c", &other.blinded_messages()).is_err());
        // Wrong total or keyset.
        e.record("d", Target::Epoch(11), 6).unwrap();
        assert!(e.sign("d", &outputs).is_err());
    }

    #[tokio::test]
    async fn rotation_follows_the_derived_epoch_keysets() {
        use cdk::mint::{MintBuilder, UnitConfig};
        use cdk::nuts::PreMintSecrets;
        let amounts: Vec<u64> = (0..41).map(|i| 1u64 << i).collect();
        let seed = [9u8; 32];
        let mut b = MintBuilder::new(Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap()));
        b.configure_unit(
            tih_unit(),
            UnitConfig {
                amounts: amounts.clone(),
                input_fee_ppk: 0,
            },
        )
        .unwrap();
        let keystore = Arc::new(cdk_sqlite::mint::memory::empty().await.unwrap());
        let mint = b.build_with_seed(keystore.clone(), &seed).await.unwrap();
        let active = |m: &cdk::Mint| {
            m.keysets()
                .keysets
                .into_iter()
                .find(|k| k.unit == tih_unit() && k.active)
                .unwrap()
                .id
        };
        let perpetual = active(&mint);

        let e = Epochs::open(&seed, amounts.clone(), None).unwrap();
        e.set_clock(EpochClock::new(10 * 2016 + 5));
        e.set_plan_inputs(Arc::new(|| {
            (
                vec![Supply {
                    ths: 1.0,
                    hours_left: None,
                }],
                0.0,
            )
        }));
        let rotated = e.sync(&mint, keystore.as_ref(), true).await.unwrap();
        assert_eq!(rotated.len(), 1);
        assert_eq!(rotated[0].0, 10);
        let k10 = active(&mint);
        assert_ne!(k10, perpetual, "vouchers sold before epochs stay perpetual");
        assert_eq!(e.keyset_for(10).unwrap().id, k10);
        assert_eq!(e.epoch_of_keyset(&k10.to_string()), Some(10));
        assert_eq!(e.epoch_of_keyset(&perpetual.to_string()), None);
        // Nothing more to do within the epoch.
        assert!(e
            .sync(&mint, keystore.as_ref(), true)
            .await
            .unwrap()
            .is_empty());

        // Vouchers dated to epoch 12, signed ahead of time.
        e.record("fwd", Target::Epoch(12), 6).unwrap();
        let ks12 = e.keyset_for(12).unwrap();
        let premint = PreMintSecrets::random(
            ks12.id,
            Amount::from(6),
            &cdk::amount::SplitTarget::default(),
            &(0, amounts.clone()).into(),
        )
        .unwrap();
        let sigs = e.sign("fwd", &premint.blinded_messages()).unwrap();
        let proofs = cdk::dhke::construct_proofs(
            sigs,
            premint.rs(),
            premint.secrets(),
            &public_keyset(&ks12, None).keys,
        )
        .unwrap();
        // Not spendable before epoch 12.
        assert!(mint.verify_proofs(proofs.clone()).await.is_err());

        // Epoch 12 arrives; an open redemption holds the rotation back.
        e.set_clock(EpochClock::new(12 * 2016));
        assert!(e
            .sync(&mint, keystore.as_ref(), false)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(active(&mint), k10);
        let rotated = e.sync(&mint, keystore.as_ref(), true).await.unwrap();
        assert_eq!(
            rotated.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![11, 12]
        );
        assert_eq!(active(&mint), ks12.id);
        mint.verify_proofs(proofs).await.unwrap();
    }
}
