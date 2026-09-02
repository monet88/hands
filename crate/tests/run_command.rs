mod common;
use common::TestHarness;
use serde_json::json;
use serial_test::serial;

#[tokio::test]
#[serial]
async fn test_run_command_tool_descriptor_and_list() {
    let harness = TestHarness::new();
    let resp = harness.rpc("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let run_cmd = tools
        .iter()
        .find(|t| t["name"] == "run_command")
        .expect("run_command must be in tools/list");

    assert_eq!(run_cmd["annotations"]["readOnlyHint"], false);
    assert_eq!(run_cmd["annotations"]["destructiveHint"], true);

    let schema = &run_cmd["inputSchema"];
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["command"].is_object());
    assert!(schema["properties"]["args"].is_object());
    assert!(schema["properties"]["workdir"].is_object());
    assert!(schema["properties"]["stdin"].is_object());
    assert!(schema["properties"]["timeout_ms"].is_object());
    assert!(schema["properties"]["env"].is_object());

    let req = schema["required"].as_array().expect("required array");
    assert!(req.iter().any(|v| v == "command"));
}

#[tokio::test]
#[serial]
async fn test_run_command_pre_spawn_validation_rejects_shell_scripts() {
    let harness = TestHarness::new();

    for bad_cmd in &["test.cmd", "setup.bat", "RUN.BAT", "script.CMD"] {
        let resp = harness
            .rpc(
                "tools/call",
                json!({
                    "name": "run_command",
                    "arguments": {
                        "command": bad_cmd,
                        "args": []
                    }
                }),
            )
            .await;

        assert_eq!(resp["result"]["isError"], true);
        let structured = &resp["result"]["structuredContent"];
        assert_eq!(structured["execution_state"], "not_started");
        assert_eq!(structured["command_started"], false);
        assert_eq!(structured["command_completed"], false);
        assert_eq!(structured["exit_code"], serde_json::Value::Null);
    }
}

#[tokio::test]
#[serial]
async fn test_run_command_pre_spawn_validation_rejects_invalid_args() {
    let harness = TestHarness::new();

    // 1. Missing/empty command
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "   ",
                    "args": []
                }
            }),
        )
        .await;
    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(resp["result"]["structuredContent"]["execution_state"], "not_started");
    assert_eq!(resp["result"]["structuredContent"]["command_started"], false);

    // 2. Overlong command name
    let long_cmd = "a".repeat(1025);
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": long_cmd,
                }
            }),
        )
        .await;
    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(resp["result"]["structuredContent"]["execution_state"], "not_started");

    // 3. Excessive argv count
    let args: Vec<String> = (0..2001).map(|i| i.to_string()).collect();
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "echo",
                    "args": args
                }
            }),
        )
        .await;
    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(resp["result"]["structuredContent"]["execution_state"], "not_started");

    // 4. Stdin exceeding 1MB limit
    let big_stdin = "x".repeat(1024 * 1024 + 10);
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "echo",
                    "stdin": big_stdin
                }
            }),
        )
        .await;
    assert_eq!(resp["result"]["isError"], true);
    assert_eq!(resp["result"]["structuredContent"]["execution_state"], "not_started");
}

#[tokio::test]
#[serial]
async fn test_run_command_pre_spawn_missing_executable_spawn_failure() {
    let harness = TestHarness::new();

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "non_existent_executable_12345_xyz",
                    "args": ["arg1"]
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], true);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "not_started");
    assert_eq!(structured["command_started"], false);
    assert_eq!(structured["command_completed"], false);
    assert_eq!(structured["exit_code"], serde_json::Value::Null);
    assert!(structured["error"].as_str().is_some());
}

#[tokio::test]
#[serial]
async fn test_run_command_literal_argv_and_zero_exit() {
    let harness = TestHarness::new();

    // Use git hash-object --stdin or powershell/python or cargo to test literal argv
    // On Windows and Unix, git is guaranteed available in this repo.
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "git",
                    "args": ["--version"]
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "completed");
    assert_eq!(structured["command_started"], true);
    assert_eq!(structured["command_completed"], true);
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["timed_out"], false);
    let out = structured["stdout"].as_str().expect("stdout string");
    assert!(out.contains("git version"));

    // Test metacharacters verbatim without shell interpretation:
    // With shell, $ENV or quotes would be expanded or stripped.
    // git hash-object --stdin
    let sentinel = "SENTINEL_$PATH_`test`_\"quotes\"_JSON_{'a':1}";
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "git",
                    "args": ["hash-object", "--stdin"],
                    "stdin": sentinel
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "completed");
    assert_eq!(structured["exit_code"], 0);
    let hash = structured["stdout"].as_str().expect("hash string").trim();
    assert_eq!(hash.len(), 40, "git hash-object must output 40-char SHA1");
}

