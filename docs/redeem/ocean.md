# Redeem at OCEAN

[OCEAN](https://ocean.xyz) is non-custodial: no account, your username is a
Bitcoin address, and payouts go straight to that address (or over
Lightning).

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://mine.ocean.xyz:3334` |
| Worker name | `<your BTC address>.hashu` (the `.hashu` suffix is any label you like) |
| Worker password | leave blank (`x`) |

Port 3334 is the one that answered in D-Central's July 2026 probe; 3333 did
not.

## Check it arrived

Open `https://ocean.xyz/stats/<your BTC address>` **(unverified URL
pattern)**. The worker shows under your address once OCEAN has seen a few
shares. OCEAN pays on a share window (TIDES), so earnings appear gradually
over several blocks, not when the redemption ends.

## Payouts

Payouts go to your address in the coinbase once you cross OCEAN's
threshold, or over Lightning if you set a BOLT12 offer on your account page
**(unverified: threshold amount and Lightning setup steps)**. For a small
redemption, Lightning is the way to actually receive it.
