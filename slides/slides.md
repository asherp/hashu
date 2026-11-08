---
theme: default
colorSchema: dark
fonts:
  sans: 'system-ui, -apple-system, "Segoe UI"'
  mono: 'ui-monospace, SFMono-Regular, Menlo'
  provider: none
title: Hashu — Layer-0 privacy through Chaumian hashrate vouchers
info: |
  Hashu: a hashrate proxy fused with a Cashu (CDK) mint.
  Seven-slide overview of the live mainnet demo at mint.hashu.io.
layout: cover
class: hashu-title
drawings:
  persist: false
transition: slide-left
mdc: true
---

<div class="flex items-center gap-3 text-2xl font-bold"><svg viewBox="0 0 32 32" style="width:44px;height:44px" aria-hidden="true"><rect width="32" height="32" rx="7" fill="#f7931a"/><path d="M10 7v18M22 7v18M10 16h12" stroke="#111" stroke-width="3.4" stroke-linecap="round"/></svg> Hashu</div>

<h1 class="hashu-h1">Chaumian vouchers for <span>hashrate</span>.</h1>

<p class="hashu-sub">Layer-0 privacy: swap any payment for freshly mined coins</p>

<div class="abs-bl m-8 text-sm hashu-foot">mint.hashu.io · github.com/asherp/hashu · built on CDK + Tether MDK</div>

