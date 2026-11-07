# Redeem at the Hashu Testnet4 ckpool

The Hashu team ran a Testnet4 ckpool for the bitcoin++ Seoul demo. Testnet
coins have no value; use it to try a redemption end to end.

| Field | Value |
|---|---|
| Pool URL | `stratum+tcp://216.230.225.61:3333` |
| Worker name | any label, e.g. `yourname.worker1` |
| Worker password | `x` |

## Status

**Down as of 2026-11-07.** The machine that hosted it was shut down when
the mint moved to mint.hashu.io. This page stays as a template for running
your own Testnet4 pool.

## Known issue: rented rigs

The mint's demo hashrate comes from MiningRigRentals rigs. MRR's relay
refuses jobs from this ckpool because Testnet4 block templates carry an
`ntime` 20 to 70 minutes in the future, so a redemption here may not fill
with rented hashrate. A miner you plug into the proxy directly (a Bitaxe,
say) is not affected. This is why the live demo redeems to mainnet pools.

## Try it from the command line

```bash
cargo run -p hashu --example buy_and_redeem -- \
  http://127.0.0.1:8085 bolt11 2 stratum+tcp://216.230.225.61:3333 yourname.worker1
```
