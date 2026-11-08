//! HTTP 402 Payment Required for Hashu's paid endpoints.
//!
//! A paid endpoint answers an unpaid request with `402` and one challenge
//! per payment scheme it accepts, and serves a request that carries a
//! credential for one of them. The schemes are kept apart from the
//! endpoints so a new one is a new [`Scheme`] plus a [`Credential`]
//! variant, with no change to the 402 plumbing:
//!
//! - **Cashu, NUT-24** ([`CashuScheme`]): the challenge is an `X-Cashu`
//!   header holding a NUT-18 payment request (`creqA…`) with only `a`, `u`,
//!   `m` (and `nut10` when set) and no transport, since the payment comes
//!   back in band. The client retries with a `cashuB…` token in `X-Cashu`.
//!   <https://github.com/cashubtc/nuts/blob/main/24.md>
//! - **L402** (planned): `WWW-Authenticate: L402 macaroon="…", invoice="…"`
//!   and `Authorization: L402 <macaroon>:<preimage>`. It slots in as another
//!   [`Scheme`] whose challenge is priced from the same [`Price`].
//!
//! The body of a 402 is JSON naming the price and the schemes, for people
//! reading it with `curl`; clients act on the headers.

use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bitcoin::base64::engine::general_purpose::URL_SAFE;
use bitcoin::base64::Engine;
use cdk::mint_url::MintUrl;
use cdk::nuts::nut18::Nut10SecretRequest;
use cdk::nuts::{CurrencyUnit, Token};
use serde::Serialize;
use serde_json::{json, Value};
use std::str::FromStr;

/// The NUT-24 header, in requests (payment) and 402 responses (request).
pub const X_CASHU: HeaderName = HeaderName::from_static("x-cashu");

/// What a paid request costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Price {
    /// Amount in THH (terahash-hours). Every scheme prices from this.
    pub amount_thh: u64,
    /// What the payment is for, shown to a person.
    pub description: String,
}

/// A payment presented with a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// The raw `X-Cashu` header: a serialized Cashu token.
    Cashu(String),
}

/// One way to pay for a request.
pub trait Scheme: Send + Sync {
    /// Short name for the 402 body (`"cashu"`, `"l402"`).
    fn name(&self) -> &'static str;
    /// The header this scheme adds to a 402 for `price`.
    fn challenge(&self, price: &Price) -> anyhow::Result<(HeaderName, HeaderValue)>;
    /// The credential for this scheme in `headers`, if the client sent one.
    fn credential(&self, headers: &HeaderMap) -> Option<Credential>;
}

/// The first credential in `headers` for any of `schemes`, in their order.
pub fn credential(headers: &HeaderMap, schemes: &[&dyn Scheme]) -> Option<Credential> {
    schemes.iter().find_map(|s| s.credential(headers))
}

/// A `402 Payment Required` carrying every scheme's challenge.
pub fn payment_required(price: &Price, schemes: &[&dyn Scheme]) -> Response {
    let mut headers = HeaderMap::new();
    let mut offered = Vec::new();
    let mut exposed = Vec::new();
    for s in schemes {
        match s.challenge(price) {
            Ok((name, value)) => {
                offered.push(json!({ "scheme": s.name(), "header": name.as_str(), "value": value.to_str().unwrap_or_default() }));
                exposed.push(name.as_str().to_string());
                headers.append(name, value);
            }
            Err(e) => tracing::warn!(scheme = s.name(), error = %e, "payment challenge failed"),
        }
    }
    if offered.is_empty() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "no payment scheme could price this request" })),
        )
            .into_response();
    }
    // Browser clients can only read the challenge if it is exposed.
    if let Ok(v) = HeaderValue::from_str(&exposed.join(", ")) {
        headers.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, v);
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    let body: Value = json!({
        "error": "payment required",
        "amount_thh": price.amount_thh,
        "description": price.description,
        "accepts": offered,
    });
    (StatusCode::PAYMENT_REQUIRED, headers, Json(body)).into_response()
}

/// NUT-24: pay with a Cashu token of `unit` from one of `mints`.
#[derive(Debug, Clone)]
pub struct CashuScheme {
    pub mints: Vec<MintUrl>,
    pub unit: CurrencyUnit,
    /// A NUT-10 locking condition the token must carry. Hashu sets none;
    /// [`Self::accept`] refuses to run with one until it can check it.
    pub nut10: Option<Nut10SecretRequest>,
}

/// The payment request NUT-24 puts in `X-Cashu`: exactly `a`, `u`, `m`
/// and `nut10` when set. cdk's `PaymentRequest` also writes `i`, `s` and
/// `d` as CBOR nulls, so it is only used to read requests back.
#[derive(Serialize)]
struct Nut24Request<'a> {
    a: u64,
    u: &'a CurrencyUnit,
    m: &'a [MintUrl],
    #[serde(skip_serializing_if = "Option::is_none")]
    nut10: Option<&'a Nut10SecretRequest>,
}

