# Girasol v3 implementation plan

Goal: extend the existing Solana engine with multichain execution, an always-on local dashboard, and evidence-based wallet research. Keep dry-run default; require --real and authenticated panel confirmation for live trading. No deployment or token changes.

1. B1: inspect local Robinhood reader and Elysium reference. Add chain configuration, target listeners, replay simulation and bounded execution; reject unknown mainnet metadata. Verify with parser, mock RPC, signing/gating tests and cargo check/test. Report network observations separately from execution latency.
2. B2: fix the panel blocking engine startup, start panel without trading configuration, generate a local auth token, add network/RPC/LLM/Telegram settings and real dashboard data. Verify authenticated HTTP smoke, malformed requests, UI against running binary.
3. B3: ingest real history, derive behavioural profiles and probable observed wallet graph, synthesize confidence-scored strategies and paper decisions, daily refresh and optional keyed LLM/Telegram. Verify fixtures with known timing, transfer and profit outcomes and local mock services.
4. B4: integrate runtime execution gate and trade data, preserve v2 rug/replay code; update README and PRODUCT_STATUS with demonstrated capabilities and exact limitations, capture actual UI screenshot. Run cargo check --locked, cargo test --locked and process/HTTP smoke. Commit small logical blocks and create branch PR if GitHub credentials permit.
5. Write /home/gera/.dsh/missions-v31/REPORT-girasol-v3.md with requirement matrix, tests, external availability and measured EVM latency definitions. Never substitute mock latency for on-chain results.

Architecture: retain current tokio/custom HTTP/vault stack; add focused evm, runtime, intelligence and telegram modules. Secrets remain vault-backed; panel API never returns them. No external free LLM endpoint or mainnet contract is assumed without evidence.
