# Moving the live demo to a DigitalOcean droplet

The stack in [`docker-compose.yml`](docker-compose.yml) runs unchanged on a
DigitalOcean droplet. This runbook covers what is different from the devbox
(216.230.225.61): sizing, the cloud firewall, a stable IP, DNS on Squarespace,
where state lives, and moving the **existing** mint and phoenixd wallet
without losing issued vouchers, channel state or sats. Read [README.md](README.md)
first; it is still the reference for configuration and operations.

The short version:

1. Create a 1 vCPU / 2 GB droplet with a reserved IP and a cloud firewall.
2. Install Docker, clone the repo, copy `.env`, pull the published images.
3. Cut over: stop the devbox stack, copy three volumes, start on the droplet,
   repoint DNS. Downtime is a few minutes; open redemptions resume (PR #20).
4. Never start phoenixd on the devbox again.

## 1. Droplet

| Setting | Choice | Why |
|---|---|---|
| Image | Ubuntu 24.04 LTS x64 | Docker's install script supports it |
| Size | Basic, Regular, **1 vCPU / 2 GB / 50 GB** (`s-1vcpu-2gb`, ~$12/mo) | The droplet pulls prebuilt images (§3), so it never compiles. At runtime the whole stack (mint, phoenixd JVM, Caddy, MDK demo) fits in ~1.5 GB |
| Region | `nyc3` or `fra1` (or wherever is closest to the pools you route to) | Stratum latency to Braiins/MRR matters more than to wallets; both are fine |
| Auth | SSH key only | |
| Backups | On (weekly, +20%) | Snapshot of the volumes on top of the seed backups below |
| Monitoring | On | Free CPU/RAM/disk graphs |
| IPv6 | Off unless you also add AAAA records | Caddy and Stratum work fine on v4 only |

Building on the droplet instead (`docker compose up -d --build`) needs
~4 GB for the first `cargo build`: use `s-2vcpu-4gb` (~$24/mo), or add 4 GB
of swap to the 2 GB size. Add 1 GB of swap either way as headroom for
phoenixd's JVM:

```bash
fallocate -l 1G /swapfile && chmod 600 /swapfile && mkswap /swapfile \
  && swapon /swapfile && echo '/swapfile none swap sw 0 0' >> /etc/fstab
```

**Reserved IP.** Assign a reserved IP to the droplet and point DNS at that,
not at the droplet's own address. It is free while attached, and you can
rebuild or replace the droplet later without touching Squarespace again.

With `doctl` (or do the same in the control panel):

```bash
doctl compute droplet create hashu \
  --region nyc3 --size s-1vcpu-2gb --image ubuntu-24-04-x64 \
  --ssh-keys <your-key-fingerprint> --enable-backups --enable-monitoring --wait
doctl compute reserved-ip create --droplet-id <droplet-id>
```

## 2. Firewall

Use a DigitalOcean **Cloud Firewall** rather than ufw. It filters before
traffic reaches the droplet, so Docker's iptables rules can't bypass it the
way they bypass ufw.

Inbound:

| Port | Protocol | Source | What |
|---|---|---|---|
| 22 | TCP | your IP (or anywhere, key-only SSH) | SSH |
| 80 | TCP | anywhere | Caddy: HTTP→HTTPS and Let's Encrypt HTTP-01 |
| 443 | TCP | anywhere | Caddy: mint API, checkout, operator GUI, slides, MDK demo |
| 443 | UDP | anywhere | Caddy HTTP/3 (optional) |
| 3335 | TCP | anywhere | Stratum proxy |

Outbound: allow all TCP, UDP and ICMP (the default). phoenixd needs outbound
9735 (ACINQ's Lightning peer) and 50002 (Electrum); the mint needs 443 (MRR
API, mempool) and the upstream pools' Stratum ports (3333 etc.).

8085, 3334 and 9740 stay closed: compose never publishes them on the host.

```bash
doctl compute firewall create --name hashu --droplet-ids <droplet-id> \
  --inbound-rules "protocol:tcp,ports:22,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:80,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:443,address:0.0.0.0/0,address:::/0 protocol:udp,ports:443,address:0.0.0.0/0,address:::/0 protocol:tcp,ports:3335,address:0.0.0.0/0,address:::/0" \
  --outbound-rules "protocol:tcp,ports:all,address:0.0.0.0/0,address:::/0 protocol:udp,ports:all,address:0.0.0.0/0,address:::/0 protocol:icmp,address:0.0.0.0/0,address:::/0"
```

**Keep Stratum on 3335.** The droplet has no ckpool, so 3333 would be free,
but MRR pool profiles, the MDK heater app and the docs all say
`mint.hashu.io:3335`. Keeping the port means only DNS changes.

## 3. Prepare the droplet (no downtime yet)

```bash
ssh root@<reserved-ip>
curl -fsSL https://get.docker.com | sh
git clone https://github.com/asherp/hashu && cd hashu/deploy
```

Copy the devbox's `.env` over (it holds the phoenixd passwords, the operator
password hash and the MRR key; never commit it):

