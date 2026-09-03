use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use keyring::Entry;
use serde::Deserialize;
use serde_json::{Value, json};
use zeroize::{Zeroize, Zeroizing};

use crate::env_file::{EnvValue, resolve_env_path, validate_env_name, write_env_file};
use crate::registry::{Registry, SecretRecord};
use crate::{AppConfig, SERVICE_NAME, ui};

const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_INTERACTIVE_REQUESTS_PER_MINUTE: usize = 10;

#[derive(Default)]
struct InteractionGate {
    recent: VecDeque<Instant>,
}

impl InteractionGate {
    fn check(&mut self) -> Result<(), String> {
        let now = Instant::now();
        while self
            .recent
            .front()
            .is_some_and(|instant| now.duration_since(*instant) >= Duration::from_secs(60))
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_INTERACTIVE_REQUESTS_PER_MINUTE {
            return Err("too many interactive requests; wait before opening another popup".into());
        }
        self.recent.push_back(now);
        Ok(())
    }
}

pub(crate) fn run_stdio(config: AppConfig) -> Result<(), String> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let registry = Registry::new(&config.data_dir);
    let mut interaction_gate = InteractionGate::default();
    let mut input = stdin.lock();
    let mut line = Vec::new();

    loop {
        let Some(oversized) = read_capped_line(&mut input, &mut line)
            .map_err(|error| format!("cannot read stdin: {error}"))?
        else {
            break;
        };
        if oversized {
            write_message(
                &mut stdout,
                error_response(Value::Null, -32600, "Request exceeds 1 MiB limit"),
            )?;
            continue;
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let request: Value = match serde_json::from_slice(&line) {
            Ok(value) => value,
            Err(_) => {
                write_message(
                    &mut stdout,
                    error_response(Value::Null, -32700, "Parse error"),
                )?;
                continue;
            }
        };
        let Some(response) = handle_message(&config, &registry, &mut interaction_gate, request)
        else {
            continue;
        };
        write_message(&mut stdout, response)?;
    }
    Ok(())
}

fn read_capped_line(reader: &mut impl BufRead, output: &mut Vec<u8>) -> io::Result<Option<bool>> {
    output.clear();
    let mut oversized = false;
    let mut saw_data = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(saw_data.then_some(oversized));
        }
        saw_data = true;
        let chunk_len = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if !oversized {
            let remaining = MAX_MESSAGE_BYTES.saturating_sub(output.len());
            let copy_len = chunk_len.min(remaining);
            output.extend_from_slice(&available[..copy_len]);
            if copy_len < chunk_len {
                oversized = true;
            }
        }
        let found_newline = available[..chunk_len].last() == Some(&b'\n');
        reader.consume(chunk_len);
        if found_newline {
            return Ok(Some(oversized));
        }
    }
}

fn write_message(writer: &mut impl Write, message: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *writer, &message)
        .map_err(|error| format!("cannot encode response: {error}"))?;
    writer
        .write_all(b"\n")
        .and_then(|()| writer.flush())
        .map_err(|error| format!("cannot write stdout: {error}"))
}

fn handle_message(
    config: &AppConfig,
    registry: &Registry,
    interaction_gate: &mut InteractionGate,
    request: Value,
) -> Option<Value> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let Some(object) = request.as_object() else {
        return Some(error_response(Value::Null, -32600, "Invalid Request"));
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.get("method").and_then(Value::as_str).is_none()
        || (!id.is_null() && !id.is_string() && !id.is_number())
    {
        return Some(error_response(Value::Null, -32600, "Invalid Request"));
    }
    if !object.contains_key("id") {
        return None;
    }
    let method = object.get("method").and_then(Value::as_str).unwrap_or("");

    let result = match method {
        "initialize" => initialize(&request),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions() })),
        "tools/call" => call_tool(config, registry, interaction_gate, &request),
        _ => return Some(error_response(id, -32601, "Method not found")),
    };

    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": error }],
                "isError": true
            }
        }),
    })
}