#[tokio::test]
#[serial]
async fn test_run_command_nonzero_exit_is_completed_not_tool_failure() {
    let harness = TestHarness::new();

    // git checkout invalid-ref-xyz-12345 exits non-zero (typically 1 or 128)
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "git",
                    "args": ["rev-parse", "--verify", "refs/heads/non-existent-branch-12345"]
                }
            }),
        )
        .await;

    // A non-zero exit code is a completed command outcome, not an MCP tool failure (isError: false).
    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "completed");
    assert_eq!(structured["command_started"], true);
    assert_eq!(structured["command_completed"], true);
    assert_ne!(structured["exit_code"], 0);
    assert_eq!(structured["timed_out"], false);
}

#[tokio::test]
#[serial]
async fn test_run_command_env_and_workdir_isolation() {
    let harness = TestHarness::new();
    let temp_sub = harness.temp.path().join("subfolder");
    std::fs::create_dir_all(&temp_sub).unwrap();

    // Run git rev-parse --show-toplevel inside subfolder
    // Notice: env values and stdin are NOT echoed back in structuredContent metadata
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": "git",
                    "args": ["status", "--short"],
                    "workdir": temp_sub.display().to_string(),
                    "env": {
                        "SECRET_ENV_VAR_TEST": "secret_value_must_not_leak"
                    },
                    "stdin": "secret_stdin_must_not_leak"
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "completed");
    // Acceptance criterion: Environment values and stdin are never echoed back merely as execution metadata.
    assert!(structured.get("env").is_none());
    assert!(structured.get("stdin").is_none());
    let resp_str = serde_json::to_string(&resp).unwrap();
    assert!(!resp_str.contains("secret_value_must_not_leak"));
    assert!(!resp_str.contains("secret_stdin_must_not_leak"));
}

#[tokio::test]
#[serial]
async fn test_run_command_minimal_timeout() {
    let harness = TestHarness::new();

    // Run a command with a very short timeout (e.g. 50ms) that sleeps longer
    #[cfg(windows)]
    let (cmd, args) = ("powershell", vec!["-NoProfile", "-Command", "Start-Sleep -Milliseconds 2000"]);
    #[cfg(not(windows))]
    let (cmd, args) = ("sleep", vec!["2"]);

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": cmd,
                    "args": args,
                    "timeout_ms": 100
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "completed");
    assert_eq!(structured["command_started"], true);
    assert_eq!(structured["command_completed"], true);
    assert_eq!(structured["timed_out"], true);
    assert_eq!(structured["exit_code"], -1);
}
#[tokio::test]
#[serial]
async fn test_run_command_bounded_output() {
    let harness = TestHarness::new();

    // Generate output larger than 40KB
    #[cfg(windows)]
    let (cmd, args) = ("powershell", vec!["-NoProfile", "-Command", "1..1500 | ForEach-Object { 'OUTPUT_LINE_EXCEEDING_LIMIT_TEST_1234567890' }"]);
    #[cfg(not(windows))]
    let (cmd, args) = ("sh", vec!["-c", "for i in $(seq 1 1500); do echo OUTPUT_LINE_EXCEEDING_LIMIT_TEST_1234567890; done"]);

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_command",
                "arguments": {
                    "command": cmd,
                    "args": args
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "completed");
    assert_eq!(structured["exit_code"], 0);

    let stdout = structured["stdout"].as_str().expect("stdout string");
    assert!(stdout.len() > 40_000, "raw stdout should exceed 40KB");

    let content_text = resp["result"]["content"][0]["text"].as_str().expect("content text");
    assert!(content_text.contains("[Output truncated: showing first"));
    assert!(content_text.len() < stdout.len(), "content text must be bounded");
}
