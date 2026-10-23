# v3 operations and evidence limits

The local HTTP dashboard starts on `127.0.0.1:8078` with every launch, including an installation without a Solana config. Run `cargo run --release -- --panel-only` for research. The generated `data/panel-auth-token` is a bearer secret with mode 0600; read it locally and unlock the page. An optional `PANEL_AUTH_TOKEN` must be at least 16 characters. Keep the bind local or behind an authenticated private tunnel.

## Execution

Solana uses the existing `config.toml` (see `config.example.toml`). Put its signing key in the panel's `solana-wallet` slot. Live startup rejects plaintext signing-key settings; move and remove those before starting live. Settings/endpoints and the Solana signing key are loaded at engine startup; restart after changes. Hot reload updates only risk/sizing, TP/SL and mirror/replay policy fields; it preserves startup transports, listeners, target and wallet. A damaged or unreadable existing position snapshot stops engine startup instead of resetting holdings. Legacy configuration/env fallbacks remain supported for provider settings. Wallet/API secret values are never returned from the panel API.

Use `--real` and the panel phrase `I UNDERSTAND REAL FUNDS ARE AT RISK` to authorize live. Authorization is never saved across restarts. Revoking it pauses new live orders; transactions already submitted cannot be cancelled. Solana waits for initial confirmation before starting its live engine. Paper and live holdings use separate files in `SNIPER_DATA_DIR` (`positions.paper.json`, `positions.json`). When upgrading v2, move the old cwd `positions.json` to the selected data directory before starting the live engine; do not copy paper positions into it. Live sell orders paused by revocation remain subject to retry when the gate reopens.

Independent live strategy execution (`src/live_strategy.rs`) sits behind the same dual gate: the engine starts OFF, an explicit operator action enables it, and a kill-switch (`LiveEngine::kill`) stops everything until restart. Entry/exit decisions are the bot's own evaluation of independently observed market prices (the paper `decide` logic plus limits) — never a replay of another wallet's transaction. Fee/slippage costs are charged on both legs via configurable `fee_fraction` / `estimated_slippage_fraction`, and risk limits (`max_position_native`, `max_open_positions`, `daily_stop_net_native`) are separate from copy-mode caps. The daily stop and kill-switch hold even when the outer live gate is open; any network submission also passes `runtime::require_live_authorization()` at the broadcast boundary.

Use `--evm-config PATH` to start Robinhood or Elysium observation independently of Solana. Each config requires nonzero copy address, at least one different target and verified endpoints. The EVM engine copies destination, value and calldata with its own nonce and signer; native value, gas and fee caps apply. It calls the configured RPC's `eth_call` and `eth_estimateGas` with the copy wallet, checks chain ID and signing address, and checks the shared live gate directly before broadcast. Pending replacements from the same sender/source nonce are first-observation-wins, bounded to the most recent 8,192 source nonce keys; replacement gas changes do not create another copy. No signing key is needed for paper. Signing keys live in `evm_robinhood_private_key` / `evm_elysium_private_key` vault slots.

`eth_call` is provider simulation, not local revm. Calldata replay is not an assurance of equivalent economic effect: original recipients, permit signatures, approvals, sender restrictions and contract state can prevent or alter execution. Live exact-calldata replay additionally requires `allow_unmodified_calldata=true`. Do not replay an unreviewed route with real assets. Contract creation and protocol-specific system envelopes are subject to parser/support limits. Token spending caps and honeypot detection for arbitrary EVM contracts are not implemented.

Elysium testnet ID 99801 and HYPE wrapper metadata were observed from the supplied launchpad. Its public API does not provide an RPC URL or a complete router map. Mainnet template fields are deliberately invalid until authoritative chain ID/endpoints are supplied and `allow_new_chain` is explicitly enabled. Testnet addresses must not be assumed to exist on mainnet.

## Research and paper strategy

