# Mining-pool payout simulation

This SimPy model sends the same accepted-share and pool-block events through
three accounting policies:

- **Streaming realized price:** a pool block creates proportional, per-miner
  entitlement. After 100 confirmations it joins the miner's outstanding BTC
  balance `B` and associated work balance `Q`. The current price is `B / Q`,
  and each tick pays `EWMA_hashrate * (B / Q) * dt` while consuming the same
  amount of work from `Q`.
- **PPS:** every accepted share is paid its expected block value immediately.
- **PPLNS:** an actual block reward is divided across the trailing configurable
  window of work (`N * expected network-block work`) and released at maturity.

The network outside the pool is simulated independently. A pool block is not a
second independent process: it is an accepted pool share that also passes the
network target, with probability `share_difficulty / network_difficulty`.

## Run

```bash
python -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
python pool_sim.py --days 365 --half-life-days 14 --pplns-n 2
python -m unittest -v
```

## Interactive dashboard

```bash
python app.py
```

Open the local address printed in the terminal (normally
`http://127.0.0.1:8050`). The dashboard provides controls for network and pool
parameters, executes a new shared-event simulation on demand, and displays:

- cumulative payout under all three policies for a selected miner;
- streaming unpaid balance alongside current EWMA hashrate;
- earned, paid, and unpaid BTC for every miner and policy;
- realized and expected pool-block counts.

Outputs:

- `results/timeseries.csv`: cumulative payouts, EWMA hashrate, and streaming
  balances at every payout tick.
- `results/summary.csv`: earned, paid, and unpaid BTC by policy and miner.

## Current assumptions

- Constant network difficulty, hashrates, reward, and share difficulties.
- No pool fee, stale shares, invalid shares, transaction costs, or insolvency.
- PPS payments are immediate; realized-price and PPLNS rewards observe coinbase
  maturity.
- The EWMA initializes at zero, so the chosen half-life produces an intentional
  startup ramp. This can be changed to the miner's configured hashrate when the
  experiment should start in steady state.
- PPLNS uses a work window, so variable-difficulty shares are handled correctly.

The next useful extension is a hashrate schedule per miner. Step changes and
on/off behavior are the important cases for testing whether the streaming
scheme accumulates or clears unpaid entitlement.
