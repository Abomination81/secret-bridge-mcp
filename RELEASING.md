# Releasing

SecretBridge uses GitHub Actions to test source and build convenience archives for macOS and Windows. The archives are unsigned until the project adds platform signing certificates; the README therefore recommends building from source.

## Before tagging

1. Update `version` in `Cargo.toml` and add the version to `CHANGELOG.md`.
2. Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`.
3. Test create, restart, reuse, replace, cross-workspace grant, env write, cancellation, and deletion with dummy credentials on macOS and Windows.
4. Confirm that the security boundary in `README.md` and `SECURITY.md` still matches the implementation.
5. Confirm the commit is clean and the GitHub CI and RustSec audit are green.

## Publish

Create and push an annotated version tag matching `Cargo.toml`, for example:

```sh
git tag -a v0.1.0 -m "SecretBridge MCP v0.1.0"
git push origin v0.1.0
```

The release workflow builds archives for Apple Silicon macOS, Intel macOS, and Windows x64, creates checksums and provenance attestations, and publishes a GitHub Release.

After publishing, download each archive on a clean machine, verify its checksum, and repeat the basic store/reuse/write flow using dummy values. Never use a real credential in release testing, screenshots, issues, or logs.
