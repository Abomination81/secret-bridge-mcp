# Changelog

All notable changes to SecretBridge are documented here.

## 0.3.0 - 2026-10-30

- Added `--workspace-per-request` for shared MCP installations. Every call carries its current project directory, so new chats cannot inherit a globally pinned previous project.
- Reject missing, invalid, or mismatched workspace arguments before any prompt, credential retrieval, or write. Each request uses an immutable workspace snapshot; existing per-credential grants still apply.
- Include the resolved workspace in secret receipts, metadata listings, and env-write receipts. Updated client defaults and configuration examples to prevent global MCP entries being repointed between chats.
- Added regression coverage for interleaved workspaces, concurrent stdio sessions launched from another directory, and exports confined to the requesting project.

## 0.2.1 - 2026-10-28

- Let macOS credential popups grow to fit the request message instead of clipping it into a two-line area.
- Measure wrapped text at its actual width, including line breaks and long client names, so message endings and footer text remain readable.
- Show a persistent scrollbar when a request or confirmation is too long to fit on screen, start at the top, and keep the input and buttons visible.

## 0.2.0 - 2026-10-22

- Replaced the neon macOS popup with AppKit: system controls and typography, a lock symbol, a secure native field, and Finder-style workspace breadcrumbs. Windows uses a simplified light layout.
- Made the macOS dialog draggable by its background and bounded long request details in a scroll area so the field and buttons remain visible. Added a dedicated drag surface to the Windows layout.
- Added standard macOS editing commands, including ⌘V, Return to submit, and Escape to cancel. Initial focus no longer steals focus back from other controls.
- Kept entry dialogs compatible with older running brokers during an in-place update; reconnecting activates the new consent behavior.
- Save & Use now authorizes storage and env writes in the displayed workspace without a second approval. Existing workspace grants follow the same behavior. Cross-workspace grants, deletion confirmations, and all destination checks remain in place.
- Updated MCP receipts and client instructions to make the authorization explicit.

- Added a one-time setup for Codex and Claude Code to prefer SecretBridge whenever a credential is needed, with a shared instruction template and portable scripts that preserve existing user preferences.

## 0.1.5 - 2026-09-04

- Made the secure-input guard an explicit RAII type on Windows as well as macOS, keeping teardown semantics consistent and satisfying the Windows CI lint gate.

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
