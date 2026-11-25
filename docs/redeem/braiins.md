# Redeem at Braiins Pool

[Braiins Pool](https://braiins.com/pool) needs an account. Sign up first and
note your username.

| Field | Value |
|---|---|
| Pool URL | `stratum+ssl://stratum.braiins.com:3443` (TLS; backup `stratum+ssl://stratum.braiins.com:33443`) |
| Worker name | `<your username>.hashu` |
| Worker password | anything, or blank |

Source: Braiins Academy, "Connection security" (the certificate is from a
public CA, so the proxy's certificate check passes). Plain
`stratum+tcp://stratum.braiins.com:3333` still works but is not encrypted:
on 2026-11-25 Braiins [reported attackers hijacking plain Stratum V1
connections](https://braiins.com/blog/bitcoin-hashrate-hijacking-unencrypted-stratum-v1)
to take miners' shares, so prefer TLS.

Stratum V2 also works:
`stratum2+tcp://stratum.braiins.com:3333/9awtMD5KQgvRUh2yFbjVeT7b6hjipWcAsQHd6wEhgtDT9soosna`
(the last part is Braiins' authority key, which pins the pool's identity).
Hashu's Stratum V2 upstream is newer and less tested than TLS.

## Check it arrived

In the Braiins dashboard, open the Mining tab: the `hashu` worker appears
in the worker list and in the "Recent Hash Rate" graph.

## Payouts

Braiins pays FPPS, so the balance grows as shares are accepted. It offers
on-chain and Lightning payouts **(unverified: current minimums)**;
Lightning is better for small redemptions.

## Braiins Solo

To try for a whole block instead, use
`stratum+tcp://solo.stratum.braiins.com:3333` with your BTC address as the
worker name **(unverified: username format, from D-Central's survey)**. See
[ckpool-solo.md](ckpool-solo.md) for why solo is a lottery at TiH scale.