fn initialize(request: &Value) -> Result<Value, String> {
    let requested = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(SUPPORTED_PROTOCOLS[0]);
    let protocol = if SUPPORTED_PROTOCOLS.contains(&requested) {
        requested
    } else {
        SUPPORTED_PROTOCOLS[0]
    };
    Ok(json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "secret-bridge-mcp",
            "title": "SecretBridge",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": "Secret values never appear in MCP results. Use secret_request to obtain an opaque secret_id, then env_write to materialize it only after the user approves the exact local path and variable names. When secret_request returns safe_to_continue=true, tell the user SecretBridge confirmed secure receipt and proceed without asking whether the popup was submitted. Secret IDs are workspace-granted; request the same label to ask for a one-time grant in another workspace. Never ask the user to paste a secret into chat. Never attempt to read a generated .env file. Public-prefixed secret variables are refused."
    }))
}

fn call_tool(
    config: &AppConfig,
    registry: &Registry,
    interaction_gate: &mut InteractionGate,
    request: &Value,
) -> Result<Value, String> {
    let name = request
        .pointer("/params/name")
        .and_then(Value::as_str)
        .ok_or("missing tool name")?;
    let arguments = request
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    if matches!(name, "secret_request" | "env_write" | "secret_delete") {
        interaction_gate.check()?;
    }
    match name {
        "secret_request" => secret_request(config, registry, arguments),
        "secret_list" => secret_list(config, registry),
        "env_write" => env_write(config, registry, arguments),
        "secret_delete" => secret_delete(config, registry, arguments),
        _ => Err(format!("unknown tool: {name}")),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretRequestArgs {
    label: String,
    description: String,
    #[serde(default)]
    suggested_env_var: Option<String>,
    #[serde(default)]
    replace_existing: bool,
}

fn secret_request(
    config: &AppConfig,
    registry: &Registry,
    arguments: Value,
) -> Result<Value, String> {
    let args: SecretRequestArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid secret_request arguments: {error}"))?;
    let label = validate_text("label", args.label, 3, 120, false)?;
    let description = validate_text("description", args.description, 3, 500, true)?;
    let suggested_env_var = args
        .suggested_env_var
        .map(|name| {
            validate_env_name(&name)?;
            Ok::<_, String>(name)
        })
        .transpose()?;

    let existing = registry.find_by_label(&label)?;
    if let Some(record) = &existing
        && !args.replace_existing
    {
        let entry = keyring_entry(&record.id)?;
        match entry.get_password() {
            Ok(mut existing_value) => {
                existing_value.zeroize();
                let workspace = workspace_id(config);
                let record = if record
                    .allowed_workspaces
                    .iter()
                    .any(|allowed| allowed == &workspace)
                {
                    record.clone()
                } else {
                    let message = format!(
                        "Allow this stored secret in another workspace?\n\nLabel: {}\nWorkspace: {}\n\nThis grants future env writes in this workspace, but every env write still requires approval.",
                        record.label,
                        config.workspace_root.display()
                    );
                    if !ui::confirm(&config.client_name, "Grant workspace access", &message)? {
                        return Err("user denied workspace access to the stored secret".into());
                    }
                    registry.grant_workspace(&record.id, &workspace)?
                };
                let audit_recorded = audit_best_effort(registry.audit(
                    &config.client_name,
                    "secret_reused",
                    json!({ "secret_id": record.id, "label": record.label }),
                ));
                return secret_completion_response(
                    &record,
                    SecretCompletion::Reused,
                    audit_recorded,
                );
            }
            Err(keyring::Error::NoEntry) => {
                return Err("stored secret metadata exists but its credential is unavailable; retry with replace_existing=true".into());
            }
            Err(_) => {
                return Err("the operating-system credential store could not be read".into());
            }
        }
    }

    let replacing = existing.is_some();
    let provisional_id = format!("sb_{}", uuid::Uuid::new_v4().simple());
    let stored = ui::prompt_and_store_secret(
        &provisional_id,
        &config.client_name,
        &label,
        &description,
        suggested_env_var.as_deref(),
        existing.is_some(),
    );
    let stored = match stored {
        Ok(stored) => stored,
        Err(error) => {
            let _ =
                keyring_entry(&provisional_id).and_then(|entry| match entry.delete_credential() {
                    Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                    Err(_) => Err("cleanup failed".into()),
                });
            return Err(error);
        }
    };
    if !stored {
        return Err("user cancelled secret entry".into());
    }

    let record = if let Some(existing) = existing {
        let record = match registry.replace_with_id(
            &existing.id,
            provisional_id.clone(),
            label,
            description,
            suggested_env_var,
            &workspace_id(config),
        ) {
            Ok(record) => record,
            Err(error) => {
                let _ = keyring_entry(&provisional_id).and_then(|entry| {
                    entry
                        .delete_credential()
                        .map_err(|_| "cleanup failed".into())
                });
                return Err(error);
            }
        };
        let old_deleted = match keyring_entry(&existing.id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => true,
            Err(_) => false,
        };
        if !old_deleted {
            let rollback = registry.restore_record(&provisional_id, existing);
            let _ = keyring_entry(&provisional_id).and_then(|entry| {
                entry
                    .delete_credential()
                    .map_err(|_| "cleanup failed".into())
            });
            rollback?;
            return Err("could not remove the prior operating-system credential; replacement was rolled back".into());
        }
        record
    } else {
        match registry.insert_with_id(
            provisional_id.clone(),
            label,
            description,
            suggested_env_var,
            workspace_id(config),
        ) {
            Ok(record) => record,
            Err(error) => {
                let _ = keyring_entry(&provisional_id).and_then(|entry| {
                    entry
                        .delete_credential()
                        .map_err(|_| "cleanup failed".into())
                });
                return Err(error);
            }
        }
    };
    let audit_recorded = audit_best_effort(registry.audit(
        &config.client_name,
        if replacing {
            "secret_updated"
        } else {
            "secret_created"
        },
        json!({ "secret_id": record.id, "label": record.label }),
    ));
    secret_completion_response(
        &record,
        if replacing {
            SecretCompletion::Updated
        } else {
            SecretCompletion::Stored
        },
        audit_recorded,
    )
}

#[derive(Clone, Copy)]
enum SecretCompletion {
    Reused,
    Stored,
    Updated,
}

fn secret_completion_response(
    record: &SecretRecord,
    completion: SecretCompletion,
    audit_recorded: bool,
) -> Result<Value, String> {
    let (status, message, user_confirmed, secret_received, reused) = match completion {
        SecretCompletion::Reused => (
            "reused",
            format!(
                "An existing stored secret is available. Continue with secret_id {}. No secret value was returned to the AI.",
                record.id
            ),
            false,
            false,
            true,
        ),
        SecretCompletion::Stored => (
            "stored",
            format!(
                "Secret entry confirmed: the user submitted a non-empty value in the secure popup, and SecretBridge stored it in the operating-system credential store. Continue with secret_id {}. The secret value was not returned to the AI.",
                record.id
            ),
            true,
            true,
            false,
        ),
        SecretCompletion::Updated => (
            "updated",
            format!(
                "Secret entry confirmed: the user submitted a non-empty replacement value in the secure popup, and SecretBridge updated it in the operating-system credential store. Continue with secret_id {}. The secret value was not returned to the AI.",
                record.id
            ),
            true,
            true,
            false,
        ),
    };

    tool_success_with_message(
        message,
        json!({
            "status": status,
            "secret_id": record.id,
            "label": record.label,
            "user_confirmed": user_confirmed,
            "secret_received": secret_received,
            "secret_stored": true,
            "safe_to_continue": true,
            "reused": reused,
            "audit_recorded": audit_recorded
        }),
    )
}

fn secret_list(config: &AppConfig, registry: &Registry) -> Result<Value, String> {
    let workspace = workspace_id(config);
    let records = registry.list()?;
    let metadata: Vec<Value> = records
        .into_iter()
        .map(|record| {
            json!({
                "secret_id": record.id,
                "label": record.label,
                "description": record.description,
                "suggested_env_var": record.suggested_env_var,
                "created_at": record.created_at,
                "updated_at": record.updated_at
                ,"available_in_workspace": record.allowed_workspaces.iter().any(|allowed| allowed == &workspace)
            })
        })
        .collect();
    tool_success(json!({ "secrets": metadata }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvWriteArgs {
    path: String,
    entries: Vec<EnvEntryArg>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvEntryArg {
    name: String,
    secret_id: String,
}

fn env_write(config: &AppConfig, registry: &Registry, arguments: Value) -> Result<Value, String> {
    let args: EnvWriteArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid env_write arguments: {error}"))?;
    if args.entries.is_empty() || args.entries.len() > 50 {
        return Err("entries must contain between 1 and 50 mappings".into());
    }
    if args.path.len() > 512 {
        return Err("env path is too long (maximum 512 bytes)".into());
    }
    let target = resolve_env_path(&config.workspace_root, &args.path)?;
    let mut names = HashSet::new();
    let mut resolved: Vec<(String, SecretRecord)> = Vec::new();
    for mapping in args.entries {
        validate_env_name(&mapping.name)?;
        validate_secret_id(&mapping.secret_id)?;
        if !names.insert(mapping.name.clone()) {
            return Err(format!("duplicate environment variable: {}", mapping.name));
        }
        let record = registry
            .find_by_id(&mapping.secret_id)?
            .ok_or_else(|| format!("unknown secret_id: {}", mapping.secret_id))?;
        if !record
            .allowed_workspaces
            .iter()
            .any(|allowed| allowed == &workspace_id(config))
        {
            return Err(format!(
                "secret_id {} is not authorized for this workspace; request it by label first",
                mapping.secret_id
            ));
        }
        resolved.push((mapping.name, record));
    }

    let mapping_summary = resolved
        .iter()
        .map(|(name, record)| format!("{name}  <-  {}", record.label))
        .collect::<Vec<_>>()
        .join("\n");
    let message = format!(
        "Write secrets to:\n{}\n\n{}\n\nExisting unrelated variables will be preserved. The workspace .gitignore will be updated to ignore .env* files. The AI could read the resulting file if it has unrestricted filesystem access.",
        target.display(),
        mapping_summary
    );
    if !ui::confirm(&config.client_name, "Approve .env write", &message)? {
        return Err("user denied env file creation".into());
    }

    let mut values = Vec::with_capacity(resolved.len());
    for (name, record) in &resolved {
        let value = keyring_entry(&record.id)?
            .get_password()
            .map_err(|_| format!("stored value is unavailable for secret_id {}", record.id))?;
        values.push(EnvValue {
            name: name.clone(),
            value: Zeroizing::new(value),
        });
    }
    write_env_file(&config.workspace_root, &target, &values)?;
    drop(values);

    let audit_recorded = audit_best_effort(registry.audit(
        &config.client_name,
        "env_written",
        json!({
            "path": target,
            "variables": resolved.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "secret_ids": resolved.iter().map(|(_, record)| &record.id).collect::<Vec<_>>()
        }),
    ));
    tool_success(json!({
        "status": "written",
        "path": target,
        "variables": resolved.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        "audit_recorded": audit_recorded
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SecretDeleteArgs {
    secret_id: String,
}

fn secret_delete(
    config: &AppConfig,
    registry: &Registry,
    arguments: Value,
) -> Result<Value, String> {
    let args: SecretDeleteArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid secret_delete arguments: {error}"))?;
    validate_secret_id(&args.secret_id)?;
    let record = registry
        .find_by_id(&args.secret_id)?
        .ok_or_else(|| format!("unknown secret_id: {}", args.secret_id))?;
    let message = format!(
        "Permanently delete this stored credential?\n\nLabel: {}\nSecret ID: {}\n\nExisting .env files will not be changed.",
        record.label, record.id
    );
    if !ui::confirm(&config.client_name, "Delete secret", &message)? {
        return Err("user denied secret deletion".into());
    }
    registry
        .delete(&record.id)?
        .ok_or_else(|| "secret metadata disappeared before deletion".to_string())?;
    match keyring_entry(&record.id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(_) => {
            registry.restore_deleted(record.clone())?;
            return Err(
                "could not delete the operating-system credential; metadata was restored".into(),
            );
        }
    }
    let audit_recorded = audit_best_effort(registry.audit(
        &config.client_name,
        "secret_deleted",
        json!({ "secret_id": record.id, "label": record.label }),
    ));
    tool_success(json!({
        "status": "deleted",
        "secret_id": record.id,
        "audit_recorded": audit_recorded
    }))
}

fn keyring_entry(secret_id: &str) -> Result<Entry, String> {
    Entry::new(SERVICE_NAME, secret_id)
        .map_err(|_| "could not access the operating-system credential store".to_string())
}

fn validate_text(
    name: &str,
    value: String,
    min: usize,
    max: usize,
    allow_newlines: bool,
) -> Result<String, String> {
    let trimmed = value.trim().to_string();
    crate::validation::validate_display_text(name, &trimmed, min, max, allow_newlines)?;
    Ok(trimmed)
}

fn validate_secret_id(id: &str) -> Result<(), String> {
    crate::validation::valid_secret_id(id)
        .then_some(())
        .ok_or_else(|| "invalid secret_id".into())
}

fn workspace_id(config: &AppConfig) -> String {
    config.workspace_root.to_string_lossy().into_owned()
}

fn audit_best_effort(result: Result<(), String>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => {
            eprintln!("secret-bridge-mcp: audit recording failed: {error}");
            false
        }
    }
}

fn tool_success(structured: Value) -> Result<Value, String> {
    let text = serde_json::to_string(&structured).map_err(|error| error.to_string())?;
    tool_success_with_message(text, structured)
}

fn tool_success_with_message(message: String, structured: Value) -> Result<Value, String> {
    Ok(json!({
        "content": [{ "type": "text", "text": message }],
        "structuredContent": structured,
        "isError": false
    }))
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "secret_request",
            "title": "Request a secret securely",
            "description": "Open a native password popup for a clearly labeled secret, store it in the OS credential store, and return an explicit receipt acknowledgement plus an opaque secret_id. When safe_to_continue is true, acknowledge secure receipt and continue. Reuses an existing matching label unless replace_existing is true. Never ask the user to put the value in chat.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "label": { "type": "string", "minLength": 3, "maxLength": 120, "description": "Specific human label, e.g. 'Stripe test secret key for Acme billing'" },
                    "description": { "type": "string", "minLength": 3, "maxLength": 500, "description": "Why the secret is needed and where it will be used" },
                    "suggested_env_var": { "type": "string", "maxLength": 128, "pattern": "^[A-Z_][A-Z0-9_]*$", "description": "Optional uppercase server-only environment variable name" },
                    "replace_existing": { "type": "boolean", "default": false }
                },
                "required": ["label", "description"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
        }),
        json!({
            "name": "secret_list",
            "title": "List stored secret metadata",
            "description": "List labels, descriptions, opaque IDs, and timestamps. Secret values are never returned.",
            "inputSchema": { "type": "object", "additionalProperties": false },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false }
        }),
        json!({
            "name": "env_write",
            "title": "Write approved secrets to an env file",
            "description": "After a native approval dialog, retrieve secrets locally and merge them into a .env, .env.*, or .dev.vars file under the configured workspace. Values never pass through MCP. Also ensures .env* is gitignored.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "minLength": 4, "maxLength": 512, "description": "Relative path under the configured workspace, e.g. '.env.local'" },
                    "entries": {
                        "type": "array", "minItems": 1, "maxItems": 50,
                        "items": {
                            "type": "object", "additionalProperties": false,
                            "properties": {
                                "name": { "type": "string", "maxLength": 128, "pattern": "^[A-Z_][A-Z0-9_]*$", "description": "Uppercase server-only environment variable name" },
                                "secret_id": { "type": "string", "minLength": 35, "maxLength": 35, "pattern": "^sb_[0-9a-f]{32}$", "description": "Opaque ID returned by secret_request or secret_list" }
                            },
                            "required": ["name", "secret_id"]
                        }
                    }
                },
                "required": ["path", "entries"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
        }),
        json!({
            "name": "secret_delete",
            "title": "Delete a stored secret",
            "description": "After a native confirmation, permanently remove a secret from the OS credential store and metadata registry. Existing env files are not modified.",
            "inputSchema": {
                "type": "object", "additionalProperties": false,
                "properties": { "secret_id": { "type": "string", "minLength": 35, "maxLength": 35, "pattern": "^sb_[0-9a-f]{32}$" } },
                "required": ["secret_id"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false }
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn initialization_never_mentions_secret_values() {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        });
        let result = initialize(&request).unwrap();
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
    }

    #[test]
    fn tools_are_listed_without_touching_keychain_or_ui() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let config = AppConfig::for_test(workspace.path(), data.path());
        let registry = Registry::new(data.path());
        let request = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" });
        let response =
            handle_message(&config, &registry, &mut InteractionGate::default(), request).unwrap();
        assert_eq!(response["result"]["tools"].as_array().unwrap().len(), 4);
    }

    #[test]
    fn newly_stored_secret_explicitly_tells_the_ai_to_continue() {
        let record = SecretRecord {
            id: "sb_test_id".into(),
            label: "Test API key".into(),
            description: "Used only for this acknowledgement test".into(),
            suggested_env_var: Some("TEST_API_KEY".into()),
            created_at: 1,
            updated_at: 1,
            allowed_workspaces: vec!["/test".into()],
        };

        let result = secret_completion_response(&record, SecretCompletion::Stored, true).unwrap();
        assert_eq!(result["structuredContent"]["status"], "stored");
        assert_eq!(result["structuredContent"]["user_confirmed"], true);
        assert_eq!(result["structuredContent"]["secret_received"], true);
        assert_eq!(result["structuredContent"]["secret_stored"], true);
        assert_eq!(result["structuredContent"]["safe_to_continue"], true);
        let message = result["content"][0]["text"].as_str().unwrap();
        assert!(message.contains("Secret entry confirmed"));
        assert!(message.contains("Continue with secret_id sb_test_id"));
        assert!(!message.contains("secret-value-for-test"));
    }

    #[test]
    fn reused_secret_does_not_claim_the_user_entered_it_again() {
        let record = SecretRecord {
            id: "sb_existing_id".into(),
            label: "Existing API key".into(),
            description: "Used only for this acknowledgement test".into(),
            suggested_env_var: None,
            created_at: 1,
            updated_at: 1,
            allowed_workspaces: vec!["/test".into()],
        };

        let result = secret_completion_response(&record, SecretCompletion::Reused, true).unwrap();
        assert_eq!(result["structuredContent"]["status"], "reused");
        assert_eq!(result["structuredContent"]["user_confirmed"], false);
        assert_eq!(result["structuredContent"]["secret_received"], false);
        assert_eq!(result["structuredContent"]["secret_stored"], true);
        assert_eq!(result["structuredContent"]["safe_to_continue"], true);
    }

    #[test]
    fn capped_reader_drains_an_oversized_request() {
        let mut bytes = vec![b'x'; MAX_MESSAGE_BYTES + 1];
        bytes.extend_from_slice(b"\n{}\n");
        let mut reader = std::io::Cursor::new(bytes);
        let mut line = Vec::new();
        assert_eq!(
            read_capped_line(&mut reader, &mut line).unwrap(),
            Some(true)
        );
        assert_eq!(
            read_capped_line(&mut reader, &mut line).unwrap(),
            Some(false)
        );
        assert_eq!(line, b"{}\n");
    }

    #[test]
    fn rejects_spoofed_labels_and_invalid_ids() {
        assert!(validate_text("label", "line\nbreak".into(), 3, 120, false).is_err());
        assert!(validate_text("label", "safe\u{202e}txt".into(), 3, 120, false).is_err());
        assert!(validate_secret_id("sb_not-an-id").is_err());
        assert!(validate_secret_id("sb_0123456789abcdef0123456789abcdef").is_ok());
    }
}