```bash
# from a machine that can reach both
scp devbox:~/hashu/deploy/.env /tmp/hashu.env
scp /tmp/hashu.env root@<reserved-ip>:hashu/deploy/.env && rm /tmp/hashu.env
ssh root@<reserved-ip> chmod 600 hashu/deploy/.env
```

Then edit `.env` on the droplet:

- `HASHU_MDK_DOMAIN`: the default `mdk.216-230-225-61.sslip.io` resolves to
  the **devbox**. Change it to `mdk.<reserved-ip-with-dashes>.sslip.io`, or
  add an `mdk` A record in Squarespace and use `mdk.hashu.io`.
- Leave `PHOENIXD_SEED` empty. The wallet arrives with its volume (step 4);
  the entrypoint ignores the variable once `seed.dat` exists anyway.
- Everything else (`HASHU_DOMAIN=mint.hashu.io`, passwords, MRR key) stays.

Pull the images now so the cutover doesn't wait on the download:

```bash
docker compose pull
```

CI publishes them on every push to main (`.github/workflows/images.yml`):
`ghcr.io/asherp/hashu`, `hashu-caddy` (with the slides and splash site),
`hashu-mdk` and `hashu-phoenixd`, each as `:latest` and `:sha-<commit>`.
The packages must be **public** (GitHub → your profile → Packages → each
package → Package settings → Change visibility), or run
`docker login ghcr.io` on the droplet with a token that has `read:packages`.
To pin a build, set `HASHU_IMAGE_TAG=sha-<short>` in `.env`.

**Lower the DNS TTL.** In Squarespace → Domains → hashu.io → DNS, set the
TTL on the `mint`, `@` and `www` records to the lowest it offers, a few
hours before the cutover, so the change below propagates quickly.

## 4. Cutover: move the state

Three volumes hold state. Copy all three:

| Volume | Contents | If lost |
|---|---|---|
| `hashu_phoenixd_data` | `seed.dat` (wallet words) **and** the channel database `phoenix.mainnet.*.db` | Seed alone recovers funds, but copying the db keeps the channel and fee credit intact without a restore |
| `hashu_hashu_data` | `seed.hex` (mint signing key), `mint.sqlite` (keysets, quotes, spent proofs), `redemptions.jsonl`, `share-audit.jsonl`, `capacity.json`, `operator-settings.json` | Outstanding TiH vouchers can no longer be verified, spent proofs could be double-spent. **Must be copied** |
| `hashu_caddy_data` | Let's Encrypt certificates and account | Nothing serious: Caddy re-issues once DNS points at the droplet. Copying it gives HTTPS immediately |

Best time: no open redemption (`curl -s https://mint.hashu.io/control/status`).
If one is open it resumes on the droplet, it just pauses for the downtime.

**On the devbox**, stop the stack (no `-v`!) and archive the volumes:

```bash
cd ~/hashu/deploy
docker compose down          # keeps volumes; never add -v
for v in phoenixd_data hashu_data caddy_data; do
  docker run --rm -v hashu_$v:/v -v "$PWD":/backup alpine \
    tar czf /backup/$v.tgz -C /v .
done
ls -l *.tgz
```

Stopping phoenixd here is the important part. Two phoenixd instances running
the same wallet at once is unsafe (one of them acts on stale channel state).
From this point the devbox copy is a backup, not a wallet.

