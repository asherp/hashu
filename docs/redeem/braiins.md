# Redeem at Braiins Pool

[Braiins Pool](https://braiins.com/pool) needs an account. Sign up first and
note your username.

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://stratum.braiins.com:3333` (backup `:443`, also plain TCP) |
| Worker name | `<your username>.hashu` |
| Worker password | anything, or blank |

Source: Braiins Academy, "BTC mining setup". Braiins also runs Stratum V2
(`stratum2+tcp://…`); Hashu's proxy speaks V1 only, so use the URL above.

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
