use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::secure_create_dir;

const MAX_REGISTRY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_AUDIT_BYTES: u64 = 5 * 1024 * 1024;
const MAX_RECORDS: usize = 10_000;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SecretRecord {
    pub id: String,
    pub label: String,
    pub description: String,
    pub suggested_env_var: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(default)]
    pub allowed_workspaces: Vec<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct RegistryData {
    version: u32,
    secrets: Vec<SecretRecord>,
}

pub struct Registry {
    data_dir: PathBuf,
}

impl Registry {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
        }
    }

    pub fn list(&self) -> Result<Vec<SecretRecord>, String> {
        self.with_lock(|data| Ok(data.secrets.clone()))
    }

    pub fn find_by_label(&self, label: &str) -> Result<Option<SecretRecord>, String> {
        self.with_lock(|data| {
            Ok(data
                .secrets
                .iter()
                .find(|record| record.label.eq_ignore_ascii_case(label))
                .cloned())
        })
    }

    pub fn find_by_id(&self, id: &str) -> Result<Option<SecretRecord>, String> {
        self.with_lock(|data| Ok(data.secrets.iter().find(|record| record.id == id).cloned()))
    }

    pub fn replace_with_id(
        &self,
        existing_id: &str,
        new_id: String,
        label: String,
        description: String,
        suggested_env_var: Option<String>,
        workspace: &str,
    ) -> Result<SecretRecord, String> {
        self.with_lock_mut(|data| {
            if existing_id != new_id && data.secrets.iter().any(|record| record.id == new_id) {
                return Err("replacement secret ID already exists".into());
            }
            let now = unix_time();
            let record = data
                .secrets
                .iter_mut()
                .find(|record| record.id == existing_id)
                .ok_or_else(|| "existing secret metadata disappeared".to_string())?;
            record.id = new_id;
            record.label = label;
            record.description = description;
            record.suggested_env_var = suggested_env_var;
            if !record
                .allowed_workspaces
                .iter()
                .any(|item| item == workspace)
            {
                record.allowed_workspaces.push(workspace.to_string());
            }
            record.updated_at = now;
            Ok(record.clone())
        })
    }

    pub fn insert_with_id(
        &self,
        id: String,
        label: String,
        description: String,
        suggested_env_var: Option<String>,
        workspace: String,
    ) -> Result<SecretRecord, String> {
        self.with_lock_mut(|data| {
            if data.secrets.iter().any(|record| record.id == id) {
                return Err("generated secret ID already exists".into());
            }
            if data
                .secrets
                .iter()
                .any(|record| record.label.eq_ignore_ascii_case(&label))
            {
                return Err("a secret with this label was created concurrently; retry".into());
            }
            let now = unix_time();
            let record = SecretRecord {
                id,
                label,
                description,
                suggested_env_var,
                created_at: now,
                updated_at: now,
                allowed_workspaces: vec![workspace],
            };
            data.secrets.push(record.clone());
            Ok(record)
        })
    }

    pub fn delete(&self, id: &str) -> Result<Option<SecretRecord>, String> {
        self.with_lock_mut(|data| {
            let Some(index) = data.secrets.iter().position(|record| record.id == id) else {
                return Ok(None);
            };
            Ok(Some(data.secrets.remove(index)))
        })
    }

    pub fn grant_workspace(&self, id: &str, workspace: &str) -> Result<SecretRecord, String> {
        self.with_lock_mut(|data| {
            let record = data
                .secrets
                .iter_mut()
                .find(|record| record.id == id)
                .ok_or_else(|| "secret metadata disappeared".to_string())?;
            if !record
                .allowed_workspaces
                .iter()
                .any(|item| item == workspace)
            {
                record.allowed_workspaces.push(workspace.to_string());
                record.updated_at = unix_time();
            }
            Ok(record.clone())
        })
    }

    pub fn restore_record(&self, current_id: &str, original: SecretRecord) -> Result<(), String> {
        self.with_lock_mut(|data| {
            let record = data
                .secrets
                .iter_mut()
                .find(|record| record.id == current_id)
                .ok_or_else(|| "replacement metadata disappeared during rollback".to_string())?;
            *record = original;
            Ok(())
        })
    }

    pub fn restore_deleted(&self, original: SecretRecord) -> Result<(), String> {
        self.with_lock_mut(|data| {
            if data.secrets.iter().any(|record| record.id == original.id) {
                return Err("deleted secret metadata was recreated concurrently".into());
            }
            data.secrets.push(original);
            Ok(())
        })
    }

    pub fn audit(
        &self,
        client: &str,
        action: &str,
        details: serde_json::Value,
    ) -> Result<(), String> {
        secure_create_dir(&self.data_dir)?;
        let lock = self.audit_lock()?;
        let path = self.data_dir.join("audit.jsonl");
        inspect_regular_or_missing(&path, "audit log")?;
        if path.metadata().map(|metadata| metadata.len()).unwrap_or(0) >= MAX_AUDIT_BYTES {
            let rotated = self.data_dir.join("audit.jsonl.1");
            inspect_regular_or_missing(&rotated, "rotated audit log")?;
            if rotated.exists() {
                fs::remove_file(&rotated)
                    .map_err(|error| format!("cannot remove prior rotated audit log: {error}"))?;
            }
            fs::rename(&path, &rotated)
                .map_err(|error| format!("cannot rotate audit log: {error}"))?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options
            .open(path)
            .map_err(|error| format!("cannot open audit log: {error}"))?;
        let line = json!({
            "timestamp": unix_time(),
            "client": client,
            "action": action,
            "details": details,
        });
        let result = writeln!(file, "{line}")
            .and_then(|()| file.flush())
            .map_err(|error| format!("cannot write audit log: {error}"));
        let _ = FileExt::unlock(&lock);
        result
    }

    fn with_lock<T>(
        &self,
        operation: impl FnOnce(&RegistryData) -> Result<T, String>,
    ) -> Result<T, String> {
        let lock = self.lock()?;
        let data = self.read_data()?;
        let result = operation(&data);
        let _ = FileExt::unlock(&lock);
        result
    }

    fn with_lock_mut<T>(
        &self,
        operation: impl FnOnce(&mut RegistryData) -> Result<T, String>,
    ) -> Result<T, String> {
        let lock = self.lock()?;
        let mut data = self.read_data()?;
        let result = operation(&mut data)?;
        self.write_data(&data)?;
        let _ = FileExt::unlock(&lock);
        Ok(result)
    }

    fn lock(&self) -> Result<File, String> {
        secure_create_dir(&self.data_dir)?;
        let path = self.data_dir.join("registry.lock");
        inspect_regular_or_missing(&path, "registry lock")?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options
            .open(path)
            .map_err(|error| format!("cannot open registry lock: {error}"))?;
        file.lock_exclusive()
            .map_err(|error| format!("cannot lock registry: {error}"))?;
        Ok(file)
    }

    fn audit_lock(&self) -> Result<File, String> {
        let path = self.data_dir.join("audit.lock");
        inspect_regular_or_missing(&path, "audit lock")?;
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options
            .open(path)
            .map_err(|error| format!("cannot open audit lock: {error}"))?;
        file.lock_exclusive()
            .map_err(|error| format!("cannot lock audit log: {error}"))?;
        Ok(file)
    }

    fn read_data(&self) -> Result<RegistryData, String> {
        let path = self.data_dir.join("registry.json");
        if !path.exists() {
            return Ok(RegistryData {
                version: 1,
                secrets: Vec::new(),
            });
        }
        inspect_regular_or_missing(&path, "registry")?;
        let size = fs::metadata(&path)
            .map_err(|error| format!("cannot inspect registry: {error}"))?
            .len();
        if size > MAX_REGISTRY_BYTES {
            return Err("registry exceeds the 4 MiB safety limit".into());
        }
        let mut contents = String::new();
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        options
            .open(path)
            .and_then(|mut file| file.read_to_string(&mut contents))
            .map_err(|error| format!("cannot read registry: {error}"))?;
        let data: RegistryData = serde_json::from_str(&contents)
            .map_err(|error| format!("invalid registry: {error}"))?;
        if data.version != 1 || data.secrets.len() > MAX_RECORDS {
            return Err("registry version or record count is unsupported".into());
        }
        validate_registry_data(&data)?;
        Ok(data)
    }

    fn write_data(&self, data: &RegistryData) -> Result<(), String> {
        let temp_path = self.data_dir.join("registry.json.tmp");
        let final_path = self.data_dir.join("registry.json");
        inspect_regular_or_missing(&final_path, "registry")?;
        let encoded = serde_json::to_vec_pretty(data)
            .map_err(|error| format!("cannot encode registry: {error}"))?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|error| format!("cannot create registry: {error}"))?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("cannot save registry: {error}"))?;
        drop(file);

        #[cfg(target_os = "windows")]
        {
            if final_path.exists() {
                return replace_file_windows(&final_path, &temp_path, "registry");
            }
        }

        fs::rename(temp_path, final_path)
            .map_err(|error| format!("cannot commit registry: {error}"))
    }
}

