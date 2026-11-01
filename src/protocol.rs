use std::collections::{HashSet, VecDeque};
use std::io::{self, BufRead, Write};
use std::time::{Duration, Instant};

use keyring::Entry;
use serde::Deserialize;
use serde_json::{Value, json};
use zeroize::{Zeroize, Zeroizing};

use crate::env_file::{EnvValue, resolve_env_path, validate_env_name, write_env_file};
use crate::registry::{MAX_WORKSPACE_GRANTS, Registry, SecretRecord};
use crate::{AppConfig, SERVICE_NAME, ui};

const SUPPORTED_PROTOCOLS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const TOOL_NAMES: [&str; 4] = [
    "secret_request",
    "secret_list",
    "env_write",
    "secret_delete",
];
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
        let Some(response) = handle_payload(&config, &registry, &mut interaction_gate, request)
        else {
            continue;
        };
        write_message(&mut stdout, response)?;
    }
    Ok(())
}

/// Handles one JSON value from the wire: a single message or a JSON-RPC batch. A batch is
/// answered with an array of responses, omitting notifications, as JSON-RPC 2.0 requires.
fn handle_payload(
    config: &AppConfig,
    registry: &Registry,
    interaction_gate: &mut InteractionGate,
    payload: Value,
) -> Option<Value> {
    match payload {
        Value::Array(batch) if batch.is_empty() => {
            Some(error_response(Value::Null, -32600, "Invalid Request"))
        }
        Value::Array(batch) => {
            let responses: Vec<Value> = batch
                .into_iter()
                .filter_map(|message| handle_message(config, registry, interaction_gate, message))
                .collect();
            (!responses.is_empty()).then_some(Value::Array(responses))
        }
        message => handle_message(config, registry, interaction_gate, message),
    }
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
    if !id.is_null() && !id.is_string() && !id.is_number() {
        return Some(error_response(Value::Null, -32600, "Invalid Request"));
    }
    // A readable id is echoed even on an invalid request, so the client can match the reply.
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || object.get("method").and_then(Value::as_str).is_none()
    {
        return Some(error_response(id, -32600, "Invalid Request"));
    }
    if !object.contains_key("id") {
        return None;
    }
    let method = object.get("method").and_then(Value::as_str).unwrap_or("");

    let result = match method {
        "initialize" => initialize(config, &request),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tool_definitions(config) })),
        "tools/call" => match request.pointer("/params/name").and_then(Value::as_str) {
            Some(name) if TOOL_NAMES.contains(&name) => {
                call_tool(config, registry, interaction_gate, name, &request)
            }
            Some(name) => {
                return Some(error_response(id, -32602, &format!("Unknown tool: {name}")));
            }
            None => return Some(error_response(id, -32602, "Missing tool name")),
        },
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

fn initialize(config: &AppConfig, request: &Value) -> Result<Value, String> {
    let requested = request
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(SUPPORTED_PROTOCOLS[0]);
    let protocol = if SUPPORTED_PROTOCOLS.contains(&requested) {
        requested
    } else {
        SUPPORTED_PROTOCOLS[0]
    };
    let workspace_instructions = if config.workspace_per_request {
        "Every tool call requires workspace_root: the absolute project directory of the CURRENT task. Never reuse a previous chat's path or edit global MCP settings to switch projects. Missing scope is rejected. ".to_string()
    } else {
        format!(
            "This server is fixed to workspace {}. Supply workspace_root to verify it matches the current task. Do not use it for another project or repoint a shared global entry; use --workspace-per-request for a shared server. ",
            config.workspace_root.display()
        )
    };
    Ok(json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": {
            "name": "secret-bridge-mcp",
            "title": "SecretBridge",
            "version": env!("CARGO_PKG_VERSION")
        },
        "instructions": workspace_instructions + "Secret values never appear in MCP results. Use secret_request to obtain an opaque secret_id, then env_write with the same workspace_root to materialize it there. Save & Use authorizes writes in the displayed workspace without another prompt. Verify the receipt's workspace_root and intended relative path and variable names. When safe_to_continue=true, acknowledge secure receipt and continue. Reusing a stored credential in a different workspace requires its native workspace grant. Never ask for a secret in chat or read a generated .env file. Public-prefixed variables are refused."
    }))
}

