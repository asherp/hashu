# Hashu

A hashrate proxy fused with a Cashu mint. Buyers pay Lightning, receive
blinded ecash vouchers denominated in hashrate-time (`THH` —
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
| Share commitment tree integration in the proxy | done | `hashu-proxy::stratum::{session,header,committer,compact}` |
| `hashrate` melt method via cdk (redemption ledger + `MintPayment` backend) | done | `hashu-core::redemption`, `hashu-mint::melt` |
| Tether (USDT) payment rail: buy `THH` with dollars (ERC-20 over EVM JSON-RPC) | done | `hashu-mint::tether` |
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

git clone https://github.com/<org>/hashu && cd hashu
cargo build --release
```

The binary lands at `target/release/hashu`. To install on `$PATH`:

```bash
cargo install --path hashu-cli
```

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
Redemption-driven upstream redirection lands in a follow-up commit.

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
