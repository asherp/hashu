# Deploy a Hashu mint

Anyone can run their own Hashu mint with this directory and a `.env` file:
nothing in the code, compose file, Caddyfile or images defaults to a
particular operator's domain, pool, wallet or account. The examples below
use `mint.hashu.io`, the project's own deployment; substitute yours.

Runs the whole Hashu node on one internet-facing machine with Docker
Compose: `hashu mint run` (mint, Lightning + Tether rails, Stratum proxy,
control API and operator GUI), a mainnet **phoenixd** for Lightning, and
**Caddy** for TLS.

Moving it to a DigitalOcean droplet (sizing, cloud firewall, DNS, moving the
mint and phoenixd state): see [DIGITALOCEAN.md](DIGITALOCEAN.md).

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
| 3335 (`HASHU_STRATUM_PORT`) | public | Stratum proxy, raw TCP (host 3335 → container 3333) |
| 8085 | compose network only | Cashu mint API (behind Caddy) |
| 3334 | compose network only | control API + operator GUI (behind Caddy) |
| 9740 | compose network only | phoenixd HTTP API |
| `mdk:3030` | behind Caddy at `https://$HASHU_MDK_DOMAIN/` | optional MDK dashboard demo (`COMPOSE_PROFILES=mdk`; `/mdk` on the mint redirects there) |

phoenixd never gets a host port. Hashu talks to it with phoenixd's
**limited-access** password, which can create invoices and read payments
but cannot send, so even a compromised mint cannot spend the wallet.

## Configuration

Copy [`.env.example`](.env.example) to `.env`. Five variables are
**required** and have no defaults; `docker compose up` refuses to start
and names the first one missing (`required variable HASHU_UPSTREAM is
missing a value: set HASHU_UPSTREAM in .env ...`). The images check them
again at startup, so running them without compose fails the same way.

| Variable | Required | What |
|---|---|---|
| `HASHU_DOMAIN` | **yes** | Public host name of the mint (`mint.example.com`), an A record pointing at this machine. Caddy's certificate, the mint URL in every token (`HASHU_MINT_URL=https://$HASHU_DOMAIN`). |
| `HASHU_UPSTREAM` | **yes** | Default pool: where miners mine when no redemption is open (`stratum+tcp://host:port`). The miner's username passes through, so it must be valid there. |
| `PHOENIXD_PASSWORD` | **yes** | phoenixd full-access password (stays inside phoenixd). `openssl rand -hex 32` |
| `PHOENIXD_LIMITED_PASSWORD` | **yes** | phoenixd limited password given to hashu (invoices only, cannot send). |
| `OPERATOR_PASSWORD_HASH` | **yes** | bcrypt hash for the operator GUI at `/operator/` (`caddy hash-password`). |
| `OPERATOR_USER` | no | Operator GUI user, default `operator`. |
| `HASHU_EXTRA_DOMAINS` | no | More host names for the same site, with the splash page (`site/`) at `/`. Default none. |
| `HASHU_MINT_NAME`, `HASHU_MINT_DESCRIPTION` | no | NUT-06 name and description (defaults: `Hashu` and a generic description). |
| `HASHU_MINT_CONTACT` | no | Operator contact, `method:info` pairs, comma separated (`email:ops@example.com,nostr:npub1...`). Shown in wallets and on the landing page; default none. |
| `HASHU_OPERATOR_WORKER` | no | Your own rigs' pool user (rigs mine as `<user>.<rig>`). Set: only those rigs fill this mint's orders, other miners stay on their home pool. Unset: any connected rig may be borrowed. |
| `HASHU_HOME_SOLO_UPSTREAM`, `HASHU_HOME_POOLS`, `HASHU_HOME_POOLS_OFF` | no | Home pools for Bitaxes ([docs/mine/bitaxe.md](../docs/mine/bitaxe.md)): default solo pool `solo.ckpool.org:3333`, allowed `pool=` hosts (public pools), or `true` to turn home pools off. |
| `HASHU_STRATUM_PORT` | no | Host port for miners, default `3335`. |
| `HASHU_PRICE`, `HASHU_SATS_PER_THH`, `HASHU_NETWORK` | no | Pricing: `mrr` (default), `esplora` or `fixed`; fallback 40 sats/THH. |
| `HASHU_LIGHTNING`, `PHOENIXD_CHAIN` | no | `phoenixd` (default), `fake`, `off`; `mainnet`. |
| `HASHU_USDT_DEPOSIT`, `HASHU_USDC_DEPOSIT` + `HASHU_USDT_PER_THH` or `HASHU_USD_PER_BTC` | no | Stablecoin rails, each on when its deposit address is set. Token, RPC, chain id and confirmations default to the canonical USDT (Ethereum) and USDC (Base) contracts and public RPCs. |
| `MRR_API_KEY`, `MRR_API_SECRET`, `HASHU_MRR_RENTAL_IDS` | no | Capacity from your MiningRigRentals rentals. |
| `HASHU_HEAT_J_PER_TH` | no | Heat estimate default, 30 J/TH. |
| `COMPOSE_PROFILES=mdk` + `HASHU_MDK_DOMAIN`, `MDK_DEMO_USER`, `MDK_DEMO_PASSWORD` | no | The mock MDK dashboard demo. Off by default; when on, all three are required. |
| `HASHU_REGISTRY`, `HASHU_IMAGE_TAG` | no | Image source for `docker compose pull` (default `ghcr.io/asherp`, `latest`). Point at your fork's registry to deploy your own builds. |

