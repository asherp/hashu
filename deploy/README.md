# Live demo deploy

Runs the whole Hashu node on one internet-facing machine with Docker
Compose: `hashu mint run` (mint, Lightning + Tether rails, Stratum proxy,
control API and operator GUI), a mainnet **phoenixd** for Lightning, and
**Caddy** for TLS.

```
                internet
   wallets ──https──┐        miners ──stratum+tcp:3335──┐
                    ▼                                   ▼
             ┌────────────┐  :8085 mint API     ┌──────────────┐  ──► redeemer's pool
             │   Caddy    │ ──────────────────► │    hashu     │  ──► default pool
             │  80 / 443  │  :3334 /operator/   │  mint run    │
             └────────────┘   (basic auth)      └──────┬───────┘
                                                       │ :9740 (limited password)
                                                ┌──────▼───────┐
                                                │   phoenixd   │ ──► Lightning (mainnet)
                                                └──────────────┘
```

| Port | Exposed? | What |
|---|---|---|
| 80, 443 (tcp+udp) | public | Caddy: landing page at `/`, mint API at `/v1/`, buyer checkout at `/checkout/`, operator GUI at `/operator/`, `GET /control/status` |
| 3335 | public | Stratum proxy, raw TCP (host 3335 → container 3333; ckpool keeps 3333) |
| 8085 | compose network only | Cashu mint API (behind Caddy) |
| 3334 | compose network only | control API + operator GUI (behind Caddy) |
| 9740 | compose network only | phoenixd HTTP API |

phoenixd never gets a host port. Hashu talks to it with phoenixd's
**limited-access** password, which can create invoices and read payments
but cannot send, so even a compromised mint cannot spend the wallet.

## 1. Prerequisites

- A Linux box with a public IP and Docker Engine + Compose v2
  (`curl -fsSL https://get.docker.com | sh`). 2 GB RAM is enough once
  built; the first `cargo build` inside Docker wants ~4 GB, so on a small
  VPS add swap or build the image elsewhere and `docker save | ssh … docker load`.
- A DNS name for the box: **`mint.hashu.io`** → the devbox (216.230.225.61).
  See "DNS on Squarespace" below. Until it resolves, `216-230-225-61.sslip.io`
  works as a stand-in (sslip.io maps it to that IP).
- Ports 80 and 443 free on the host for Caddy. Check first with
  `sudo ss -tlnp | grep -E ':(80|443|3335) '`; if something (e.g. a faucet
  web server) already has 80/443, either put the Hashu routes in that
  server or set Caddy's ports to others in `docker-compose.yml`.
- Firewall: allow inbound **22, 80, 443/tcp, 443/udp, 3335/tcp** (plus whatever the box already serves, e.g. ckpool on 3333).

  ```bash
  # devbox: keep ckpool (3333) and anything else it already serves open
  # BEFORE enabling ufw, or you cut off the pool. Check `sudo ufw status` first.
  sudo ufw allow 22/tcp && sudo ufw allow 3333/tcp && sudo ufw allow 80/tcp \
    && sudo ufw allow 443 && sudo ufw allow 3335/tcp && sudo ufw enable
  ```

  Note that Docker publishes ports through iptables and bypasses ufw, so
  the real protection for 8085/3334/9740 is that compose never publishes
  them. Check with `sudo ss -tlnp` after start: only 80, 443, 3335, ckpool's 3333 and 22.
  If the cloud provider has its own security group, open the same ports there.

### DNS on Squarespace

1. Squarespace → **Domains** → `hashu.io` → **DNS** (DNS Settings).
2. Under **Custom records**, **Add record**: Host `mint`, Type `A`,
   Data `216.230.225.61`, TTL default. Save. Leave the existing
   Squarespace records for the apex/`www` alone.
3. Wait until it resolves (usually minutes):

   ```bash
   dig +short mint.hashu.io      # -> 216.230.225.61
   ```

Start Caddy only after that: Let's Encrypt checks the name over port 80,
and repeated failures hit its rate limit. If Squarespace says the domain
uses external nameservers, add the same A record at that DNS provider instead.

## 2. Configure

