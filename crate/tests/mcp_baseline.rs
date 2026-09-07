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
    assert_eq!(
        host::GROK_BUILD_PINNED_SHA,
        "72a61251fcffb464bcc687aeb5a998e5a98ec0c9"
    );

    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let prov_file = std::path::Path::new(manifest_dir).join(".hands-source-rev");

    if prov_file.exists() {
        // Injected Hands: MUST prove BOTH Hands source ancestry and outer grok-build pinned SHA
        let manifest = std::path::PathBuf::from(manifest_dir);
        let cur = dunce::canonicalize(&manifest).unwrap_or(manifest);

        // 1. Hands source repo discovery and ancestry check
        let mut hands_repo = None;
        let mut p = cur.as_path();
        while let Some(parent) = p.parent() {
            let sibling = parent.join("hands");
            if sibling.join("scripts").join("inject.py").is_file() && sibling.join(".git").exists()
            {
                hands_repo = Some(sibling);
                break;
            }
            if parent.join("scripts").join("inject.py").is_file() && parent.join(".git").exists() {
                hands_repo = Some(parent.to_path_buf());
                break;
            }
            p = parent;
        }
        let hands_repo = hands_repo.unwrap_or_else(|| {
            panic!(
                "failed to locate Hands source repository for injected crate at {}",
                cur.display()
            )
        });
        assert!(
            hands_repo.join(".git").exists(),
            "Hands source repository must contain .git at {}",
            hands_repo.display()
        );
        let status = std::process::Command::new("git")
            .args([
                "-C",
                hands_repo.to_str().unwrap(),
                "merge-base",
                "--is-ancestor",
                host::UPSTREAM_BASE_COMMIT,
                "HEAD",
            ])
            .status()
            .expect("failed to execute git merge-base on Hands source repository");
        assert!(
            status.success(),
            "Hands source repository at {} HEAD must descend from upstream Hands commit {}",
            hands_repo.display(),
            host::UPSTREAM_BASE_COMMIT
        );

        // 2. Outer grok-build repo discovery and pinned SHA check
        let mut grok_build_repo = None;
        let mut p = cur.as_path();
        while let Some(parent) = p.parent() {
            if parent.join("Cargo.toml").is_file()
                && parent.join("crates").join("codegen").join("xai-grok-tools").is_dir()
                && parent.join(".git").exists()
            {
                grok_build_repo = Some(parent.to_path_buf());
                break;
            }
            p = parent;
        }
        let grok_build_repo = grok_build_repo.unwrap_or_else(|| {
            panic!(
                "failed to locate outer grok-build repository for injected hands at {}",
                cur.display()
            )
        });
        let output = std::process::Command::new("git")
            .args(["-C", grok_build_repo.to_str().unwrap(), "rev-parse", "HEAD"])
            .output()
            .expect("failed to execute git rev-parse HEAD on outer grok-build repository");
        assert!(
            output.status.success(),
            "git rev-parse HEAD failed on outer grok-build repository at {}",
            grok_build_repo.display()
        );
        let head_sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert_eq!(
            head_sha,
            host::GROK_BUILD_PINNED_SHA,
            "outer grok-build HEAD at {} must match pinned grok-build SHA {}",
            grok_build_repo.display(),
            host::GROK_BUILD_PINNED_SHA
        );
    } else {
        // Standalone Hands: MUST prove Hands ancestry
        let manifest = std::path::PathBuf::from(manifest_dir);
        let hands_repo = dunce::canonicalize(&manifest)
            .unwrap_or(manifest)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| panic!("failed to resolve standalone Hands repository root"));
        assert!(
            hands_repo.join(".git").exists(),
            "standalone Hands repository must contain .git at {}",
            hands_repo.display()
        );
        let status = std::process::Command::new("git")
            .args([
                "-C",
                hands_repo.to_str().unwrap(),
                "merge-base",
                "--is-ancestor",
                host::UPSTREAM_BASE_COMMIT,
                "HEAD",
            ])
            .status()
            .expect("failed to execute git merge-base on standalone Hands repository");
        assert!(
            status.success(),
            "standalone Hands HEAD must descend from upstream Hands commit {}",
            host::UPSTREAM_BASE_COMMIT
        );
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
