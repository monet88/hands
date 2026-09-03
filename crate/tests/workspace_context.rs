mod common;
use common::TestHarness;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;

#[tokio::test]
#[serial]
async fn test_issue_50_workspace_info_distinguishes_default_workspace() {
    let harness = TestHarness::new();

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "workspace_info",
                "arguments": {}
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    // Must contain default_workspace and backward-compatible workspace
    assert_eq!(
        structured["default_workspace"],
        structured["workspace"],
        "default_workspace and workspace must match in workspace_info"
    );
    assert!(
        structured["is_default"].as_bool().unwrap_or(false),
        "workspace_info must indicate is_default: true"
    );

    let content_text = resp["result"]["content"][0]["text"].as_str().expect("text");
    assert!(
        content_text.contains("default workspace:") || content_text.contains("default Workspace:"),
        "workspace_info content text must clearly state 'default workspace:', got: {content_text}"
    );
}

#[tokio::test]
#[serial]
async fn test_issue_50_command_execution_distinguishes_default_workspace_from_effective_cwd() {
    // Repo A is default workspace
    let harness = TestHarness::new();
    let default_ws_path = dunce::canonicalize(harness.temp.path()).unwrap();
    let default_ws_str = default_ws_path.display().to_string();

    // Repo B is separate explicit directory
    let repo_b = TempDir::new().expect("repo b tempdir");
    let repo_b_path = dunce::canonicalize(repo_b.path()).unwrap();
    let repo_b_str = repo_b_path.display().to_string();

    // Create a sentinel file in Repo B
    let sentinel_file = repo_b_path.join("repo_b_sentinel.txt");
    std::fs::write(&sentinel_file, "in repo b").unwrap();

    // 1. run_command with explicit workdir in Repo B
    let run_cmd_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "git",
                    "args": ["--version"],
                    "workdir": repo_b_str
                }
            }),
        )
        .await;

    assert_eq!(run_cmd_res["result"]["isError"], false);
    let cmd_structured = &run_cmd_res["result"]["structuredContent"];
    assert_eq!(cmd_structured["execution_state"], "completed");

    // Must expose default_workspace and effective cwd / workdir
    assert_eq!(
        cmd_structured["default_workspace"].as_str().unwrap(),
        default_ws_str,
        "default_workspace must remain Repo A"
    );
    assert_eq!(
        cmd_structured["cwd"].as_str().unwrap(),
        repo_b_str,
        "effective cwd must report Repo B"
    );

    // 2. Verify default workspace in harness was NOT mutated
    let ws_info = harness
        .rpc(
            "tools/call",
            json!({
                "name": "workspace_info",
                "arguments": {}
            }),
        )
        .await;
    assert_eq!(
        ws_info["result"]["structuredContent"]["workspace"].as_str().unwrap(),
        default_ws_str,
        "pinned default workspace must not be mutated by explicit target in other repo"
    );

    // 3. run_terminal_cmd in default workspace without explicit workdir
    #[cfg(windows)]
    let test_cmd = "powershell -NoProfile -Command \"Write-Output 'DEFAULT_WS_EXEC'\"";
    #[cfg(not(windows))]
    let test_cmd = "echo 'DEFAULT_WS_EXEC'";

    let terminal_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": test_cmd,
                    "description": "execute in default workspace"
                }
            }),
        )
        .await;

    assert_eq!(terminal_res["result"]["isError"], false);
    let term_structured = &terminal_res["result"]["structuredContent"];
    assert_eq!(
        term_structured["default_workspace"].as_str().unwrap(),
        default_ws_str,
        "run_terminal_cmd must expose default_workspace"
    );
    assert!(
        term_structured["cwd"].is_string(),
        "run_terminal_cmd must expose effective cwd"
    );
}

#[tokio::test]
#[serial]
async fn test_issue_50_explicit_absolute_file_target_outside_workspace() {
    let harness = TestHarness::new();
    let default_ws_path = dunce::canonicalize(harness.temp.path()).unwrap();
    let default_ws_str = default_ws_path.display().to_string();

    let outside_dir = TempDir::new().expect("outside tempdir");
    let outside_file = outside_dir.path().join("external_file.txt");
    std::fs::write(&outside_file, "external content").unwrap();
    let outside_file_canonical = dunce::canonicalize(&outside_file).unwrap();
    let outside_file_str = outside_file_canonical.display().to_string();

    let read_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "read_file",
                "arguments": {
                    "target_file": outside_file_str
                }
            }),
        )
        .await;

    assert_eq!(read_res["result"]["isError"], false);
    let read_structured = &read_res["result"]["structuredContent"];
    assert_eq!(
        read_structured["default_workspace"].as_str().unwrap(),
        default_ws_str,
        "read_file must expose default_workspace"
    );
    assert_eq!(
        read_structured["target_path"].as_str().unwrap(),
        outside_file_str,
        "target_path must identify the external absolute file"
    );

    // Verify workspace was not mutated
    let ws_info = harness
        .rpc(
            "tools/call",
            json!({
                "name": "workspace_info",
                "arguments": {}
            }),
        )
        .await;
    assert_eq!(
        ws_info["result"]["structuredContent"]["workspace"].as_str().unwrap(),
        default_ws_str,
        "pinned default workspace must not be mutated"
    );

    // 4. Relative operations still resolve deterministically from default Workspace
    let relative_file = harness.temp.path().join("relative.txt");
    std::fs::write(&relative_file, "relative inside default ws").unwrap();
    let rel_canonical = dunce::canonicalize(&relative_file).unwrap();

    let rel_read = harness
        .rpc(
            "tools/call",
            json!({
                "name": "read_file",
                "arguments": {
                    "target_file": "relative.txt"
                }
            }),
        )
        .await;
    assert_eq!(rel_read["result"]["isError"], false);
    let rel_structured = &rel_read["result"]["structuredContent"];
    assert_eq!(
        rel_structured["default_workspace"].as_str().unwrap(),
        default_ws_str
    );
    assert_eq!(
        rel_structured["target_path"].as_str().unwrap(),
        rel_canonical.display().to_string(),
        "relative read_file must resolve from default workspace"
    );
}