```bash
git clone https://github.com/asherp/hashu && cd hashu/deploy
cp .env.example .env
openssl rand -hex 32   # -> PHOENIXD_PASSWORD
openssl rand -hex 32   # -> PHOENIXD_LIMITED_PASSWORD
docker run --rm caddy:2 caddy hash-password --plaintext 'pick-a-password'
                       # -> OPERATOR_PASSWORD_HASH (keep the single quotes)
$EDITOR .env           # HASHU_DOMAIN, the above, optional USDT
```

## 3. Start

```bash
docker compose up -d --build
docker compose logs -f
```

The first build compiles the Rust workspace and takes several minutes.
Expect `mint ready`, `control API listening`, `stratum proxy listening`,
and Caddy `certificate obtained successfully` for your domain.

phoenixd only opens its HTTP API once it has connected to ACINQ's
Lightning peer and an Electrum server (needs outbound 9735 and 50002).
Until then `hashu` exits with `phoenixd: error sending request for url
(http://phoenixd:9740/getinfo)` and Docker restarts it; that settles
within a few seconds of phoenixd logging `connected to lightning peer`.
If it doesn't, check the box's outbound firewall.

## 4. Back up the wallet seed (do this now)

If you did not set `PHOENIXD_SEED`, phoenixd created a fresh mainnet
wallet on first boot. The 12 words are the only way to recover its funds:

```bash
docker compose exec phoenixd cat /phoenix/.phoenix/seed.dat; echo
```

Write them down offline. Also keep the mint's own seed, which signs the
`THH` ecash (losing it means outstanding tokens cannot be verified):

```bash
docker compose exec hashu cat /data/seed.hex; echo
```

Both live in named volumes (`hashu_phoenixd_data`, `hashu_hashu_data`) and
survive `docker compose down` / rebuilds. `docker compose down -v` deletes
them: don't.

To move an existing phoenixd wallet (e.g. the one overpay uses) instead,
put its words in `PHOENIXD_SEED` **before the first start**. Running the
same seed in two phoenixd instances at once is unsafe; stop the other one.

## 5. First Lightning payment opens the channel

A new phoenixd has no channel. The first incoming payment makes ACINQ's
LSP open one on the fly and takes a fee out of it (about 2,000+ sats plus
1% at the time of writing, more when mainnet fees are high). So:

- Make the first buy yourself, **before the demo**, and make it big enough
  to cover that fee, e.g. a mint quote of ~10,000 sats. A smaller first
  payment may be held as fee credit instead of opening a channel.
- Then check the channel:

  ```bash
  docker compose exec phoenixd /phoenix/phoenix-cli \
    --http-password="$(grep ^PHOENIXD_PASSWORD= .env | cut -d= -f2)" getinfo
  ```

  `channels` should show one `Normal` channel with inbound liquidity.
  Later demo-size payments then settle instantly.

## 6. Smoke test

From any machine:

```bash
D=https://mint.hashu.io

curl -s $D/v1/info | jq '{name, nuts: (.nuts|keys)}'          # mint is up, TLS works
curl -s $D/v1/keysets | jq '.keysets[] | {id, unit, active}'  # unit "thh"
curl -s -X POST $D/v1/mint/quote/bolt11 -H 'content-type: application/json' \
  -d '{"amount": 1, "unit": "thh"}' | jq                      # real mainnet invoice
curl -s -o /dev/null -w '%{http_code}\n' $D/checkout/       # 200, public buyer page
curl -s $D/control/status | jq                                # redemptions (read-only)
curl -s -o /dev/null -w '%{http_code}\n' $D/operator/         # 401 without login
curl -s -o /dev/null -w '%{http_code}\n' -X POST $D/control/redemptions   # 404, stays private
nc -vz mint.hashu.io 3335                                  # Stratum reachable
```

End to end with the example wallet (from a checkout of the repo):

```bash
cargo run -p hashu-cli --example buy_and_redeem -- \
  https://mint.hashu.io bolt11 2 stratum+tcp://stratum.braiins.com:3333 <braiins-user>.redeem
```

It prints the invoice; pay it from any Lightning wallet, then it redeems
to that mainnet pool and worker. Point a miner (Bitaxe, or the Heatbit via the MDK app) at
`stratum+tcp://mint.hashu.io:3335` and watch the redemption fill in the
operator GUI at `https://mint.hashu.io/operator/`.

