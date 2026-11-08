# Hashu

A hashrate proxy fused with a Cashu mint. Buyers pay Lightning, receive
Chaumian hashrate vouchers: blind-signed tokens denominated in
hashrate-time (`THH` —
terahash-hours), and redeem them to direct real hashrate at a pool of
their choice.

Hashu is **self-hosted software**, not a service. Anyone with hashrate
can run a Hashu instance and start issuing vouchers backed by it. The
trust model, components, and on-the-wire artifacts are described in
[ARCHITECTURE.md](ARCHITECTURE.md).

- Domain: **hashu.io**
- Language: Rust (workspace)
- License: MIT or Apache-2.0 (dual)

## Status

Early. The project is a working scaffold of the core components but is
**not yet a complete mint** — issuance, melt, and the operator-facing
mint daemon are still ahead.

| Component | Status | Where |
|---|---|---|
| Workspace + dual-license scaffold | done | `77663f7` |
| Append-only Merkle commitment tree (RFC 6962) | done | `hashu-core::share` |
| Hashprice oracle trait + interpolating buffer | done | `hashu-core::hashprice` |
| Esplora hashprice adapter (free, default) | done | `hashu-mint::oracle::esplora` |
| Luxor hashprice adapter (paid alternative) | done | `hashu-mint::oracle::luxor` |
| MiningRigRentals rental-floor hashprice adapter (free, Testnet4 demo) | done | `hashu-mint::oracle::mrr` |
| Operator manifest publisher (kind 0 nostr event) | done | `hashu-mint::manifest` |
| Stratum V1 pass-through proxy + share counter | done | `hashu-proxy::stratum` |
| `hashu` operator CLI (init, config, manifest, oracle, proxy) | done | `hashu-cli` |
| Mint daemon: `hashu mint run` (mint + rails + proxy + control API) | done | `hashu-cli::mint` |
| Share commitment tree integration in the proxy | done | `hashu-proxy::stratum::{session,header,committer,compact}` |
| `hashrate` melt method via cdk (redemption ledger + `MintPayment` backend) | done | `hashu-core::redemption`, `hashu-mint::melt` |
| Tether (USDT) payment rail: buy `THH` with dollars (ERC-20 over EVM JSON-RPC) | done | `hashu-mint::tether` |
| Proxy control API (`GET /status`, demo `POST /redemptions`) | done | `hashu-proxy::control` |
| Per-redemption share tree + audit API (`GET /redemptions/{id}/root`, `/proof/{n}`) | done | `hashu-proxy::{audit,control}` |
| Mint operator GUI (live hashrate, redemption delivery, obligations, pause/markup) | done | `hashu-cli::operator`, `hashu-proxy::meter` |
| Buyer checkout page (pay by Lightning QR or USDT, get a token, redeem to a pool) | done | `hashu-cli::checkout` |
| MDK heater stack: Worker + app switching pools on redeem, thermostat | done | [`mdk/`](mdk/README.md) |
| Per-share nostr receipts + completion certificate | planned | — |
| Stratum V2 via SRI | fast-follow | — |
| Lightning backend (LND/CLN via cdk) | planned | — |

99 tests across the workspace, plus a loopback end-to-end test for the
proxy that asserts an accepted share gets committed to the per-connection
share tree (and a rejected one does not).

## Build

You need a recent stable Rust toolchain plus `protoc` on `PATH` (the
`cdk-signatory` build script invokes it).

```bash
brew install protobuf            # macOS
# or: apt install protobuf-compiler

git clone https://github.com/asherp/hashu && cd hashu
cargo build --release
```

The binary lands at `target/release/hashu`. To install on `$PATH`:

```bash
cargo install --path hashu-cli
```

Or install the published crate from crates.io (still needs `protoc`):

```bash
cargo install hashu --locked
```

The CLI crate is published as `hashu` (source in `hashu-cli/`). The library crates are `hashu-core`, `hashu-proxy` and `hashu-mint`.

## Quick start

### 1. Generate operator state

```bash
hashu init
```

