# Changelog

All notable changes to SecretBridge are documented here.

## 0.1.4 - 2026-09-04

- Moved each native prompt into a one-shot child process of the installed SecretBridge executable.
- The prompt process now terminates after approval, cancellation, or credential storage, making native-window destruction an operating-system guarantee rather than an eframe close handshake.
- The persistent MCP broker never creates a native window; prompt metadata enters the child over stdin, while secret values remain inside the child and are written directly to the OS credential store.
- Added click-through and hide-before-exit teardown so a slow credential-store operation cannot block desktop input.
- Reproduced the blank 520x540 macOS blocker and verified both secret-entry and confirmation flows leave zero WindowServer surfaces after completion.

## 0.1.3 - 2026-09-03

- Removed the persistent hidden native window from idle MCP broker processes.
- Native windows are now created only for an active prompt and destroyed immediately after submission or cancellation.
- Added mouse-passthrough-before-close teardown and a regression test that enforces the command order.
- Verified with three simultaneous macOS brokers that idle processes own zero WindowServer surfaces.

## 0.1.2 - 2026-09-03

- Classified the macOS MCP broker as an accessory UI element so persistent client connections do not create Dock or app-switcher icons.

## 0.1.1 - 2026-09-03

- Removed the blank translucent startup frame by deferring visibility until content is ready.
- Moved credential-store writes off the rendering thread so submission closes immediately.
- Reworked the secret-entry layout to prevent text, status, button, and footer overlap.
- Removed scrolling from the normal secret-entry dialog and made every disclosure visible.
- Fixed clipped rounded corners and transparent-window shadow artifacts on macOS.

## 0.1.0 - 2026-09-02

- Added native masked secret entry on macOS and Windows.
- Added reusable storage through macOS Keychain and Windows Credential Manager.
- Added opaque MCP secret IDs and explicit stored/reused completion receipts.
- Added approved, workspace-confined `.env` writing without returning values through MCP.
- Added secret deletion, metadata-only listing, and local metadata audit records.
- Added source installers, macOS/Windows CI, dependency auditing, and GitHub release packaging.
- Added Abomination81 branding, a neon-green native UI, and a responsive GitHub Pages installation site.
