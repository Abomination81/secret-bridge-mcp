use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

use zeroize::Zeroizing;

const MAX_ENV_FILE_BYTES: u64 = 1024 * 1024;
const MAX_GITIGNORE_BYTES: u64 = 1024 * 1024;
const TEMPLATE_FILENAMES: [&str; 7] = [
    ".env.example",
    ".env.sample",
    ".env.template",
    ".env.dist",
    ".env.defaults",
    ".env.schema",
    ".env.test.example",
];

pub struct EnvValue {
    pub name: String,
    pub value: Zeroizing<String>,
}

pub fn validate_env_name(name: &str) -> Result<(), String> {
    if name.len() > 128 {
        return Err("environment variable name is too long (maximum 128 bytes)".into());
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("environment variable name cannot be empty".into());
    };
    if !(first == '_' || first.is_ascii_uppercase())
        || !chars.all(|character| {
            character == '_' || character.is_ascii_uppercase() || character.is_ascii_digit()
        })
    {
        return Err(format!(
            "invalid secret environment variable {name:?}; use uppercase letters, digits, and underscores"
        ));
    }
    const PUBLIC_PREFIXES: [&str; 6] = [
        "NEXT_PUBLIC_",
        "VITE_",
        "PUBLIC_",
        "EXPO_PUBLIC_",
        "REACT_APP_",
        "NUXT_PUBLIC_",
    ];
    if PUBLIC_PREFIXES
        .iter()
        .any(|prefix| name.starts_with(prefix))
    {
        return Err(format!(
            "refusing {name}: public-prefixed variables are bundled into client code"
        ));
    }
    Ok(())
}

pub fn resolve_env_path(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let relative = Path::new(requested);
    if relative.is_absolute() {
        return Err("env path must be relative to the configured workspace root".into());
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err("env path cannot contain '.', '..', a drive prefix, or a root".into());
    }
    let Some(file_name) = relative.file_name().and_then(|name| name.to_str()) else {
        return Err("env path must end in a UTF-8 filename".into());
    };
    if !(file_name == ".env" || file_name.starts_with(".env.") || file_name == ".dev.vars") {
        return Err("target filename must be .env, .env.*, or .dev.vars".into());
    }
    if TEMPLATE_FILENAMES.contains(&file_name)
        || file_name.ends_with(".example")
        || file_name.ends_with(".sample")
        || file_name.ends_with(".template")
        || file_name.ends_with(".dist")
    {
        return Err("refusing to write secrets to a committable env template file".into());
    }

    let target = root.join(relative);
    let parent = target.parent().ok_or("env path has no parent directory")?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| format!("env parent directory must already exist: {error}"))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("cannot resolve workspace root: {error}"))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err("env path escapes the configured workspace root".into());
    }
    reject_symlink_components(
        &canonical_root,
        relative.parent().unwrap_or_else(|| Path::new("")),
    )?;
    if target.exists() {
        let metadata = fs::symlink_metadata(&target)
            .map_err(|error| format!("cannot inspect env target: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("env target must be a regular file, not a symlink".into());
        }
    }
    Ok(target)
}

pub fn write_env_file(root: &Path, target: &Path, values: &[EnvValue]) -> Result<(), String> {
    let relative = target
        .strip_prefix(root)
        .map_err(|_| "env target is outside the configured workspace".to_string())?;
    let requested = relative
        .to_str()
        .ok_or_else(|| "env target path must be UTF-8".to_string())?;
    let revalidated = resolve_env_path(root, requested)?;
    if revalidated != target {
        return Err("env target changed while awaiting approval".into());
    }
    refuse_tracked_target(root, relative)?;
    ensure_gitignore(root)?;
    verify_gitignored(root, relative)?;
    let existing = if target.exists() {
        let metadata = fs::metadata(target)
            .map_err(|error| format!("cannot inspect existing env file: {error}"))?;
        if metadata.len() > MAX_ENV_FILE_BYTES {
            return Err("existing env file exceeds the 1 MiB safety limit".into());
        }
        Zeroizing::new(read_small_file(
            target,
            MAX_ENV_FILE_BYTES,
            "existing env file",
        )?)
    } else {
        Zeroizing::new(String::new())
    };

    let replacements: HashMap<&str, &str> = values
        .iter()
        .map(|entry| (entry.name.as_str(), entry.value.as_str()))
        .collect();
    let mut seen = HashSet::new();
    let mut output = Zeroizing::new(String::new());

    for line in existing.lines() {
        if let Some(name) = assignment_name(line)
            && let Some(value) = replacements.get(name)
        {
            if seen.insert(name.to_string()) {
                output.push_str(name);
                output.push('=');
                append_encoded_dotenv(&mut output, value)?;
                output.push('\n');
            }
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }

    for entry in values {
        if seen.insert(entry.name.clone()) {
            output.push_str(&entry.name);
            output.push('=');
            append_encoded_dotenv(&mut output, &entry.value)?;
            output.push('\n');
        }
    }

    secure_write(target, output.as_bytes())
}

fn assignment_name(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let line = line.strip_prefix("export ").unwrap_or(line);
    let (name, _) = line.split_once('=')?;
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first == '_' || first.is_ascii_alphabetic())
        || !chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return None;
    }
    Some(name)
}