Prompts for instance URL, profile fields (name / about / nip05 / lud16
/ website — all optional), the hashprice oracle (`esplora`, `luxor` or `mrr`)
with a sensible URL default per source, and a relay list. Writes
`~/.hashu/nostr.nsec` (mode 0600), `~/.hashu/manifest_state.json`, and
prints the operator's `npub`.

`hashu init --no-interactive` writes a stub state with placeholder
fields you edit by hand.

### 2. Inspect or update state

```bash
hashu config show
hashu config set --instance-url https://mint.example.com
hashu config set --oracle-source luxor      # auto-resets oracle URL to Luxor's default
hashu config set --relays "wss://relay.damus.io,wss://nos.lol"
```

### 3. Publish the operator manifest

```bash
hashu manifest dry-run --keyset-id 00deadbeefcafebab    # sign + print, no relay contact
hashu manifest publish --keyset-id 00deadbeefcafebab    # actually push to configured relays
```

The manifest is a kind 0 event whose `content` merges standard nostr
profile fields with a namespaced `hashu` object listing your instance
URL, supported units (`THH`), supported melt methods, oracle source,
and an **append-only** history of mint keyset IDs. Append-only means
rotated keysets stay in the list with `status: "retired"` rather than
disappearing — preserving attestation for tokens issued under prior
keysets even though kind 0 is replaceable.

The `--keyset-id` flag is a placeholder until cdk integration lands;
later it'll be read from the running mint via the `KeysetSource`
trait.

### 4. Query a hashprice sample

```bash
hashu oracle ping                                            # Esplora @ blockstream.info
hashu oracle ping --source luxor                             # needs LUXOR_API_KEY
hashu oracle ping --base-url https://my-esplora.example.com/api
hashu oracle ping --network testnet4                         # Esplora @ mempool.space/testnet4
hashu oracle ping --source mrr --network testnet4            # MRR rental floor + Testnet4 cost per block
```

`--network` picks the public Esplora for `bitcoin` (default),
`testnet4`, `testnet` (Testnet3) or `signet`. Esplora pings also print
the chain's real difficulty and the THH one expected block costs; on
Testnet4 that skips BIP 94 difficulty-1 blocks (≈1.04e9 real difficulty
≈ 1,240 THH per block on 2026-11-05).

For a Testnet4 mint, price THH with `--source mrr`: the
MiningRigRentals SHA-256 rental floor (BTC/PH/day, public, no key) is
what the hashrate actually costs, whichever chain it mines. Combined
with Testnet4's real difficulty it gives the BTC cost of one expected
block (≈0.00045 BTC/PH/day × 1.24 PH-hours ≈ $1.76 on 2026-11-05, as
read by `fx-faucet`'s `btc-rent.py market`). The Esplora hashprice on a
testnet is in test sats.

### 5. Run the Stratum V1 proxy

```bash
hashu proxy run --upstream stratum+tcp://your-pool.example.com:3333
```

Listens on `0.0.0.0:3333` (configurable via `--bind`) and forwards
every accepted miner to the configured upstream pool. As traffic flows
the proxy maintains a per-connection [share commitment tree
(§4.7.2)](ARCHITECTURE.md): each accepted share is reconstructed into
its 80-byte block header, packaged into a `ShareLeaf`, and appended.
On disconnect the final root + leaf count is logged. Periodic metric
snapshots (including `shares_committed`) are logged via `tracing`.
With `--control-bind 127.0.0.1:3334` the proxy also serves a small HTTP
control API (`GET /status`: redemptions and progress) that the
[MDK heater app](mdk/README.md) polls to switch miners onto the proxy.

**Share audit.** Every share credited to a redemption is also appended to
that redemption's own Merkle tree (all miners serving it, in credit
order). Leaves hold only PoW evidence: block header, `target_bits`,
`ntime`, `seq`.

```bash
curl -s 127.0.0.1:3334/redemptions/$ID/root
# {"id":"…","root":"<hex or null>","leaf_count":42}
curl -s 127.0.0.1:3334/redemptions/$ID/proof/7
# {"root":"…","tree_size":42,"leaf_index":7,"leaf_hash":"…",
#  "leaf":{"preimage":"<80-byte header hex>","target_bits":…,"ntime":…,"seq":7},
#  "siblings":["…", …]}
```

