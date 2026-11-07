# Redeem at F2Pool

[F2Pool](https://www.f2pool.com) needs an account.

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://btc.f2pool.com:1314` (also `:3333`, `:25`; regional e.g. `btc-na.f2pool.com:1314`) |
| Worker name | `<your account>.hashu` |
| Worker password | `x` **(unverified: any value is believed to work)** |

Source: F2Pool help center, "BTC mining guide". Its SSL endpoint
`stratum+ssl://btcssl.f2pool.com:1300` should also work over TLS **(unverified:
whether its certificate chains to a public CA)**.

## Check it arrived

On F2Pool's dashboard (or mobile app), search your account name: the
`hashu` worker shows in the worker list **(unverified UI details)**.
Unregistered account names are accepted at connect time, so check the
spelling.

## Payouts

F2Pool pays PPS+ daily once your balance passes its minimum payout
**(unverified: current minimum)**.
