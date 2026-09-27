# Contributing

## Before opening a change

- Use an issue for substantial behavioral changes.
- Never include wallet keys, seed phrases, API credentials, private endpoints or production configuration.
- Keep changes focused and preserve dry-run-first behavior.
- Do not add claims of guaranteed execution, profit or latency.

## Local checks

```bash
cargo fmt --check
cargo check --locked
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

## Pull requests

Describe the risk boundary, test evidence and configuration impact. Changes affecting signing, vault encryption, transaction construction, real-mode activation or network exposure require explicit security notes.

Report exploitable findings according to [SECURITY.md](SECURITY.md), not through a public issue.
