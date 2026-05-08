# Hashu — Architecture (Draft v0.1)

A hashrate proxy fused with a Cashu mint. Buyers pay Lightning, receive
blinded ecash vouchers denominated in hashrate-time, and redeem them to
direct real hashrate at a pool of their choice.

Domain: **hashu.io**
License: dual-licensed under MIT or Apache-2.0 (Rust ecosystem default)
Language: Rust

---

## 1. Vision

Today, buying hashrate is custodial, KYC-heavy, and contractually rigid.
Cashu gives us a privacy-preserving, blinded-signature bearer instrument.
A Stratum proxy gives us programmable control over where real hashrate
is pointed. Compose the two and you get a **bearer voucher for
hashrate-time** that can be transferred, swapped, melted to Lightning,
or redeemed for actual mining work — all without the mint knowing who
the holder is.

Hashu is **self-hosted software**, not a service. Anyone with hashrate
can run a Hashu instance and start issuing vouchers backed by it.
Spiritually closer to `fedimintd` or `cashu-mintd` than to a centralized
hashrate marketplace. Each Hashu instance is a single trust domain: one
operator, one mint, one pool of hashrate. Vouchers from different
operators interoperate through standard Cashu and Lightning rails
(swap, BOLT11 melt) but are issued under separate trust assumptions.

## 2. Core actors

