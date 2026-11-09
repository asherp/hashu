# Point a Bitaxe at Hashu

Any Bitaxe (or NerdQaxe, or other AxeOS / ESP-Miner device) can mine
through Hashu with one fixed endpoint. You keep mining for yourself on
your usual pool; while someone redeems THH, the proxy borrows your
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

A user that isn't a Bitcoin address and has no `pool=` goes to the
operator's default pool, as before.

## What happens to your hashrate

1. **Idle.** Your shares go to your home pool, under your user. Anything
   you find there is yours.
2. **Borrowed.** When a THH redemption needs hashrate, the proxy moves your
   connection to the redeemer's pool without reconnecting the Bitaxe
   (same socket, a new clean job). Your shares there count towards the
   redemption, and the work is tallied under your address.
3. **Back home.** When the redemption fills (or the redeemer's pool is
   unreachable), you go straight back to your home pool.

Small miners get small orders first (size matching, PR #60), so a 1 TH/s
Bitaxe is not parked on a 1,000 THH order.

## Checking your contribution

`GET https://mint.hashu.io/control/status` lists `suppliers`: per address, the live connections, hashrate, home pool and
`delivered_thh` (work delivered to redemptions since the proxy started).

## For operators

| Variable | Default | Meaning |
|---|---|---|
| `HASHU_HOME_SOLO_UPSTREAM` | `solo.ckpool.org:3333` | Solo pool for address users; `off` turns address detection off |
| `HASHU_HOME_POOLS` | ckpool solo (3 regions), public-pool.io, Braiins, OCEAN | Hosts allowed in `pool=` |
| `HASHU_HOME_POOLS_OFF` | `false` | `true` restores the old behaviour: everyone idles on `HASHU_UPSTREAM` |

With home pools on, a new connection waits up to 2 s for the miner's
`mining.authorize` before the proxy dials a pool. ESP-Miner sends
configure, subscribe and authorize back to back, so this is one round
trip in practice. Firmware that waits for the subscribe answer first is
placed on the default pool after 2 s (which may refuse an address user);
its home pool is used from its next redemption on.

Not done yet: paying suppliers for borrowed work. `delivered_thh` is the
number a payout would be based on.
