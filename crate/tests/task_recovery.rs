mod common;
use common::TestHarness;
use serde_json::json;
use serial_test::serial;
#[tokio::test]
#[serial]
async fn test_list_terminal_tasks_schema_and_face() {
    let harness = TestHarness::new();
    let resp = harness.rpc("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let list_tool = tools
        .iter()
        .find(|t| t["name"] == "list_terminal_tasks")
        .expect("list_terminal_tasks must be registered");

    assert_eq!(list_tool["annotations"]["readOnlyHint"], true);
    assert_eq!(list_tool["annotations"]["destructiveHint"], false);
    assert_eq!(list_tool["annotations"]["idempotentHint"], true);
    assert_eq!(list_tool["title"], "List tasks");
    assert_eq!(list_tool["_meta"]["openai/toolInvocation/invoking"], "Listing tasks...");
    assert_eq!(list_tool["_meta"]["openai/toolInvocation/invoked"], "Listed tasks");

    let props = &list_tool["inputSchema"]["properties"];
    assert!(
        props.get("status_filter").is_none(),
        "list_terminal_tasks must not expose status_filter"
    );
    // Explicitly prove no execution_mode or yield_after_ms is exposed anywhere
    assert!(
        props.get("execution_mode").is_none(),
        "list_terminal_tasks must not expose execution_mode"
    );
    assert!(
        props.get("yield_after_ms").is_none(),
        "list_terminal_tasks must not expose yield_after_ms"
    );

    let run_cmd = tools
        .iter()
        .find(|t| t["name"] == "run_terminal_cmd")
        .expect("run_terminal_cmd tool");
    let cmd_props = &run_cmd["inputSchema"]["properties"];
    assert!(
        cmd_props.get("execution_mode").is_none(),
        "run_terminal_cmd must not expose execution_mode"
    );
    assert!(
        cmd_props.get("yield_after_ms").is_none(),
        "run_terminal_cmd must not expose yield_after_ms"
    );
}

#[tokio::test]
#[serial]
async fn test_list_terminal_tasks_empty() {
    let harness = TestHarness::new();
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {}
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let tasks = resp["result"]["structuredContent"]["tasks"]
        .as_array()
        .expect("tasks array");
    assert!(tasks.is_empty(), "initially there should be no tasks");
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Total tasks: 0"));
}

#[tokio::test]
#[serial]
async fn test_list_terminal_tasks_lifecycle_recovery_and_kill() {
    let harness = TestHarness::new();

    // Start a long-running background task
    #[cfg(windows)]
    let cmd = "powershell -Command \"Start-Sleep -Seconds 30\"";
    #[cfg(not(windows))]
    let cmd = "sleep 30";

    let bg_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Background task for recovery test",
                    "is_background": true
                }
            }),
        )
        .await;

    assert_eq!(bg_resp["result"]["isError"], false);
    let task_id = bg_resp["result"]["structuredContent"]["task_id"]
        .as_str()
        .expect("background task_id")
        .to_string();

    // Discover the running task via list_terminal_tasks
    let list_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {}
            }),
        )
        .await;

    assert_eq!(list_resp["result"]["isError"], false);
    let tasks = list_resp["result"]["structuredContent"]["tasks"]
        .as_array()
        .expect("tasks array");
    assert!(!tasks.is_empty(), "should discover running background task");

    let found = tasks
        .iter()
        .find(|t| t["task_id"] == task_id)
        .expect("must find our task_id in list_terminal_tasks");

    assert_eq!(found["status"], "running");
    assert_eq!(found["completed"], false);
    assert!(found["command"].as_str().unwrap().contains("Background task for recovery test"));
    assert!(found["output_file"].is_string());
    assert!(found["cwd"].is_string(), "must include cwd in snapshot");
    assert!(found["duration_secs"].is_number(), "must include duration_secs");
    assert!(found["truncated"].is_boolean(), "must include truncated");
    assert!(found["total_bytes"].is_number(), "must include total_bytes");

    // Verify bounded snapshot does not expose secrets/environment variables
    assert!(found.get("env").is_none(), "must not expose env");
    assert!(found.get("environment").is_none(), "must not expose environment");
    assert!(found.get("secrets").is_none(), "must not expose secrets");

    // Stop/kill the task using the recovered task_id
    let kill_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "kill_task",
                "arguments": {
                    "task_id": task_id
                }
            }),
        )
        .await;
    assert_eq!(kill_resp["result"]["isError"], false);

    // Give actor brief moment to update snapshot
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // Discover the cancelled/killed task via list_terminal_tasks
    let all_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {}
            }),
        )
        .await;

    let all_tasks = all_resp["result"]["structuredContent"]["tasks"]
        .as_array()
        .unwrap();
    let settled = all_tasks
        .iter()
        .find(|t| t["task_id"] == task_id)
        .expect("settled task must remain visible");

    assert_eq!(settled["completed"], true);
    assert_eq!(settled["status"], "cancelled");
    assert!(settled["cwd"].is_string());
    assert!(settled["duration_secs"].is_number());
    assert!(settled["truncated"].is_boolean());
    assert!(settled["total_bytes"].is_number());
}

