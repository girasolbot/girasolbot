# INSTALL.md — Girasol setup from scratch

## 1. Prerequisites

| Tool | Required | Notes |
|------|----------|-------|
| Rust (stable) | yes | validated on 1.96; `rustup` is the easiest installer |
| Solana CLI | optional | handy for keypair generation (`solana-keygen`) |
| git | yes | to clone the repository |

```bash
# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"

# Solana CLI (optional)
sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"
```

## 2. Clone and build

```bash
git clone <repo-url> girasol
cd girasol
cargo build --release
```

> A vendored OpenSSL is used, so no system OpenSSL headers are needed.
> First-build time depends on CPU, memory and disk; Solana dependencies can take tens of minutes.

## 3. Create your configuration

```bash
cp config.example.toml config.toml
```

`config.example.toml` contains only placeholders. The fields you must edit:

| Field | Meaning |
|-------|---------|
| `target_wallet` | base58 pubkey of the wallet whose swaps you want to copy |
| `solana_rpc_urls` | HTTP JSON-RPC endpoint(s) |
| `solana_ws_urls`  | WebSocket endpoint(s) for `logsSubscribe` |
| `pump_fun_program` / `metadata_program` | leave defaults unless you know better |
| `listener_mode` | `shreds` (fastest), `geyser`, or `websocket` |
| `shreds_url` / `geyser_url` | endpoints for the fast listeners |
| `buy_amount` | SOL spent per copied buy |
| `tp_levels` / `sl_levels` | take-profit / stop-loss levels |
| `dry_run` | keep `true` until you have tested everything |

Everything else has sensible defaults — see the comments in
`config.example.toml`.

## 4. Where to get keys

Girasol needs credentials only for the network endpoints it talks to.
Two ways to provide them:

1. **config.toml** — simple, fine for a first run.
2. **API-key panel** (below) — encrypted at rest, recommended for anything
   longer-lived.

| Endpoint | What you need | Typical pricing |
|----------|---------------|-----------------|
| RPC HTTP / WSS | an RPC URL; the free public endpoint works, paid providers (Helius, ERPC, QuickNode, Alchemy…) are faster and rate-limit less | free tier available |
| ShredStream | a shred-stream endpoint from a provider that offers one (e.g. edge RPC providers with shred/gRPC access) | provider-specific |
| Yellowstone gRPC Geyser | a gRPC Geyser endpoint + access token | provider-specific |
| Jito Block Engine | no key needed; tips are paid per bundle | **tips** (you set the tip) |
| bloXroute | account + API token | subscription |
| NextBlock / ZeroSlot / Nozomi (Temporal) | API key | **tips**-based on some plans |
| Helius | API key (sender + enhanced RPC) | freemium |

**tips-friendly** = the provider is paid per-transaction tips rather than a
fixed subscription; you only pay when your transactions land. These are the
cheapest way to start and are highlighted in the panel.

Keys are placed in these config fields (all optional):

```toml
helius_api_key    = "..."
bloxroute_api_key = "..."
nextblock_api_key = "..."
zeroslot_api_key  = "..."
nozomi_api_key    = "..."
```

## 5. API-key panel

The panel is a small web UI + REST API built into the bot. It stores every
key in an encrypted local vault instead of plaintext config.

The panel starts automatically. You may set a token before starting the bot:

```bash
export PANEL_AUTH_TOKEN="a-long-random-string"   # min 16 chars
export SNIPER_PANEL_BIND="127.0.0.1:8078"        # optional, this is the default
export SNIPER_DATA_DIR="data"                    # optional, default ./data
cargo run --release
```

If `PANEL_AUTH_TOKEN` is unset, a random token is saved locally to `data/panel-auth-token` with mode 0600. Read that file locally to unlock the page. `cargo run --release -- --panel-only` starts the dashboard without trading config.

**What you get**

- Web UI at `http://127.0.0.1:8078/` — paste the token, then view/update
  RPC/tip endpoints, signing keys, provider slots, LLM and Telegram
  settings across three networks. Storing a provider slot alone does not
  activate that provider; engine configuration must enable its transport.
- REST API (same token, `Authorization: Bearer <PANEL_AUTH_TOKEN>`):

```
GET    /api/panel/keys          # masked status for all slots
POST   /api/panel/keys/{slot}   # body {"value": "<key>"}; non-empty string required; use DELETE to remove
DELETE /api/panel/keys/{slot}   # delete stored key
GET    /api/panel/health        # vault file health
```

**Storage details**

- File: `$SNIPER_DATA_DIR/apikeys.enc.json` — AES-256-GCM ciphertext only,
  permissions 0600, never committed (the `data/` dir is gitignored).
- Encryption key is derived from `VAULT_PASSPHRASE` if set, otherwise from a
  random per-installation secret in `data/.apikeys-secret` (0600).
- Reading a key through the panel always shows a masked form (`ab…cd90`);
  plaintext never leaves the vault except inside the bot process.
- Startup overrides are wired for Solana RPC HTTP/WSS, Jito/tip RPC,
  bloXroute/NextBlock and the vaulted wallet, plus EVM RPC/tip/signing.
  Other provider catalog entries retain their existing config integration.
  Restart the engine after changing transport/signing settings.

## 6. Run

```bash
# dry-run (default — no real transactions)
cargo run --release

# real mode — vaulted solana-wallet key + --real + fresh panel confirmation
cargo run --release -- --real
```

On first real-mode start the bot validates the keypair and exits if none is
configured. Stop with `Ctrl+C` — a session statistics summary is printed
(detections, buys, skips, sells, realized PnL).

## 7. Recommended rollout

1. **Dry-run with a real target wallet** for at least a few days. Watch the
   `TIMING detect/buy` logs and the statistics summary.
2. Verify your endpoint latencies (the bot logs per-provider timings).
3. Start real mode with a **small** `buy_amount` and low max positions.
4. Keep `$SNIPER_DATA_DIR/positions.json` backed up for live holdings;
   paper holdings use `positions.paper.json` in the same directory.

## 8. Troubleshooting

| Symptom | Fix |
|---------|-----|
| `Config validation failed` | a required field is empty or a URL has a wrong scheme — compare with `config.example.toml` |
| No detections | wrong `target_wallet`, or the WSS endpoint drops `logsSubscribe` — try another provider |
| `Shreds listener fatal error` | shred endpoint unreachable/unsupported — switch `listener_mode` to `geyser` or `websocket` |
| Buys skipped: duplicate | the same signature was seen twice (multi-listener dedup working as intended) |
| Panel returns 401 | missing/wrong `Authorization: Bearer` header |
| Panel authentication | Read `data/panel-auth-token`, or set `PANEL_AUTH_TOKEN` (16+ characters) |

## 9. Security checklist

- `config.toml` holds secrets in plaintext — keep it out of git (already
  gitignored) and out of backups you share.
- Prefer the panel vault for long-lived keys; rotate `VAULT_PASSPHRASE`
  deliberately: changing it does not re-encrypt existing data. Export or
  migrate with the original passphrase first; unreadable existing entries
  block writes to prevent key loss.
- Keep `SNIPER_PANEL_BIND` on a loopback/private interface; do not expose
  the panel to the public internet.
- The wallet keypair controls real funds. Use a dedicated hot wallet with a
  limited balance.