mod common;
use common::TestHarness;
use serde_json::json;
use serial_test::serial;
#[tokio::test]
#[serial]
async fn test_run_terminal_cmd_sentinel_in_structured_content() {
    let harness = TestHarness::new();
    let sentinel = "SENTINEL_TICKET_37_FOREGROUND_OUTPUT_PROVED";
    let cmd = format!("echo {sentinel}");

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Prove foreground command output visibility in structuredContent"
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);

    // Assert authoritative structuredContent is present and shaped
    let structured = &resp["result"]["structuredContent"];
    assert!(structured.is_object(), "structuredContent must be a JSON object: {resp}");
    assert_eq!(structured["type"], "Bash");
    assert_eq!(structured["exit_code"], 0);

    let output_str = structured["output"].as_str().expect("structuredContent.output must be a string");
    assert!(
        output_str.contains(sentinel),
        "sentinel '{sentinel}' must appear in structuredContent.output, got: {output_str}"
    );

    // Assert content[].text contains the output and does not say '(no output)'
    let content_text = resp["result"]["content"][0]["text"].as_str().expect("content[0].text string");
    assert!(
        content_text.contains(sentinel),
        "content[].text must contain sentinel: {content_text}"
    );
    assert!(
        !content_text.contains("(no output)"),
        "content[].text must never claim (no output) when output exists: {content_text}"
    );
}

#[tokio::test]
#[serial]
async fn test_run_terminal_cmd_failure_is_error() {
    let harness = TestHarness::new();

    // Command that fails with exit code 1
    #[cfg(windows)]
    let cmd = "powershell -Command exit 1";
    #[cfg(not(windows))]
    let cmd = "exit 1";

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Verify exit 1 marks isError true"
                }
            }),
        )
        .await;

    assert_eq!(
        resp["result"]["isError"], true,
        "failing command must produce isError: true, got: {resp}"
    );
    let structured = &resp["result"]["structuredContent"];
    assert!(structured.is_object());
    assert_eq!(structured["exit_code"], 1);
}

#[tokio::test]
#[serial]
async fn test_large_output_concise_text_truncation() {
    let harness = TestHarness::new();

    // Generate output larger than 4KB to verify bounded summary
    #[cfg(windows)]
    let cmd = "powershell -Command \"1..1000 | ForEach-Object { Write-Output 'LINE_OUTPUT_CHUNK_DATA_TEST_1234567890' }\"";
    #[cfg(not(windows))]
    let cmd = "seq 1 1000 | while read i; do echo LINE_OUTPUT_CHUNK_DATA_TEST_1234567890; done";

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": cmd,
                    "description": "Generate large output for truncation proof"
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false);
    let structured = &resp["result"]["structuredContent"];
    assert!(structured.is_object());
    let full_output = structured["output"].as_str().expect("structuredContent.output string");
    assert!(
        full_output.len() > 10_000,
        "full output should exceed 10KB, got: {} bytes",
        full_output.len()
    );

    let content_text = resp["result"]["content"][0]["text"].as_str().expect("content[0].text string");
    // Assert content[].text does not duplicate huge output (capped around 4KB + banner)
    assert!(
        content_text.len() < full_output.len(),
        "content[].text must be bounded and shorter than full output"
    );
    assert!(
        content_text.contains("[Output truncated: showing first"),
        "content[].text must have truncation marker, got: {content_text}"
    );
}

#[tokio::test]
#[serial]
async fn test_get_task_output_structured_framing() {
    let harness = TestHarness::new();

    // Query non-existent task
    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "get_task_output",
                "arguments": {
                    "task_ids": ["non-existent-task-id-12345"]
                }
            }),
        )
        .await;

    assert_eq!(
        resp["result"]["isError"], true,
        "TaskNotFound must report isError: true"
    );
    let structured = &resp["result"]["structuredContent"];
    assert!(structured.is_object());
    assert_eq!(structured["type"], "TaskOutput");
    assert!(
        structured["error"].as_str().is_some() || structured["TaskNotFound"].as_str().is_some(),
        "structuredContent must carry error information: {structured}"
    );
}

#[tokio::test]
#[serial]
async fn test_edit_diff_preserves_authoritative_structured_payload() {
    let harness = TestHarness::new();
    let target = harness.temp.path().join("edit-diff.txt");
    std::fs::write(&target, "hello\n").expect("write edit fixture");

    let resp = harness
        .rpc(
            "tools/call",
            json!({
                "name": "search_replace",
                "arguments": {
                    "file_path": target,
                    "old_string": "hello",
                    "new_string": "hello world"
                }
            }),
        )
        .await;

    assert_eq!(resp["result"]["isError"], false, "edit failed: {resp}");
    let result = &resp["result"];
    let structured = &result["structuredContent"];

    assert_eq!(
        structured["type"], "SearchReplace",
        "typed ToolOutput must remain authoritative: {structured}"
    );
    assert_eq!(structured["kind"], "edited");
    assert!(structured["default_workspace"].is_string());
    assert!(structured["target_path"].is_string());

    let diff = structured["diff"].as_str().expect("edit diff string");
    assert!(diff.contains("-hello"), "missing removed line in diff: {diff}");
    assert!(diff.contains("+hello world"), "missing added line in diff: {diff}");
    assert!(
        result["_meta"]["openai/outputTemplate"].is_string(),
        "edit result must retain upstream widget metadata: {result}"
    );
}