fn call_tool(
    config: &AppConfig,
    registry: &Registry,
    interaction_gate: &mut InteractionGate,
    name: &str,
    request: &Value,
) -> Result<Value, String> {
    let arguments = request
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let (config, arguments) = scope_tool_arguments(config, arguments)?;
    match name {
        "secret_request" => secret_request(&config, registry, interaction_gate, arguments),
        "secret_list" => {
            if !arguments
                .as_object()
                .is_some_and(|object| object.is_empty())
            {
                return Err("secret_list accepts only workspace_root".into());
            }
            secret_list(&config, registry)
        }
        "env_write" => env_write(&config, registry, arguments),
        "secret_delete" => secret_delete(&config, registry, interaction_gate, arguments),
        _ => Err(format!("unknown tool: {name}")),
    }
}

fn scope_tool_arguments(
    config: &AppConfig,
    mut arguments: Value,
) -> Result<(AppConfig, Value), String> {
    let object = arguments
        .as_object_mut()
        .ok_or("tool arguments must be an object")?;
    let workspace = object.remove("workspace_root");
    let requested = workspace
        .as_ref()
        .map(|value| value.as_str().ok_or("workspace_root must be a string"))
        .transpose()?;
    Ok((config.for_workspace_request(requested)?, arguments))
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
    interaction_gate: &mut InteractionGate,
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
        && args.replace_existing
        && !available_in_workspace(record, config)
    {
        // Replacing changes the value every granted workspace receives, so it must come from a
        // workspace that already has access.
        return Err(format!(
            "\"{}\" is stored for another workspace. Request it without replace_existing first so the user can allow this workspace, then replace it. If its stored value is missing, delete it with secret_delete and request it again.",
            record.label
        ));
    }
    if let Some(record) = &existing
        && !args.replace_existing
    {
        let entry = keyring_entry(&record.id)?;
        match entry.get_password() {
            Ok(mut existing_value) => {
                existing_value.zeroize();
                let workspace = workspace_id(config);
                let record = if available_in_workspace(record, config) {
                    record.clone()
                } else {
                    // Refuse before asking: an approved grant that could not be saved would
                    // leave the user with a dialog that did nothing.
                    if record.allowed_workspaces.len() >= MAX_WORKSPACE_GRANTS {
                        return Err(format!(
                            "\"{}\" is already available in {MAX_WORKSPACE_GRANTS} workspaces. Use it from one of those, or delete it with secret_delete and store it again.",
                            record.label
                        ));
                    }
                    let message = format!(
                        "Allow this stored secret in another workspace?\n\nLabel: {}\nWorkspace: {}\n\nThis allows future .env writes in this workspace without another prompt. Those files are readable by local processes with filesystem access.",
                        record.label,
                        config.workspace_root.display()
                    );
                    interaction_gate.check()?;
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
                    config,
                    &record,
                    SecretCompletion::Reused,
                    audit_recorded,
                );
            }
            Err(keyring::Error::NoEntry) if available_in_workspace(record, config) => {
                return Err("stored secret metadata exists but its credential is unavailable; retry with replace_existing=true".into());
            }
            Err(keyring::Error::NoEntry) => {
                return Err("stored secret metadata exists but its credential is unavailable; delete it with secret_delete, then request it again".into());
            }
            Err(_) => {
                return Err("the operating-system credential store could not be read".into());
            }
        }
    }

    let replacing = existing.is_some();
    let provisional_id = format!("sb_{}", uuid::Uuid::new_v4().simple());
    interaction_gate.check()?;
    let stored = ui::prompt_and_store_secret(
        &provisional_id,
        &config.client_name,
        &label,
        &description,
        suggested_env_var.as_deref(),
        existing.is_some(),
        &workspace_id(config),
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
        config,
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
    config: &AppConfig,
    record: &SecretRecord,
    completion: SecretCompletion,
    audit_recorded: bool,
) -> Result<Value, String> {
    let (status, message, user_confirmed, secret_received, reused) = match completion {
        SecretCompletion::Reused => (
            "reused",
            format!(
                "An existing stored secret is available. Continue with secret_id {}; env writes in its granted workspace need no additional approval. No secret value was returned to the AI.",
                record.id
            ),
            false,
            false,
            true,
        ),
        SecretCompletion::Stored => (
            "stored",
            format!(
                "Secret entry confirmed: the user submitted a non-empty value in the secure popup, and SecretBridge stored it in the operating-system credential store. Continue with secret_id {}. Submission also authorizes env writes in this workspace; do not ask for another approval. The secret value was not returned to the AI.",
                record.id
            ),
            true,
            true,
            false,
        ),
        SecretCompletion::Updated => (
            "updated",
            format!(
                "Secret entry confirmed: the user submitted a non-empty replacement value in the secure popup, and SecretBridge updated it in the operating-system credential store. Continue with secret_id {}. Submission also authorizes env writes in this workspace; do not ask for another approval. The secret value was not returned to the AI.",
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
            "workspace_root": config.workspace_root,
            "label": record.label,
            "user_confirmed": user_confirmed,
            "secret_received": secret_received,
            "secret_stored": true,
            "safe_to_continue": true,
            "workspace_use_authorized": true,
            "env_write_requires_approval": false,
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
    tool_success(json!({ "workspace_root": config.workspace_root, "secrets": metadata }))
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
    env_write_with(config, registry, arguments, |id| {
        keyring_entry(id)?
            .get_password()
            .map(Zeroizing::new)
            .map_err(|_| format!("stored value is unavailable for secret_id {id}"))
    })
}

fn env_write_with(
    config: &AppConfig,
    registry: &Registry,
    arguments: Value,
    mut read_credential: impl FnMut(&str) -> Result<Zeroizing<String>, String>,
) -> Result<Value, String> {
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
        if !available_in_workspace(&record, config) {
            return Err(format!(
                "secret_id {} is not authorized for this workspace; request it by label first",
                mapping.secret_id
            ));
        }
        resolved.push((mapping.name, record));
    }

    // Save & Use (or a cross-workspace grant) already authorizes materialization here.
    // Validate every mapping before retrieving any values; the writer revalidates the file.
    let mut values = Vec::with_capacity(resolved.len());
    for (name, record) in &resolved {
        values.push(EnvValue {
            name: name.clone(),
            value: read_credential(&record.id)?,
        });
    }
    write_env_file(&config.workspace_root, &target, &values, &config.data_dir)?;
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
        "workspace_root": config.workspace_root,
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
    interaction_gate: &mut InteractionGate,
    arguments: Value,
) -> Result<Value, String> {
    let args: SecretDeleteArgs = serde_json::from_value(arguments)
        .map_err(|error| format!("invalid secret_delete arguments: {error}"))?;
    validate_secret_id(&args.secret_id)?;
    let record = registry
        .find_by_id(&args.secret_id)?
        .ok_or_else(|| format!("unknown secret_id: {}", args.secret_id))?;
    // Deletion works from any workspace, so a credential whose workspaces are gone can still be
    // removed. The confirmation says where the request comes from and who else uses it.
    let available = available_in_workspace(&record, config);
    let others = record
        .allowed_workspaces
        .len()
        .saturating_sub(usize::from(available));
    let message = format!(
        "Permanently delete this stored credential?\n\nLabel: {}\nSecret ID: {}\nRequested from: {}\n{}\n\nExisting .env files will not be changed.",
        record.label,
        record.id,
        config.workspace_root.display(),
        match (available, others) {
            (true, 0) => "Used only by this workspace.".to_string(),
            (true, 1) => "Also used by 1 other workspace.".to_string(),
            (true, count) => format!("Also used by {count} other workspaces."),
            (false, 1) => "Not used by this workspace; used by 1 other workspace.".to_string(),
            (false, count) => {
                format!("Not used by this workspace; used by {count} other workspaces.")
            }
        }
    );
    interaction_gate.check()?;
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

fn available_in_workspace(record: &SecretRecord, config: &AppConfig) -> bool {
    let workspace = workspace_id(config);
    record
        .allowed_workspaces
        .iter()
        .any(|allowed| allowed == &workspace)
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

fn tool_definitions(config: &AppConfig) -> Vec<Value> {
    let mut definitions = vec![
        json!({
            "name": "secret_request",
            "title": "Request a secret securely",
            "description": "Open a native password popup for a clearly labeled secret, store it in the OS credential store, and return an explicit receipt acknowledgement plus an opaque secret_id. Save & Use authorizes env writes in the displayed workspace without another approval. When safe_to_continue is true, acknowledge secure receipt and continue. Reuses an existing matching label unless replace_existing is true. Never ask the user to put the value in chat.",
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
            "description": "Retrieve workspace-authorized secrets locally and merge them into a .env, .env.*, or .dev.vars file under the requested workspace. Save & Use or a workspace grant already authorizes this; no second popup or permission question is needed. Values never pass through MCP. Also ensures .env* is gitignored.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "path": { "type": "string", "minLength": 4, "maxLength": 512, "description": "Relative path under workspace_root, e.g. '.env.local'" },
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
    ];
    for tool in &mut definitions {
        let schema = &mut tool["inputSchema"];
        if schema.get("properties").is_none() {
            schema["properties"] = json!({});
        }
        schema["properties"]["workspace_root"] = json!({
            "type": "string", "minLength": 1, "maxLength": 4096,
            "description": "Absolute existing project directory for the CURRENT task. Use the same path for secret_request and env_write. Never copy the previous chat's path."
        });
        if config.workspace_per_request {
            if schema.get("required").is_none() {
                schema["required"] = json!([]);
            }
            schema["required"]
                .as_array_mut()
                .unwrap()
                .push(json!("workspace_root"));
        }
    }
    definitions
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
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let config = AppConfig::for_test(workspace.path(), data.path());
        let result = initialize(&config, &request).unwrap();
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

        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let config = AppConfig::for_test(workspace.path(), data.path());
        let result =
            secret_completion_response(&config, &record, SecretCompletion::Stored, true).unwrap();
        assert_eq!(
            result["structuredContent"]["workspace_root"],
            json!(config.workspace_root)
        );
        assert_eq!(result["structuredContent"]["status"], "stored");
        assert_eq!(result["structuredContent"]["user_confirmed"], true);
        assert_eq!(result["structuredContent"]["secret_received"], true);
        assert_eq!(result["structuredContent"]["secret_stored"], true);
        assert_eq!(result["structuredContent"]["safe_to_continue"], true);
        assert_eq!(
            result["structuredContent"]["workspace_use_authorized"],
            true
        );
        assert_eq!(
            result["structuredContent"]["env_write_requires_approval"],
            false
        );
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

        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let config = AppConfig::for_test(workspace.path(), data.path());
        let result =
            secret_completion_response(&config, &record, SecretCompletion::Reused, true).unwrap();
        assert_eq!(result["structuredContent"]["status"], "reused");
        assert_eq!(result["structuredContent"]["user_confirmed"], false);
        assert_eq!(result["structuredContent"]["secret_received"], false);
        assert_eq!(result["structuredContent"]["secret_stored"], true);
        assert_eq!(result["structuredContent"]["safe_to_continue"], true);
        assert_eq!(
            result["structuredContent"]["workspace_use_authorized"],
            true
        );
        assert_eq!(
            result["structuredContent"]["env_write_requires_approval"],
            false
        );
    }

    #[test]
    fn workspace_grant_allows_env_write_without_a_second_prompt() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let data_path = data.path().join("private");
        let config = AppConfig::for_test(workspace.path(), &data_path);
        let registry = Registry::new(&data_path);
        let record = registry
            .insert_with_id(
                "sb_0123456789abcdef0123456789abcdef".into(),
                "Test credential".into(),
                "Integration test only".into(),
                Some("TEST_TOKEN".into()),
                workspace_id(&config),
            )
            .unwrap();
        let target = workspace.path().join(".env.local");
        std::fs::write(&target, "KEEP=yes\n").unwrap();
        let result = env_write_with(
            &config,
            &registry,
            json!({
                "path": ".env.local",
                "entries": [{"name": "TEST_TOKEN", "secret_id": record.id}]
            }),
            |id| {
                assert_eq!(id, record.id);
                Ok(Zeroizing::new("dummy-regression-value".into()))
            },
        )
        .unwrap();
        assert_eq!(result["structuredContent"]["status"], "written");
        assert_eq!(
            std::fs::read_to_string(target).unwrap(),
            "KEEP=yes\nTEST_TOKEN=\"dummy-regression-value\"\n"
        );
        assert!(!result.to_string().contains("dummy-regression-value"));
        let audit = std::fs::read_to_string(data_path.join("audit.jsonl")).unwrap();
        assert!(!audit.contains("dummy-regression-value"));
        assert!(audit.contains("env_written"));
    }

    #[test]
    fn automatic_env_write_rejects_unauthorized_workspace_and_unsafe_mappings() {
        let workspace = tempdir().unwrap();
        let other = tempdir().unwrap();
        let data = tempdir().unwrap();
        let data_path = data.path().join("private");
        let config = AppConfig::for_test(workspace.path(), &data_path);
        let registry = Registry::new(&data_path);
        let id = "sb_0123456789abcdef0123456789abcdef";
        registry
            .insert_with_id(
                id.into(),
                "Test credential".into(),
                "Integration test only".into(),
                None,
                other.path().to_string_lossy().into_owned(),
            )
            .unwrap();
        let args = json!({"path": ".env", "entries": [{"name": "TOKEN", "secret_id": id}]});
        let error = env_write_with(&config, &registry, args, |_| {
            panic!("must not read a credential before workspace authorization")
        })
        .unwrap_err();
        assert!(error.contains("not authorized for this workspace"));
        assert!(!workspace.path().join(".env").exists());

        registry
            .grant_workspace(id, &workspace_id(&config))
            .unwrap();
        for (path, name) in [
            ("../.env", "TOKEN"),
            (".env.example", "TOKEN"),
            (".env", "NEXT_PUBLIC_TOKEN"),
        ] {
            assert!(
                env_write_with(
                    &config,
                    &registry,
                    json!({
                        "path": path, "entries": [{"name": name, "secret_id": id}]
                    }),
                    |_| panic!("unsafe mapping must be rejected before retrieval")
                )
                .is_err()
            );
        }
        // A bad second mapping must prevent retrieval of the otherwise valid first one.
        assert!(
            env_write_with(
                &config,
                &registry,
                json!({
                    "path": ".env", "entries": [
                        {"name": "TOKEN", "secret_id": id},
                        {"name": "VITE_TOKEN", "secret_id": id}
                    ]
                }),
                |_| panic!("validate the entire batch first")
            )
            .is_err()
        );
        assert!(!workspace.path().join(".env").exists());
    }

    #[test]
    fn interleaved_workspace_requests_do_not_inherit_the_previous_chat() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let data = tempdir().unwrap();
        let mut server = AppConfig::for_test(first.path(), &data.path().join("private"));
        server.workspace_per_request = true;
        let registry = Registry::new(&server.data_dir);
        let first_root = first.path().canonicalize().unwrap();
        let second_root = second.path().canonicalize().unwrap();
        registry
            .insert_with_id(
                "sb_0123456789abcdef0123456789abcdef".into(),
                "First project credential".into(),
                "Dummy metadata only".into(),
                None,
                first_root.to_string_lossy().into_owned(),
            )
            .unwrap();
        let mut gate = InteractionGate::default();
        for (id, root, available) in [
            ("chat-one", &first_root, true),
            ("chat-two", &second_root, false),
            ("chat-one-again", &first_root, true),
        ] {
            let response = handle_message(
                &server,
                &registry,
                &mut gate,
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": {"name": "secret_list", "arguments": {"workspace_root": root}}
                }),
            )
            .unwrap();
            assert_eq!(response["id"], id);
            assert_eq!(response["result"]["isError"], false);
            assert_eq!(
                response["result"]["structuredContent"]["workspace_root"],
                json!(root)
            );
            assert_eq!(
                response["result"]["structuredContent"]["secrets"][0]["available_in_workspace"],
                available
            );
        }
        for tool in tool_definitions(&server) {
            assert!(
                tool["inputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("workspace_root"))
            );
            let response = handle_message(
                &server,
                &registry,
                &mut gate,
                json!({
                    "jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": tool["name"], "arguments": {}}
                }),
            )
            .unwrap();
            assert_eq!(response["result"]["isError"], true);
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("workspace_root is required")
            );
        }
        assert_eq!(server.workspace_root, first.path());
        assert!(server.workspace_per_request);
    }

    #[test]
    fn a_request_writes_only_to_its_own_workspace_even_if_another_call_intervenes() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let data = tempdir().unwrap();
        let mut server = AppConfig::for_test(first.path(), &data.path().join("private"));
        server.workspace_per_request = true;
        let args = |root: &std::path::Path| {
            json!({
                "workspace_root": root, "path": ".env.local",
                "entries": [{"name": "TOKEN", "secret_id": "sb_0123456789abcdef0123456789abcdef"}]
            })
        };
        let (first_call, first_args) = scope_tool_arguments(&server, args(first.path())).unwrap();
        let (second_call, second_args) =
            scope_tool_arguments(&server, args(second.path())).unwrap();
        let registry = Registry::new(&server.data_dir);
        registry
            .insert_with_id(
                "sb_0123456789abcdef0123456789abcdef".into(),
                "Dummy scope check".into(),
                "Regression test only".into(),
                None,
                workspace_id(&second_call),
            )
            .unwrap();
        let response = env_write_with(&second_call, &registry, second_args, |_| {
            Ok(Zeroizing::new("dummy-project-two-value".into()))
        })
        .unwrap();
        assert_eq!(
            response["structuredContent"]["workspace_root"],
            json!(second_call.workspace_root)
        );
        assert!(second.path().join(".env.local").exists());
        assert!(!first.path().join(".env.local").exists());
        assert!(!response.to_string().contains("dummy-project-two-value"));
        let error = env_write_with(&first_call, &registry, first_args, |_| {
            panic!("must not retrieve a credential for another workspace")
        })
        .unwrap_err();
        assert!(error.contains("not authorized for this workspace"));
        assert!(!first.path().join(".env.local").exists());
    }

    #[test]
    fn invalid_or_mismatched_workspace_arguments_never_change_the_server_scope() {
        let first = tempdir().unwrap();
        let second = tempdir().unwrap();
        let data = tempdir().unwrap();
        let pinned = AppConfig::new_with_data_dir(
            Some(first.path().into()),
            "Test client".into(),
            Some(data.path().join("private")),
        )
        .unwrap();
        assert!(
            scope_tool_arguments(&pinned, json!({"workspace_root": second.path()}))
                .unwrap_err()
                .contains("fixed workspace")
        );
        assert_eq!(
            scope_tool_arguments(&pinned, json!({}))
                .unwrap()
                .0
                .workspace_root,
            pinned.workspace_root
        );
        let mut dynamic = pinned.clone();
        dynamic.workspace_per_request = true;
        let file = second.path().join("not-a-directory");
        std::fs::write(&file, "test").unwrap();
        for invalid in [
            json!(null),
            json!(17),
            json!("relative/project"),
            json!(""),
            json!(file),
            json!(second.path().join("missing")),
        ] {
            assert!(scope_tool_arguments(&dynamic, json!({"workspace_root": invalid})).is_err());
        }
        assert!(scope_tool_arguments(&dynamic, json!({})).is_err());
        assert!(scope_tool_arguments(&dynamic, json!([])).is_err());
        assert_eq!(dynamic.workspace_root, pinned.workspace_root);
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
    fn batches_and_invalid_requests_follow_json_rpc() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let config = AppConfig::for_test(workspace.path(), data.path());
        let registry = Registry::new(data.path());
        let mut gate = InteractionGate::default();
        let mut handle = |payload| handle_payload(&config, &registry, &mut gate, payload);

        let batch = handle(json!([
            { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            { "jsonrpc": "2.0", "method": "notifications/initialized" },
            { "jsonrpc": "2.0", "id": "two", "method": "tools/list" }
        ]))
        .unwrap();
        let batch = batch.as_array().unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0]["id"], 1);
        assert_eq!(batch[1]["id"], "two");
        assert!(
            handle(json!([{ "jsonrpc": "2.0", "method": "notifications/initialized" }])).is_none()
        );
        assert_eq!(handle(json!([])).unwrap()["error"]["code"], -32600);

        let invalid = handle(json!({ "jsonrpc": "2.0", "id": 7, "method": 5 })).unwrap();
        assert_eq!(invalid["id"], 7);
        assert_eq!(invalid["error"]["code"], -32600);

        let unknown = handle(json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": { "name": "nope" }
        }))
        .unwrap();
        assert_eq!(unknown["id"], 8);
        assert_eq!(unknown["error"]["code"], -32602);
    }

    #[test]
    fn calls_that_open_no_popup_do_not_spend_the_popup_budget() {
        let workspace = tempdir().unwrap();
        let data = tempdir().unwrap();
        let config = AppConfig::for_test(workspace.path(), &data.path().join("private"));
        let registry = Registry::new(&config.data_dir);
        let secret_id = "sb_0123456789abcdef0123456789abcdef";
        registry
            .insert_with_id(
                secret_id.into(),
                "Elsewhere".into(),
                "Granted to another workspace".into(),
                None,
                "/another/workspace".into(),
            )
            .unwrap();
        let mut gate = InteractionGate::default();
        // Each call parses, resolves its mapping, and is refused at authorization: the furthest
        // env_write gets without reading the keychain. None of it opens a popup.
        for id in 0..(MAX_INTERACTIVE_REQUESTS_PER_MINUTE + 2) {
            let response = handle_message(
                &config,
                &registry,
                &mut gate,
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": { "name": "env_write", "arguments": {
                        "path": ".env", "entries": [{ "name": "TOKEN", "secret_id": secret_id }]
                    } }
                }),
            )
            .unwrap();
            assert!(
                response["result"]["content"][0]["text"]
                    .as_str()
                    .unwrap()
                    .contains("not authorized for this workspace")
            );
        }
        assert!(gate.recent.is_empty());
    }

    #[test]
    fn other_workspaces_credentials_cannot_be_replaced_from_here() {
        let workspace = tempdir().unwrap();
        let other = tempdir().unwrap();
        let data = tempdir().unwrap();
        let config = AppConfig::for_test(workspace.path(), &data.path().join("private"));
        let registry = Registry::new(&config.data_dir);
        let id = "sb_0123456789abcdef0123456789abcdef";
        registry
            .insert_with_id(
                id.into(),
                "Other project key".into(),
                "Belongs to another workspace".into(),
                None,
                other.path().to_string_lossy().into_owned(),
            )
            .unwrap();
        let mut gate = InteractionGate::default();
        let replace = secret_request(
            &config,
            &registry,
            &mut gate,
            json!({
                "label": "Other project key",
                "description": "Attempted replacement",
                "replace_existing": true
            }),
        )
        .unwrap_err();
        assert!(replace.contains("stored for another workspace"));
        assert!(gate.recent.is_empty());
        assert!(registry.find_by_id(id).unwrap().is_some());
    }

    #[test]
    fn rejects_spoofed_labels_and_invalid_ids() {
        assert!(validate_text("label", "line\nbreak".into(), 3, 120, false).is_err());
        assert!(validate_text("label", "safe\u{202e}txt".into(), 3, 120, false).is_err());
        assert!(validate_secret_id("sb_not-an-id").is_err());
        assert!(validate_secret_id("sb_0123456789abcdef0123456789abcdef").is_ok());
    }
}