Outside Docker, `hashu mint run` reads the same `HASHU_*` variables (or
the matching flags, see `hashu mint run --help`). There `HASHU_UPSTREAM`
is required too; `HASHU_MINT_URL` may be left unset for a rehearsal on
localhost, and the daemon warns when it is unset on a public bind.

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

**hashu.io and www.hashu.io (slides at https://hashu.io/slides/).** Caddy
also answers for `$HASHU_EXTRA_DOMAINS` (default `hashu.io www.hashu.io`),
serving the same site, except that `/` there is the Hashu project splash page
(`site/index.html`, baked into the caddy image) instead of the mint's landing
page. To point them here, in the same DNS settings:

- Delete the Squarespace defaults for the apex and `www` (the `@` A records
  to 198.185.159.x / 198.49.23.x and the `www` CNAME to `ext-cust.squarespace.com`).
  Squarespace may first ask you to disconnect the domain from a Squarespace site.
- Add Host `@`, Type `A`, Data `216.230.225.61`.
- Add Host `www`, Type `A`, Data `216.230.225.61` (or a CNAME to `mint.hashu.io`).

Until those resolve, Caddy simply keeps retrying their certificates and
`https://mint.hashu.io/slides/` works. To stop it trying, set
`HASHU_EXTRA_DOMAINS=` (empty) in `.env`.

The deck is built from `slides/` into the caddy image (`deploy/caddy/Dockerfile`),
together with the site and the `Caddyfile` itself,
so `docker compose up -d --build` republishes it.

## 2. Configure

```bash
git clone https://github.com/asherp/hashu && cd hashu/deploy
cp .env.example .env
openssl rand -hex 32   # -> PHOENIXD_PASSWORD
openssl rand -hex 32   # -> PHOENIXD_LIMITED_PASSWORD
docker run --rm caddy:2 caddy hash-password --plaintext 'pick-a-password'
                       # -> OPERATOR_PASSWORD_HASH (keep the single quotes)
$EDITOR .env           # HASHU_DOMAIN, HASHU_UPSTREAM, the above, optional rails
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
issued `THH` vouchers (losing it means outstanding tokens cannot be verified):

```bash
docker compose exec hashu cat /data/seed.hex; echo
```

Both live in named volumes (`hashu_phoenixd_data`, `hashu_hashu_data`) and
survive `docker compose down` / rebuilds. `docker compose down -v` deletes
them: don't.

To move an existing phoenixd wallet (e.g. the one overpay uses) instead,
put its words in `PHOENIXD_SEED` **before the first start**. Running the
same seed in two phoenixd instances at once is unsafe; stop the other one.

## 5. First payments, fee credit and the channel

**The mint absorbs its LSP's fees.** A buyer who pays the advertised
invoice always gets the full THH: the mint credits the invoiced amount,
never what phoenixd netted after channel fees (`hashu-mint/src/phoenixd.rs`).

A new phoenixd has no channel. What happens to incoming payments
(phoenixd 0.9.1 defaults, lightning-kmp 1.13):

- **Small payments are accepted as fee credit.** While the running total
  is below what a channel open would cost, each payment settles normally
  (the payer's wallet shows success, phoenixd marks it paid, the mint
  issues THH) but the sats go into phoenixd's *fee credit*, not its
  balance. `getinfo`/`getbalance` show them as `feeCreditSat`; they can't
  be withdrawn, only spent on the channel open. Demo-size buys (tens to
  hundreds of sats) all land here, which is fine: the mint never pays out.
- **The channel opens once the credit (plus the next payment) can pay for
  it.** ACINQ opens a channel with `--auto-liquidity` of inbound liquidity
  (default 2,000,000 sats) and charges about 1% of that as a service fee
  (~20,000 sats) plus the on-chain mining fee, taken from the fee credit
  and the triggering payment. After that, payments settle straight into
  the balance with no fee.
- **When a payment is rejected.** Only in two cases, both tied to high
  on-chain feerates: the fee credit would pass `--max-fee-credit`
  (default 2.5% of auto-liquidity = 50,000 sats) before it covers an open,
  or the channel open's mining fee is above `--max-mining-fee` (default 1%
  of auto-liquidity = 20,000 sats). At normal
  feerates neither happens.

Knobs (set in `.env`, passed through `PHOENIXD_EXTRA_ARGS`):

```bash
# more headroom when mainnet fees spike
PHOENIXD_EXTRA_ARGS=--max-fee-credit=125k --max-mining-fee=50000
```

Don't set `--auto-liquidity=off`: it also turns the fee credit off
(it defaults to 2.5% of 0), so every payment is rejected until a
channel exists. There is no setting that opens a channel for less than
the 2m-sat minimum's ~1% service fee.

**Pre-opening the channel (optional).** If you want buyer sats to be
real, spendable balance from the first sale, or want the ~20k sat open
out of the way before the demo, make one buy yourself of about 25,000
sats (more when mempool fees are high). Then check:

```bash
docker compose exec phoenixd /phoenix/phoenix-cli \
  --http-password="$(grep ^PHOENIXD_PASSWORD= .env | cut -d= -f2)" getinfo
```

`channels` should show one `Normal` channel with inbound liquidity.
Skipping this is fine for the demo: small buys succeed as fee credit.

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
cargo run -p hashu --example buy_and_redeem -- \
  https://mint.hashu.io bolt11 2 stratum+tcp://stratum.braiins.com:3333 <braiins-user>.redeem
```

It prints the invoice; pay it from any Lightning wallet, then it redeems
to that mainnet pool and worker. Point a miner (Bitaxe, see [docs/mine/bitaxe.md](../docs/mine/bitaxe.md), or the Heatbit via the MDK app) at
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

### MDK dashboard on the site

With `COMPOSE_PROFILES=mdk` in `.env`, `docker compose up -d` also starts
the MDK demo for judges to click through, with no hardware:

- **`mdk`**: the MDK stack (Kernel, heater Worker with its mock heater,
  Hashu Heater app, Gateway), two fake pools that accept every share, and
  the built MDK dashboard, served with the Gateway behind one port (3030).
- **`mdk-hashu`**: a separate demo mint + proxy (`--lightning fake`,
  `--price fixed`, `--control-allow-open`, upstream = the fake home pool).

Set `HASHU_MDK_DOMAIN` (e.g. `mdk.<ip-with-dashes>.sslip.io`, no DNS
change needed), `MDK_DEMO_USER` and `MDK_DEMO_PASSWORD` (12+ characters);
the mdk container refuses to start without the last two. Open
**https://$HASHU_DOMAIN/mdk** (redirects to `https://$HASHU_MDK_DOMAIN/`),
sign in with `MDK_DEMO_USER` / `MDK_DEMO_PASSWORD`, open
**Hashu Heater** and press **Redeem**. The heater switches to the demo
proxy, the 1 THH fills in about 90 s, and it switches back.

The dashboard needs its own host because the MDK shell serves from `/`.
The demo is isolated on purpose: the mock heater's shares are fake, so it
never mines on the live mint or a real pool, and the demo mint is only
reachable on the `mdk` network. Kernel state resets on every restart. To
use your own name, add an A record and set `HASHU_MDK_DOMAIN`.

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

- `mining.configure` (version rolling) and `mining.set_difficulty` are
  forwarded unchanged; the rig gets a small proxy-owned extranonce so it
  can be moved between pools in place. So
  difficulty is whatever the upstream pool's vardiff sets (Braiins handles
  big rigs fine). Crediting is by
  share difficulty (`difficulty × 2^32` hashes), so a THH is a THH at any
  difficulty.