impl CashuScheme {
    /// THH from `mint`, unlocked.
    pub fn thh(mint: MintUrl) -> Self {
        Self {
            mints: vec![mint],
            unit: CurrencyUnit::Custom("thh".into()),
            nut10: None,
        }
    }

    /// The encoded NUT-18 request for `price`: `"creqA" +
    /// base64_urlsafe(CBOR({a, u, m}))`, with no transport (in band).
    pub fn request(&self, price: &Price) -> anyhow::Result<String> {
        let req = Nut24Request {
            a: price.amount_thh,
            u: &self.unit,
            m: &self.mints,
            nut10: self.nut10.as_ref(),
        };
        let mut cbor = Vec::new();
        ciborium::into_writer(&req, &mut cbor)?;
        Ok(format!("creqA{}", URL_SAFE.encode(cbor)))
    }

    /// Check a presented token against the request for `price` without
    /// spending it (NUT-24 "Payment response"): it must be a `cashuB`
    /// token, from a mint in `m`, of unit `u`, worth at least `a`. A token
    /// that fails any of these gets a 400. `a` is checked against the
    /// token's face value here; the caller re-checks what is unspent.
    pub fn accept(&self, raw: &str, price: &Price) -> Result<Token, Rejection> {
        if self.nut10.is_some() {
            // Never take a token without checking a condition we asked for.
            return Err(Rejection::Locking);
        }
        let raw = raw.trim();
        if !raw.starts_with("cashuB") {
            return Err(Rejection::NotCashuB);
        }
        let token = Token::from_str(raw).map_err(|_| Rejection::NotCashuB)?;
        let mint = token.mint_url().map_err(|_| Rejection::NotCashuB)?;
        if !self.mints.iter().any(|m| same_mint(m, &mint)) {
            return Err(Rejection::WrongMint);
        }
        let unit_ok = token
            .unit()
            .as_ref()
            .is_some_and(|u| same_unit(u, &self.unit));
        if !unit_ok {
            return Err(Rejection::WrongUnit);
        }
        let value = token
            .value()
            .map(u64::from)
            .map_err(|_| Rejection::NotCashuB)?;
        if value < price.amount_thh {
            return Err(Rejection::Insufficient {
                have: value,
                need: price.amount_thh,
            });
        }
        Ok(token)
    }
}

impl Scheme for CashuScheme {
    fn name(&self) -> &'static str {
        "cashu"
    }

    fn challenge(&self, price: &Price) -> anyhow::Result<(HeaderName, HeaderValue)> {
        Ok((X_CASHU, HeaderValue::from_str(&self.request(price)?)?))
    }

    fn credential(&self, headers: &HeaderMap) -> Option<Credential> {
        let v = headers.get(&X_CASHU)?.to_str().ok()?.trim();
        (!v.is_empty()).then(|| Credential::Cashu(v.to_string()))
    }
}

/// Why a presented payment is refused. Always a 400 (NUT-24).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    NotCashuB,
    WrongMint,
    WrongUnit,
    Insufficient {
        have: u64,
        need: u64,
    },
    /// The request set `nut10`, which this server cannot verify yet.
    Locking,
}

impl Rejection {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotCashuB => "not_cashu_b",
            Self::WrongMint => "wrong_mint",
            Self::WrongUnit => "wrong_unit",
            Self::Insufficient { .. } => "insufficient",
            Self::Locking => "locking",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NotCashuB => "X-Cashu must hold a cashuB token".into(),
            Self::WrongMint => "token is from a mint this endpoint does not accept".into(),
            Self::WrongUnit => "token is not in the requested unit (thh)".into(),
            Self::Insufficient { have, need } => {
                format!("token is worth {have} THH, this request needs at least {need}")
            }
            Self::Locking => {
                "this endpoint cannot verify the locking condition it asked for".into()
            }
        }
    }
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": self.message(), "code": self.code() })),
        )
            .into_response()
    }
}

/// Mint URLs compare after cdk's normalisation (case, trailing slash).
fn same_mint(a: &MintUrl, b: &MintUrl) -> bool {
    a.to_string().eq_ignore_ascii_case(&b.to_string())
}