#[tokio::test]
#[serial]
async fn test_list_terminal_tasks_completed_discovery() {
    let harness = TestHarness::new();

    // Run a quick command in the background that completes immediately
    #[cfg(windows)]
    let cmd = "powershell -Command \"Write-Output 'quick-bg-done'\"";
    #[cfg(not(windows))]
    let cmd = "echo 'quick-bg-done'";

    let bg_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Quick background task",
                    "is_background": true
                }
            }),
        )
        .await;

    assert_eq!(bg_resp["result"]["isError"], false);
    let task_id = bg_resp["result"]["structuredContent"]["task_id"]
        .as_str()
        .expect("background task_id")
        .to_string();

    // Wait briefly for process to finish
    let poll_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "get_task_output",
                "arguments": {
                    "task_id": task_id,
                    "timeout_ms": 5000
                }
            }),
        )
        .await;
    assert_eq!(poll_resp["result"]["isError"], false);

    // Now list completed tasks
    let list_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {}
            }),
        )
        .await;

    assert_eq!(list_resp["result"]["isError"], false);
    let tasks = list_resp["result"]["structuredContent"]["tasks"]
        .as_array()
        .expect("tasks array");

    let found = tasks
        .iter()
        .find(|t| t["task_id"] == task_id)
        .expect("must find completed task_id");

    assert_eq!(found["status"], "completed");
    assert_eq!(found["completed"], true);
    assert_eq!(found["exit_code"], 0);

    // Verify bounded fields
    assert!(found["cwd"].is_string(), "snapshot must contain cwd");
    assert!(found["duration_secs"].is_number(), "snapshot must contain duration_secs");
    assert!(found["output_file"].is_string());
    assert!(found["truncated"].is_boolean(), "snapshot must contain truncated");
    assert!(found["total_bytes"].is_number(), "snapshot must contain total_bytes");
    assert!(found.get("env").is_none());
    assert!(found.get("environment").is_none());
}

