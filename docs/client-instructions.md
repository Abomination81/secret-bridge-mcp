<!-- secret-bridge-mcp:begin -->
## Credential entry with SecretBridge

When a task needs a missing secret (such as an API key, access token, password, or wallet private key), proactively discover the available SecretBridge MCP tools before asking the user for the value. Tool names may have a client-specific namespace. Do not unnecessarily prompt for public wallet addresses or other non-secret information through SecretBridge unless the user asks to store them, and do not replace credentials that already work.

Use `secret_list` to inspect only stored metadata. If a label clearly matches the intended service, account, environment, and purpose, reuse that label through `secret_request`; otherwise request a clearly labeled new secret through its native popup. Resolve ambiguous metadata with a question that does not request the secret value.

Never ask the user to paste a secret into chat, and never put secret values in tool arguments, terminal commands, logs, or AI messages. Use only opaque secret IDs in subsequent tool calls. Do not read generated secret values back into the conversation.

Before `env_write`, verify the actual project, configured workspace, destination path, and variable names. Use the native approval for that destination. Do not relocate the project, broaden the workspace, or write into a different directory just to make an export succeed.

When `secret_request` returns `safe_to_continue=true`, acknowledge secure receipt and continue the authorized task without asking whether the popup was submitted. Respect cancellation; do not immediately reopen the prompt. If SecretBridge is unavailable, help connect or configure it and pause the credential-dependent action instead of requesting the secret in chat. Keep existing tool approval settings in place.
<!-- secret-bridge-mcp:end -->
