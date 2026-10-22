use std::io::{Read, Write};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::SERVICE_NAME;
use crate::{AppConfig, protocol};

#[cfg(any(test, not(target_os = "macos")))]
#[cfg_attr(target_os = "macos", allow(dead_code))]
mod fallback;
#[cfg(target_os = "macos")]
mod macos;

const MAX_PROMPT_REQUEST_BYTES: u64 = 65_536;
const PROMPT_CANCELLED_EXIT_CODE: u8 = 20;
const PROMPT_FAILED_EXIT_CODE: i32 = 21;

#[derive(Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Mode {
    Secret {
        secret_id: String,
        client: String,
        label: String,
        description: String,
        env_var: Option<String>,
        #[serde(default)]
        workspace: String,
        replacing: bool,
    },
    Confirm {
        client: String,
        title: String,
        message: String,
    },
}

fn validate_ui_text(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
    allow_newlines: bool,
) -> Result<(), String> {
    crate::validation::validate_display_text(name, value, min, max, allow_newlines)
}

fn valid_env_name(name: &str) -> bool {
    if name.len() > 128 {
        return false;
    }
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_uppercase())
        && chars.all(|character| {
            character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit()
        })
}

fn valid_secret_id(id: &str) -> bool {
    crate::validation::valid_secret_id(id)
}

#[cfg(any(test, not(target_os = "macos")))]
fn vault_message() -> &'static str {
    #[cfg(target_os = "macos")]
    return "Saved in macOS Keychain. Never sent to chat.";

    #[cfg(target_os = "windows")]
    return "Saved in Windows Credential Manager. Never sent to chat.";

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    return "Saved in your system credential store. Never sent to chat.";
}

pub(crate) fn prompt_and_store_secret(
    secret_id: &str,
    client: &str,
    label: &str,
    description: &str,
    env_var: Option<&str>,
    replacing: bool,
    workspace: &str,
) -> Result<bool, String> {
    if !valid_secret_id(secret_id) {
        return Err("invalid provisional secret ID".into());
    }
    validate_ui_text("workspace", workspace, 1, 4096, false)?;
    validate_ui_text("client", client, 1, 80, false)?;
    validate_ui_text("label", label, 3, 120, false)?;
    validate_ui_text("description", description, 3, 500, true)?;
    if env_var.is_some_and(|name| !valid_env_name(name)) {
        return Err("invalid environment variable name".into());
    }
    run_prompt_process(Mode::Secret {
        secret_id: secret_id.to_string(),
        client: client.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        env_var: env_var.map(str::to_string),
        workspace: workspace.to_string(),
        replacing,
    })
}

pub(crate) fn confirm(client: &str, title: &str, message: &str) -> Result<bool, String> {
    validate_ui_text("client", client, 1, 80, false)?;
    validate_ui_text("title", title, 1, 80, false)?;
    validate_ui_text("message", message, 1, 12_000, true)?;
    run_prompt_process(Mode::Confirm {
        client: client.to_string(),
        title: title.to_string(),
        message: message.to_string(),
    })
}

fn run_prompt_process(mode: Mode) -> Result<bool, String> {
    let executable = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|error| format!("cannot resolve the SecretBridge executable: {error}"))?;
    let mut command = Command::new(executable);
    command
        .arg("--native-prompt")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        // The MCP broker is a console binary. Suppress a transient console window for the
        // one-shot child while still allowing its native SecretBridge window to appear.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start the native prompt: {error}"))?;

    let write_result = child
        .stdin
        .take()
        .ok_or_else(|| "native prompt input is unavailable".to_string())
        .and_then(|mut input| {
            serde_json::to_writer(&mut input, &mode)
                .map_err(|error| format!("cannot encode native prompt request: {error}"))?;
            input
                .flush()
                .map_err(|error| format!("cannot send native prompt request: {error}"))
        });
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for the native prompt: {error}"))?;
    write_result?;

    match status.code() {
        Some(0) => Ok(true),
        Some(code) if code == i32::from(PROMPT_CANCELLED_EXIT_CODE) => Ok(false),
        Some(PROMPT_FAILED_EXIT_CODE) => {
            Err("the native prompt could not complete the requested operation".into())
        }
        Some(_) => Err("the native prompt exited unexpectedly".into()),
        None => Err("the native prompt was terminated unexpectedly".into()),
    }
}

fn ensure_entry_is_empty(entry: &keyring::Entry) -> Result<(), String> {
    match entry.get_password() {
        Ok(mut existing) => {
            existing.zeroize();
            Err("refusing to overwrite an existing credential ID".into())
        }
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(_) => Err("operating-system credential lookup failed".into()),
    }
}