#[cfg(target_os = "windows")]
fn replace_file_windows(final_path: &Path, replacement: &Path, kind: &str) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{REPLACEFILE_WRITE_THROUGH, ReplaceFileW};

    let final_wide: Vec<u16> = final_path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let replacement_wide: Vec<u16> = replacement
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: Both paths are owned, NUL-terminated UTF-16 buffers that remain alive for the call.
    let replaced = unsafe {
        ReplaceFileW(
            final_wide.as_ptr(),
            replacement_wide.as_ptr(),
            std::ptr::null(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if replaced == 0 {
        let error = std::io::Error::last_os_error();
        let _ = fs::remove_file(replacement);
        return Err(format!("cannot atomically replace {kind}: {error}"));
    }
    Ok(())
}

fn inspect_regular_or_missing(path: &Path, kind: &str) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(format!("{kind} must be a regular file, not a symlink"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect {kind}: {error}")),
    }
}

fn validate_registry_data(data: &RegistryData) -> Result<(), String> {
    let mut ids = HashSet::new();
    let mut labels = HashSet::new();
    for record in &data.secrets {
        if !crate::validation::valid_secret_id(&record.id) || !ids.insert(record.id.as_str()) {
            return Err("registry contains an invalid or duplicate secret ID".into());
        }
        validate_metadata_text("label", &record.label, 3, 120, false)?;
        validate_metadata_text("description", &record.description, 3, 500, true)?;
        if !labels.insert(record.label.to_ascii_lowercase()) {
            return Err("registry contains duplicate secret labels".into());
        }
        if let Some(name) = &record.suggested_env_var {
            crate::env_file::validate_env_name(name)
                .map_err(|_| "registry contains an invalid environment variable".to_string())?;
        }
        if record.allowed_workspaces.len() > 100
            || record.allowed_workspaces.iter().any(|workspace| {
                crate::validation::validate_display_text(
                    "workspace grant",
                    workspace,
                    1,
                    4096,
                    false,
                )
                .is_err()
            })
        {
            return Err("registry contains an invalid workspace grant".into());
        }
    }
    Ok(())
}

fn validate_metadata_text(
    name: &str,
    value: &str,
    min: usize,
    max: usize,
    allow_newlines: bool,
) -> Result<(), String> {
    crate::validation::validate_display_text(name, value, min, max, allow_newlines)
        .map_err(|error| format!("registry contains invalid metadata: {error}"))
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn secure_tempdir() -> tempfile::TempDir {
        let directory = tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        directory
    }

    #[test]
    fn replacement_and_rollback_preserve_original_record() {
        let data = secure_tempdir();
        let registry = Registry::new(data.path());
        let original = registry
            .insert_with_id(
                "sb_0123456789abcdef0123456789abcdef".into(),
                "Test secret".into(),
                "Original purpose".into(),
                Some("TEST_SECRET".into()),
                "/workspace/one".into(),
            )
            .unwrap();
        let replacement = registry
            .replace_with_id(
                &original.id,
                "sb_fedcba9876543210fedcba9876543210".into(),
                "Test secret".into(),
                "New purpose".into(),
                Some("NEW_SECRET".into()),
                "/workspace/two",
            )
            .unwrap();
        assert_eq!(replacement.allowed_workspaces.len(), 2);
        registry
            .restore_record(&replacement.id, original.clone())
            .unwrap();
        assert_eq!(
            registry
                .find_by_id(&original.id)
                .unwrap()
                .unwrap()
                .description,
            "Original purpose"
        );
    }

    #[test]
    fn audit_log_rotates_at_limit() {
        let data = secure_tempdir();
        let audit = data.path().join("audit.jsonl");
        File::create(&audit)
            .unwrap()
            .set_len(MAX_AUDIT_BYTES)
            .unwrap();
        let registry = Registry::new(data.path());
        registry.audit("test", "event", json!({})).unwrap();
        assert!(data.path().join("audit.jsonl.1").is_file());
        assert!(fs::metadata(audit).unwrap().len() < MAX_AUDIT_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn registry_symlink_is_rejected() {
        use std::os::unix::fs::symlink;

        let data = secure_tempdir();
        let outside = data.path().join("outside.json");
        fs::write(&outside, r#"{"version":1,"secrets":[]}"#).unwrap();
        symlink(&outside, data.path().join("registry.json")).unwrap();
        let registry = Registry::new(data.path());
        assert!(registry.list().unwrap_err().contains("not a symlink"));
    }
}