To check a share: rebuild `leaf_hash = SHA256(u32_be(80) ‖ header ‖
u32_be(target_bits) ‖ u32_be(ntime) ‖ u64_be(seq))`, fold it with
`siblings` per RFC 6962 §2.1.1 (`hashu_core::share::verify_inclusion`)
to get `root`, then double-SHA256 the header and compare it with the
target. Signed nostr publication of roots is still planned.

### 6. Run the mint

`hashu mint run` is the whole Hashu node in one process: the cdk mint for
`THH`, the payment rails that sell it, the `hashrate` redemption method,
the Stratum proxy that fills redemptions, and the control API the
[MDK app](mdk/README.md) polls.

```bash
# Mainnet Lightning via phoenixd (same variables as overpay), USDT on
# Ethereum mainnet and USDC on Base,
# priced from the MiningRigRentals floor, default pool = the Testnet4 demo pool.
export PHOENIXD_URL=http://127.0.0.1:9740 PHOENIXD_PASSWORD=...
hashu mint run \
  --usdt-deposit 0xYourAddress --usdc-deposit 0xYourAddress --usd-per-btc 100000 \
  --mint-url https://mint.example.com
```

| Port | What |
|---|---|
| `0.0.0.0:8085` (`--listen`) | Cashu mint API: `/v1/info`, `/v1/keys`, `/v1/mint/quote/{bolt11,usdt,usdc}`, `/v1/melt/quote/hashrate`, …, the buyer **checkout page** at `/checkout/`, and the NUT-24 paid redeem endpoint `POST /api/redeem` |
| `0.0.0.0:3333` (`--stratum-bind`) | Stratum proxy for miners |
| `127.0.0.1:3334` (`--control-bind`) | **Operator GUI** at `/`, its API under `/api/`, and the control API: `GET /status`, `GET /redemptions/{id}/root`, `GET /redemptions/{id}/proof/{n}` |

- **Lightning** (`--lightning phoenixd|fake|off`): phoenixd is used when
  `PHOENIXD_URL` or `PHOENIXD_HOSTPORT` is set. It only buys `THH`; the
  mint never pays out. `fake` pays every invoice by itself, for rehearsals.
- **Stablecoins** (USDT, USDC): each is on when its `--usdt-deposit` /
  `--usdc-deposit` is set, as methods `usdt` and `usdc`. Each coin has its
  own chain: USDT defaults to Ethereum mainnet (12 confirmations), USDC to
  Circle's native USDC on Base (30 confirmations); change one with
  `--usdc-token`, `--usdc-rpc`, `--usdc-chain-id` (same for `usdt`).
  Open quotes are kept in `<data-dir>/{usdt,usdc}-quotes.json` so a restart loses none. Price both with `--usdt-per-thh 0.05` or derive
  it from sats with `--usd-per-btc`.
- **Price** (`--price mrr|esplora|fixed`): MRR rental floor by default;
  `--sats-per-thh` is the fixed price, and the fallback while a live
  source has no fresh sample. `--markup-bps` adds the operator's margin.
- **Redemption**: a wallet melts `THH` with method `hashrate` and request
  `{"pool_url", "worker_name", "amount_thh", "nonce"?}`. The proxy routes
  miners to that pool under that worker until the amount is filled, then
  the melt turns `PAID`. `--upstream` is where miners go otherwise.
