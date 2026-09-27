# Threat Model

## Scope

This document covers the public Rust Solana bot, its local configuration, encrypted provider-key vault, signing wallet, network listeners, submission providers and local key-management panel.

It does not claim to eliminate market, smart-contract, validator, chain, provider or operator risk.

## Assets

- wallet private key and funds;
- vault passphrase and encrypted provider credentials;
- provider endpoints and access tokens;
- target-wallet and risk configuration;
- signed transactions and local position state;
- integrity of source, dependencies and release artifacts.

## Trust boundaries

### Operator host

The host can access decrypted secrets while the process runs. Malware, a compromised account or overly broad filesystem permissions can bypass application-level controls.

### Local panel

The panel is intended for loopback or private-network use with bearer authentication. Public exposure increases credential-theft and brute-force risk.

### RPC, streaming and submission providers

Providers can be unavailable, slow, inconsistent, rate-limited or malicious. Multiple providers improve resilience but expand the external trust surface.

### Solana and integrated programs

Transactions depend on chain state, validators and third-party programs. Reorgs, congestion, program changes, malicious tokens and unexpected account layouts can cause failure or loss.

### Dependency supply chain

Cargo dependencies, build tooling and future release pipelines can be compromised. Review lockfile changes and dependency advisories before production deployment.

## Primary threats and mitigations

| Threat | Existing or recommended mitigation |
|---|---|
| Wallet compromise | Dedicated limited-balance hot wallet; restrictive file permissions; never share seed material |
| Provider-key disclosure | AES-256-GCM vault; local storage; masked panel responses; secret-free logs |
| Accidental real execution | Dry-run default; explicit real-mode configuration and wallet validation |
| Public panel exposure | Loopback/private bind; bearer token; firewall and reverse-proxy controls if remote access is unavoidable |
| Malicious or stale provider data | Multiple sources; timeouts; validation; bounded retries; operator monitoring |
| Duplicate execution | Signature deduplication, position tracking and duplicate-buy protection |
| Excessive loss | Position limits, TP/SL configuration, limited wallet balance and staged rollout |
| Supply-chain compromise | Locked dependencies, CI checks, advisory scanning and review of dependency updates |
| Impersonation or fake token | Official-channel directory and synchronized contract publication after deployment |

## Operator checklist

1. Build from the official repository and review the commit used.
2. Keep `dry_run = true` until sanitized logs match expected behavior.
3. Use a dedicated wallet with only the amount you can lose.
4. Restrict the panel and local state files.
5. Rotate credentials after suspected exposure.
6. Verify provider URLs and TLS endpoints independently.
7. Monitor failed submissions, unexpected positions and balance changes.
8. Stop the process before changing sensitive configuration.

## Residual risk

Low latency does not guarantee inclusion, ordering or profitability. Encryption at rest does not protect secrets on a compromised running host. Risk controls reduce exposure but cannot prevent all losses.
