//! Unified tool execution engine behind MCP.
//!
//! Owns bridge lifecycle, Workspace-aware bridge caching, virtual tool
//! injection (e.g. `workspace_info`), native tools (e.g. `run_command`),
//! and common execution/result/error shaping.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use xai_grok_tools::bridge::ToolBridge;

use crate::host;

pub const READ_ONLY_TOOLS: &[&str] = &[
    "workspace_info",
    "read_file",
    "grep",
    "list_dir",
    "glob",
    "get_task_output",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallResult {
    pub content: Vec<ToolContent>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolContent {
    Text { text: String },
}

impl ToolCallResult {
    pub fn text(text: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            is_error,
        }
    }

    pub fn to_value(&self) -> Value {
        json!({
            "content": self.content.iter().map(|c| match c {
                ToolContent::Text { text } => json!({
                    "type": "text",
                    "text": text
                }),
            }).collect::<Vec<_>>(),
            "isError": self.is_error
        })
    }

    pub fn from_value(value: Value) -> Self {
        let is_error = value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let content = value
            .get("content")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        item.get("text")
                            .and_then(Value::as_str)
                            .map(|t| ToolContent::Text {
                                text: t.to_string(),
                            })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Self { content, is_error }
    }
}

pub struct ToolEngine {
    fallback_cwd: PathBuf,
    cached: Mutex<Option<(PathBuf, ToolBridge)>>,
    call_seq: AtomicU64,
}

impl ToolEngine {
    pub fn new(fallback_cwd: PathBuf) -> Self {
        Self {
            fallback_cwd,
            cached: Mutex::new(None),
            call_seq: AtomicU64::new(1),
        }
    }

    pub fn workspace(&self) -> PathBuf {
        host::resolve_workspace(&self.fallback_cwd)
    }

    pub async fn bridge(&self) -> Result<ToolBridge, String> {
        let cwd = self.workspace();
        {
            let cache = self.cached.lock().await;
            if let Some((path, bridge)) = cache.as_ref()
                && path == &cwd
            {
                return Ok(bridge.clone());
            }
        }
        let bridge = host::build_bridge(cwd.clone()).await?;
        let mut cache = self.cached.lock().await;
        if let Some((path, cached_bridge)) = cache.as_ref()
            && path == &cwd
        {
            return Ok(cached_bridge.clone());
        }
        *cache = Some((cwd, bridge.clone()));
        Ok(bridge)
    }

    #[cfg(test)]
    pub async fn cached_workspace(&self) -> Option<PathBuf> {
        let cache = self.cached.lock().await;
        cache.as_ref().map(|(path, _)| path.clone())
    }

    pub async fn list_tools(&self) -> Result<Vec<Value>, String> {
        let mut tools = vec![json!({
            "name": "workspace_info",
            "description": "Return the active local workspace root. Call this before other tools if the user switched repos with hands use.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "openWorldHint": false,
            }
        })];
        tools.push(crate::run_proc::tool_json());
        let defs = self.bridge().await?.tool_definitions().await;
        tools.extend(defs.into_iter().map(|d| {
            let name = d.function.name;
            let read_only = READ_ONLY_TOOLS.contains(&name.as_str());
            json!({
                "name": name,
                "description": d.function.description.unwrap_or_default(),
                "inputSchema": d.function.parameters,
                "annotations": {
                    "readOnlyHint": read_only,
                    "destructiveHint": !read_only,
                    "openWorldHint": false,
                }
            })
        }));
        Ok(tools)
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallResult, String> {
        if name == "workspace_info" {
            let cwd = self.workspace();
            return Ok(ToolCallResult::text(
                format!(
                    "workspace: {}\nsource_git_sha: {}",
                    cwd.display(),
                    crate::build_provenance::SOURCE_GIT_SHA
                ),
                false,
            ));
        }
        if name == crate::run_proc::TOOL_NAME {
            let ws = self.workspace();
            let ws_str = ws.to_string_lossy().to_string();
            let val = crate::run_proc::handle_call(&arguments, Some(&ws_str)).await;
            return Ok(ToolCallResult::from_value(val));
        }
        let call_id = format!("mcp-{}", self.call_seq.fetch_add(1, Ordering::Relaxed));
        let bridge = self.bridge().await?;
        match bridge.call(name, arguments, &call_id).await {
            Ok(result) => Ok(ToolCallResult::text(result.prompt_text, false)),
            Err(e) => Ok(ToolCallResult::text(e.to_string(), true)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex as StdMutex;

    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    struct EnvGuard {
        saved_config_dir: Option<std::ffi::OsString>,
        saved_workspace: Option<std::ffi::OsString>,
        saved_legacy: Option<std::ffi::OsString>,
        root: PathBuf,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.saved_config_dir {
                    Some(v) => std::env::set_var("HANDS_CONFIG_DIR", v),
                    None => std::env::remove_var("HANDS_CONFIG_DIR"),
                }
                match &self.saved_workspace {
                    Some(v) => std::env::set_var("HANDS_WORKSPACE", v),
                    None => std::env::remove_var("HANDS_WORKSPACE"),
                }
                match &self.saved_legacy {
                    Some(v) => std::env::set_var("GROK_HARNESS_WORKSPACE", v),
                    None => std::env::remove_var("GROK_HARNESS_WORKSPACE"),
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn isolate_env(name: &str) -> (std::sync::MutexGuard<'static, ()>, EnvGuard) {
        let guard = TEST_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!(
            "hands_engine_test_{}_{}_{}",
            name,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("create isolated test config dir");
        let env_guard = EnvGuard {
            saved_config_dir: std::env::var_os("HANDS_CONFIG_DIR"),
            saved_workspace: std::env::var_os("HANDS_WORKSPACE"),
            saved_legacy: std::env::var_os("GROK_HARNESS_WORKSPACE"),
            root: root.clone(),
        };
        unsafe {
            std::env::set_var("HANDS_CONFIG_DIR", &root);
            std::env::remove_var("HANDS_WORKSPACE");
            std::env::remove_var("GROK_HARNESS_WORKSPACE");
        }
        (guard, env_guard)
    }

    #[tokio::test]
    async fn test_tool_call_result_serialization() {
        let res = ToolCallResult::text("hello world", false);
        let val = res.to_value();
        assert_eq!(val["isError"], false);
        assert_eq!(val["content"][0]["type"], "text");
        assert_eq!(val["content"][0]["text"], "hello world");

        let roundtrip = ToolCallResult::from_value(val);
        assert_eq!(roundtrip, res);

        let err_res = ToolCallResult::text("something broke", true);
        let err_val = err_res.to_value();
        assert_eq!(err_val["isError"], true);
        assert_eq!(ToolCallResult::from_value(err_val), err_res);
    }

    #[tokio::test]
    async fn test_tool_listing_includes_virtual_native_and_bridge_tools() {
        let (_lock, _guard) = isolate_env("list");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");

        let engine = ToolEngine::new(ws_dir);
        let tools = engine.list_tools().await.expect("list_tools should succeed");

        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();

        assert!(names.contains(&"workspace_info"), "must include workspace_info");
        assert!(names.contains(&"run_command"), "must include run_command");
        assert!(names.contains(&"read_file"), "must include read_file");
        assert!(names.contains(&"grep"), "must include grep");
        assert!(names.contains(&"list_dir"), "must include list_dir");

        let ws_tool = tools.iter().find(|t| t["name"] == "workspace_info").unwrap();
        assert_eq!(ws_tool["annotations"]["readOnlyHint"], true);
        assert_eq!(ws_tool["annotations"]["destructiveHint"], false);
    }

    #[tokio::test]
    async fn test_virtual_tool_workspace_info_call() {
        let (_lock, _guard) = isolate_env("ws_info");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");

        let engine = ToolEngine::new(ws_dir);
        let res = engine
            .call_tool("workspace_info", json!({}))
            .await
            .expect("call_tool should succeed");

        assert!(!res.is_error);
        let text = match &res.content[0] {
            ToolContent::Text { text } => text,
        };
        assert!(text.contains("workspace:"));
        assert!(text.contains("source_git_sha:"));
        assert!(text.contains(crate::build_provenance::SOURCE_GIT_SHA));
    }

    #[tokio::test]
    async fn test_bridge_backed_tool_call() {
        let (_lock, _guard) = isolate_env("bridge_call");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");
        let test_file = ws_dir.join("sample.txt");
        fs::write(&test_file, "hello from bridge test").expect("write sample file");

        let engine = ToolEngine::new(ws_dir);
        let res = engine
            .call_tool("read_file", json!({ "target_file": "sample.txt" }))
            .await
            .expect("read_file call should return result");

        assert!(!res.is_error);
        let text = match &res.content[0] {
            ToolContent::Text { text } => text,
        };
        assert!(text.contains("hello from bridge test"));
    }

    #[tokio::test]
    async fn test_error_shaping_for_unknown_tool_and_invalid_arguments() {
        let (_lock, _guard) = isolate_env("error_shape");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");
        let engine = ToolEngine::new(ws_dir);

        // Unknown tool returns ToolCallResult with is_error = true
        let unknown_res = engine
            .call_tool("definitely_nonexistent_tool_xyz", json!({}))
            .await
            .expect("call_tool for unknown tool returns shaped result");
        assert!(unknown_res.is_error);

        // Missing required arg returns is_error = true
        let invalid_arg_res = engine
            .call_tool("read_file", json!({}))
            .await
            .expect("call_tool with invalid args returns shaped result");
        assert!(invalid_arg_res.is_error);
    }

    #[tokio::test]
    async fn test_workspace_cache_refresh_on_workspace_change() {
        let (_lock, _guard) = isolate_env("cache_refresh");
        let temp_dir1 = _guard.root.join("cache1");
        let temp_dir2 = _guard.root.join("cache2");
        fs::create_dir_all(&temp_dir1).expect("create dir1");
        fs::create_dir_all(&temp_dir2).expect("create dir2");

        let engine = ToolEngine::new(temp_dir1.clone());
        assert_eq!(engine.cached_workspace().await, None);

        // Warm cache for dir1
        let _ = engine.list_tools().await.expect("list tools 1");
        assert_eq!(engine.cached_workspace().await, Some(temp_dir1.clone()));

        // Second call reuses cached bridge
        let _ = engine.list_tools().await.expect("list tools 1 cached");
        assert_eq!(engine.cached_workspace().await, Some(temp_dir1.clone()));

        // Pin new workspace directory
        let pinned2 = host::pin_workspace(&temp_dir2).expect("set pin for dir2");

        // ToolEngine detects workspace change and refreshes bridge cache
        let _ = engine.list_tools().await.expect("list tools 2");
        assert_eq!(engine.cached_workspace().await, Some(pinned2));
    }
}
