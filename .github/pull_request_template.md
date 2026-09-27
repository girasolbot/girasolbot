## Summary

Describe the focused change and why it is needed.

## Risk boundary

- [ ] No secret or production configuration is included.
- [ ] Dry-run-first behavior is preserved or the change is explicitly explained.
- [ ] Signing, vault, transaction, provider or panel exposure changes are documented.
- [ ] No guaranteed execution, latency or profit claim is introduced.

## Verification

- [ ] `cargo fmt --check`
- [ ] `cargo check --locked`
- [ ] `cargo test --locked`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