pub fn run_desktop(config: AppConfig) -> Result<(), String> {
    protocol::run_stdio(config)
}

pub fn run_native_prompt_child() -> Result<u8, String> {
    let mut encoded = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_PROMPT_REQUEST_BYTES + 1)
        .read_to_end(&mut encoded)
        .map_err(|error| format!("cannot read native prompt request: {error}"))?;
    if encoded.len() as u64 > MAX_PROMPT_REQUEST_BYTES {
        return Err("native prompt request is too large".into());
    }
    let mode: Mode = serde_json::from_slice(&encoded)
        .map_err(|error| format!("invalid native prompt request: {error}"))?;

    let secure_input = match &mode {
        Mode::Secret {
            secret_id,
            client,
            label,
            description,
            env_var,
            workspace,
            ..
        } => {
            if !valid_secret_id(secret_id) {
                return Err("invalid provisional secret ID".into());
            }
            // Older running brokers omit this field; they retain their per-write approval.
            if !workspace.is_empty() {
                validate_ui_text("workspace", workspace, 1, 4096, false)?;
            }
            validate_ui_text("client", client, 1, 80, false)?;
            validate_ui_text("label", label, 3, 120, false)?;
            validate_ui_text("description", description, 3, 500, true)?;
            if env_var.as_deref().is_some_and(|name| !valid_env_name(name)) {
                return Err("invalid environment variable name".into());
            }
            Some(SecureInputGuard::try_new()?)
        }
        Mode::Confirm {
            client,
            title,
            message,
        } => {
            validate_ui_text("client", client, 1, 80, false)?;
            validate_ui_text("title", title, 1, 80, false)?;
            validate_ui_text("message", message, 1, 12_000, true)?;
            None
        }
    };

    #[cfg(target_os = "macos")]
    {
        macos::run(mode, secure_input)
    }
    #[cfg(not(target_os = "macos"))]
    {
        fallback::run_request_window(mode, secure_input)?;
        Ok(PROMPT_CANCELLED_EXIT_CODE)
    }
}

#[cfg(target_os = "macos")]
struct SecureInputGuard(bool);

#[cfg(target_os = "macos")]
impl SecureInputGuard {
    fn try_new() -> Result<Self, String> {
        #[link(name = "Carbon", kind = "framework")]
        unsafe extern "C" {
            fn EnableSecureEventInput() -> i32;
        }
        // SAFETY: This process owns the secret-entry window and pairs a successful call with
        // DisableSecureEventInput in Drop.
        if unsafe { EnableSecureEventInput() } == 0 {
            Ok(Self(true))
        } else {
            Err("macOS Secure Event Input could not be enabled; secret entry was blocked".into())
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for SecureInputGuard {
    fn drop(&mut self) {
        if self.0 {
            #[link(name = "Carbon", kind = "framework")]
            unsafe extern "C" {
                fn DisableSecureEventInput() -> i32;
            }
            // SAFETY: Balances the successful EnableSecureEventInput call made by new().
            let _ = unsafe { DisableSecureEventInput() };
        }
    }
}

#[cfg(not(target_os = "macos"))]
struct SecureInputGuard;

#[cfg(not(target_os = "macos"))]
impl SecureInputGuard {
    fn try_new() -> Result<Self, String> {
        Ok(Self)
    }
}

#[cfg(not(target_os = "macos"))]
impl Drop for SecureInputGuard {
    fn drop(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_ipc_contains_metadata_but_has_no_secret_value_field() {
        let mode = Mode::Secret {
            secret_id: "sb_0123456789abcdef0123456789abcdef".into(),
            client: "Test client".into(),
            label: "Test credential".into(),
            description: "Used only for serialization validation".into(),
            env_var: Some("TEST_CREDENTIAL".into()),
            workspace: "/test/workspace".into(),
            replacing: false,
        };
        let encoded = serde_json::to_value(mode).expect("serialize prompt metadata");
        let object = encoded.as_object().expect("prompt request object");

        assert_eq!(
            object.get("kind").and_then(serde_json::Value::as_str),
            Some("secret")
        );
        assert!(!object.contains_key("secret"));
        assert!(!object.contains_key("value"));
        assert!(!object.contains_key("password"));
    }
    #[test]
    fn older_brokers_can_still_open_entry_during_an_update() {
        let mode: Mode = serde_json::from_value(serde_json::json!({
            "kind": "secret", "secret_id": "sb_0123456789abcdef0123456789abcdef",
            "client": "Older client", "label": "Test credential",
            "description": "Legacy prompt metadata", "env_var": null, "replacing": false
        }))
        .unwrap();
        assert!(matches!(mode, Mode::Secret { workspace, .. } if workspace.is_empty()));
    }
}
