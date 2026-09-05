# SecretBridge security model

## Supported versions

Security fixes are provided for the latest tagged release. Upgrade before reporting a problem that only occurs on an older version.

## Goal

SecretBridge is designed to prevent the common failure mode where a person pastes a credential into an AI conversation, terminal transcript, tool argument, or MCP result. The broker launches a one-shot prompt from its own executable. Only validated display metadata enters that process; the secret is entered and stored directly into the OS credential facility there and is never serialized back to the broker.

SecretBridge trusts the local user account and operating system. It is not an endpoint-security or malware-detection product. Once a user approves writing a `.env` file, that file is intentionally ordinary local plaintext.

The model may know:

- the credential's label and purpose;
- its suggested environment-variable name;
- an opaque `secret_id`;
- whether an operation succeeded;
- the approved env-file path and variable names.

The model must never receive the credential value.

## Trust boundaries

Trusted for this prototype:

- the local SecretBridge executable you installed;
- macOS Keychain or Windows Credential Manager;
- the person approving the native dialog;
- the operating-system user account and desktop session.

Not trusted:

- model output or MCP tool arguments;
- project files and repository content;
- secret labels or descriptions proposed by an AI;
- arbitrary paths proposed by an AI;
- an AI client's promise not to read a materialized file.

## Controls

- No API can reveal a raw value.
- The popup never writes a value to stdout, an environment variable, a command argument, or an IPC channel. The one-shot child writes it directly to the OS credential store; the broker receives only the child's exit status.
- Persistent broker processes own no native windows. A one-shot prompt process is created on demand, made mouse-active only when shown, made click-through before teardown, and terminated after completion or cancellation so the OS destroys every native surface it owns.
- Every creation and replacement gets a fresh random credential ID. The broker refuses to overwrite an existing ID.
- The server uses local stdio and opens no listening socket.
- Native secret entry uses a masked password field.
- macOS secret entry fails closed if Secure Event Input cannot be enabled. Clipboard contents are cleared immediately after a detected paste event.
- Values are stored by the OS credential facility under service `dev.secretbridge.mcp`.
- In-memory secret strings use best-effort zeroization after use.
- Metadata and audit records contain no values and use user-only permissions on macOS.
- Env paths are relative, revalidated after approval, confined to one canonical workspace, and limited to non-template dotenv filenames.
- Existing/parent symlinks and a symlinked `.gitignore` are rejected.
- Git-tracked targets are refused and Git ignore status is verified before writing.
- Existing env files and inbound JSON-RPC messages have size caps.
- Secret variables with public client-bundle prefixes are rejected.
- Labels reject line breaks, control characters, and Unicode bidirectional overrides.
- Cross-workspace reuse requires a one-time native grant.
- Env writes and deletions require a local dialog that defaults to cancel.
- The tool result reports names and status, never file contents.
- `.env*` is automatically gitignored while template files remain committable and cannot receive secrets.
- Registry writes use a lock and atomic replacement; credential/metadata creation and replacement have compensating rollback.
- Audit logs are locked and rotate at 5 MiB.

## Known limits

1. **A `.env` file is readable data.** Once approved and written, another local process—or an AI with sufficient filesystem access—may read it. File materialization is a compatibility feature, not the strongest isolation mode.
2. **Arbitrary local execution and secrecy are fundamentally in tension.** If a model can run any command with a credential in its environment, it can ask that command to print or transmit the credential. This prototype therefore does not expose a generic `run_with_secrets` tool.
3. **Same-user compromise is outside the threat model.** Malware may capture input, tamper with an executable, read a materialized `.env` file, or invoke OS credential APIs. Process-name blocklists create false confidence and are intentionally not used.
4. **Client identity is display context, not authentication.** `--client-name` can be chosen by whoever launches the server. Verify the requested label, destination, and variable names rather than trusting the client name alone.
5. **UI spoofing remains possible.** Install only a binary you built or verified, and do not approve surprising prompts.
6. **Memory zeroization is best effort.** GUI and OS APIs may make internal copies outside Rust's control.
7. **Windows Credential Manager is not an app-isolation boundary.** Generic credentials are readable by other processes under the same Windows user. This does not expose the value to MCP by itself, but it means SecretBridge does not protect a compromised Windows account.
8. **Screen-capture exclusion is not a security guarantee.** Platform window-exclusion APIs are advisory and do not cover cameras, kernel components, or all capture software. The UI does not claim that recording was detected or prevented.
9. **Filesystem race resistance is best effort in v0.1.** Paths and symlinks are checked again immediately before an atomic same-directory write, but a hostile same-user process that can mutate workspace directories concurrently remains inside the same-user-malware limitation.

## Possible future features

- Signed and notarized macOS binaries and Authenticode-signed Windows packages.
- A small tray companion that authenticates MCP server instances and shows a persistent audit UI.
- Per-secret grants such as “this workspace + this env name,” with expiry and revocation.
- Ephemeral, non-file injection adapters for specific trusted tools instead of a generic shell runner.
- Import/export through established password managers without revealing values to the model.

## Reporting a vulnerability

Do not include real credentials, keychain exports, or secret-bearing logs in a report. Provide a minimal reproduction using dummy values and describe the operating system, client, and SecretBridge version.

Use [GitHub private vulnerability reporting](https://github.com/abomination81/secret-bridge-mcp/security/advisories/new). Do not open a public issue for an unpatched vulnerability.
