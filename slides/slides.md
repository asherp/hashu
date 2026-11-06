---
theme: default
title: Hashu — ecash for computation, not money
info: |
  Hashu: a hashrate proxy fused with a Cashu (CDK) mint.
  Five-slide overview of the Testnet4 prototype.
class: text-left
drawings:
  persist: false
transition: slide-left
mdc: true
---

# Hashu

### An ecash mint that sells computation, not money

<div class="mt-6 grid grid-cols-2 gap-10 text-left text-sm">
<div>

**The problem**

- A Cashu mint holds bitcoin and issues IOUs redeemable for bitcoin
- Accepting value and paying it back out to someone else looks like **money transmission**
- So operators face licensing and KYC risk, or stay small and hidden

</div>
<div>

**Hashu's answer**

- Hashu tokens (**THH**) are backed **only by miners' hashrate**, not by bitcoin
- **1 THH = 1 TH/s × 3,600 s** of hashing pointed where the holder chooses
- Redeeming a token returns **work**, never coins
- Running a Hashu mint means **selling computation**, not custodying or sending bitcoin
- **Hashrate at Home:** a home heater can sell its hashrate as THH (Tether MDK)

</div>
</div>

<div class="abs-bl m-6 text-sm opacity-60">hashu.io · built on CDK · Rust · MIT / Apache-2.0</div>

<!--
Framing: most Cashu mints custody BTC and pay it out on request, which is the
shape regulators call money transmission. Hashu changes what backs the token.
A THH token is a claim on hashing work from the operator's miners. The operator
never holds or sends the holder's bitcoin; it sells time on its machines, like
a cloud provider sells CPU-hours.
(This is our positioning, not a legal opinion.)
-->

---

# Dollars → hashrate → bitcoin, no exchange

```mermaid {scale: 0.55}
%%{init: {'sequence': {'mirrorActors': false, 'messageMargin': 22}}}%%
sequenceDiagram
  participant B as Buyer
  participant M as Hashu mint (CDK, unit THH)
  participant P as Stratum proxy
  participant L as Buyer's pool
  B->>M: pay in Tether (USDT) or Lightning
  M-->>B: blinded THH ecash, priced by hashprice oracle
  Note over B: hold or transfer like any Cashu token
  B->>M: redeem THH {pool_url, worker}
  M->>P: schedule redemption
  P->>L: redirect miners' hashrate to the buyer's pool
  L-->>B: pool pays the buyer newly mined BTC
  P-->>M: accepted shares credited + Merkle-committed
```

<div class="grid grid-cols-3 gap-6 text-xs mt-1">
<div>

**In: dollars** · pay with USDT; price locks in today's hashprice:
`N · hashprice · 1h + fee`

</div>
<div>

**Middle: computation** · THH is a bearer claim on hashing work, private by Cashu's blind signatures

</div>
<div>

**Out: bitcoin** · the buyer's own pool pays them for the work; the mint never sends BTC

</div>
</div>

<!--
The on-ramp without an exchange: USD stablecoin in, hashrate out, and the
bitcoin comes from mining, paid by the buyer's chosen pool. Hashu never
custodies or transmits the mined sats; it only routes Stratum traffic.
Each accepted share is credited to the redemption and committed to a
per-connection RFC 6962 Merkle tree (nostr receipts are next).
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

- Each accepted share is rebuilt into its 80-byte block header and appended to a per-connection **RFC 6962 Merkle tree**
- Root and leaf count are logged today; next, publish them as signed nostr receipts the holder checks against their pool

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

# From scaffold to demo, in one day

<style>
.journey li { line-height: 1.4; margin-top: 0.15rem; margin-bottom: 0.15rem; }
.journey p { margin-top: 0.2rem; margin-bottom: 0.4rem; }
</style>

<div class="journey grid grid-cols-[1fr_1.6fr_1fr] gap-5 text-sm mt-2">
<div class="p-3 rounded bg-gray-500/10">

**This morning**

- Stratum V1 proxy + share Merkle tree
- Hashprice oracles (Esplora, Luxor)
- Nostr operator manifest, `hashu` CLI
- ❌ No minting, no payments
- ❌ No Testnet4, no Tether
- ❌ No redemption or redirection

</div>
<div>

**What we tackled** (tracking issue #3)

- ✅ #2 &nbsp;`hashrate` melt method + redemption ledger (CDK)
- ✅ #4 &nbsp;Dropped the Lightning melt: a Hashu mint never pays out BTC
- ✅ #5 &nbsp;Testnet4 + MiningRigRentals hashprice oracle
- ✅ #6 &nbsp;Lightning rail: buy THH with bolt11
- ✅ #8 &nbsp;Tether rail: buy THH with USDT
- ✅ #7 &nbsp;Proxy credits shares to the redemption
- ✅ #9 &nbsp;Redirect hashrate to the redeemer's pool
- ✅ #12 &nbsp;Hashu Heater on Tether MDK (Bitaxe Worker)
- 🚧 #11 &nbsp;Mainnet Lightning via phoenixd
- 🚧 #13 &nbsp;`hashu mint run`: mint + rails + proxy, one process

</div>
<div class="p-3 rounded bg-green-500/10">

**The demo**

1. Pay USDT or Lightning
2. Mint THH, blind-signed
3. Melt `hashrate` to your pool
4. MDK app moves the heater to the Hashu proxy
5. Shares land on the Testnet4 pool; heater goes home when filled

</div>
</div>

<!--
Walk the arc: this morning the repo was plumbing only (proxy, oracles, manifest).
Every row in the middle is a merged or in-review PR toward the demo milestone (#3).
#4 matters for the pitch: we removed the cash-out path so the mint only ever
receives payment and delivers work, which is the "selling computation" argument.
#12 is the Hashrate at Home angle: a home heater sells its hashrate as THH, and the
MDK app switches it to the redeemer's pool only while a redemption is open.
Still open: phoenixd mainnet (#11), the one-process daemon (#13), a real Bitaxe run.
-->