**Rehearsal without money:** set `HASHU_LIGHTNING=fake` and
`HASHU_PRICE=fixed` in `.env`, then `docker compose up -d hashu`. Every
invoice pays itself. Switch back before judges try it.

## 7. MDK heater app

The MDK app polls the control API to know when to move the heater onto
the proxy. Point it at the public read-only route:

```
hashuControlUrl = https://mint.hashu.io/control
```

and the miner pool at `stratum+tcp://mint.hashu.io:3335`.
`/control/status` lists open redemptions (pool URL, worker name,
progress). If that should not be public, remove the `/control/*` block
from the Caddyfile and reach 3334 over an SSH tunnel instead.

## 8. Rented hashrate from MiningRigRentals

With no Bitaxe at hand, rent SHA-256 hashrate on MiningRigRentals (MRR)
and point it at the **Hashu proxy**, not at the pool directly. Only shares that pass
through the proxy are seen, committed and credited to redemptions; a rig
pointed straight at the pool mines fine but Hashu never knows.

The demo mines **mainnet**: MRR rigs refuse Testnet4 jobs (Testnet4's
future-stamped `ntime`), so the idle route is a mainnet pool (Braiins,
`HASHU_UPSTREAM`) and redemptions go to mainnet pools too.

```
MRR rig ──stratum+tcp://mint.hashu.io:3335──► hashu proxy ──► braiins (default, mainnet)
                                                         └──► redeemer's pool (while a redemption is open)
```

**Add the pool on MRR** (Account → Pool Profiles, or the pool list when
renting):

| Field | Value |
|---|---|
| Algorithm | SHA256 |
| Pool host | `stratum+tcp://mint.hashu.io` |
| Port | `3335` |
| Worker / username | `<braiins-user>.mrr`: on the idle route this name reaches Braiins unchanged, so it must be a Braiins account |
| Password | `x` |

Don't append NiceHash's `#xnsub` to the URL. Hold off on a backup pool
that goes straight to the pool: MRR fails over to it whenever the proxy
drops the rig (see below), and those shares bypass Hashu. If you want a
backup, add the same proxy URL again.

**How much to rent.** 1 TH/s for one hour is 1 THH, so a rig of R TH/s
fills R/60 THH per minute. A 100 TH/s rental fills a 5 THH redemption in
3 minutes; 1 PH/s fills it in 18 seconds. For a live demo, size the
redemption so it takes 2–5 minutes at the rented rate, so the judges see
the graph climb and the melt turn `PAID`. MRR rental hashrate takes a few
minutes to ramp up after the rental starts, so start it ~15 minutes early.

**What the proxy does with an MRR rig** (it is a pass-through Stratum V1
proxy, one upstream connection per miner connection):

- `mining.subscribe`/`configure` (version rolling), `extranonce`
  handling and `mining.set_difficulty` are forwarded unchanged, so
  difficulty is whatever the upstream pool's vardiff sets (Braiins handles
  big rigs fine). Crediting is by
  share difficulty (`difficulty × 2^32` hashes), so a THH is a THH at any
  difficulty.
- When a redemption opens or fills, the proxy **closes the rig's
  connection** so it reconnects to the new route, with the worker name
  rewritten to the redeemer's. MRR rigs reconnect in seconds; the MRR
  rental page may briefly show the pool as down. That is expected.
- The redemption pool in the demo is Braiins too, under a different
  worker (`<braiins-user>.redeem`), so the Braiins dashboard shows the
  redeemed hashrate as its own worker.

**Confirm hashrate is arriving:**

1. MRR rental page: the rig shows the pool connected and a nonzero
   hashrate after ramp-up.
2. `docker compose logs -f hashu`: a new connection from MRR's IP, then
   metrics lines every 30 s with `shares_accepted` climbing.
3. `curl -s https://mint.hashu.io/control/status | jq .metrics`:
   `connections_accepted` ≥ 1 and `shares_accepted` rising.
4. Operator GUI at `https://mint.hashu.io/operator/`: the live hashrate
   graph tracks the rented TH/s (averaged over `--hashrate-window`,
   default 60 s). Open a redemption (e.g. `buy_and_redeem` above): the
   rig reconnects, `shares_credited` starts rising and the cumulative THH
   graph climbs to the target, then the rig goes back to the idle route.

## 9. Demo script (mainnet)