#[tokio::test]
#[serial]
async fn test_auto_background_on_timeout_identity_and_no_restart() {
    // Set a short foreground block budget (1000ms) so upstream auto-backgrounds promptly
    unsafe {
        std::env::set_var("GROK_FOREGROUND_BLOCK_BUDGET_MS", "1000");
    }

    let harness = TestHarness::new();
    let marker_file = harness.temp.path().join("auto_bg_marker.txt");
    let marker_path = marker_file.to_str().unwrap().replace('\\', "/");

    // Command appends a single marker, then sleeps. If restarted, it would write twice.
    #[cfg(windows)]
    let cmd = format!(
        "powershell -NoProfile -Command \"Set-Content -Path '{}' -Value 'ONE_RUN'; Start-Sleep -Seconds 15\"",
        marker_path
    );
    #[cfg(not(windows))]
    let cmd = format!("echo 'ONE_RUN' > '{}'; sleep 15", marker_path);

    // Invoke run_terminal_cmd with NO is_background argument
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Auto-background identity test command"
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(
        structured["status"], "running",
        "auto-backgrounded task must be returned as running: {resp}"
    );
    assert_ne!(
        structured["status"], "timed_out",
        "auto-background handoff must never be reported as timed_out"
    );

    let task_id = structured["task_id"]
        .as_str()
        .expect("auto-background task_id")
        .to_string();
    assert!(!task_id.is_empty(), "task_id must not be empty");

    // Now call list_terminal_tasks and assert the recovered task is exactly this task_id
    let list_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {}
            }),
        )
        .await;

    assert_eq!(list_resp["result"]["isError"], false);
    let summary_text = list_resp["result"]["content"][0]["text"].as_str().unwrap();
    // Check bounded human summary prefers description
    assert!(
        summary_text.contains("Auto-background identity test command"),
        "summary text must prefer description: {summary_text}"
    );

    let tasks = list_resp["result"]["structuredContent"]["tasks"]
        .as_array()
        .expect("tasks array");
    let found = tasks
        .iter()
        .find(|t| t["task_id"] == task_id)
        .expect("recovered task must be the same original execution/task identity");

    assert_eq!(found["status"], "running");
    // Bounded wait for marker file to appear (must appear, cannot skip)
    let start = std::time::Instant::now();
    let mut marker_found = false;
    while start.elapsed() < std::time::Duration::from_secs(15) {
        if marker_file.exists() && std::fs::read_to_string(&marker_file).map(|s| s.contains("ONE_RUN")).unwrap_or(false) {
            marker_found = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }

    // Clean up: kill task
    let _ = harness
        .rpc(
            "tools/call",
            json!({
                "name": "kill_task",
                "arguments": { "task_id": task_id }
            }),
        )
        .await;
    assert!(
        marker_found,
        "marker file must be written by the auto-backgrounded command"
    );

    let content = std::fs::read_to_string(&marker_file).unwrap();
    let count = content.lines().filter(|l| l.contains("ONE_RUN")).count();
    assert_eq!(count, 1, "command must execute exactly once without restart");
    unsafe {
        std::env::remove_var("GROK_FOREGROUND_BLOCK_BUDGET_MS");
    }
}

#[tokio::test]
#[serial]
async fn test_list_terminal_tasks_long_command_bounded_structured_content() {
    let harness = TestHarness::new();
    // Create a very long command with a secret-like tail that must not be exposed
    let secret_tail = "SECRET_TAIL_TOKEN_DO_NOT_EXPOSE_UNBOUNDED_ABC123XYZ999";
    #[cfg(windows)]
    let long_cmd = format!(
        "powershell -NoProfile -Command \"Start-Sleep -Seconds 30 # padding_padding_padding_padding_padding_padding_padding_padding_padding_{}\"",
        secret_tail
    );
    #[cfg(not(windows))]
    let long_cmd = format!(
        "sleep 30 # padding_padding_padding_padding_padding_padding_padding_padding_padding_{}",
        secret_tail
    );

    let bg_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": long_cmd,
                    "description": "",
                    "is_background": true
                }
            }),
        )
        .await;
    if bg_resp["result"]["isError"] == true {
        panic!("bg_resp error: {:?}", bg_resp);
    }
    assert_eq!(bg_resp["result"]["isError"], false);
    let task_id = bg_resp["result"]["structuredContent"]["task_id"]
        .as_str()
        .unwrap()
        .to_string();

    let list_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {}
            }),
        )
        .await;
    assert_eq!(list_resp["result"]["isError"], false);

    let tasks = list_resp["result"]["structuredContent"]["tasks"]
        .as_array()
        .expect("tasks array");
    let found = tasks
        .iter()
        .find(|t| t["task_id"] == task_id)
        .expect("must find task");

    let cmd_in_structured = found["command"].as_str().expect("command in structuredContent");
    assert!(
        cmd_in_structured.len() <= 120,
        "command in structuredContent must be bounded <= 120 chars: got len {}",
        cmd_in_structured.len()
    );
    assert!(
        !cmd_in_structured.contains(secret_tail),
        "command in structuredContent must not expose full tail: {cmd_in_structured}"
    );

    // Human summary text must also be bounded and not contain the secret tail
    let text = list_resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        !text.contains(secret_tail),
        "summary text must not expose secret tail: {text}"
    );

    // Cleanup
    let _ = harness
        .rpc(
            "tools/call",
            json!({
                "name": "kill_task",
                "arguments": { "task_id": task_id }
            }),
        )
        .await;
}

