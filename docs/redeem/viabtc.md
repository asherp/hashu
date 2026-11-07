# Redeem at ViaBTC

[ViaBTC](https://www.viabtc.com) needs an account to receive payouts.
Create one and a sub-account for mining.

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://btc.viabtc.io:3333` (alt `btc.viabtc.com:3333`) |
| Worker name | `<your sub-account>.hashu` |
| Worker password | `x` **(unverified: ViaBTC's docs suggest `123`; any value is believed to work)** |

ViaBTC also lists ports 25 and 443 **(unverified)**. Use a `stratum+tcp://`
URL; its SSL endpoints won't route through Hashu.

## Check it arrived

In the ViaBTC web app, open Pool → Workers for the sub-account
**(unverified menu path)**. ViaBTC accepts unregistered names, so a typo in
the account name means the work goes nowhere you can claim. Double-check
it before redeeming.

## Payouts

ViaBTC pays PPS+ or PPLNS, chosen per sub-account, with a minimum payout
before withdrawal **(unverified: current minimum)**. Small redemptions will
sit in the balance until it crosses that.
