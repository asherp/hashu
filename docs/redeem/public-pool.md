# Redeem at public-pool.io

[public-pool.io](https://web.public-pool.io) is an open-source solo pool
with no account: your username is your Bitcoin address, and a found block
pays that address.

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://public-pool.io:21496` |
| Worker name | `<your BTC address>.hashu` **(unverified: `.worker` suffix handling)** |
| Worker password | `x` |

Source: D-Central endpoint survey (July 2026).

## Check it arrived

Open `https://web.public-pool.io/#/app/<your BTC address>`
**(unverified URL pattern)**; the `hashu` worker shows with its hashrate
and best difficulty.

Like [solo.ckpool.org](ckpool-solo.md), this is a lottery at THH scale.
