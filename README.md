# 🌻 girasol

> same-block copytrade for solana, rust-built

![girasol hero](docs/hero.jpg)

girasol watches a wallet you pick and mirrors its swaps in the same block. it uses shredstream/geyser or websocket for detection, pre-built transaction templates, and concurrent SWQoS submission to keep latency low end-to-end. your private keys stay in an encrypted local vault — and you can run it in dry-run mode before letting it touch real funds.

- 🔒 your keys, your vault
- 🧪 dry-run first
- ⚡ same-block execution

---

## How it works

```
┌────────────────────────────────────────────────────────────────────┐
│ listeners (pick one or combine — first detection wins)             │
│   shreds_listener   — Jito-style ShredStream gRPC (pre-block)      │
│   geyser_listener   — Yellowstone gRPC, direct tx parse            │
│   ws_listener       — logsSubscribe WebSocket fallback             │
└──────────────┬─────────────────────────────────────────────────────┘
               ▼
        copy_engine.rs
        target-wallet match, BUY/SELL discriminator parse,
        DEX detect (PumpFun / PumpSwap / Raydium V4 / CPMM)
               ▼
        buy executor ── blockhash cache (400ms refresh)
               │            leader cache, buy template (pre-built)
               ▼
        swqos_sender.rs — concurrent fan-out to all configured
        providers (Jito, bloXroute, NextBlock, ZeroSlot, Nozomi,
        Helius…), first submission wins, losers aborted
               ▼
        position_monitor.rs — TP/SL levels, mirror sells,
        positions.json persistence, duplicate protection
```

**Highlights**

- **Multi-listener detection** — ShredStream (raw shreds before block
  formation), Yellowstone gRPC Geyser, or plain WebSocket; run them in
  parallel, first detection wins, duplicate signatures deduped.
- **Pre-built transaction templates** — all mint-independent values are
  computed once at startup; per-trade work is a local PDA derivation.
- **Concurrent SWQoS submission** — one signed transaction fanned out to
  every configured provider simultaneously; losing provider tasks are
  aborted after the first success.
- **DEX support** — PumpFun, PumpSwap, Raydium V4, Raydium CPMM (buy + sell).
- **Risk management** — multi-level TP/SL, mirror sells, duplicate-buy
  protection, max-positions enforcement, position persistence.
