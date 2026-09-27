# Product Status

Last reviewed: 27 September 2026.

This page separates public code from environment-dependent behavior and broader product direction. A feature description is not a promise of execution quality or financial performance.

## Public in this repository

- Rust Solana execution engine.
- Dry-run enabled by default.
- WebSocket, Geyser and ShredStream listener paths.
- Supported DEX parsing and transaction-construction components.
- Concurrent configurable submission-provider fan-out.
- Encrypted local API-key vault and authenticated management panel.
- Position persistence and configurable risk controls.
- Automated unit and integration-style tests included in the Rust project.

## Environment-dependent

- Detection and submission latency.
- Same-block or first-block outcomes.
- Provider availability and transaction acceptance.
- Fill price, slippage and realized PnL.
- Reliability of third-party streams, relays, validators and RPC services.

Measure these in the operator's own environment. Record the observation window, region, providers, fees, target activity, success criteria and failed outcomes. Do not present selected successful transactions as guaranteed performance.

## Experimental or evolving

- Behavioral wallet intelligence and confidence-scored signals.
- Connected-wallet analysis.
- Automated strategy synthesis and updates.
- Robinhood Chain execution components maintained outside this public Rust repository.
- Further same-block optimization and provider-routing research.

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
