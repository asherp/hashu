# Point a Bitaxe at Hashu

Any Bitaxe (or NerdQaxe, or other AxeOS / ESP-Miner device) can mine
through Hashu with one fixed endpoint. You keep mining for yourself on
your usual pool; while someone redeems TiH, the proxy borrows your
hashrate for them and then sends it straight back.

## AxeOS settings

| Field | Value |
|---|---|
| Stratum URL | `mint.hashu.io` |
| Port | `3335` |
| User | `<your BTC address>.<device name>` (e.g. `bc1q….bitaxe1`) |
| Password | `x` |

That's it. With a Bitcoin address as the user, your hashrate goes to
**solo.ckpool.org** under your own address while nobody is redeeming, the
same as pointing the Bitaxe at solo.ckpool.org directly.

### Another home pool

Name it in the password with `pool=`:

| Home pool | Password |
|---|---|
| Public Pool | `pool=public-pool.io:21496` |
| CKPool solo, EU | `pool=eusolo.ckpool.org:3333` |
| OCEAN | `pool=mine.ocean.xyz:3334` |
| Braiins (your account as the user) | `pool=stratum.braiins.com:3333` |

Other password options still work and are passed to your pool, e.g.
`d=512,pool=public-pool.io:21496`. The proxy only dials pools on its
allow-list (`HASHU_HOME_POOLS`); any other `pool=` is ignored and logged.

A user that isn't a Bitcoin address and has no `pool=` has no home pool.
On `mint.hashu.io` (where `HASHU_OPERATOR_WORKER` is set) it is refused at
authorize with "no home pool: use a Bitcoin address as the user, or add
pool=host:port to the password". On a proxy without that setting it goes
to the operator's default pool, as before.

## What happens to your hashrate

1. **Idle.** Your shares go to your home pool, under your user. Anything
   you find there is yours.
2. **Borrowed** (only on a proxy without `HASHU_OPERATOR_WORKER`, or
   once you have your own mint). When a TiH redemption needs hashrate, the proxy moves your
   connection to the redeemer's pool without reconnecting the Bitaxe
   (same socket, a new clean job). Your shares there count towards the
   redemption, and the work is tallied under your address.
3. **Back home.** When the redemption fills (or the redeemer's pool is
   unreachable), you go straight back to your home pool.

Small miners get small orders first (size matching, PR #60), so a 1 TH/s
Bitaxe is not parked on a 3,000,000 TiH order.

## Checking your contribution

The operator sees `GET https://mint.hashu.io/operator/suppliers` (behind
the operator password; not on the public `/control/status`): per address,
the live connections, hashrate, home pool and `delivered_tih` (work
delivered to redemptions since the proxy started).

## For operators

| Variable | Default | Meaning |
|---|---|---|
| `HASHU_HOME_SOLO_UPSTREAM` | `solo.ckpool.org:3333` | Solo pool for address users; `off` turns address detection off |
| `HASHU_HOME_POOLS` | ckpool solo (3 regions), public-pool.io, Braiins, OCEAN | Hosts allowed in `pool=` |
| `HASHU_HOME_POOLS_OFF` | `false` | `true` restores the old behaviour: everyone idles on `HASHU_UPSTREAM` |
| `HASHU_OPERATOR_WORKER` | unset | Hashu's own worker account (your Braiins user; MRR rentals mine as `<user>.<rig>`). Set: only those rigs are borrowed for this mint's redemptions, other miners stay on their home pool, and users with no home pool are refused. Unset: any rig may be borrowed |

On `mint.hashu.io`, which is reserved for Hashu's own hashrate, set
`HASHU_OPERATOR_WORKER`: a Bitaxe then mines to its home pool through the
proxy and is not borrowed until it has its own mint
(`<name>.mint.hashu.io`, see below).

With home pools on, a new connection waits up to 2 s for the miner's
`mining.authorize` before the proxy dials a pool. ESP-Miner sends
configure, subscribe and authorize back to back, so this is one round
trip in practice. Firmware that waits for the subscribe answer first is
placed on the default pool after 2 s (which may refuse an address user);
its home pool is used from its next redemption on.

Not done yet: paying suppliers for borrowed work. `delivered_tih` is the
number a payout would be based on.

## Registered suppliers (your own mint)

A registered supplier `alice` has their own mint at
`alice.<mint host>` (e.g. `alice.mint.hashu.io`), with its own keys, served by the same process as the
operator's mint. Alice proves each rig belongs to her with a secret:

| AxeOS field | Value |
|---|---|
| User | `<name>.<rig>`, e.g. `alice.rig1` |
| Password | `pw=<your secret>,pool=public-pool.io:21496` |

Orders bought from Alice's mint are filled only by rigs that log in as
`alice.*` with the right `pw=`. Each rig serves one order at a time, and
spare rigs join an order already being filled, so a large order on one
invoice spreads over all of Alice's free rigs. `pw=` is never sent to any
pool. A registered name with a wrong or missing secret is refused, and so
is a supplier rig with no home pool.

### Sign up

Open `https://mint.hashu.io/join/`, pick a name, and enter your Lightning
address, your ask (msat per PiH, i.e. per 1,024 TiH; about 625 is 2 sats per
TH-hour, and the page shows the sats per TH/s per day equivalent) and your horizon. The page checks the
address can be invoiced and supports payment verification (LUD-21
`verify`; not every provider has it, and the page says so if yours
doesn't), then shows your
worker secret once and the AxeOS settings above. Your mint is live at
`https://<name>.mint.hashu.io` immediately.

### Selling

- Buyers mint TiH from your mint over Lightning with any Cashu wallet.
  Each sale is an invoice from **your** wallet (LNURL-pay), and your mint
  issues TiH once your wallet's `verify` URL reports it paid. Hashu never
  holds the payment.
- What's for sale: `measured hashrate of your verified rigs × horizon −
  TiH your mint already owes`, in TiH (a 1.2 TH/s Bitaxe over 24 h is
  ≈ 94,000 TiH). The smallest sale is 4,096 TiH, so a Bitaxe needs a
  horizon of about 3 hours or more to sell anything (in circulation plus open redemptions). The
  horizon is yours to choose, 1 hour or more (default 24). With no rigs
  online your mint is sold out.
- Melting TiH on your mint (method `hashrate`) opens a redemption only your
  rigs fill.
- Your mint's page (`/`) shows ask, TiH for sale and TH/s online; open
  **Supplier settings** with your worker secret to change ask, horizon or
  Lightning address (`GET`/`POST /api/settings`, header `X-Hashu-Secret`).

Sign-up is limited to 10 new names per hour per proxy. A buy and redeem
page on supplier mints and NWC as a second payment option come next.

Suppliers are kept in a JSON file (`HASHU_SUPPLIERS_FILE`), written by
sign-up and re-read when it changes (operators can also edit it by hand);
only the SHA-256 of each secret is stored. Without the setting, sign-up is
closed:

```json
{ "suppliers": { "alice": { "secret_sha256": "<sha256 hex of the secret>" } } }
```

`printf %s "$SECRET" | sha256sum` gives the hash. A supplier's mint lives
in `<data-dir>/mints/<name>/` (seed and database; back up the seed) and is
started on the next request after the name is added. It needs
`HASHU_MINT_URL` (supplier mints are named under its host) and a wildcard
DNS record `*.<mint host>`. The bundled Caddy gets each supplier's
certificate on first visit, asking the mint's `GET /tls-ask?domain=` first,
which answers 200 only for registered names. Names are 3 to 32
lowercase letters, digits and hyphens. For testing, `POST /redemptions` on
the control API (with `--control-allow-open`) takes `"supplier": "alice"`.