- **Redeem over HTTP 402** ([NUT-24](https://github.com/cashubtc/nuts/blob/main/24.md)):
  `POST /api/redeem` with `{"pool_url", "worker_name", "worker_password"?, "amount_thh"?}`.
  Without payment it answers `402 Payment Required` and an `X-Cashu`
  header holding a NUT-18 request (`creqA…`: `a` = `amount_thh`, default
  the smallest redemption; `u` = `thh`; `m` = this mint). Retry with a
  `cashuB…` THH token in `X-Cashu` to open the redemption; the reply has
  `redemption_id` and a `status_url`. A token from another mint, in
  another unit or worth less than `a` gets `400`. The whole unspent token
  is redeemed, so paying more buys more hashrate. Any NUT-24 client
  works, and so does `curl`:

  ```bash
  body='{"pool_url":"stratum+tcp://pool.example:3333","worker_name":"me.rig1","amount_thh":2}'
  curl -si -X POST https://mint.hashu.io/api/redeem -d "$body"          # 402 + X-Cashu: creqA…
  curl -s  -X POST https://mint.hashu.io/api/redeem -d "$body" -H "X-Cashu: cashuB…"
  ```

  The 402 plumbing (`hashu-cli/src/paid.rs`) takes any number of payment
  schemes, so L402 can be added next to Cashu on the same endpoint.
- State lives in `--data-dir` (`mint.sqlite`, `seed.hex`, plus
  `redemptions.jsonl` and `share-audit.jsonl`). Open redemptions and their
  share trees are replayed on startup, so a restart resumes them.
- **Operator GUI** (`http://127.0.0.1:3334/`): real-time hashrate through
  the proxy (from accepted shares, difficulty × 2^32 hashes each, averaged
  over `--hashrate-window` seconds), the current redemption's delivered
  THH against its target, outstanding obligations (vouchers held by users,
  paid-but-unminted quotes, open redemptions), and controls to pause
  issuance or change the markup live. It trusts whoever can reach the
  port, like `POST /redemptions`: keep it on loopback or behind auth. It
  uses relative URLs, so it can sit behind a path prefix.
- **Checkout** (`http://<mint>/checkout/`): a buyer picks THH, pays the
  invoice QR (or the USDT request), and gets a `cashuB` token; pasting it
  back with a pool and worker redeems it and shows the hashrate arriving.
  The daemon runs a throwaway wallet per checkout, so it briefly holds the
  buyer's vouchers; any Cashu wallet with the `thh` unit can skip the page.
  Per-pool redemption guides (OCEAN, Braiins, ViaBTC, F2Pool, solo pools,
  Testnet4) are in [docs/redeem/](docs/redeem/README.md).

To run it live on an internet-facing machine (Docker Compose with
phoenixd and Caddy for TLS), see [deploy/README.md](deploy/README.md).

Rehearse the whole loop without money or hardware:

```bash
hashu mint run --lightning fake --price fixed --sats-per-thh 40
cargo run -p hashu --example buy_and_redeem -- \
  http://127.0.0.1:8085 bolt11 2 stratum+tcp://216.230.225.61:3333 yourname.worker1
```

Or do the same from a browser: open `http://127.0.0.1:8085/checkout/` to buy
and redeem, and `http://127.0.0.1:3334/` to watch it as the operator.

## Workspace layout

```
hashu-core/    Pure data structures: share commitment tree, hashprice trait
hashu-mint/    Cashu mint integration, manifest publisher, oracle adapters
hashu-proxy/   Stratum V1 listener and forwarder
hashu-cli/     The 'hashu' operator binary
```

## Tests

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The proxy has an integration test (`hashu-proxy/tests/integration.rs`)
that wires fake-miner ⇄ proxy ⇄ fake-pool over loopback and asserts on
`ProxyMetrics`.

## Design

[ARCHITECTURE.md](ARCHITECTURE.md) is the source of truth. Sections of
particular note:

- §1 Vision (self-hosted model)
- §4.1 Cashu mint (THH unit, NUTs in scope)
- §4.2 Stratum proxy
- §4.4 Hashprice oracle adapter
- §4.7.1 Operator manifest (the kind 0 binding pattern)
- §4.7.2 Share commitment tree (PoW-only attestation, RFC 6962)
- §5 Voucher types & lifecycle
- §6 Trust model
- §9 Phased roadmap

## Contributing

This project is in early scaffolding. Expect API churn and
unstabilized internals. Issues, design feedback, and pull requests are
welcome via GitHub.

## License

Dual-licensed under either of:

- Apache License 2.0 (see [LICENSE-APACHE](LICENSE-APACHE))
- MIT License (see [LICENSE-MIT](LICENSE-MIT))

Contributions submitted are licensed under both.
