mod common;
use common::TestHarness;
use hands::host;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;
#[tokio::test]
#[serial]
async fn test_windows_workspace_path_with_spaces() {
    let base_temp = TempDir::new().expect("base tempdir");
    let space_dir = base_temp.path().join("workspace with spaces");
    std::fs::create_dir_all(&space_dir).expect("create space dir");

    let harness = TestHarness::new_with_dir(base_temp);

    // 1. Test set_workspace with space-containing path
    let set_ws = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": space_dir.to_str().unwrap()
                }
            }),
        )
        .await;
    assert_eq!(set_ws["result"]["isError"], false);
    let pinned_ws = set_ws["result"]["structuredContent"]["workspace"]
        .as_str()
        .expect("workspace string");
    assert!(
        pinned_ws.contains("workspace with spaces"),
        "workspace must contain space path: {pinned_ws}"
    );
    assert!(
        !pinned_ws.starts_with(r"\\?\"),
        "workspace must not have verbatim UNC \\\\?\\ prefix: {pinned_ws}"
    );

    // 2. Test workspace_info reflects the workspace
    let ws_info = harness
        .rpc(
            "tools/call",
            json!({
                "name": "workspace_info",
                "arguments": {}
            }),
        )
        .await;
    assert_eq!(ws_info["result"]["isError"], false);
    let info_ws = ws_info["result"]["structuredContent"]["workspace"]
        .as_str()
        .expect("workspace string");
    assert_eq!(info_ws, pinned_ws);

    // 3. Test file tools (write and read_file) inside workspace with spaces
    let file_path = space_dir.join("test file.txt");
    let file_path_str = file_path.to_str().unwrap();

    let write_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "write",
                "arguments": {
                    "file_path": file_path_str,
                    "content": "Hello Windows World"
                }
            }),
        )
        .await;
    assert_eq!(write_res["result"]["isError"], false, "write tool must succeed: {:?}", write_res);

    let read_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "read_file",
                "arguments": {
                    "target_file": file_path_str
                }
            }),
        )
        .await;
    assert_eq!(read_res["result"]["isError"], false, "read_file must succeed: {:?}", read_res);
    let text = read_res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Hello Windows World"), "read content mismatch: {text}");
}

#[tokio::test]
#[serial]
async fn test_windows_terminal_command_bounded_output() {
    let base_temp = TempDir::new().expect("base tempdir");
    let space_dir = base_temp.path().join("terminal space");
    std::fs::create_dir_all(&space_dir).expect("create space dir");

    let harness = TestHarness::new_with_dir(base_temp);

    // Pin workspace to space_dir
    let _ = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": space_dir.to_str().unwrap()
                }
            }),
        )
        .await;

    // Execute a harmless command via direct run_terminal_cmd
    let cmd_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": "echo WindowsHostExecutionOK",
                    "description": "harmless smoke test command"
                }
            }),
        )
        .await;

    assert_eq!(cmd_res["result"]["isError"], false, "command failed: {:?}", cmd_res);
    let text = cmd_res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("WindowsHostExecutionOK"),
        "command output missing expected text: {text}"
    );

    // Verify output is bounded and not empty
    assert!(!text.is_empty());
    assert!(text.len() < 100_000, "output should be bounded");
}

#[tokio::test]
#[serial]
async fn test_upstream_direct_dispatch_architecture_contract() {
    let base_temp = TempDir::new().expect("base tempdir");
    let harness = TestHarness::new_with_dir(base_temp);

    let resp = harness.rpc("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let run_cmd = tools
        .iter()
        .find(|t| t["name"] == "run_terminal_cmd")
        .expect("run_terminal_cmd tool");

    let cmd_props = &run_cmd["inputSchema"]["properties"];

    // Prove NO execution_mode or yield_after_ms parameters
    assert!(
        cmd_props.get("execution_mode").is_none(),
        "Must NOT introduce execution_mode"
    );
    assert!(
        cmd_props.get("yield_after_ms").is_none(),
        "Must NOT introduce yield_after_ms"
    );

    // Ensure UPSTREAM_BASE_COMMIT is 26f9001
    assert_eq!(host::UPSTREAM_BASE_COMMIT, "26f9001");
}

#[tokio::test]
#[serial]
async fn test_windows_command_resolution_no_cwd_preemption() {
    let base_temp = TempDir::new().expect("base tempdir");
    let ws_dir = base_temp.path().join("workspace");
    std::fs::create_dir_all(&ws_dir).expect("create ws dir");

    // Place dummy script in workspace matching common shell/command names
    #[cfg(windows)]
    {
        let fake_cmd = ws_dir.join("powershell.bat");
        std::fs::write(&fake_cmd, "@echo off\necho CWD_SCRIPT_PREEMPTED\n").expect("write fake script");
    }
    #[cfg(not(windows))]
    {
        let fake_cmd = ws_dir.join("sh");
        std::fs::write(&fake_cmd, "#!/bin/sh\necho CWD_SCRIPT_PREEMPTED\n").expect("write fake script");
    }

    let harness = TestHarness::new_with_dir(base_temp);
    let set_ws = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": ws_dir.to_str().unwrap()
                }
            }),
        )
        .await;
    assert_eq!(set_ws["result"]["isError"], false);

    // Run safe command
    #[cfg(windows)]
    let test_cmd = "powershell -NoProfile -Command \"Write-Output 'SAFE_RESOLVED'\"";
    #[cfg(not(windows))]
    let test_cmd = "echo 'SAFE_RESOLVED'";

    let run_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": test_cmd,
                    "description": "Verify no cwd script preemption"
                }
            }),
        )
        .await;
    assert_eq!(run_res["result"]["isError"], false);
    let text = run_res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("SAFE_RESOLVED"), "output must contain SAFE_RESOLVED: {text}");
    assert!(!text.contains("CWD_SCRIPT_PREEMPTED"), "cwd script must not preempt system command: {text}");
}