- When a redemption opens or fills, the proxy **switches the rig's pool
  in place**: the rig's connection stays up and gets the new pool's
  difficulty and a clean job, with the worker name rewritten to the
  redeemer's. The log shows `switched pool without reconnecting the
  miner` with `switch_ms`. If the new pool can't take the rig in place
  (extranonce2 under 6 bytes, version rolling not granted) the proxy
  closes the connection instead and the rig reconnects in seconds; the
  MRR rental page may then briefly show the pool as down. Set
  `HASHU_SWITCH_MODE=reconnect` to always reconnect.
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
   rig switches pools, `shares_credited` starts rising and the cumulative THH
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
2. Do one tiny rehearsal buy. Before the first channel it settles as fee
   credit (section 5), which is fine.

On stage:

1. **Buy.** Open `https://mint.hashu.io/checkout/` (or run the wallet
   example) for **5 THH**; pay the ~10 sat invoice from a phone wallet.
   The tokens appear.
2. **Redeem.** Melt 5 THH with method `hashrate` to
   `stratum+tcp://stratum.braiins.com:3333`, worker `<braiins-user>.redeem`:

   ```bash
   cargo run -p hashu --example buy_and_redeem -- \
     https://mint.hashu.io bolt11 5 stratum+tcp://stratum.braiins.com:3333 <braiins-user>.redeem
   ```

   (does buy + redeem in one go: it prints the invoice, waits for payment,
   then redeems).
