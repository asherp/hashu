# Redeeming THH at your pool

A Hashu token holds THH (terahash-hours; 1 THH is 1 TH/s for one hour).
Redeeming it points the mint's miners at **your** pool, under **your**
worker name, until that much work has been delivered. Your pool then pays
you for the work the way it pays any miner.

This page covers what every pool has in common. Pick your pool below for
the exact values to type in.

| Pool | Account needed | Username you give Hashu | Guide |
|---|---|---|---|
| OCEAN | No | BTC address (+ `.worker`) | [ocean.md](ocean.md) |
| Braiins Pool | Yes | `account.worker` | [braiins.md](braiins.md) |
| ViaBTC | Yes, for payouts | `account.worker` | [viabtc.md](viabtc.md) |
| F2Pool | Yes | `account.worker` | [f2pool.md](f2pool.md) |
| Foundry USA | Yes, institutional | `account.worker` | [foundry.md](foundry.md) |
| solo.ckpool.org | No | BTC address (+ `.worker`) | [ckpool-solo.md](ckpool-solo.md) |
| public-pool.io | No | BTC address (+ `.worker`) | [public-pool.md](public-pool.md) |
| Hashu Testnet4 ckpool | No | any label | [testnet4.md](testnet4.md) |

## What you supply

The checkout page (`/checkout/` on the mint, for example
<https://mint.hashu.io/checkout/>) asks for four things. A Cashu wallet that
melts with the `hashrate` method sends the same fields as JSON:
`{"pool_url", "worker_name", "worker_password"?, "amount_thh"}`.

| Field | What Hashu accepts | Notes |
|---|---|---|
| Token | a `cashuB…` token, pasted or scanned | Links like `cashu:…` work too. |
| Pool URL | `stratum+tcp://host:port` | **Plain TCP only, with a port.** See the warning below. |
| Worker name | any non-empty string | Sent to your pool in `mining.authorize` in place of the miner's own name. This is how the pool knows the work is yours. |
| Worker password | optional, defaults to `x` | Only matters if your pool checks it. |

> **Use a `stratum+tcp://` URL.** The mint accepts `stratum+ssl://` in a
> request, but the proxy cannot dial TLS stratum yet, so a TLS redemption
> is skipped and never fills. A URL without a port (or with a path) is
> also unroutable. Every guide below lists a plain-TCP endpoint.

The redemption must be at least the mint's minimum (`min_redeem_thh`, shown
by `GET /checkout/api/info`; 1 THH by default).

## What happens next

1. The mint burns your token and opens a redemption. If another redemption
   is running, yours queues behind it (oldest first).
2. The proxy disconnects its miners; when they reconnect, it sends them to
   your pool with your worker name.
3. Each share **your pool accepts** counts its difficulty × 2^32 hashes
   toward your THH. Shares your pool rejects count for nothing.
4. When the THH is delivered, miners go back to the operator's pool and the
   redemption shows as paid.

If your pool can't be reached, the proxy sends the miners back to the
default pool for 60 s and then tries yours again. Nothing is credited
while they are away.

## How to check the hashrate arrived

- **On the checkout page.** After you redeem, the "Hashrate delivered to
  your pool" chart shows THH delivered against the target, with an ETA.
  The same data is at `GET /checkout/api/redemption/{id}`.
- **On your pool.** Look for the worker name you entered. Pool dashboards
  average over 5 to 60 minutes, so expect the worker to appear a few
  minutes after the chart starts moving, and the hashrate to look low at
  first.
- **Proof of work, independently.** The mint operator can serve a Merkle
  root and per-share inclusion proofs for your redemption
  (`/redemptions/{id}/root`, `/redemptions/{id}/proof/{n}`); see the
  README's "Share audit" section.

## How much will I earn?

Whatever your pool pays for that much work, minus its fee. At current
hashprice a single THH is worth only a few sats, so mind your pool's
**minimum payout**: small redemptions may sit in your pool balance until it
crosses the threshold. Pools with Lightning payouts (OCEAN, Braiins) suit
small amounts best. Solo pools pay only if a block is found, which for a
few THH almost never happens.

## About the pool details in these guides

Hashu's own behaviour (fields, TCP-only, crediting) is from the code. Pool
endpoints, username rules and payout terms come from the pools' docs and a
third-party endpoint survey (D-Central, probed July 2026), and pools change
them. Anything we could not confirm is marked **(unverified)**. Check your
pool's own setup page before redeeming a large amount.
