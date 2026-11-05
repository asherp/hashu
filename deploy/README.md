# Live demo deploy

Runs the whole Hashu node on one internet-facing machine with Docker
Compose: `hashu mint run` (mint, Lightning + Tether rails, Stratum proxy,
control API and operator GUI), a mainnet **phoenixd** for Lightning, and
**Caddy** for TLS.

```
                internet
   wallets ──https──┐        miners ──stratum+tcp:3333──┐
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
| 80, 443 (tcp+udp) | public | Caddy: mint API at `/`, operator GUI at `/operator/`, `GET /control/status` |
| 3333 | public | Stratum proxy, raw TCP |
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
- A DNS name (A record) pointing at the box, e.g. `mint.hashu.io`.
- Firewall: allow inbound **22, 80, 443/tcp, 443/udp, 3333/tcp**; nothing else.

  ```bash
  sudo ufw allow 22/tcp && sudo ufw allow 80/tcp && sudo ufw allow 443 \
    && sudo ufw allow 3333/tcp && sudo ufw enable
  ```

  Note that Docker publishes ports through iptables and bypasses ufw, so
  the real protection for 8085/3334/9740 is that compose never publishes
  them. Check with `sudo ss -tlnp` after start: only 80, 443, 3333 (and 22).
  If the cloud provider has its own security group, open the same ports there.

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
D=https://mint.example.com

curl -s $D/v1/info | jq '{name, nuts: (.nuts|keys)}'          # mint is up, TLS works
curl -s $D/v1/keysets | jq '.keysets[] | {id, unit, active}'  # unit "thh"
curl -s -X POST $D/v1/mint/quote/bolt11 -H 'content-type: application/json' \
  -d '{"amount": 1, "unit": "thh"}' | jq                      # real mainnet invoice
curl -s $D/control/status | jq                                # redemptions (read-only)
curl -s -o /dev/null -w '%{http_code}\n' $D/operator/         # 401 without login
curl -s -o /dev/null -w '%{http_code}\n' -X POST $D/control/redemptions   # 404, stays private
nc -vz mint.example.com 3333                                  # Stratum reachable
```

End to end with the example wallet (from a checkout of the repo):

```bash
cargo run -p hashu-cli --example buy_and_redeem -- \
  https://mint.example.com bolt11 2 stratum+tcp://216.230.225.61:3333 yourname.worker1
```

It prints the invoice; pay it from any Lightning wallet, then it redeems.
Point a miner (Bitaxe, or the Heatbit via the MDK app) at
`stratum+tcp://mint.example.com:3333` and watch the redemption fill in the
operator GUI at `https://mint.example.com/operator/`.

**Rehearsal without money:** set `HASHU_LIGHTNING=fake` and
`HASHU_PRICE=fixed` in `.env`, then `docker compose up -d hashu`. Every
invoice pays itself. Switch back before judges try it.

## 7. MDK heater app

The MDK app polls the control API to know when to move the heater onto
the proxy. Point it at the public read-only route:

```
hashuControlUrl = https://mint.example.com/control
```

and the miner pool at `stratum+tcp://mint.example.com:3333`.
`/control/status` lists open redemptions (pool URL, worker name,
progress). If that should not be public, remove the `/control/*` block
from the Caddyfile and reach 3334 over an SSH tunnel instead.

## Operations

```bash
docker compose ps
docker compose logs -f hashu            # mint + proxy
docker compose logs -f phoenixd
docker compose restart hashu            # note: open redemptions are in memory and are lost
git pull && docker compose up -d --build hashu   # deploy a new version
```

Caveats for the demo:

- Open redemptions live in memory: restarting `hashu` mid-redemption drops
  them (the melt never completes). Don't redeploy during the demo.
- Stratum on 3333 is plaintext TCP, like most pools.
- The mint API has no rate limiting; fine for a hackathon, not production.
- The limited-access phoenixd password was not tested end to end before
  merging (phoenixd needs a live Lightning peer). If `hashu` logs a 401
  from phoenixd, temporarily set its `PHOENIXD_PASSWORD` in
  `docker-compose.yml` to `${PHOENIXD_PASSWORD}` (full access) and tell us.