3. **Watch it fill.** Operator GUI: the rig switches to the redemption pool, the
   cumulative THH graph climbs to 5 over ~3 minutes, the melt turns
   `PAID`, and the rig goes back to the idle route. On Braiins the
   `redeem` worker appears with ~100 TH/s for those minutes.

**Tell the mint how much it can sell.** The mint can only sell THH the
rentals will still deliver: `rented TH/s × hours left − THH owed` (vouchers
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
  goes back to automatic (capped). It is saved in `/data/capacity.json`.

The other operator settings (issuance paused, markup, heat J/TH) are saved in
`/data/operator-settings.json` whenever they change in the GUI, and reloaded
on start. A saved value wins over its flag or env var (`--markup-bps`,
`HASHU_HEAT_J_PER_TH`); delete the file to go back to the flags.

With no rentals (no MRR key, or no active MRR rental) nothing can be
sold: the mint refuses quotes and the checkout and landing page show
"Sold out" with the buy button off. Set `{"limit": false}` (Capacity
card → Off) only to sell without a cap, for example in local testing.

## Operations

```bash
docker compose ps
docker compose logs -f hashu            # mint + proxy
docker compose logs -f phoenixd
docker compose restart hashu            # open redemptions resume after the restart
# deploy a new version: CI-published images (after the `images` workflow is green)
git pull && docker compose pull && docker compose up -d
# or build from this checkout
git pull && docker compose up -d --build
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
