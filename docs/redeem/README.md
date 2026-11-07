# Redeeming THH at your pool

A Hashu token holds THH (terahash-hours; 1 THH is 1 TH/s for one hour).
Redeeming it points the mint's miners at **your** pool, under **your**
worker name, until that much work has been delivered. Your pool then pays
you for the work the way it pays any miner.

This page covers what every pool has in common. Pick your pool below for
the exact values to type in.

| Pool | Account needed | Username you give Hashu |
|---|---|---|
| [OCEAN](ocean.md) | No | BTC address (+ `.worker`) |
| [Braiins Pool](braiins.md) | Yes | `account.worker` |
| [ViaBTC](viabtc.md) | Yes, for payouts | `account.worker` |
| [F2Pool](f2pool.md) | Yes | `account.worker` |
| [Foundry USA](foundry.md) | Yes, institutional | `account.worker` |
| [solo.ckpool.org](ckpool-solo.md) | No | BTC address (+ `.worker`) |
| [public-pool.io](public-pool.md) | No | BTC address (+ `.worker`) |
| [Hashu Testnet4 ckpool](testnet4.md) | No | any label |

## What you supply

The checkout page (`/checkout/` on the mint, for example
<https://mint.hashu.io/checkout/>) asks for four things. A Cashu wallet that
melts with the `hashrate` method sends the same fields as JSON:
`{"pool_url", "worker_name", "worker_password"?, "amount_thh"}`.

| Field | What Hashu accepts | Notes |
|---|---|---|
| Token | a `cashuB…` token, pasted or scanned | Links like `cashu:…` work too. |
| Pool URL | `stratum+tcp://host:port` or `stratum+ssl://host:port` | **Must have a port.** See the note below. |
| Worker name | any non-empty string | Sent to your pool in `mining.authorize` in place of the miner's own name. This is how the pool knows the work is yours. |
| Worker password | optional, defaults to `x` | Only matters if your pool checks it. |

> **Plain TCP or TLS, with a port.** The proxy dials `stratum+ssl://` pools
> over TLS and checks the pool's certificate against the public CA roots. A
> pool with a self-signed or expired certificate fails the handshake, and
> your redemption waits on the operator's pool instead of filling, so use
> its `stratum+tcp://` endpoint. The mint refuses a URL without a port, with
> a path, or with another scheme (such as Stratum V2's `stratum2+tcp://`)
> when you ask for a quote, before you pay anything.

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
