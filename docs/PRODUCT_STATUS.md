# Product Status

Last reviewed: 23 October 2026 (v3 branch; network execution evidence remains separate from implementation).

This page separates public code from environment-dependent behavior and broader product direction. A feature description is not a promise of execution quality or financial performance.

## Public implementation

| Capability | Status and evidence boundary |
|---|---|
| Solana listeners, supported DEX mirror/replay, concurrent submission | Existing execution pipeline; inclusion and fills depend on providers/network |
| Rug gates, blacklist, authority/fee screening, trailing stop and post-buy detection | Continued from v2; not a universal honeypot detector |
| Dashboard | Always started, separate task; can run without trading config; network selector, RPC/tip vault settings, actual local positions/history/logs, paper strategy PnL |
| Key custody | Local AES-GCM/scrypt vault; wallet signing reads encrypted slots only; legacy provider environment/config fallbacks remain |
| Live authorization | `--real` plus fresh authenticated panel phrase; gate checked at concrete broadcast boundaries; paper/live holdings separated by file and data directory |
| EVM observation | Robinhood Nitro signed feed, pending WebSocket and bounded confirmed-block fallback; dedup and target filters |
| EVM replay | Own wallet/nonce, bounded value/gas/fees, RPC `eth_call`/`eth_estimateGas`, chain checks and signed EIP-1559 submission gated by panel |
| Elysium | Testnet chain 99801 and HYPE reference metadata; operator supplies verified RPC. Mainnet configuration template awaits authoritative ID/endpoints |
| Wallet intelligence | Bounded real history, activity timing, routes, conservative matched swap results and post-loss reentry, sample/coverage confidence |
| Underworld | Observed transfer counterparties/routes; probable relationships only, no proven identity; optional operator exchange labels |
| Strategy synthesis | Evidence-derived sizing/timing and paper hypotheses; independent market-price observations, paper holdings and TP/SL; daily refresh/ranking of saved targets |
| LLM | Local deterministic research without a key; optional keyed OpenAI/Anthropic/DeepSeek/GLM/Qwen |
| Telegram | Optional vaulted credentials and chat allowlist; research/status/paper commands; no remote live activation |

Implementation and fixture/socket tests establish local behavior, not profitability or live network acceptance. See [v3 operations](V3_OPERATIONS.md) for coverage and limits.

## Partial, unsupported or unverified

- Local `revm` state simulation is not implemented; EVM simulation is delegated to RPC.
- Arbitrary target transactions cannot be guaranteed to have equivalent effects from a different sender. Permit signatures, approvals, recipient bindings, contract state and unsupported envelopes can block or change replay. `allow_unmodified_calldata` is an explicit operator acknowledgement, not proof of safety.
- Elysium reference does not publish a complete DEX/router deployment map or RPC endpoint. Mainnet chain metadata/contracts and real live fills are unverified; November 9 is the requested preparation date, not proof of network readiness.
- Strategy-generated live trades are not implemented. Strategy mode is paper only. Thresholds without sufficient observed matched exits are labeled hypotheses.
- No verified free unauthenticated DeepSeek V4 endpoint is bundled.
- Wallet history is bounded (500 Solana signatures / 256 recent EVM blocks plus transfer logs); internal EVM traces and complete actor attribution are unavailable. Exchange classifications require operator labels.
- Historical matched PnL uses conservative recognized native/token swaps. Solana engine cost estimates inherit whole-wallet balance-delta attribution and can be affected by concurrent transactions. Paper PnL excludes fees, slippage and market impact. Missing evidence remains unavailable.
- No target-to-inclusion latency or live trading sample is claimed for Robinhood/Elysium. HTTP readiness/RPC round-trip observations are not trade execution metrics.

## Environment-dependent

Same-block outcomes, detection/submission latency, fills/slippage, provider availability, LLM/Telegram service availability and index/history completeness depend on the operator's environment. Record successful, failed and unknown submission outcomes; a returned transaction hash is not a confirmed fill. Already-submitted transactions cannot be recalled by revoking the local gate.

## Evidence standard

A public performance claim should identify:

- exact commit or release;
- dry-run or real mode;
- dates and sample size;
- endpoint regions and provider classes without exposing credentials;
- measurement definition and clock source;
- successful, failed and excluded observations;
- limitations and conflicts of interest.

## Token status

No official Girasol token exists as of 27 September 2026. See [girasolbot.com/token](https://girasolbot.com/token) for the authoritative anti-scam notice.

## Current quality gate note

Build and tests are blocking CI checks. Formatting and strict Clippy are initially visible but non-blocking because the imported public source predates enforced repository-wide style. They should become blocking after a dedicated formatting and warning-cleanup change, rather than mixing a large unrelated code rewrite into the trust hardening release.

Current dependency-audit status, including explicitly deferred Solana 2.x advisories, is published in [SECURITY_ADVISORIES.md](SECURITY_ADVISORIES.md).


## Token

The official Girasol token is live on Solana. Contract: `7twNHFHac7y4CzMfYtH6D1M1eDc72v7Czoh1sWUapump` — [BUY GIRASOL](https://pump.fun/coin/7twNHFHac7y4CzMfYtH6D1M1eDc72v7Czoh1sWUapump).
