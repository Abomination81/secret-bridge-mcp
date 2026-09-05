mod env_file;
mod native_ui;
mod protocol;
mod registry;
mod ui;
mod validation;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub use native_ui::{run_desktop, run_native_prompt_child};

pub const SERVICE_NAME: &str = "dev.secretbridge.mcp";

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub workspace_root: PathBuf,
    pub data_dir: PathBuf,
    pub client_name: String,
}

impl AppConfig {
    pub fn new(workspace_root: Option<PathBuf>, client_name: String) -> Result<Self, String> {
        Self::new_with_data_dir(workspace_root, client_name, None)
    }

    pub fn new_with_data_dir(
        workspace_root: Option<PathBuf>,
        client_name: String,
        data_dir: Option<PathBuf>,
    ) -> Result<Self, String> {
        crate::validation::validate_display_text("client name", &client_name, 1, 80, false)?;
        let root = match workspace_root {
            Some(path) => path,
            None => env::current_dir().map_err(|error| format!("cannot read cwd: {error}"))?,
        };
        let workspace_root = root
            .canonicalize()
            .map_err(|error| format!("workspace root must exist: {error}"))?;
        if !workspace_root.is_dir() {
            return Err("workspace root must be a directory".into());
        }

        let data_dir = match data_dir {
            Some(path) => {
                if path.as_os_str().is_empty() {
                    return Err("data directory cannot be empty".into());
                }
                path
            }
            None => default_data_dir()?,
        };
        secure_create_dir(&data_dir)?;
        let data_dir = data_dir
            .canonicalize()
            .map_err(|error| format!("cannot resolve data directory: {error}"))?;
        if !data_dir.is_dir() {
            return Err("data directory must be a directory".into());
        }

        Ok(Self {
            workspace_root,
            data_dir,
            client_name,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(workspace_root: &Path, data_dir: &Path) -> Self {
        Self {
            workspace_root: workspace_root.to_path_buf(),
            data_dir: data_dir.to_path_buf(),
            client_name: "Test client".into(),
        }
    }
}

fn default_data_dir() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        let home = env::var_os("HOME").ok_or("HOME is not set")?;
        Ok(PathBuf::from(home)
            .join("Library")
            .join("Application Support")
            .join("SecretBridge"))
    }

    #[cfg(target_os = "windows")]
    {
        let app_data = env::var_os("APPDATA").ok_or("APPDATA is not set")?;
        Ok(PathBuf::from(app_data).join("SecretBridge"))
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let base = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .ok_or("neither XDG_STATE_HOME nor HOME is set")?;
        Ok(base.join("secret-bridge"))
    }
}

pub(crate) fn secure_create_dir(path: &Path) -> Result<(), String> {
    let existed = path.exists();
    if existed {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect data directory: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err("data directory must be a real directory, not a symlink".into());
        }
    }
    fs::create_dir_all(path).map_err(|error| format!("cannot create data directory: {error}"))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if existed {
            let mode = fs::metadata(path)
                .map_err(|error| format!("cannot inspect data directory permissions: {error}"))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(
                    "data directory is accessible to other users; set its permissions to 0700"
                        .into(),
                );
            }
        } else {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("cannot secure data directory: {error}"))?;
        }
    }

    Ok(())
}