| Actor                | What they do                                                                                              | Trust placed in Hashu                                       |
| -------------------- | --------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| **Operator**         | Runs Hashu. Sources hashrate (own ASICs / NiceHash / co-op / etc), holds mint signing keys + LN wallet.   | (this is the trusted role — see §6)                         |
| **Voucher buyer**    | Pays Lightning, receives `THH` vouchers, redeems against any pool.                                        | Mint will redeem; proxy will route honestly.                |
| **Pool operator**    | Receives shares routed by Hashu on behalf of redeemers (or the operator's default pool during idle).      | None directly; just sees inbound stratum traffic.           |
| **Hashprice oracle** | Publishes sats/Th/day. Initial source: Luxor.                                                             | Operator selects; oracle is the pricing source-of-truth.    |

**Hashrate sourcing is out of scope.** How the operator obtains the
hashrate they direct — owning ASICs, buying via NiceHash / Mining Rig
Rentals, paying a co-located fleet, agreements with co-op miners — and
how they compensate that source is a relationship managed by the
operator outside Hashu. From Hashu's perspective there are simply
inbound Stratum connections it is authorized to direct.

## 3. System diagram

```
                     ┌────────────────────────────────────────────────┐
                     │                  Hashu Daemon                  │
                     │                                                │
   Lightning ◀──────▶│ ┌─────────────┐    ┌────────────────────────┐  │
                     │ │ Cashu Mint  │◀──▶│ Voucher Accounting     │  │
                     │ │ (cdk-mintd) │    │ - issuance ledger      │  │
                     │ └─────────────┘    │ - redemption schedule  │  │
                     │        ▲           │ - hashprice integral   │  │
                     │        │           └─────────┬──────────────┘  │
   Hashprice ───────▶│ ┌──────┴──────┐              │                 │
   (Luxor API)       │ │ Oracle Adp. │              ▼                 │
                     │ └─────────────┘    ┌────────────────────────┐  │
                     │                    │ Pool Routing Controller│  │
                     │                    └─────────┬──────────────┘  │
                     │                              │                 │
                     │                    ┌─────────┴──────────────┐  │
   Miners ──Stratum──▶│ Stratum Proxy ◀──┤ per-connection routing │  │
                     │ (V1 first, V2 next)│  table & share meter   │  │
                     │                    └─────────┬──────────────┘  │
                     └────────────────────────────────────────────────┘
                                                    │
                                                    ▼
                                         Upstream pools (any)
```

## 4. Components

### 4.1 Cashu mint

- Built on **`cdk` (Cashu Development Kit)** — actively maintained Rust
  reference. Use `cdk-mintd` as a starting binary or embed `cdk` as a
  library.
- Custom currency units:
  - `THH` — terahash-hours (1 unit = 1 Th sustained for 1 hour ≈ 3.6 PH).
  - `THD` — terahash-days (1 unit = 1 Th sustained for 24 hours). Optional
    convenience denom.
- Sat-equivalent vouchers (§5.2) are still issued as `THH`-denominated
  tokens; the *redemption mode* is the variable, not the unit. This
  keeps the mint's denomination tree stable.
- Standard NUTs in scope: 00, 01, 02, 03, 04 (mint), 05 (melt), 06, 07–09
  (token state). See §7 for proposed extensions.

### 4.2 Stratum proxy

- **Stratum V1** for MVP — universal pool compatibility.
- **Stratum V2** as fast-follow using SRI (Stratum Reference
  Implementation, Rust). V2's job-declaration mode is a natural fit
  because the proxy already wants to dictate work assignment.
- Per-connection state: connection ID, hashrate EMA, current upstream
  pool binding, share counter.
- **Operator config: default pool URL + credentials.** All inbound
  hashrate is relayed to this pool whenever no redemption is active.
  This is typically the operator's own mining pool account so they
  earn block rewards from idle hashrate.
- **Active redemption: redirect.** When a redemption is live, Hashu
  drops the upstream connection to the default pool and opens a new
  one to the redeemer's pool with the redeemer's credentials. New jobs
  flow down; share submissions go to the redeemer's pool. On
  redemption end, Hashu reopens the default-pool connection and
  resumes passthrough.

### 4.3 Voucher accounting

- Source-of-truth ledger for:
  - Issued vouchers (amount, denom, issuance timestamp).
  - Active redemptions (voucher hash → pool URL → start time → progress).
- Redemption progress is updated on every accepted share from the
  Stratum proxy. For sats-equivalent redemptions, the running integral
  is updated whenever (a) a share is accepted or (b) the hashprice
  oracle posts a new tick — whichever is finer.

### 4.4 Hashprice oracle adapter

- v0: Luxor Hashrate Index API. Pull cadence: 5 min.
- Cache last N samples; expose interpolated value at arbitrary `t`.
- Pluggable — define a `HashpriceOracle` trait so we can add Hashrate
  Index, Braiins, or self-derived (network difficulty + fee market) later.

### 4.5 Lightning node

- LND or CLN via gRPC/CLN-gRPC. Pluggable through `cdk`'s LN backend
  abstraction — both are already supported.
- Holds incoming sats from voucher purchases. **Funds in this wallet
  are operator liability**, not user funds proper — see trust model.

### 4.6 Pool routing controller

- Receives "redirect" intents from voucher accounting.
- Maintains a per-redemption allocation. Strategy v0: proportional —
  every connected hashrate source contributes a slice weighted by its
  hashrate EMA, so concurrent redemptions progress at roughly equal
  share-rate even if a connection drops.
- Alternative v1 strategy: dedicated assignment (whole connections
  pinned to a single redemption) for lower variance per redemption but
  worse utilization.

### 4.7 Nostr attestation service

The operator holds a long-lived nostr keypair (`npub` published at
startup) that serves as the public identity of the Hashu instance.
Three artifact classes get signed:

- **Operator manifest** (§4.7.1). Binds the nostr identity to the
  current mint keyset.
- **Share commitment tree** (§4.7.2). An append-only Merkle tree of
  PoW share leaves per redemption. Roots are published periodically as
  signed nostr events. The leaves themselves stay private and are
  delivered only to the redeemer.
- **Completion certificate** (§4.7.3). Final signed event committing
  to the terminal root, total difficulty, and integrated value of the
  redemption.

Trust value: turns "trust the operator's database" into "trust the
operator hasn't lost their nostr key AND the upstream pool's records
agree." Redeemers can refute fraudulent claims publicly without ever
disclosing which pool they routed to.

#### 4.7.1 Operator manifest

The nostr identity key and the Cashu mint signing keys are
**separate** — different cryptographic primitives (Schnorr vs. BDHKE),
different rotation cadences, different blast radius on compromise. The
manifest is what binds them.

On first run, and again whenever the mint rotates its keyset, Hashu
publishes a **kind 0 (metadata)** event signed by the operator's nostr
key. The kind 0 `content` is a JSON object combining the standard
nostr profile fields with a namespaced `hashu` object carrying the
machine-readable manifest:

```
{
  "name":    "<operator display name>",
  "about":   "Hashu mint at mint.example.com",
  "nip05":   "operator@example.com",
  "lud16":   "operator@example.com",
  "website": "https://mint.example.com",

  "hashu": {
    "instance_url":     "https://mint.example.com",
    "mint_pubkey_ids":  [
      {"id": "<keyset-id-1>", "status": "active"},
      {"id": "<keyset-id-0>", "status": "retired", "retired_at": <unix>}
    ],
    "supported_units":  ["THH"],
    "supported_melts":  ["bolt11", "hashrate", "hashrate-sats"],
    "hashprice_oracle": "luxor",
    "manifest_version": 1,
    "issued_at":        <unix>
  }
}
```

Kind 0 is replaceable — only the latest event per pubkey is kept by
relays — so historical attribution is preserved by making
`mint_pubkey_ids` **append-only**: rotated keysets remain listed with
`status: "retired"` rather than being removed. A wallet holding a `THH`
token issued under a retired keyset can still verify the keyset was
attested by checking the current kind 0. Standard nostr clients render
the profile fields normally and ignore the unknown `hashu` key.

**Operators should dedicate the nostr keypair to their Hashu instance**
rather than reuse a personal nostr identity. Every keyset rotation
rewrites the kind 0 event, so an operator's personal profile would
otherwise become a deployment artifact and any personal display fields
would be subject to whatever the manifest publisher writes. A
Hashu-only npub can also be linked back to a personal identity later
via NIP-39 `i` tags or a co-signed nostr post, without coupling the
two at the key level.

Verifiers — typically wallet software receiving `THH` tokens — can
fetch the manifest, confirm the issuing mint's keyset ID matches one
the operator has attested to, and treat the nostr npub as the
canonical operator identity. Same pattern as DNSSEC KSK signing ZSKs,
or PGP identity keys signing release keys.

Implementation note: nostr signing is BIP-340 Schnorr over secp256k1
— the same curve we already need for Cashu, so no new cryptographic
dependency. The nostr key is a separate scalar from any mint keyset
key.

#### 4.7.2 Share commitment tree

**Goal: prove the work without leaking customer's worker info.** A redeemer needs to
know that Hashu actually directed hashrate during their redemption
window — not which pool ID was named in Hashu's logs. PoW is
self-validating: anyone with the share preimage can re-hash and check
it against the claimed difficulty target. So the attestation commits
to *PoW solutions*, not to *pool routing*.

Per redemption, Hashu maintains an append-only Merkle tree. Each
accepted share becomes a leaf:

```
leaf_i = SHA256(
    share_preimage_i  ||   // bytes that hash to the share
    target_bits_i     ||   // claimed share difficulty
    ntime_i           ||   // share timestamp from header
    seq_i                  // monotonic per-redemption counter
)
```

Notably absent from the leaf: pool URL, worker name, redeemer
identity. The leaf is fully self-contained PoW evidence.

**Public stream (signed by operator npub):** at a configurable cadence
(default: every 1024 leaves OR every 5 minutes, whichever comes first),
Hashu publishes a signed nostr event:

```
{
  "redemption_id": "<opaque random id>",
  "root":          "<merkle root hex>",
  "leaf_count":    <int, monotonically increasing>,
  "first_seq":     <int>,
  "last_seq":      <int>,
  "wall_ts":       <unix>
}
```

The `redemption_id` is a Hashu-internal opaque handle. Redeemers can
keep it private (and thus unlinkable) or publish it to assert claims.
The roots are non-replaceable so the chain of commitments is durable.

**Private stream (delivered to redeemer only):** at any point during or
after the redemption, Hashu hands the redeemer the leaves themselves
(preimages, difficulties, timestamps, sequence numbers) over an
authenticated channel — typically encrypted to the redeemer's nostr
pubkey via NIP-44, or returned in the melt response if the redeemer
provides a delivery pubkey.

**Verification by the redeemer:**

1. Re-derive each `leaf_i` from preimage + difficulty + ntime + seq;
   verify Merkle inclusion against the published roots.
2. Re-hash each `share_preimage_i`; verify `hash ≤ target_bits_i`.
3. Cross-reference the share set against the redeemer's pool's
   reported share submissions for their account during the window.
   Each accepted share at the pool must have a matching leaf in the
   tree (modulo a configured tolerance for share rejection variance).

**Privacy properties:**

- Public observers see a stream of roots and counts. They learn that
  *some* redemption is being fulfilled but learn nothing about the
  pool, the redeemer, or even individual share data.
- The pool sees normal share submissions on the redeemer's account,
  with no Hashu-specific marker.
- Only the redeemer (holding both the leaves and their pool's records)
  can link the two.

**Caveat — block discovery is the one leak.** If a share happens to
solve a block, it gets published on chain, including the pool's
coinbase payout address. A public observer can then check whether the
block hash appears as a leaf in any nearby Hashu commitment, and
confirm Hashu was directing at that pool *for that block leaf*. Most
shares never become blocks, so the routing privacy holds for
near-totality of share traffic. We do not mitigate this further in v0;
a redeemer who needs absolute pool-routing privacy must accept losing
attestation, or use an oblivious pool-routing layer outside Hashu.

#### 4.7.3 Completion certificate

At redemption end Hashu publishes a final signed nostr event:

```
{
  "redemption_id":       "<opaque>",
  "final_root":          "<terminal merkle root>",
  "final_leaf_count":    <int>,
  "total_difficulty":    "<sum of share targets>",
  "integrated_value":    "<sats, computed from oracle>",
  "oracle_samples_root": "<merkle root over hashprice ticks>",
  "start_ts":            <unix>,
  "end_ts":              <unix>,
  "outcome":             "completed" | "partial-reissued"
}
```

No pool URL. No worker name. The `oracle_samples_root` commits to the
hashprice tick stream consumed by the integral, so a redeemer can
audit the integration if Hashu later publishes the tick log.

## 5. Voucher types & lifecycle

### 5.1 Th-hour voucher (denomination: `THH`)

```
Mint:
    Buyer ──▶ POST /v1/mint/quote/bolt11 { unit:"THH", amount: N }
    Mint  ──▶ { quote_id, invoice }   // invoice priced via hashprice oracle
    Buyer ──▶ pays invoice
    Buyer ──▶ POST /v1/mint/bolt11 { quote_id, blinded_messages }
    Mint  ──▶ blinded signatures      // standard NUT-04

Redeem (custom melt method "hashrate"):
    Holder ──▶ POST /v1/melt/quote/hashrate { unit:"THH", proofs, pool_url, worker_name }
    Mint   ──▶ verifies proofs, schedules redemption, returns redemption_id
    Proxy  ──▶ binds pool_url to a slice of connected hashrate
    Proxy  ──▶ logs accepted shares against redemption_id
    Mint   ──▶ marks redemption complete when ∫ hashrate dt ≥ N · 1h
```

Pricing at mint time: `invoice_sats = N · hashprice_now · 1h + mint_fee`.
Buyer is locking in *today's* hashprice for *future* hashrate-time —
this is the product's economic primitive.

### 5.2 Sats-equivalent redemption (same `THH` token, different melt method)

Hashrate is directed at the **redeemer's chosen pool** until cumulative
*expected* revenue (∫ hashprice(t) · hashrate(t) dt) reaches the
voucher's declared sat value. The redeemer's pool pays them in BTC
out-of-band as normal; Hashu provides only the routing and a
nostr-signed completion certificate (§4.7). Hashu does **not** run a
pool and does not custody the mined sats.

```
Redeem (melt method "hashrate-sats"):
    Holder ──▶ POST /v1/melt/quote/hashrate-sats { proofs, pool_url, target_sats }
    Mint   ──▶ converts proofs → sat-equivalent at current hashprice,
               verifies target_sats ≤ converted amount,
               schedules redemption
    Proxy  ──▶ routes hashrate, accumulates ∫ hashprice·hashrate dt
    Mint   ──▶ marks redemption complete when integral ≥ target_sats
```

### 5.3 Lightning melt (escape hatch)

Standard NUT-05 BOLT11 melt, denominated by hashprice at melt time.
Lets holders bail out to sats without ever redeeming hashrate. This is
important: it makes `THH` tokens *fungible with sats* and lets the
secondary market price them.

## 6. Trust model

This is **a custodial mint** like every Cashu mint today. Trust
assumptions:

1. **Buyer trusts mint to redeem** — same as any Cashu mint.
2. **Buyer trusts operator to actually route hashrate** during
   redemption (rather than fabricating shares or routing to a sham
   pool). Mitigation: nostr-signed share receipts (§4.7) the redeemer
   can verify against the upstream pool's reported share submissions.
3. **All parties trust the hashprice oracle.** Mitigation: include
   oracle samples in nostr completion certificates; allow
   operator-pluggable oracle so disputes don't require trusting Luxor
   specifically.

The operator's relationship with whoever provides the underlying
hashrate (NiceHash account, co-op partners, owned ASICs) is **outside
this trust model** — Hashu does not mediate it.

Hashu is **not trustless**. It's a Chaumian mint with extra plumbing.
The right comparison is Fedimint/Cashu mints, not a DEX.

## 7. Cashu NUT extensions

Two custom melt methods:

| Method            | Purpose                                       | Spec status |
| ----------------- | --------------------------------------------- | ----------- |
| `hashrate`        | Redeem `THH` for routed hashrate-time.        | Hashu-local; propose as NUT extension once stable. |
| `hashrate-sats`   | Redeem `THH` for routed-until-N-sats.         | Same.       |

Both follow NUT-05's quote/melt request structure but replace the
BOLT11 invoice field with a `{pool_url, worker_name, target}` payload
and replace settlement-by-payment with settlement-by-share-accumulation.

`THH` as a custom unit needs no new NUT — Cashu already supports
arbitrary `unit` strings in NUT-04/05; clients just need to display it.

## 8. Open questions

Resolved 2026-05-07:
- ~~Q1 — sats-equivalent semantics:~~ **(a)** redeemer's chosen pool;
  Hashu does not run a pool.
- ~~Q2 — idle-time hashrate destination:~~ **operator's configured
  default pool**.
- ~~Q3 — redirect compensation:~~ **out of scope.** Hashu is
  self-hosted; the operator's relationship with their hashrate source
  is managed outside Hashu.
- ~~Q4 — redemption SLA:~~ on proxy outage or unfillable redemption,
  the unredeemed portion of `THH` is **re-issued as fresh blinded
  signatures back to the holder**. Holder can re-redeem, hold, or melt
  to BOLT11.
- ~~Q7 — miner registration UX:~~ **out of scope** (no third-party
  miners connect to a stranger's Hashu under this model).

Open:
- **Q5** — Minimum redemption granularity. Below ~1 Th-minute, share
  variance dominates the integral; refuse tiny redemptions or require
  a minimum redemption duration.
- **Q6** — Pool URL allowlist vs. open routing. Open is more
  censorship-resistant but exposes operators to pools that blacklist
  proxies. Likely answer: open by default, operator-pluggable denylist.

## 9. Phased roadmap

**Phase 0 — Skeleton**
- Rust workspace: `hashu-mint`, `hashu-proxy`, `hashu-core`, `hashu-cli`.
- Pull `cdk` as dep, stub `THH` unit.
- Stub Stratum V1 listener that just echoes shares to a hardcoded pool.

**Phase 1 — Th-hour MVP**
- `hashrate` melt method, real share-counted redemption.
- Luxor oracle adapter. (or point to esplora instance/node)
- Nostr operator manifest published at startup; re-published on
  keyset rotation.
- Per-share nostr receipts and end-of-redemption completion
  certificate.
- Single-miner, single-redemption happy path end-to-end.

**Phase 2 — Concurrent redemptions + UX**
- Multiple concurrent redemptions, proportional routing.
- Web UI for buyers (mint, view balance, redeem).
- Operator dashboard (default pool config, active redemptions, hashrate
  inflow, mint balance).

**Phase 3 — Sats-equivalent + secondary market**
- `hashrate-sats` melt method.
- Lightning melt for bailout (NUT-05 standard).
- Public token spec so other wallets can hold `THH`.

**Phase 4 — Stratum V2**
- Migrate proxy to SRI; expose V2 endpoint alongside V1.
- Job declaration mode for finer routing control.



