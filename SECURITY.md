# Security Policy

## Supported versions

The default branch and the latest tagged release, when one exists, receive security fixes. Older commits and untagged third-party builds are not supported.

## Reporting a vulnerability

Do not open a public issue for an exploitable vulnerability.

Start a private disclosure request through the official support bot, [@girasolsupportbot](https://t.me/girasolsupportbot), or the official X account, [@girasolbot](https://x.com/girasolbot). Send only a short non-destructive summary first so maintainers can arrange an appropriate private channel.

Include:

- affected commit or release;
- affected component;
- impact and realistic attack conditions;
- minimal reproduction steps with secrets removed;
- suggested mitigation, if known.

Never send seed phrases, wallet private keys, vault passphrases or live provider credentials.

We will acknowledge a valid report when operationally possible, investigate it, coordinate remediation and credit the reporter if requested and safe. No fixed response or bounty is promised.

## Security boundaries

- Use a dedicated limited-balance hot wallet.
- Keep the key-management panel on loopback or a private network.
- Start with `dry_run = true` and verify behavior before real execution.
- Treat RPC, relay and streaming providers as external trust dependencies.
- Review [the threat model](docs/THREAT_MODEL.md) before production use.

## Out of scope

- loss caused by market movement, slippage or copied strategy performance;
- attacks requiring a seed phrase or private key voluntarily disclosed by the operator;
- unsupported forks or modified binaries;
- denial of service against third-party RPC, relay, validator or chain infrastructure.
