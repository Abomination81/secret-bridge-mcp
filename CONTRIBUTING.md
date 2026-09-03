# Contributing to SecretBridge

Thanks for helping improve SecretBridge. Keep changes focused on its core promise: secret values must not enter AI messages, MCP payloads, logs, command arguments, or committed files.

## Development

Install the Rust toolchain selected by `rust-toolchain.toml`, then run:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

Use dummy values in all tests and screenshots. Do not paste a real credential into an issue, pull request, fixture, terminal transcript, or log.

## Pull requests

- Explain the user-visible behavior and supported platforms.
- Add or update tests for protocol, path, registry, or dotenv behavior.
- Update the README and security model when a trust boundary changes.
- Do not add a tool that reveals secret values through MCP.
- Keep the dependency graph small and commit `Cargo.lock` changes.

Report vulnerabilities privately as described in [SECURITY.md](SECURITY.md).