<style>
.slidev-layout.hashu-title { padding: 3.5rem 4rem; }
.hashu-h1 { font-size: 5.2rem !important; line-height: 1.05 !important; font-weight: 800; letter-spacing: -0.02em; margin: 2.5rem 0 1.2rem !important; color: #f1efea; }
.hashu-h1 span { color: #f7931a; }
.hashu-sub { font-size: 1.6rem; color: #b5b2ab; margin: 0 !important; opacity: 1 !important; }
.hashu-foot { color: #85827c; }
</style>

<!--
Chaumian hashrate vouchers: blind-signed tokens for hashing time, not money and
not backed by bitcoin. Open with the privacy hook,
then the next slide explains why fresh coins matter.
-->

---
class: text-left
---


# Layer-0 privacy

### Swap any payment for freshly mined coins

<div class="mt-6 grid grid-cols-2 gap-10 text-left text-sm">
<div>

**The problem**

- Exchange data leaks tie bitcoin holders to names and addresses, which makes them **targets**
- Every coin you buy carries its **history**, and KYC'd coins carry your identity
- Cashu mints that hold and pay out bitcoin operate as **money service businesses**, which exposes their operators to jurisdictional risk

</div>
<div>

**Hashu's answer**

- **Mining is Layer-0 privacy**: a freshly mined coin has no history at all
- Miners run a self-hosted proxy that mints **hashrate-backed Cashu tokens (THH)**, 1 THH = 1 TH/s × 1 h
- Pay with **USDT or Lightning**, even with KYC'd sats. Redeem THH and the hashrate is pointed at **your** pool, which pays you freshly mined coins
- No exchange, and the mint never touches your bitcoin: it **sells computation**, not money

</div>
</div>

<div class="abs-bl m-6 text-sm opacity-60">hashu.io · built on the Cashu Development Kit and Tether's Mining Development Kit</div>

<!--
Privacy first: coins bought on an exchange carry a KYC trail, and exchange data
leaks have turned that trail into a physical-security problem for holders.
Mining is the one way to get bitcoin with no prior history ("Layer 0").
Hashu lets anyone get there: pay any way you like (USDT, Lightning, even KYC'd
sats), receive THH vouchers (blind-signed, so the mint cannot link buyer to redeemer),
and redeem it as hashrate pointed at your own pool. The pool pays you new coins.
The mint only ever sells hashing time, so it is not custodying or transmitting
bitcoin. (Our positioning, not a legal opinion. Your pool account is still your
pool's business: pick one whose payout policy fits your privacy needs.)
-->

---

# Pay → hashrate → bitcoin, no exchange

```mermaid {scale: 0.55}
%%{init: {'sequence': {'mirrorActors': false, 'messageMargin': 22}}}%%
sequenceDiagram
  participant B as Buyer
  participant M as Hashu mint (CDK, unit THH)
  participant P as Stratum proxy
  participant L as Buyer's pool
  B->>M: pay with Lightning (or USDT)
  M-->>B: blind-signed THH vouchers, priced at the MiningRigRentals market
  Note over B: hold in Cashu.me, pass on as a QR code
  B->>M: redeem THH {pool, worker, password}
  M->>P: queue redemption (first come, first served)
  P->>L: redirect rented hashrate to the buyer's pool
  L-->>B: pool pays the buyer newly mined BTC
  P-->>M: accepted shares credited + Merkle-committed
```

<div class="grid grid-cols-3 gap-6 text-xs mt-1">
<div>

**In: sats or dollars** · mainnet Lightning via phoenixd, tested live. The USDT rail is built but not yet tested live

</div>
<div>

**Middle: computation** · THH is a bearer Cashu token for hashing work, private by blind signatures. Sales are capped at rented capacity

</div>
<div>

**Out: bitcoin** · the buyer's own pool pays them for the work; the mint never sends BTC

</div>
</div>

<!--
The on-ramp without an exchange: USD stablecoin in, hashrate out, and the
bitcoin comes from mining, paid by the buyer's chosen pool. Hashu never
custodies or transmits the mined sats; it only routes Stratum traffic.
Live: Lightning buys on mainnet (small buys held as phoenixd fee credit; the
mint absorbs the LSP fee). USDT code exists but has not been run live.
Each accepted share is credited to the redemption and committed to a
per-redemption RFC 6962 Merkle tree (signed nostr receipts are roadmap).
-->

---

# Share accounting: how work is counted

<div class="grid grid-cols-2 gap-8 text-sm mt-2">
<div>

**Exact integer math, in hashes**

```text
1 THH    = 1 TH/s × 3,600 s = 3.6 × 10¹⁵ hashes
1 share  = difficulty × 2³² hashes
melt N   → target = N × 3.6 × 10¹⁵ hashes
```

<div class="text-xs opacity-80 mt-1">

At difficulty 65,536, 1 THH ≈ 12.8 shares. A 40 TH/s heater fills 1 THH in 90 s.

</div>

**What counts**

- Only shares the **redeemer's pool accepts**; rejects earn nothing
- Shares sent to the operator's default pool earn nothing
- The redemption is fixed when `mining.submit` goes out, so a late ack after a pool switch still credits the right holder
- Shares in flight after the target is hit are ignored

</div>
<div>

**Lifecycle of one redemption**

<div class="flex items-center gap-2 text-xs my-3 flex-wrap">
<span class="px-2 py-1 rounded bg-gray-500/15">melt <code>hashrate</code></span>→
<span class="px-2 py-1 rounded bg-gray-500/15">Pending</span>→
<span class="px-2 py-1 rounded bg-blue-500/15">Active: + diff × 2³² per share</span>→
<span class="px-2 py-1 rounded bg-green-500/15">Completed: hashes ≥ target</span>
</div>

Completion is broadcast once: the router sends hashrate back to the default pool and the melt quote flips to **PAID**.


**Proof of work routed**

- Each accepted share is rebuilt into its 80-byte block header and appended to a per-redemption **RFC 6962 Merkle tree**
- The audit API serves the root and an inclusion proof per share: `/redemptions/{id}/root`, `/proof/{n}`
- Redemptions persist across restarts (journaled ledger + share trees)
- Roadmap: publish roots as signed nostr receipts

</div>
</div>

<!--
The ledger never uses floating-point THH: progress is an integer count of
expected hashes, so "done" is decided without rounding drift.
Credit = difficulty × 2^32 per share the redeemer's pool accepted (#7).
The routing controller tells the proxy which redemption a connection serves (#9);
that binding is sampled at submit time, so acks that arrive after a switch still
credit the right redemption. When the total crosses the target, the crediter
broadcasts completion once: the router moves hashrate back to the default pool and
the mint's hashrate melt quote flips to PAID (#13; the redemption id is derived
from the melt request, so cdk can look it up and nobody pays twice).
-->

---

# Hashrate at Home, on Tether MDK

<div class="grid grid-cols-[1.05fr_1fr] gap-6 text-sm mt-1">
<div>

**A home heater that sells its hashrate as THH**

1. Someone redeems THH; the proxy opens a redemption
2. The **Hashu Heater app** sees it on the proxy's control API and sends `setupPools` through the MDK Kernel: home pool → Hashu proxy
3. Shares go to the redeemer's pool and are credited
4. Filled → `setupPools` back to the home pool

<div class="text-xs mt-3 leading-snug">

- **Heater Worker** speaks the Bitaxe AxeOS API: hashrate, power, temperature, pool, heat level (`setPowerMode`)
- **Thermostat** with a hard ceiling: pauses even mid-redemption, then resumes
- **Fail-safe:** proxy unreachable → heater goes back home
- Heatbit firmware is closed, so it stays pointed at the proxy

</div>
</div>
<div>

<img src="/mdk-heater.png" class="rounded shadow" />

<div class="text-xs mt-2 leading-snug">

**~62 sats per kWh of heat** at demo pricing (32.4 J/TH), against 0 for an electric heater with the same output.

Demo with no hardware: `cd mdk && ./scripts/demo.sh`

</div>
</div>
</div>

<!--
Built for the bitcoin++ Seoul Hashrate at Home award, which requires Tether's MDK (#12).
Every watt a miner draws ends up as heat, just like an electric heater; this one also
earns by selling THH. The mock heater is 40 TH/s, 1300 W and really mines over Stratum,
so a 1 THH redemption fills through the proxy in about 90 s with no hardware.
The app's policy is pure and unit tested: redemption open → Hashu proxy, otherwise home;
thermostat with hysteresis plus a ceiling; manual override per heater.
-->

---

# What works today

<div class="grid grid-cols-2 gap-8 text-sm mt-2">
<div>

**Live on mainnet at mint.hashu.io**

- ✅ CDK mint, unit **THH**, works in the **Cashu.me** wallet
- ✅ Buy with **mainnet Lightning** (phoenixd), tested end to end; the mint absorbs LSP fees on small buys
- ✅ Tokens move as QR codes, animated for long tokens
- ✅ Redeem to **any pool + worker**, optional worker password. Redeeming live to our Braiins account now
- ✅ Hashrate from **MiningRigRentals**: 413 TH/s, 70/70 shares accepted at Braiins
- ✅ Sales capped at rented capacity; redemptions first come, first served
- ✅ Redemptions survive restarts; per-redemption Merkle root + inclusion proofs
- ✅ Operator page: hashrate graphs, test mint, redeem. Checkout shows delivery time

</div>
<div>

**Hashrate at Home**

- ✅ Hashu Heater app on Tether MDK switches the heater's pool on redeem and back when filled (mock 40 TH/s heater mines for real)

**Built, not yet proven live**

- 💵 USDT (Ethereum) and USDC (Base) rails
- 🧪 A physical heater run (Bitaxe / Heatbit)

**Roadmap**

- 🔜 Nostr-signed Merkle roots and completion certificates
- 🔜 More hashrate sources than one rental market

</div>
</div>

<!--
Everything in the left column runs on the devbox behind mint.hashu.io right now.
The rigs are rented on MiningRigRentals and pointed at the Hashu proxy; when nobody
is redeeming they mine on Braiins. We moved the demo from Testnet4 to mainnet
because MRR's relay rejects Testnet4 jobs (future-stamped ntime).
Be explicit about the right column: USDT code exists and is unit tested but has not
taken a live payment; nostr attestation (attestation.rs) is not built.
-->

---

# Live demo

<div class="grid grid-cols-2 gap-10 text-sm mt-4">
<div>

**Try it**

- Buy THH: **[mint.hashu.io/checkout/](https://mint.hashu.io/checkout/)**
- Mint URL for Cashu.me: `https://mint.hashu.io`
- Code: **[github.com/asherp/hashu](https://github.com/asherp/hashu)** (Rust, MIT / Apache-2.0)
- These slides: hashu.io/slides

</div>
<div>

**What we'll show**

1. Pay a Lightning invoice, get a THH token as a QR
2. Scan it into Cashu.me
3. Redeem to a Braiins worker
4. Watch the rented hashrate move on the operator graph
5. Shares land at the pool; the redemption fills and hashrate goes home

</div>
</div>

<!--
Have the operator page open in a second tab. Keep the buy small so it fills during the talk;
checkout shows the delivery-time estimate before you pay.
-->
