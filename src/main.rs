use std::env;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::ExitCode;

use secret_bridge_mcp::{AppConfig, run_desktop, run_native_prompt_child};

fn print_help() {
    eprintln!(
        "secret-bridge-mcp {version}\n\
         \n\
         Usage:\n\
           secret-bridge-mcp [--workspace-root PATH] [--client-name NAME] [--data-dir PATH]\n\
         \n\
         Options:\n\
           --workspace-root PATH  Restrict .env writes to this directory (default: cwd)\n\
           --client-name NAME     Name shown in local approval dialogs (default: MCP client)\n\
           --data-dir PATH        Override the non-secret metadata directory\n\
           -V, --version          Show the version\n\
           -h, --help             Show this help\n",
        version = env!("CARGO_PKG_VERSION")
    );
}

fn parse_args() -> Result<AppConfig, String> {
    let mut workspace_root: Option<PathBuf> = None;
    let mut data_dir: Option<PathBuf> = None;
    let mut client_name = String::from("MCP client");
    let mut args = env::args_os().skip(1);

    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--workspace-root" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--workspace-root requires a path".to_string())?;
                workspace_root = Some(PathBuf::from(value));
            }
            "--client-name" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--client-name requires a value".to_string())?;
                client_name = value.to_string_lossy().trim().to_string();
            }
            "--data-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--data-dir requires a path".to_string())?;
                data_dir = Some(PathBuf::from(value));
            }
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "-V" | "--version" => {
                println!("secret-bridge-mcp {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            unknown => return Err(format!("unknown argument: {unknown}")),
        }
    }

    AppConfig::new_with_data_dir(workspace_root, client_name, data_dir)
}

fn main() -> ExitCode {
    if env::args_os().nth(1).as_deref() == Some(OsStr::new("--native-prompt")) {
        return match run_native_prompt_child() {
            Ok(code) => ExitCode::from(code),
            Err(_) => ExitCode::FAILURE,
        };
    }

    let config = match parse_args() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("secret-bridge-mcp: {error}");
            print_help();
            return ExitCode::from(2);
        }
    };

    if let Err(error) = run_desktop(config) {
        eprintln!("secret-bridge-mcp: {error}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}
