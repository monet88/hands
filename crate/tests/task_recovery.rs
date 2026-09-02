use std::sync::Arc;
use hands::mcp::McpHost;
use serde_json::{json, Value};
use serial_test::serial;
use tempfile::TempDir;

struct TestHarness {
    _temp: TempDir,
    _config_dir: TempDir,
    host: Arc<McpHost>,
}

impl TestHarness {
    fn new() -> Self {
        let temp = TempDir::new().expect("workspace tempdir");
        let config_dir = TempDir::new().expect("config tempdir");
        unsafe {
            std::env::set_var("HANDS_CONFIG_DIR", config_dir.path());
        }
        let host = McpHost::new(temp.path().to_path_buf());
        Self {
            _temp: temp,
            _config_dir: config_dir,
            host,
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        let req = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        self.host
            .handle_rpc(req)
            .await
            .expect("response expected for request with id")
    }
}

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
    assert!(props.get("status_filter").is_some());

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
                "arguments": {
                    "status_filter": "running"
                }
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
    assert!(found["command"].as_str().unwrap().to_lowercase().contains("sleep"));
    assert!(found["output_file"].is_string());

    // Verify bounded snapshot does not expose secrets/environment variables
    assert!(found.get("env").is_none(), "must not expose env");
    assert!(found.get("environment").is_none(), "must not expose environment");
    assert!(found.get("secrets").is_none(), "must not expose secrets");

    // Filter by completed should NOT match our running task
    let completed_list = harness
        .rpc(
            "tools/call",
            json!({
                "name": "list_terminal_tasks",
                "arguments": {
                    "status_filter": "completed"
                }
            }),
        )
        .await;
    let completed_tasks = completed_list["result"]["structuredContent"]["tasks"]
        .as_array()
        .unwrap();
    assert!(
        !completed_tasks.iter().any(|t| t["task_id"] == task_id),
        "running task should not show up under completed filter"
    );

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
                "arguments": {
                    "status_filter": "all"
                }
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
                "arguments": {
                    "status_filter": "completed"
                }
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
    assert!(found["duration_secs"].is_number());
    assert!(found["output_file"].is_string());
    assert!(found.get("env").is_none());
    assert!(found.get("environment").is_none());
}
