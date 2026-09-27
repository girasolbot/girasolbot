# Security Advisory Status

Last reviewed: 27 September 2026.

The CI dependency audit blocks newly detected RustSec vulnerabilities. The following advisory IDs are temporarily allowlisted because they enter through Solana 2.x transitive dependencies and cannot be replaced independently without a Solana SDK major migration:

| Advisory | Dependency path | Status |
|---|---|---|
| `RUSTSEC-2024-0344` | `solana-keypair` → `ed25519-dalek` → `curve25519-dalek 3.x` | Deferred to Solana SDK 3 migration |
| `RUSTSEC-2022-0093` | `solana-keypair` → `ed25519-dalek 1.x` | Deferred to Solana SDK 3 migration |
| `RUSTSEC-2026-0098` | Solana 2.x TLS graph → `rustls-webpki 0.101.x` | Deferred to Solana SDK 3 migration |
| `RUSTSEC-2026-0104` | Solana 2.x TLS graph → `rustls-webpki 0.101.x` | Deferred to Solana SDK 3 migration |
| `RUSTSEC-2026-0099` | Solana 2.x TLS graph → `rustls-webpki 0.101.x` | Deferred to Solana SDK 3 migration |

This allowlist is not a claim that the advisories are harmless. Operators should:

- use a dedicated limited-balance hot wallet;
- keep the software and key panel on a controlled host;
- avoid custom or untrusted certificate authorities;
- begin in dry-run mode;
- monitor the Solana SDK 3 migration before wider production use.

## Resolved in the 27 September 2026 patch batch

- `crossbeam-epoch 0.9.18 → 0.9.20`
- `h2 0.4.13 → 0.4.16`
- `quinn-proto 0.11.14 → 0.11.15`
- `rustls 0.23.37 → 0.23.45`
- `rustls-webpki 0.103.9 → 0.103.15`
- `anyhow 1.0.102 → 1.0.104`
- `event-listener 5.4.1 → 5.4.2`
- `rand 0.8.5 → 0.8.6`
- `rand 0.9.2 → 0.9.3`
- `lru 0.12.x → 0.18.x`

The allowlist must shrink, not grow, unless a new entry includes a documented dependency path, mitigation and removal plan.