- **API-key panel** — encrypted local vault (AES-256-GCM) for every provider
  key, masked display, bearer-auth REST + web UI. See
  [INSTALL.md](INSTALL.md#api-key-panel).
- **Shared vault** — one `apikeys.enc.json` (vault v2, scrypt) serves BOTH
  the Rust (Solana) and TS (EVM) bots. See the section below.
- **Dry-run by default** — no transaction is submitted unless you explicitly
  enable real mode.

## Shared vault

The API-key vault is **shared between both girasol bots** — the Rust Solana
bot (this repo) and the TS EVM bot (robinhood-bot-aws) read and write the
same `data/apikeys.enc.json` in the unified **vault v2** format:

```json
{
  "version": 2,
  "kdf": "scrypt",
  "kdf_params": { "N": 16384, "r": 8, "p": 1 },
  "salt": "<32-byte hex salt>",
  "slots": { "<slot_id>": { "iv": "<12-byte hex>", "ct": "<hex>", "tag": "<16-byte hex>" } }
}
```

- One file (`<DATA_DIR>/apikeys.enc.json`), one
  [`VAULT_PASSPHRASE`](INSTALL.md#api-key-panel) env var for both bots.
  Run both bots with the same `VAULT_PASSPHRASE`, or neither — otherwise
  the other bot's keys become undecryptable (they are skipped safely and
  re-encrypted on the next write with a valid passphrase).
- AES-256-GCM per slot; the AES key is `scrypt(passphrase, salt, 32)` with
  `N=16384, r=8, p=1` (identical in Rust and TS implementations).
- Legacy **v1** files (the old per-slot-salt Rust layout and the old
  single-salt TS layout) are still readable; the first write converts the
  whole file to v2, preserving every stored key.

Slot catalog (every slot carries a `network` field: `evm`, `solana` or
`both`):

| Slot | Network | Used for |
|---|---|---|
| `rpc-http` | solana | JSON-RPC reads (config `solana_rpc_urls`) |
| `rpc-wss` | solana | logsSubscribe feed (config `solana_ws_urls`) |
| `erpc` | solana | low-latency edge RPC / WSS |
| `geyser` | solana | Yellowstone gRPC stream |
| `shreds` | solana | ShredStream |
| `jito` | solana | SWQoS block engine |
| `bloxroute` | solana | QUIC/HTTP SWQoS |
| `nextblock` | solana | QUIC SWQoS |
| `zeroslot` | solana | SWQoS |
| `nozomi` | solana | SWQoS (Temporal) |
| `zerox` | evm | 0x swap quotes (TS bot) |
| `dexscreener` | evm | token metadata (TS bot) |
| `coingecko` | evm | price/metadata (TS bot) |
| `alchemy` | evm | archive/enhanced RPC (TS bot) |
| `quicknode` | evm | dedicated RPC (TS bot) |
| `infura` | evm | backup RPC (TS bot) |
| `explorer` | evm | Blockscout API (TS bot) |
| `helius` | **both** | sender/SWQoS + enhanced RPC — one shared slot |

Cross-bot round-trip is covered by a fixed test vector
(passphrase `roundtrip-test-1234`, slot `alchemy`, value `sk-test-vector-00`):
each repo's test-suite writes a v2 file "as the other bot would" and reads it
back — see `test_roundtrip_ts_written_v2_file` here and the mirror test in
the TS bot's `tests/api-key-vault.test.ts`.

## Repository layout

```
src/
  main.rs             orchestration, executors, shutdown/stats
  shreds_listener.rs  ShredStream gRPC listener (prost structs, no codegen)
  geyser_listener.rs  Yellowstone gRPC listener, direct tx parsing
  ws_listener.rs      logsSubscribe WebSocket listener
  copy_engine.rs      target-wallet detection + DEX parsing
  buyer.rs / tx_template.rs / tx_builder.rs   buy path
  swqos_sender.rs     concurrent provider fan-out
  helius_sender.rs    single-provider submission, tips, CU pricing
  quic_sender.rs      bloXroute (mTLS QUIC) + NextBlock (QUIC) senders
  rpc.rs              price/liquidity fetch, sell flow
  raydium_v4.rs / raydium_cpmm.rs / pumpswap.rs   DEX execution
  position_monitor.rs TP/SL + persistence
  api_key_vault.rs    encrypted API-key vault (AES-256-GCM)
  key_panel.rs        API-key management panel (REST + web UI)
proto/                ShredStream / shared proto definitions (reference)
idl/                  PumpFun IDL files
config.example.toml   example configuration (placeholders only)
```

## Requirements

- Rust 1.75+ (stable)
- A Solana RPC endpoint (free public RPC works; low-latency providers are
  better — see [INSTALL.md](INSTALL.md#where-to-get-keys))
- Optional: ShredStream / Geyser access for the fastest detection paths

## Quick start

```bash
git clone <repo-url> girasol && cd girasol
cp config.example.toml config.toml
# edit config.toml: set your target_wallet, RPC/WSS URLs
cargo run --release          # dry-run mode (default)
```

Full setup instructions — including where to get each key, the API-key
panel, and safe real-mode steps — are in [INSTALL.md](INSTALL.md).

## Safety

- `dry_run = true` by default. Real submissions require `dry_run = false`
  in `config.toml` or the `--real` flag **and** a loaded wallet keypair.
- `config.toml`, `positions.json` and the `data/` vault directory are
  gitignored — never commit them.
- No key material is ever logged or returned by the panel in plaintext.

## Legal / disclaimer

This software is provided for research and educational purposes, "AS IS",
without warranty of any kind. Trading crypto assets is risky; you can lose
your entire deposit. You are solely responsible for complying with the laws
of your jurisdiction and with the terms of service of any RPC or SWQoS
provider you use. Nothing in this repository is financial advice, and no
statement here is a promise of performance or profit.

## License

MIT — see [LICENSE](LICENSE).