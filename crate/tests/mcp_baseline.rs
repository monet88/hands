mod common;
use common::TestHarness;
use hands::host;
use hands::service;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;
#[tokio::test]
#[serial]
async fn test_mcp_initialize() {
    let harness = TestHarness::new();
    let resp = harness
        .rpc(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "clientInfo": { "name": "test-client", "version": "1.0" }
            }),
        )
        .await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["serverInfo"]["name"], "Hands");

    let version = resp["result"]["serverInfo"]["version"]
        .as_str()
        .expect("serverInfo.version string");
    assert!(
        version.contains("+c059e0d."),
        "serverInfo.version '{version}' must contain upstream provenance +c059e0d.<rev>"
    );

    assert_eq!(resp["result"]["capabilities"]["tools"]["listChanged"], false);
    assert_eq!(
        resp["result"]["capabilities"]["resources"]["listChanged"],
        false
    );
    assert!(resp["result"]["capabilities"]["extensions"]["io.modelcontextprotocol/skills"].is_object());
    assert!(resp["result"]["instructions"].is_string());
}

#[tokio::test]
#[serial]
async fn test_mcp_tools_list() {
    let harness = TestHarness::new();
    let resp = harness.rpc("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();

    assert!(names.contains(&"workspace_info"), "must list workspace_info");
    assert!(names.contains(&"set_workspace"), "must list set_workspace");
    assert!(names.contains(&"read_file"), "must list read_file");
    assert!(names.contains(&"run_terminal_cmd"), "must list run_terminal_cmd");
    assert!(names.contains(&"run_command"), "must list run_command");

    // Check annotations and _meta
    for tool in tools {
        assert!(tool["annotations"].is_object(), "tool must have annotations");
        assert!(
            tool["annotations"]["readOnlyHint"].is_boolean(),
            "tool must have readOnlyHint"
        );
        assert!(tool["_meta"].is_object(), "tool must have _meta");
        assert!(
            tool["_meta"]["openai/toolInvocation/invoking"].is_string(),
            "tool must have invoking metadata"
        );
        assert!(
            tool["_meta"]["openai/toolInvocation/invoked"].is_string(),
            "tool must have invoked metadata"
        );
    }

    let read_file = tools
        .iter()
        .find(|t| t["name"] == "read_file")
        .expect("read_file tool");
    assert_eq!(read_file["annotations"]["readOnlyHint"], true);

    let run_cmd = tools
        .iter()
        .find(|t| t["name"] == "run_terminal_cmd")
        .expect("run_terminal_cmd tool");
    assert_eq!(run_cmd["annotations"]["readOnlyHint"], false);

    // Explicitly assert run_terminal_cmd schema does NOT contain execution_mode or yield_after_ms
    let cmd_props = &run_cmd["inputSchema"]["properties"];
    assert!(
        cmd_props.get("execution_mode").is_none(),
        "run_terminal_cmd schema must not contain execution_mode"
    );
    assert!(
        cmd_props.get("yield_after_ms").is_none(),
        "run_terminal_cmd schema must not contain yield_after_ms"
    );
}

#[tokio::test]
#[serial]
async fn test_mcp_resources_and_skills() {
    let harness = TestHarness::new();

    let res_list = harness.rpc("resources/list", json!({})).await;
    assert_eq!(
        res_list["result"]["resources"][0]["uri"],
        "skill://hands/hands-code/SKILL.md"
    );

    let res_read = harness
        .rpc(
            "resources/read",
            json!({
                "uri": "skill://hands/hands-code/SKILL.md"
            }),
        )
        .await;
    assert!(
        res_read["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hands-code")
    );

    let skill_list = harness.rpc("skills/list", json!({})).await;
    assert_eq!(
        skill_list["result"]["skills"][0]["uri"],
        "skill://hands/hands-code/SKILL.md"
    );

    let skill_get = harness
        .rpc(
            "skills/get",
            json!({
                "uri": "skill://hands/hands-code/SKILL.md"
            }),
        )
        .await;
    assert_eq!(
        skill_get["result"]["skill"]["uri"],
        "skill://hands/hands-code/SKILL.md"
    );
}

#[tokio::test]
#[serial]
async fn test_mcp_tools_call() {
    let harness = TestHarness::new();

    let info = harness
        .rpc(
            "tools/call",
            json!({
                "name": "workspace_info",
                "arguments": {}
            }),
        )
        .await;
    assert_eq!(info["result"]["isError"], false);
    assert!(info["result"]["structuredContent"]["workspace"].is_string());

    let target_dir = TempDir::new().unwrap();
    let set_ws = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": target_dir.path().to_str().unwrap()
                }
            }),
        )
        .await;
    assert_eq!(set_ws["result"]["isError"], false);
    assert!(set_ws["result"]["structuredContent"]["workspace"].is_string());
}

#[tokio::test]
#[serial]
async fn test_mcp_error_handling() {
    let harness = TestHarness::new();

    // Method not found (-32601)
    let resp = harness.rpc("non_existent_method", json!({})).await;
    assert_eq!(resp["error"]["code"], -32601);

    // Invalid params (-32602)
    let resp = harness.rpc("tools/call", json!({})).await;
    assert_eq!(resp["error"]["code"], -32602);
}

#[test]
fn test_git_ancestry_provenance() {
    assert_eq!(host::UPSTREAM_BASE_COMMIT, "c059e0d");
    let status = std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", "c059e0d", "HEAD"])
        .status();
    if let Ok(s) = status {
        assert!(s.success(), "HEAD must descend from upstream commit c059e0d");
    }
}

#[test]
#[serial]
fn test_host_and_service_provenance_and_isolation() {
    let config_dir = TempDir::new().unwrap();
    unsafe {
        std::env::set_var("HANDS_CONFIG_DIR", config_dir.path());
    }
    assert_eq!(host::config_dir(), config_dir.path());
    assert_eq!(
        host::tunnel_client_dir(),
        config_dir.path().join("tunnel-client")
    );

    let status = service::status_json(config_dir.path());
    assert_eq!(status["upstream_base"], "c059e0d");
    assert_eq!(status["git_revision"], host::DEV_GIT_REV);

    // Assert provenance git_revision matches .hands-source-rev or hands source git rev
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let prov_file = std::path::Path::new(manifest_dir).join(".hands-source-rev");
    if prov_file.exists() {
        let rev = std::fs::read_to_string(&prov_file).unwrap();
        assert_eq!(host::DEV_GIT_REV, rev.trim(), "DEV_GIT_REV must match injected .hands-source-rev");
    } else {
        let git_out = std::process::Command::new("git")
            .args(["-C", manifest_dir, "rev-parse", "--short", "HEAD"])
            .output();
        if let Ok(out) = git_out {
            if out.status.success() {
                let rev = String::from_utf8(out.stdout).unwrap();
                assert_eq!(host::DEV_GIT_REV, rev.trim(), "DEV_GIT_REV must match source repo git rev");
            }
        }
    }
}
