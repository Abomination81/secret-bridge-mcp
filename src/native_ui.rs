use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;

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
        /// Set by brokers that keep the prompt's stdin open until it exits.
        #[serde(default)]
        watch_parent: bool,
    },
    Confirm {
        client: String,
        title: String,
        message: String,
        #[serde(default)]
        watch_parent: bool,
    },
}

impl Mode {
    fn watch_parent(&self) -> bool {
        match self {
            Mode::Secret { watch_parent, .. } | Mode::Confirm { watch_parent, .. } => *watch_parent,
        }
    }
}

/// The credential to store from what the field holds. Pasted keys often carry a trailing
/// newline or stray spaces, and none of that belongs to the credential.
fn credential_text(value: &str) -> &str {
    value.trim()
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
        watch_parent: true,
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
        watch_parent: true,
    })
}

fn prompt_executable() -> std::io::Result<PathBuf> {
    let path = std::env::current_exe()?;
    path.canonicalize().or_else(|error| {
        // After an in-place upgrade Linux reports "<path> (deleted)"; the installed path now
        // holds the new build, which accepts this broker's requests.
        path.to_str()
            .and_then(|text| text.strip_suffix(" (deleted)"))
            .ok_or(error)
            .and_then(|installed| PathBuf::from(installed).canonicalize())
    })
}

fn run_prompt_process(mode: Mode) -> Result<bool, String> {
    let executable = prompt_executable()
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

    let mut input = child.stdin.take();
    let write_result = input
        .as_mut()
        .ok_or_else(|| "native prompt input is unavailable".to_string())
        .and_then(|input| {
            serde_json::to_writer(&mut *input, &mode)
                .map_err(|error| format!("cannot encode native prompt request: {error}"))?;
            input
                .write_all(b"\n")
                .and_then(|()| input.flush())
                .map_err(|error| format!("cannot send native prompt request: {error}"))
        });
    // Hold stdin open while the prompt runs. If this broker dies, the pipe closes and the
    // prompt closes itself instead of storing a credential that no client will receive.
    let status = child
        .wait()
        .map_err(|error| format!("cannot wait for the native prompt: {error}"))?;
    drop(input);
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
    // Current brokers end the request with a newline and keep the pipe open; older ones send
    // the request alone and close it. Reading to the first newline handles both.
    let mut encoded = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_PROMPT_REQUEST_BYTES + 1)
        .read_until(b'\n', &mut encoded)
        .map_err(|error| format!("cannot read native prompt request: {error}"))?;
    if encoded.len() as u64 > MAX_PROMPT_REQUEST_BYTES {
        return Err("native prompt request is too large".into());
    }
    let mode: Mode = serde_json::from_slice(&encoded)
        .map_err(|error| format!("invalid native prompt request: {error}"))?;
    if mode.watch_parent() {
        exit_when_broker_exits();
    }

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
            ..
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

/// The broker holds this process's stdin open until the prompt exits, so end-of-file means it
/// is gone. Close the prompt then: a credential saved now would be stored under an ID that no
/// client receives, and the dialog would float with nothing waiting for its answer.
fn exit_when_broker_exits() {
    let _ = thread::Builder::new()
        .name("secret-bridge-broker-watch".into())
        .spawn(|| {
            let mut stdin = std::io::stdin();
            let mut discard = [0u8; 64];
            loop {
                match stdin.read(&mut discard) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            std::process::exit(i32::from(PROMPT_CANCELLED_EXIT_CODE));
        });
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
            watch_parent: true,
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
    fn pasted_line_endings_and_padding_are_not_part_of_the_credential() {
        assert_eq!(credential_text("sk-abc\n"), "sk-abc");
        assert_eq!(credential_text("  sk-abc \r\n"), "sk-abc");
        assert_eq!(credential_text("in ner"), "in ner");
        assert!(credential_text("\n\t ").is_empty());
    }

    #[test]
    fn only_current_brokers_ask_the_prompt_to_watch_for_them() {
        let current: Mode = serde_json::from_value(serde_json::json!({
            "kind": "confirm", "client": "Client", "title": "Delete secret",
            "message": "Message", "watch_parent": true
        }))
        .unwrap();
        assert!(current.watch_parent());
        let older: Mode = serde_json::from_value(serde_json::json!({
            "kind": "confirm", "client": "Client", "title": "Delete secret", "message": "Message"
        }))
        .unwrap();
        assert!(!older.watch_parent());
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
