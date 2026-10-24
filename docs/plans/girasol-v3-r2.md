# Girasol r2 execution plan

Scope: continue 9c035c9, preserve existing code/branch, no Elysium edits, no model switch, no real transactions. Existing user request authorizes completing the current strategy design.

1. Reproduce cash-cost, global position limit and daily-stop exit defects in live_strategy tests (RED), then implement portfolio accounting with entry+exit costs, UTC-day reset, global exposure/count limits and sticky kill-switch. Invalid values fail closed. Preserve marked estimated accounting versus real fills.
2. Ask configured Gemini/Cliproxy for strict JSON buy/sell/hold. Untrusted model output cannot change limits, token, price or live authorization. Timeout/error/malformed output causes no ledger trade. Read GIRASOL_LLM_API_KEY from environment (or encrypted slot); no hardcoded credentials. Default model gemini-3.8-flash with explicit optional override.
3. Wire authenticated panel strategy status/config/enable/kill/tick APIs for Solana/Robinhood only. Independent market observation feeds brain; calls are operator-triggered and always dry-run. Check state revision after async model request; no send adapter in these routes. Persist simulated portfolio only; enable is nonpersistent.
4. Extend dashboard with actual strategy controls/status and Gemini option. Run targeted tests then full cargo test --offline --locked. Build real binary, smoke both startup modes and strategy HTTP safety; capture real dashboard/AI/strategy/network screens at127.0.0.1:8080 into mission panel-screens.
5. Record source hashes, tests/commands, screenshots, dry-run deployment steps and remaining limits in REPORT-girasol-v3-r2.md. Commit scoped changes; do not deploy production or send transactions.
