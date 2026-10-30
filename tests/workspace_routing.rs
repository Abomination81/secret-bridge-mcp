use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::tempdir;

fn run_session(cwd: &Path, data: &Path, requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_secret-bridge-mcp"))
        .args(["--workspace-per-request", "--data-dir"])
        .arg(data)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut stdin = child.stdin.take().unwrap();
        for request in requests {
            serde_json::to_writer(&mut stdin, request).unwrap();
            stdin.write_all(b"\n").unwrap();
        }
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn separate_stdio_sessions_use_their_request_scope_not_the_launch_directory() {
    let launch = tempdir().unwrap();
    let first = tempdir().unwrap();
    let second = tempdir().unwrap();
    let data = tempdir().unwrap();
    let config = secret_bridge_mcp::AppConfig::new_with_data_dir(
        Some(launch.path().into()),
        "Workspace routing test".into(),
        Some(data.path().join("private")),
    )
    .unwrap();
    std::thread::scope(|scope| {
        let mut workers = Vec::new();
        for (index, root) in [first.path(), second.path()].into_iter().enumerate() {
            let launch = launch.path();
            let data = config.data_dir.as_path();
            workers.push(scope.spawn(move || {
                let responses = run_session(
                    launch,
                    data,
                    &[
                        json!({"jsonrpc":"2.0", "id":0, "method":"initialize", "params":{}}),
                        json!({"jsonrpc":"2.0", "id":1, "method":"tools/list"}),
                        json!({"jsonrpc":"2.0", "id":index + 10, "method":"tools/call", "params":{
                            "name":"secret_list", "arguments":{"workspace_root":root}
                        }}),
                        json!({"jsonrpc":"2.0", "id":3, "method":"tools/call", "params":{
                            "name":"secret_list", "arguments":{}
                        }}),
                    ],
                );
                assert_eq!(responses.len(), 4);
                for tool in responses[1]["result"]["tools"].as_array().unwrap() {
                    assert!(
                        tool["inputSchema"]["required"]
                            .as_array()
                            .unwrap()
                            .contains(&json!("workspace_root"))
                    );
                }
                assert_eq!(responses[2]["id"], index + 10);
                assert_eq!(responses[2]["result"]["isError"], false);
                assert_eq!(
                    responses[2]["result"]["structuredContent"]["workspace_root"],
                    json!(root.canonicalize().unwrap())
                );
                assert_eq!(responses[3]["result"]["isError"], true);
                assert!(
                    responses[3]["result"]["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("workspace_root is required")
                );
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
}

#[test]
fn ambiguous_workspace_modes_are_rejected_at_startup() {
    let workspace = tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_secret-bridge-mcp"))
        .args(["--workspace-per-request", "--workspace-root"])
        .arg(workspace.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cannot be combined")
    );
}
