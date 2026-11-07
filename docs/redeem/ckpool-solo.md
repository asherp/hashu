# Redeem at solo.ckpool.org

[solo.ckpool.org](https://solo.ckpool.org) needs no account. Your username
is your Bitcoin address. If the work finds a block, you get the block
reward (minus a 2% fee) paid straight to that address in the coinbase.

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://solo.ckpool.org:3333` (EU: `stratum+tcp://eusolo.ckpool.org:3333`; also port 4334) |
| Worker name | `<your BTC address>.hashu` (the suffix is optional) |
| Worker password | anything (`x`) |

Source: Bitcoin Wiki, "Solo.ckpool": "Set your username to your btcaddress
with any or even no worker extension".

## Check it arrived

Open `https://solo.ckpool.org/users/<your BTC address>` **(unverified URL
pattern)** to see your hashrate, shares and best share.

## What you're buying

This is a lottery ticket. A few THH has a vanishingly small chance of
finding a mainnet block, and if it doesn't, you get nothing back. The
expected value is the same as a pool, the variance is enormous. Pick a
shared pool if you want a reliable payout.