/// cdk writes custom units lowercase; compare custom units ignoring case.
fn same_unit(a: &CurrencyUnit, b: &CurrencyUnit) -> bool {
    match (a, b) {
        (CurrencyUnit::Custom(x), CurrencyUnit::Custom(y)) => x.eq_ignore_ascii_case(y),
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cdk::nuts::nut18::PaymentRequest;
    use cdk::nuts::{Id, Proof, PublicKey};
    use cdk::secret::Secret;
    use cdk::Amount;

    fn thh() -> CurrencyUnit {
        CurrencyUnit::Custom("thh".into())
    }

    fn scheme() -> CashuScheme {
        CashuScheme::thh(MintUrl::from_str("https://mint.hashu.io").unwrap())
    }

    fn price(a: u64) -> Price {
        Price {
            amount_thh: a,
            description: "test".into(),
        }
    }

    fn token(mint: &str, unit: CurrencyUnit, amounts: &[u64]) -> String {
        let id = Id::from_str("009a1f293253e41e").unwrap();
        let c = PublicKey::from_hex(
            "02698c4e2b5f9534cd0687d87513c759790cf829aa5739184a3e3735471fbda904",
        )
        .unwrap();
        let proofs = amounts
            .iter()
            .map(|a| Proof::new(Amount::from(*a), id, Secret::generate(), c))
            .collect();
        Token::new(MintUrl::from_str(mint).unwrap(), proofs, None, unit).to_string()
    }

    #[test]
    fn challenge_is_a_nut18_request_without_transport() {
        let (name, value) = scheme().challenge(&price(4)).unwrap();
        assert_eq!(name, "x-cashu");
        let v = value.to_str().unwrap();
        assert!(v.starts_with("creqA"), "{v}");
        let req = PaymentRequest::from_str(v).unwrap();
        assert_eq!(req.amount, Some(Amount::from(4)));
        assert_eq!(req.unit, Some(thh()));
        assert_eq!(req.mints, scheme().mints);
        assert!(req.transports.is_empty());
        assert!(req.nut10.is_none());
        // Exactly the NUT-24 fields on the wire, `u` lowercase as mints
        // and wallets write it.
        let cbor = URL_SAFE.decode(&v["creqA".len()..]).unwrap();
        let map: std::collections::BTreeMap<String, ciborium::Value> =
            ciborium::from_reader(&cbor[..]).unwrap();
        assert_eq!(map.keys().collect::<Vec<_>>(), ["a", "m", "u"]);
        assert_eq!(map["u"], ciborium::Value::Text("thh".into()));
        assert_eq!(map["a"], ciborium::Value::Integer(4.into()));
    }

    #[tokio::test]
    async fn payment_required_carries_the_header_and_a_json_body() {
        let s = scheme();
        let resp = payment_required(&price(2), &[&s]);
        assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
        let h = resp.headers();
        assert!(h["x-cashu"].to_str().unwrap().starts_with("creqA"));
        assert_eq!(h[header::ACCESS_CONTROL_EXPOSE_HEADERS], "x-cashu");
        let body = axum::body::to_bytes(resp.into_body(), 1 << 16)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["amount_thh"], 2);
        assert_eq!(v["accepts"][0]["scheme"], "cashu");
    }

    #[test]
    fn reads_the_credential_header() {
        let s = scheme();
        let mut h = HeaderMap::new();
        assert_eq!(credential(&h, &[&s]), None);
        h.insert(X_CASHU, HeaderValue::from_static("  "));
        assert_eq!(credential(&h, &[&s]), None);
        h.insert(X_CASHU, HeaderValue::from_static("cashuBxyz"));
        assert_eq!(
            credential(&h, &[&s]),
            Some(Credential::Cashu("cashuBxyz".into()))
        );
    }

    #[test]
    fn accepts_a_matching_token() {
        let t = token("https://mint.hashu.io", thh(), &[4, 1]);
        let tok = scheme().accept(&t, &price(5)).unwrap();
        assert_eq!(u64::from(tok.value().unwrap()), 5);
        // More than asked is fine (>= a); trailing slash and case don't matter.
        let t = token(
            "https://Mint.Hashu.io/",
            CurrencyUnit::Custom("THH".into()),
            &[8],
        );
        assert!(scheme().accept(&t, &price(5)).is_ok());
    }

    #[test]
    fn rejects_per_nut24() {
        let s = scheme();
        let other = token("https://other.mint", thh(), &[8]);
        assert_eq!(s.accept(&other, &price(1)), Err(Rejection::WrongMint));
        let sats = token("https://mint.hashu.io", CurrencyUnit::Sat, &[8]);
        assert_eq!(s.accept(&sats, &price(1)), Err(Rejection::WrongUnit));
        let small = token("https://mint.hashu.io", thh(), &[2]);
        assert_eq!(
            s.accept(&small, &price(3)),
            Err(Rejection::Insufficient { have: 2, need: 3 })
        );
        assert_eq!(s.accept("hello", &price(1)), Err(Rejection::NotCashuB));
        assert_eq!(s.accept("cashuBnope", &price(1)), Err(Rejection::NotCashuB));
        // A request with a locking condition is never accepted unchecked.
        let locked = CashuScheme {
            nut10: Some(Nut10SecretRequest::new(
                cdk::nuts::Kind::P2PK,
                "02698c4e2b5f9534cd0687d87513c759790cf829aa5739184a3e3735471fbda904",
                None::<Vec<Vec<String>>>,
            )),
            ..scheme()
        };
        let ok = token("https://mint.hashu.io", thh(), &[8]);
        assert_eq!(locked.accept(&ok, &price(1)), Err(Rejection::Locking));
        let r = Rejection::WrongMint.into_response();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }
}