fn append_encoded_dotenv(encoded: &mut String, value: &str) -> Result<(), String> {
    if value.chars().any(|character| {
        character == '\0' || (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    }) {
        return Err(
            "secret contains a control character that cannot be represented safely in dotenv"
                .into(),
        );
    }
    encoded.push('"');
    for character in value.chars() {
        match character {
            '\\' => encoded.push_str("\\\\"),
            '"' => encoded.push_str("\\\""),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            '$' => encoded.push_str("\\$"),
            other => encoded.push(other),
        }
    }
    encoded.push('"');
    Ok(())
}

fn ensure_gitignore(root: &Path) -> Result<(), String> {
    let path = root.join(".gitignore");
    let existing = if path.exists() {
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect .gitignore: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("refusing to modify a symlink or non-file .gitignore".into());
        }
        read_small_file(&path, MAX_GITIGNORE_BYTES, ".gitignore")?
    } else {
        String::new()
    };
    let has_env_pattern = existing.lines().any(|line| line.trim() == ".env*");
    let has_dev_vars = existing.lines().any(|line| line.trim() == ".dev.vars");
    if has_env_pattern && has_dev_vars {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("# SecretBridge: local secret files\n");
    if !has_env_pattern {
        updated.push_str(".env*\n!.env.example\n!.env.sample\n!.env.template\n!.env.dist\n");
    }
    if !has_dev_vars {
        updated.push_str(".dev.vars\n");
    }
    atomic_write(&path, updated.as_bytes(), 0o644, "gitignore")
}

fn reject_symlink_components(root: &Path, relative_parent: &Path) -> Result<(), String> {
    let mut current = root.to_path_buf();
    for component in relative_parent.components() {
        let Component::Normal(part) = component else {
            return Err("env path contains an unsupported component".into());
        };
        current.push(part);
        let metadata = fs::symlink_metadata(&current)
            .map_err(|error| format!("cannot inspect env parent path: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("env parent path contains a symlink or non-directory component".into());
        }
    }
    Ok(())
}

fn read_small_file(path: &Path, max_bytes: u64, kind: &str) -> Result<String, String> {
    let metadata =
        fs::symlink_metadata(path).map_err(|error| format!("cannot inspect {kind}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{kind} must be a regular file, not a symlink"));
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "{kind} exceeds the {} byte safety limit",
            max_bytes
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .map_err(|error| format!("cannot open {kind}: {error}"))?;
    let mut contents = String::with_capacity(metadata.len() as usize);
    file.take(max_bytes + 1)
        .read_to_string(&mut contents)
        .map_err(|error| format!("cannot read {kind}: {error}"))?;
    if contents.len() as u64 > max_bytes {
        return Err(format!("{kind} grew beyond the safety limit while reading"));
    }
    Ok(contents)
}

fn git_status(root: &Path, args: &[&str]) -> Result<bool, String> {
    let git = trusted_git_path()?;
    let output = Command::new(git).arg("-C").arg(root).args(args).output();
    match output {
        Ok(output) => Ok(output.status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("git is required to verify that env targets cannot be committed".into())
        }
        Err(error) => Err(format!("cannot run git safety check: {error}")),
    }
}

fn trusted_git_path() -> Result<PathBuf, String> {
    #[cfg(unix)]
    {
        let path = PathBuf::from("/usr/bin/git");
        path.is_file()
            .then_some(path)
            .ok_or_else(|| "trusted Git executable was not found at /usr/bin/git".into())
    }

    #[cfg(target_os = "windows")]
    {
        for candidate in [
            r"C:\Program Files\Git\cmd\git.exe",
            r"C:\Program Files (x86)\Git\cmd\git.exe",
        ] {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return Ok(path);
            }
        }
        Err("trusted Git for Windows was not found under Program Files".into())
    }

    #[cfg(not(any(unix, target_os = "windows")))]
    Err("Git safety checks are unsupported on this platform".into())
}

fn is_git_workspace(root: &Path) -> Result<bool, String> {
    git_status(root, &["rev-parse", "--is-inside-work-tree"])
}

fn path_arg(path: &Path) -> Result<&str, String> {
    path.to_str()
        .ok_or_else(|| "git safety checks require a UTF-8 env path".into())
}

fn refuse_tracked_target(root: &Path, relative: &Path) -> Result<(), String> {
    if !is_git_workspace(root)? {
        return Ok(());
    }
    let relative = path_arg(relative)?;
    if git_status(root, &["ls-files", "--error-unmatch", "--", relative])? {
        return Err("refusing to write secrets to a file already tracked by git; remove it from the index first".into());
    }
    Ok(())
}

fn verify_gitignored(root: &Path, relative: &Path) -> Result<(), String> {
    if !is_git_workspace(root)? {
        return Ok(());
    }
    let relative = path_arg(relative)?;
    if !git_status(root, &["check-ignore", "--no-index", "-q", "--", relative])? {
        return Err(
            "git does not report the env target as ignored; refusing to write secrets".into(),
        );
    }
    Ok(())
}

fn secure_write(path: &Path, contents: &[u8]) -> Result<(), String> {
    atomic_write(path, contents, 0o600, "env file")
}

fn atomic_write(path: &Path, contents: &[u8], _unix_mode: u32, kind: &str) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = path.with_file_name(format!(
            ".secretbridge.{}.{}.tmp",
            std::process::id(),
            nonce
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(_unix_mode)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&temp)
            .map_err(|error| format!("cannot create temporary {kind}: {error}"))?;
        if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
            let _ = fs::remove_file(&temp);
            return Err(format!("cannot write {kind}: {error}"));
        }
        drop(file);
        if let Err(error) = fs::rename(&temp, path) {
            let _ = fs::remove_file(&temp);
            return Err(format!("cannot commit {kind}: {error}"));
        }
        fs::set_permissions(path, fs::Permissions::from_mode(_unix_mode))
            .map_err(|error| format!("cannot secure {kind} permissions: {error}"))?;
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let temp = path.with_file_name(format!(
            ".secretbridge.{}.{}.tmp",
            std::process::id(),
            nonce
        ));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(|error| format!("cannot open {kind}: {error}"))?;
        file.write_all(contents)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("cannot write {kind}: {error}"))?;
        drop(file);
        if path.exists() {
            replace_file_windows(path, &temp, kind)
        } else {
            fs::rename(&temp, path).map_err(|error| format!("cannot commit {kind}: {error}"))
        }
    }

    #[cfg(not(any(unix, target_os = "windows")))]
    {
        fs::write(path, contents).map_err(|error| format!("cannot write {kind}: {error}"))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_public_secret_names() {
        assert!(validate_env_name("OPENAI_API_KEY").is_ok());
        assert!(validate_env_name("NEXT_PUBLIC_OPENAI_API_KEY").is_err());
        assert!(validate_env_name("lowercase").is_err());
    }

    #[test]
    fn confines_env_paths() {
        let temp = tempdir().unwrap();
        assert!(resolve_env_path(temp.path(), ".env.local").is_ok());
        assert!(resolve_env_path(temp.path(), "../.env").is_err());
        assert!(resolve_env_path(temp.path(), "secrets.txt").is_err());
        assert!(resolve_env_path(temp.path(), ".env.example").is_err());
        assert!(resolve_env_path(temp.path(), ".env.production.sample").is_err());
    }

    #[test]
    fn merges_without_duplicating_managed_keys() {
        let temp = tempdir().unwrap();
        fs::write(
            temp.path().join(".env"),
            "KEEP=yes\nTOKEN=old\nTOKEN=duplicate\n",
        )
        .unwrap();
        let values = vec![EnvValue {
            name: "TOKEN".into(),
            value: Zeroizing::new("new value$".into()),
        }];
        write_env_file(temp.path(), &temp.path().join(".env"), &values).unwrap();
        let result = fs::read_to_string(temp.path().join(".env")).unwrap();
        assert_eq!(result, "KEEP=yes\nTOKEN=\"new value\\$\"\n");
        assert!(
            fs::read_to_string(temp.path().join(".gitignore"))
                .unwrap()
                .contains(".env*")
        );
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_gitignore() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, "keep\n").unwrap();
        symlink(&outside, temp.path().join(".gitignore")).unwrap();
        let values = vec![EnvValue {
            name: "TOKEN".into(),
            value: Zeroizing::new("dummy".into()),
        }];
        assert!(write_env_file(temp.path(), &temp.path().join(".env"), &values).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep\n");
    }

    #[test]
    fn dev_vars_is_ignored_before_write() {
        let temp = tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(temp.path())
                .status()
                .unwrap()
                .success()
        );
        let values = vec![EnvValue {
            name: "TOKEN".into(),
            value: Zeroizing::new("dummy".into()),
        }];
        let target = temp.path().join(".dev.vars");
        write_env_file(temp.path(), &target, &values).unwrap();
        assert!(
            fs::read_to_string(temp.path().join(".gitignore"))
                .unwrap()
                .lines()
                .any(|line| line == ".dev.vars")
        );
    }

    #[test]
    fn refuses_git_tracked_env_target() {
        let temp = tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .arg(temp.path())
                .status()
                .unwrap()
                .success()
        );
        let target = temp.path().join(".env");
        fs::write(&target, "TOKEN=old\n").unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .args(["add", "-f", ".env"])
                .status()
                .unwrap()
                .success()
        );
        let values = vec![EnvValue {
            name: "TOKEN".into(),
            value: Zeroizing::new("dummy".into()),
        }];
        let error = write_env_file(temp.path(), &target, &values).unwrap_err();
        assert!(error.contains("already tracked by git"));
    }
}