Authenticated `/api/panel/ai/` APIs accept JSON with `address`, optional `days` (1–90) and a `?network=solana|robinhood|elysium` selector: `profile`, `strategy`, `replan`, `underworld`, `paper`, `chat`, `status`. See the dashboard for corresponding controls. Analysis persists locally in the selected data directory; credentials stay in the separate encrypted vault.

History reads are bounded. Solana paginates signatures then parsed transactions; EVM scans a bounded recent block/log window and cannot infer internal transfers. Results expose coverage limitations. Failed transactions and transfer counterparts are observed facts; same ownership is never proven. Exchange labels, when supplied by the operator, must be treated as external assertions. Profit estimates require conservative matched swap evidence and do not classify every balance movement as trading profit.

Daily refresh recomputes saved profiles/strategy hypotheses. Paper decisions use separately fetched native-quoted market prices and paper cash, holding and TP/SL state; their PnL excludes fees, slippage and market impact. Strategy live execution and automatic selection of previously unseen targets are not available. Confidence is evidence coverage, not a calibrated return forecast. Unknown loss reactions or exchange attribution stay unknown.

Keyless mode is local deterministic research. Optional OpenAI/Anthropic/DeepSeek/GLM/Qwen calls send requested research context to that configured provider; they are opt-in API use, not background telemetry. No unauthenticated free DeepSeek V4 endpoint is bundled.

## LLM via CliproxyAPI (как у Олена)

Providers `gemini` and `deepseek` route through an OpenAI-compatible CliproxyAPI endpoint instead of their public APIs:

1. Run CliproxyAPI (default `http://127.0.0.1:8317/v1`; LAN address like `http://192.168.1.87:8317/v1` also works). Override the base with `GIRASOL_LLM_BASE_URL` (e.g. `GIRASOL_LLM_BASE_URL=http://127.0.0.1:8317/v1 same_block_sniper --panel-only`). Requests go to `{base}/chat/completions` with a Bearer header.
2. In the panel vault, set the `llm-gemini` slot (model `gemini-3.8-flash-high`) and/or the `llm-deepseek` slot (model `deepseek-v4-pro`). An empty or absent slot falls back to `local` evidence analysis.
3. Telegram autodetect prefers `gemini` first, then the other providers, only when its vault slot has a key. Model selection matches the observed Cliproxy catalog; `deepseek` no longer points at api.deepseek.com.
4. Verify: `curl -H "Authorization: Bearer KEY" http://127.0.0.1:8317/v1/models` should list `gemini-3.8-flash-high` and `deepseek-v4-pro`, then use the dashboard AI section or `/ask` on Telegram.

Telegram uses vaulted `telegram-bot-token` and allowlisted `telegram-chat-id`. It offers status, profile, strategy, paper and research chat only, with no remote live activation. It polls when credentials are configured. Telegram/LLM integration tests use local or parsing fixtures; live provider availability requires the operator's own credentials.

## Data and APIs

All `/api/panel/*` routes require `Authorization: Bearer TOKEN`; only HTML is public. Key POST requires a nonempty string; deletion uses DELETE. Requests have bounded size and timeout. Dashboard positions come from the engine snapshot; history comes from its actual event journal; empty accounts show no fabricated trades. A submission event is not a confirmed fill. Live PnL remains unavailable when matched cost/fee evidence is absent. Cost-basis estimates inherit the existing Solana whole-wallet balance-delta accounting; concurrent transactions can affect attribution, so these estimates are not an accounting ledger.

Local files: encrypted vault `apikeys.enc.json`, token `panel-auth-token`, analytics `intelligence.json`, engine events `trades.json` (last 10,000), dashboard snapshot `panel-positions.json`, and rotating `logs/sniper.log`. Back up the vault and passphrase together securely. Logs redact endpoint credentials and provider URL paths.

## Validation

Run `cargo check --locked` and `cargo test --locked`. Run the process smoke in `scripts/smoke_v3.py` against a freshly built binary; it uses an isolated temporary data directory and sends no transactions. Network round-trip measurements must be identified separately from detection-to-submission or on-chain inclusion latency. This release has no measured target-trade inclusion sample for Elysium/Robinhood.