#[tokio::test]
#[serial]
async fn test_set_workspace_preserves_session_terminal_backend_and_tasks() {
    let harness = TestHarness::new();
    let session_header = json!({ "openai/session": "chat-preserve-test-123" });

    // Start a background task in the initial workspace
    #[cfg(windows)]
    let cmd = "powershell -NoProfile -Command \"Start-Sleep -Seconds 30\"";
    #[cfg(not(windows))]
    let cmd = "sleep 30";

    let bg_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Background task for session switch test",
                    "is_background": true
                },
                "_meta": session_header
            }),
        )
        .await;
    assert_eq!(bg_resp["result"]["isError"], false, "bg_resp: {bg_resp}");
    let task_id = bg_resp["result"]["structuredContent"]["task_id"]
        .as_str()
        .expect("task_id")
        .to_string();

    // Now switch workspace in this session
    let new_dir = tempfile::TempDir::new().unwrap();
    let new_dir_str = dunce::canonicalize(new_dir.path())
        .unwrap()
        .display()
        .to_string();

    let sw_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": new_dir_str
                },
                "_meta": session_header
            }),
        )
        .await;
    assert_eq!(sw_resp["result"]["isError"], false);

    // Query tasks in this session after workspace switch
    let list_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {},
                "_meta": session_header
            }),
        )
        .await;
    assert_eq!(list_resp["result"]["isError"], false);

    let tasks = list_resp["result"]["structuredContent"]["tasks"]
        .as_array()
        .expect("tasks array");
    let found = tasks.iter().find(|t| t["task_id"] == task_id);
    assert!(
        found.is_some(),
        "task must survive set_workspace in the same session without being killed or dropped"
    );

    // Clean up task
    let _ = harness
        .rpc(
            "tools/call",
            json!({
                "name": "kill_task",
                "arguments": { "task_id": task_id },
                "_meta": session_header
            }),
        )
        .await;
}

#[tokio::test]
#[serial]
async fn test_completed_task_history_survives_workspace_roundtrip() {
    let harness = TestHarness::new();
    let session_header = json!({ "openai/session": "chat-completed-roundtrip-456" });

    // Run a short background task to completion
    #[cfg(windows)]
    let cmd = "powershell -NoProfile -Command \"Write-Output 'ROUNDTRIP_OK'\"";
    #[cfg(not(windows))]
    let cmd = "echo ROUNDTRIP_OK";

    let bg_resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Short task for roundtrip discovery",
                    "is_background": true
                },
                "_meta": session_header
            }),
        )
        .await;
    assert_eq!(bg_resp["result"]["isError"], false);
    let task_id = bg_resp["result"]["structuredContent"]["task_id"]
        .as_str()
        .expect("task_id")
        .to_string();

    // Wait until completed
    let mut completed = false;
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(10) {
        let list_resp = harness
            .rpc(
                "tools/call",
                json!({
                    "name": "list_terminal_tasks",
                    "arguments": {},
                    "_meta": session_header
                }),
            )
            .await;
        if let Some(tasks) = list_resp["result"]["structuredContent"]["tasks"].as_array() {
            if let Some(t) = tasks.iter().find(|t| t["task_id"] == task_id) {
                if t["completed"] == true {
                    completed = true;
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
    assert!(completed, "task must complete within 10s");

    // Switch workspace away and then back
    let temp_a = tempfile::TempDir::new().unwrap();
    let temp_b = tempfile::TempDir::new().unwrap();

    for dir in [&temp_a, &temp_b] {
        let dir_str = dunce::canonicalize(dir.path()).unwrap().display().to_string();
        let sw = harness
            .rpc(
                "tools/call",
                json!({
                    "name": "set_workspace",
                    "arguments": { "path": dir_str },
                    "_meta": session_header
                }),
            )
            .await;
        assert_eq!(sw["result"]["isError"], false);
    }

    // Query tasks after workspace round-trip: completed task must still be present with original CWD
    let final_list = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {},
                "_meta": session_header
            }),
        )
        .await;
    assert_eq!(final_list["result"]["isError"], false);
    let tasks = final_list["result"]["structuredContent"]["tasks"]
        .as_array()
        .expect("tasks array");
    let found = tasks.iter().find(|t| t["task_id"] == task_id);
    assert!(
        found.is_some(),
        "completed task must remain discoverable after workspace roundtrip"
    );
    let found_task = found.unwrap();
    assert_eq!(found_task["completed"], true);
    assert_eq!(found_task["status"], "completed");
    assert!(found_task["cwd"].is_string());
}