Price check: the MRR floor is about 0.00045 BTC/PH/day, i.e. ~2 sats per
THH. Check the live price with a 1 THH quote:

```bash
curl -s -X POST https://mint.hashu.io/v1/mint/quote/bolt11 \
  -H 'content-type: application/json' -d '{"amount":1,"unit":"thh"}' | jq .request
```

(decode the invoice amount in any wallet). Sizes below assume a 100 TH/s rental; scale with R/60 THH
per minute.

Before going on stage (~15 min early):

1. MRR rental running against `stratum+tcp://mint.hashu.io:3335`, user
   `<braiins-user>.mrr`. Braiins dashboard shows worker `mrr` hashing,
   operator GUI shows the live hashrate.
2. The phoenixd channel is open (section 5). Do one tiny rehearsal buy.

On stage:

1. **Buy.** Open `https://mint.hashu.io/checkout/` (or run the wallet
   example) for **5 THH**; pay the ~10 sat invoice from a phone wallet.
   The tokens appear.
2. **Redeem.** Melt 5 THH with method `hashrate` to
   `stratum+tcp://stratum.braiins.com:3333`, worker `<braiins-user>.redeem`:

   ```bash
   cargo run -p hashu-cli --example buy_and_redeem -- \
     https://mint.hashu.io bolt11 5 stratum+tcp://stratum.braiins.com:3333 <braiins-user>.redeem
   ```

   (does buy + redeem in one go: it prints the invoice, waits for payment,
   then redeems).
3. **Watch it fill.** Operator GUI: the rig reconnects within seconds, the
   cumulative THH graph climbs to 5 over ~3 minutes, the melt turns
   `PAID`, and the rig goes back to the idle route. On Braiins the
   `redeem` worker appears with ~100 TH/s for those minutes.

**Tell the mint how much it can sell.** The mint can only sell THH the
rentals will still deliver: `rented TH/s × hours left − THH owed` (ecash
held by users, paid-but-unminted quotes and the rest of open
redemptions). Once a capacity source is set, mint quotes beyond that are
refused on every rail, the checkout shows "N THH available" and the
landing page shows the same number. Two ways to set it:

- **Automatic, from MRR.** Put `MRR_API_KEY` / `MRR_API_SECRET` (the same
  key `btc-rent.py` uses) in `.env`. The mint reads `GET /rental?type=active`
  every minute and takes each rental's advertised hashrate and end time.
  If the account also has rentals mining somewhere else (the fx-faucet),
  list the ones on the proxy in `HASHU_MRR_RENTAL_IDS=123,456`.
- **By hand**, in the operator GUI's *Hashrate capacity* card (TH/s and
  hours left), or:

  ```sh
  curl -s -u operator:$OPERATOR_PASSWORD -XPOST \
    https://mint.hashu.io/operator/api/capacity \
    -d '{"rentals":[{"ths":100,"hours":3,"label":"MRR 123"}]}'
  ```

  `rentals` replaces the hand-entered list (`end_ms` in unix ms works
  instead of `hours`). `{"limit": false}` stops capping, `{"limit": null}`
  goes back to automatic. It is saved in `/data/capacity.json`.

With no rentals and no MRR key, sales stay uncapped as before.

## Operations

```bash
docker compose ps
docker compose logs -f hashu            # mint + proxy
docker compose logs -f phoenixd
docker compose restart hashu            # open redemptions resume after the restart
git pull && docker compose up -d --build hashu   # deploy a new version
```

Caveats for the demo:

- Open redemptions are journaled in the `hashu_data` volume
  (`/data/redemptions.jsonl`, share trees in `/data/share-audit.jsonl`), so a
  restart or redeploy resumes them: miners reconnect, get routed to the
  redeemer's pool again and keep crediting. Shares submitted while `hashu`
  is down are lost (miners are disconnected), so the redemption just takes
  that much longer.
- Stratum on 3335 is plaintext TCP, like most pools.
- The mint API has no rate limiting; fine for a hackathon, not production.
- The limited-access phoenixd password was not tested end to end before
  merging (phoenixd needs a live Lightning peer). If `hashu` logs a 401
  from phoenixd, temporarily set its `PHOENIXD_PASSWORD` in
  `docker-compose.yml` to `${PHOENIXD_PASSWORD}` (full access) and tell us.