Copy the archives to the droplet (they contain the seeds: keep them off
shared disks and delete them once the move is verified):

```bash
scp devbox:~/hashu/deploy/*.tgz /tmp/
scp /tmp/{phoenixd_data,hashu_data,caddy_data}.tgz root@<reserved-ip>:hashu/deploy/
```

**On the droplet**, create the volumes under the same names and restore
(running tar as root keeps file ownership, so the `hashu` user still owns
`/data`):

```bash
cd ~/hashu/deploy
for v in phoenixd_data hashu_data caddy_data; do
  docker volume create hashu_$v
  docker run --rm -v hashu_$v:/v -v "$PWD":/backup alpine \
    tar xzf /backup/$v.tgz -C /v
done
docker compose up -d
docker compose logs -f phoenixd hashu
```

Expect `seed.dat present, using the existing wallet.` from phoenixd, then
`connected to lightning peer`, then `mint ready` and `stratum proxy listening`
from hashu.

Check the wallet and mint are the same ones:

```bash
docker compose exec phoenixd /phoenix/phoenix-cli \
  --http-password="$(grep ^PHOENIXD_PASSWORD= .env | cut -d= -f2)" getinfo
# same nodeId as on the devbox, same channels / feeCreditSat
# before DNS moves: talk to the droplet under the real name (uses the copied cert)
curl -s --resolve mint.hashu.io:443:<reserved-ip> https://mint.hashu.io/v1/info
```

## 5. Cutover: DNS

In Squarespace → Domains → hashu.io → DNS, change each A record's data from
`216.230.225.61` to the reserved IP:

| Host | Type | Data |
|---|---|---|
| `mint` | A | reserved IP |
| `@` | A | reserved IP (if hashu.io already points at the devbox) |
| `www` | A (or CNAME `mint.hashu.io`) | reserved IP |
| `mdk` | A | reserved IP (only if you chose `mdk.hashu.io` above) |

```bash
dig +short mint.hashu.io   # -> reserved IP
```

Then run the smoke test from [README.md §6](README.md#6-smoke-test) against
`https://mint.hashu.io`.

**Miners.** MRR rigs and the Heatbit connect to `mint.hashu.io:3335` by
name. The devbox proxy went down in step 4, so they reconnect and pick up the
new address as their resolver's cache expires (the TTL you lowered). Watch
`docker compose logs -f hashu` for new connections, or the operator GUI's
hashrate graph. If an MRR rental stays down, stop/start the pool on the
rental page to force a fresh lookup. Nothing to change in the MRR pool
profile.

**Unchanged on the devbox:** the Testnet4 ckpool at
`216.230.225.61:3333` and the fx-faucet keep running there; they aren't
part of this stack.

## 6. After the move

- Leave the devbox stack stopped. To be sure nobody restarts it:
  `docker compose -f ~/hashu/deploy/docker-compose.yml rm -f` on the devbox
  (removes containers, keeps volumes as a cold backup). Delete the devbox
  volumes and the `.tgz` files only after a few days of the droplet running.
- Deploys: merge to main, wait for the `images` workflow to go green, then
  on the droplet:
  `cd ~/hashu && git pull && docker compose -f deploy/docker-compose.yml pull && docker compose -f deploy/docker-compose.yml up -d`.
  (`git pull` keeps the compose file and `.env.example` in step with the
  images.) Caddy routes, slide and site edits go live the same way, since the
  Caddyfile, deck and site are all baked into `hashu-caddy`.
- Backups: droplet backups are weekly snapshots. The seeds are what matter,
  and they don't change, so keep the offline copies from README §4. For
  `mint.sqlite` (spent proofs change with every sale), a nightly copy is
  cheap:

  ```bash
  # /etc/cron.daily/hashu-backup
  docker run --rm -v hashu_hashu_data:/v -v /root/backups:/b alpine \
    tar czf /b/hashu_data-$(date +%F).tgz -C /v .
  find /root/backups -name 'hashu_data-*.tgz' -mtime +14 -delete
  ```

- Optional: a DigitalOcean Block Storage volume (~$1/mo for 10 GB) for
  `/root/backups` lets the backups outlive the droplet itself.
