//! Unified tool execution engine behind MCP and CLI.
//!
//! Owns bridge lifecycle, Workspace-aware bridge caching, virtual tool
//! injection (e.g. `workspace_info`), native tools (e.g. `run_command`),
//! Workspace generation tracking, explicit shell selection, and
//! execution/result/error shaping.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};
use tokio::sync::Mutex;
use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::types::output::ToolOutput;

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
    pub structured: Option<Value>,
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolContent {
    Text { text: String },
}

impl ToolContent {
    #[cfg(test)]
    pub fn text(&self) -> &str {
        match self {
            ToolContent::Text { text } => text.as_str(),
        }
    }
}

impl ToolCallResult {
    pub fn text(text: impl Into<String>, is_error: bool) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            structured: None,
            is_error,
        }
    }

    pub fn structured(text: impl Into<String>, structured: Value, is_error: bool) -> Self {
        Self {
            content: vec![ToolContent::Text { text: text.into() }],
            structured: Some(structured),
            is_error,
        }
    }

    pub fn to_value(&self) -> Value {
        let mut val = json!({
            "content": self.content.iter().map(|c| match c {
                ToolContent::Text { text } => json!({
                    "type": "text",
                    "text": text
                }),
            }).collect::<Vec<_>>(),
            "isError": self.is_error
        });
        if let Some(ref structured) = self.structured {
            val["structuredContent"] = structured.clone();
        }
        val
    }

    pub fn from_value(value: Value) -> Self {
        let is_error = value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let structured = value
            .get("structuredContent")
            .or_else(|| value.get("structured"))
            .cloned();
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
        Self {
            content,
            structured,
            is_error,
        }
    }
}

pub struct ToolEngine {
    fallback_cwd: PathBuf,
    cached: Mutex<Option<(PathBuf, ToolBridge)>>,
    call_seq: AtomicU64,
    acknowledged_generation: Mutex<Option<String>>,
}

impl ToolEngine {
    pub fn new(fallback_cwd: PathBuf) -> Self {
        Self {
            fallback_cwd,
            cached: Mutex::new(None),
            call_seq: AtomicU64::new(1),
            acknowledged_generation: Mutex::new(None),
        }
    }

    pub fn workspace(&self) -> PathBuf {
        host::resolve_workspace(&self.fallback_cwd)
    }

    pub async fn acknowledge_generation(&self, generation: String) {
        let mut ack = self.acknowledged_generation.lock().await;
        *ack = Some(generation);
    }

    #[cfg(test)]
    pub async fn acknowledged_generation(&self) -> Option<String> {
        let ack = self.acknowledged_generation.lock().await;
        ack.clone()
    }

    /// Guard against implicit-context mistakes when the local user switches
    /// the pinned Workspace without turning the Workspace into a sandbox.
    pub async fn check_stale_context(&self, is_context_dep: bool) -> Result<(), String> {
        if !is_context_dep {
            return Ok(());
        }
        let current_gen = host::current_workspace_generation();
        let ack = self.acknowledged_generation.lock().await;
        if let Some(ref ack_gen) = *ack {
            if ack_gen != &current_gen {
                return Err(format!(
                    "Workspace context changed (generation mismatch: session acknowledged '{ack_gen}', current pin is '{current_gen}'). \
                     The pinned workspace is a default context for relative paths, not a sandbox. \
                     Call workspace_info to acknowledge the new workspace, or use an explicit absolute path / working directory for operations in other repositories."
                ));
            }
        }
        Ok(())
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
            "description": "Return the active default workspace root and generation. The workspace pin is the default context and safety anchor for relative paths and default commands, not a filesystem sandbox. Call this to acknowledge workspace changes.",
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
            let mut schema = d.function.parameters;
            if name == "run_terminal_cmd" {
                if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
                    props.insert(
                        "shell".to_string(),
                        json!({
                            "type": "string",
                            "description": "Optional per-call shell selector. Supported on Windows: 'powershell', 'cmd', 'git-bash'. Omit for default shell.",
                            "enum": ["powershell", "cmd", "git-bash"]
                        }),
                    );
                    props.insert(
                        "cwd".to_string(),
                        json!({
                            "type": "string",
                            "description": "Optional explicit working directory for this shell call. Absolute paths may target repositories outside the pinned default Workspace."
                        }),
                    );
                    props.insert(
                        "execution_mode".to_string(),
                        json!({
                            "type": "string",
                            "description": "Optional execution mode: 'foreground' (blocks until completion or timeout), 'background' (returns task_id immediately), or 'auto' (runs in foreground up to yield_after_ms, yielding to background task if still running). Default: 'foreground' (or 'background' if is_background is true).",
                            "enum": ["foreground", "background", "auto"]
                        }),
                    );
                    props.insert(
                        "yield_after_ms".to_string(),
                        json!({
                            "type": "integer",
                            "description": "Optional interaction budget in milliseconds for 'auto' execution mode. If the command does not complete within this budget, it yields into a durable background task without killing or restarting the process. Default: 10000 (10 seconds)."
                        }),
                    );
                }
            }
            json!({
                "name": name,
                "description": d.function.description.unwrap_or_default(),
                "inputSchema": schema,
                "annotations": {
                    "readOnlyHint": read_only,
                    "destructiveHint": !read_only,
                    "openWorldHint": false,
                }
            })
        }));
        Ok(tools)
    }

    /// Adapter for `hands list` CLI output.
    ///
    /// Preserves the legacy CLI contract where tool schemas are exposed
    /// under `parameters` instead of MCP's `inputSchema`.
    pub async fn list_tools_cli(&self) -> Result<Vec<Value>, String> {
        let mcp_tools = self.list_tools().await?;
        let cli_tools = mcp_tools
            .into_iter()
            .map(|t| {
                json!({
                    "name": t.get("name").cloned().unwrap_or(Value::Null),
                    "description": t.get("description").cloned().unwrap_or(Value::Null),
                    "parameters": t.get("inputSchema").cloned().unwrap_or(Value::Null),
                })
            })
            .collect();
        Ok(cli_tools)
    }

    pub async fn call_tool(&self, name: &str, mut arguments: Value) -> Result<ToolCallResult, String> {
        let is_context_dep = is_context_dependent_call(name, &arguments);
        if let Err(stale_msg) = self.check_stale_context(is_context_dep).await {
            return Ok(ToolCallResult::text(stale_msg, true));
        }

        if name == "workspace_info" {
            let cwd = self.workspace();
            let current_gen = host::current_workspace_generation();
            self.acknowledge_generation(current_gen.clone()).await;

            let prompt_text = format!(
                "workspace: {}\nworkspace_generation: {}\nsource_git_sha: {}\nnote: Pinned workspace is the default context for relative paths and default commands. Explicit absolute paths and explicit working directories may be used across multiple repositories without repinning.",
                cwd.display(),
                current_gen,
                crate::build_provenance::SOURCE_GIT_SHA
            );
            let structured = json!({
                "workspace": cwd.display().to_string(),
                "workspace_generation": current_gen,
                "source_git_sha": crate::build_provenance::SOURCE_GIT_SHA,
                "is_default_context": true
            });
            return Ok(ToolCallResult::structured(prompt_text, structured, false));
        }

        if name == crate::run_proc::TOOL_NAME {
            let ws = self.workspace();
            let ws_str = ws.to_string_lossy().to_string();
            if let Err(stale_msg) = self.check_stale_context(is_context_dep).await {
                return Ok(ToolCallResult::text(stale_msg, true));
            }
            let val = crate::run_proc::handle_call(&arguments, Some(&ws_str)).await;
            return Ok(ToolCallResult::from_value(val));
        }

        if name == "run_terminal_cmd" {
            let exec_mode = match resolve_execution_mode(&arguments) {
                Ok(m) => m,
                Err(e) => return Ok(ToolCallResult::text(format!("error: {e}"), true)),
            };

            let shell_str_opt = match arguments.get("shell") {
                Some(shell_val) => match shell_val.as_str() {
                    Some(s) => Some(s.trim().to_string()),
                    None => {
                        return Ok(ToolCallResult::text(
                            "error: shell must be a string ('powershell', 'cmd', or 'git-bash')",
                            true,
                        ));
                    }
                },
                None => None,
            };

            let command = arguments
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let description = arguments
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string);
            let explicit_cwd = arguments
                .get("cwd")
                .or_else(|| arguments.get("workdir"))
                .and_then(Value::as_str)
                .map(str::to_string);

            let cwd_path = if let Some(ref c) = explicit_cwd {
                PathBuf::from(c)
            } else {
                self.workspace()
            };
            let cwd_str = cwd_path.to_string_lossy().to_string();

            match exec_mode {
                ResolvedExecutionMode::Auto { yield_after_ms } => {
                    let bg_cmd = if let Some(shell_str) = shell_str_opt {
                        match resolve_shell_command(shell_str.as_str(), &command, true, explicit_cwd.as_deref()) {
                            Ok(ResolvedShell::Background { command: c }) => c,
                            Ok(ResolvedShell::Foreground { .. }) => unreachable!(),
                            Err(e) => return Ok(ToolCallResult::text(format!("error: {e}"), true)),
                        }
                    } else if let Some(cwd) = explicit_cwd {
                        wrap_background_cwd(command.clone(), Some(&cwd))
                    } else {
                        command.to_string()
                    };

                    let mut bg_arguments = arguments.clone();
                    if let Some(obj) = bg_arguments.as_object_mut() {
                        obj.insert("command".to_string(), json!(bg_cmd));
                        obj.insert("is_background".to_string(), json!(true));
                        obj.remove("shell");
                        obj.remove("cwd");
                        obj.remove("workdir");
                        obj.remove("execution_mode");
                        obj.remove("yield_after_ms");
                    }

                    let call_id = format!("mcp-{}", self.call_seq.fetch_add(1, Ordering::Relaxed));
                    let bridge = self.bridge().await?;
                    if let Err(stale_msg) = self.check_stale_context(is_context_dep).await {
                        return Ok(ToolCallResult::text(stale_msg, true));
                    }

                    let bg_result = match bridge.call(name, bg_arguments, &call_id).await {
                        Ok(r) => r,
                        Err(e) => return Ok(ToolCallResult::text(e.to_string(), true)),
                    };

                    let (task_id, initial_output_file, pid) = match &bg_result.output {
                        ToolOutput::BackgroundTaskStarted(b) => {
                            (b.task_id.clone(), b.output_file.clone(), b.pid)
                        }
                        _ => {
                            let structured = serde_json::to_value(&bg_result.output).unwrap_or_default();
                            let task_id = structured
                                .get("task_id")
                                .and_then(Value::as_str)
                                .unwrap_or(&call_id)
                                .to_string();
                            let output_file = structured
                                .get("output_file")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let pid = structured.get("pid").and_then(Value::as_u64).map(|p| p as u32);
                            (task_id, output_file, pid)
                        }
                    };

                    let poll_call_id = format!("mcp-{}", self.call_seq.fetch_add(1, Ordering::Relaxed));
                    let poll_args = json!({
                        "task_id": &task_id,
                        "timeout_ms": yield_after_ms
                    });

                    let poll_result = match bridge.call("get_task_output", poll_args, &poll_call_id).await {
                        Ok(r) => r,
                        Err(_) => {
                            let prompt_text = format!(
                                "[Command yielded to background after {yield_after_ms}ms. Process is still running with task_id: {task_id}]\nUse get_task_output with task_id=\"{task_id}\" to inspect subsequent output or status."
                            );
                            let structured = json!({
                                "task_id": task_id,
                                "task_type": "bash",
                                "status": "running",
                                "command": command,
                                "summary": format!("Command \"{}\" exceeded yield budget of {}ms and was yielded to background. Process is still running.", command, yield_after_ms),
                                "retrieval_hint": format!("Use get_task_output with task_id=\"{task_id}\" to inspect subsequent output or status."),
                                "output_file": initial_output_file,
                                "total_bytes": 0,
                                "description": description.as_deref(),
                                "has_output": false,
                                "execution_mode": "auto",
                                "yielded": true,
                                "backgrounded": true,
                                "pid": pid,
                            });
                            return Ok(ToolCallResult {
                                content: vec![ToolContent::Text { text: prompt_text }],
                                structured: Some(structured),
                                is_error: false,
                            });
                        }
                    };

                    let poll_val = serde_json::to_value(&poll_result.output).unwrap_or_default();
                    let result_obj = poll_val.get("Result").and_then(Value::as_object);
                    let status = result_obj
                        .and_then(|r| r.get("status"))
                        .and_then(Value::as_str)
                        .unwrap_or("running");

                    if status != "running" {
                        let exit_code = result_obj
                            .and_then(|r| r.get("exit_code"))
                            .and_then(Value::as_i64)
                            .unwrap_or(0) as i32;
                        let output_text = result_obj
                            .and_then(|r| r.get("output"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        let total_bytes = result_obj
                            .and_then(|r| r.get("raw_output_bytes").or_else(|| r.get("total_bytes")))
                            .and_then(Value::as_u64)
                            .unwrap_or(output_text.len() as u64) as usize;
                        let truncated = result_obj
                            .and_then(|r| r.get("truncated"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let final_output_file = result_obj
                            .and_then(|r| r.get("output_file"))
                            .and_then(Value::as_str)
                            .unwrap_or(&initial_output_file);
                        let has_output = !output_text.is_empty() || total_bytes > 0;

                        let (text, log_path) = if total_bytes > 40_000 || truncated {
                            let preview_prefix = crate::run_proc::truncate_utf8(output_text, 20_000);
                            let preview = format!(
                                "{preview_prefix}\n\n... (output truncated) ...\n\n[truncated - full output at: {final_output_file}]"
                            );
                            (preview, Some(final_output_file.to_string()))
                        } else {
                            (output_text.to_string(), None)
                        };

                        let prompt_text = format!(
                            "command: {command}\nexit: {exit_code}\n\n{text}"
                        );

                        let structured = json!({
                            "command": command,
                            "exit_code": exit_code,
                            "timed_out": false,
                            "termination_reason": null,
                            "current_dir": cwd_str,
                            "truncated": truncated || log_path.is_some(),
                            "output_file": final_output_file,
                            "total_bytes": total_bytes,
                            "description": description.as_deref(),
                            "has_output": has_output,
                            "error": null,
                            "execution_mode": "auto",
                            "status": status,
                            "yielded": false,
                            "pid": pid,
                        });

                        return Ok(ToolCallResult {
                            content: vec![ToolContent::Text { text: prompt_text }],
                            structured: Some(structured),
                            is_error: false,
                        });
                    } else {
                        let total_bytes = result_obj
                            .and_then(|r| r.get("raw_output_bytes").or_else(|| r.get("total_bytes")))
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as usize;
                        let final_output_file = result_obj
                            .and_then(|r| r.get("output_file"))
                            .and_then(Value::as_str)
                            .unwrap_or(&initial_output_file);

                        let prompt_text = format!(
                            "[Command yielded to background after {yield_after_ms}ms. Process is still running with task_id: {task_id}]\nUse get_task_output with task_id=\"{task_id}\" to inspect subsequent output or status."
                        );

                        let structured = json!({
                            "task_id": task_id,
                            "task_type": "bash",
                            "status": "running",
                            "command": command,
                            "summary": format!("Command \"{}\" exceeded yield budget of {}ms and was yielded to background. Process is still running.", command, yield_after_ms),
                            "retrieval_hint": format!("Use get_task_output with task_id=\"{task_id}\" to inspect subsequent output or status."),
                            "output_file": final_output_file,
                            "total_bytes": total_bytes,
                            "description": description.as_deref(),
                            "has_output": total_bytes > 0,
                            "execution_mode": "auto",
                            "yielded": true,
                            "backgrounded": true,
                            "pid": pid,
                        });

                        return Ok(ToolCallResult {
                            content: vec![ToolContent::Text { text: prompt_text }],
                            structured: Some(structured),
                            is_error: false,
                        });
                    }
                }
                ResolvedExecutionMode::Background => {
                    let bg_cmd = if let Some(shell_str) = shell_str_opt {
                        match resolve_shell_command(shell_str.as_str(), &command, true, explicit_cwd.as_deref()) {
                            Ok(ResolvedShell::Background { command: c }) => c,
                            Ok(ResolvedShell::Foreground { .. }) => unreachable!(),
                            Err(e) => return Ok(ToolCallResult::text(format!("error: {e}"), true)),
                        }
                    } else if let Some(cwd) = explicit_cwd {
                        wrap_background_cwd(command.clone(), Some(&cwd))
                    } else {
                        command.to_string()
                    };

                    let mut bg_arguments = arguments.clone();
                    if let Some(obj) = bg_arguments.as_object_mut() {
                        obj.insert("command".to_string(), json!(bg_cmd));
                        obj.insert("is_background".to_string(), json!(true));
                        obj.remove("shell");
                        obj.remove("cwd");
                        obj.remove("workdir");
                        obj.remove("execution_mode");
                        obj.remove("yield_after_ms");
                    }

                    let call_id = format!("mcp-{}", self.call_seq.fetch_add(1, Ordering::Relaxed));
                    let bridge = self.bridge().await?;
                    if let Err(stale_msg) = self.check_stale_context(is_context_dep).await {
                        return Ok(ToolCallResult::text(stale_msg, true));
                    }
                    match bridge.call(name, bg_arguments, &call_id).await {
                        Ok(result) => {
                            let mut prompt_text = result.prompt_text;
                            let structured = shape_structured_output(&result.output, &mut prompt_text);
                            let is_error = result.output.is_error();
                            return Ok(ToolCallResult {
                                content: vec![ToolContent::Text { text: prompt_text }],
                                structured: Some(structured),
                                is_error,
                            });
                        }
                        Err(e) => return Ok(ToolCallResult::text(e.to_string(), true)),
                    }
                }
                ResolvedExecutionMode::Foreground => {
                    if let Some(shell_str) = shell_str_opt {
                        let resolved = match resolve_shell_command(shell_str.as_str(), &command, false, explicit_cwd.as_deref()) {
                            Ok(r) => r,
                            Err(e) => return Ok(ToolCallResult::text(format!("error: {e}"), true)),
                        };
                        match resolved {
                            ResolvedShell::Foreground { prog, args } => {
                                let timeout = arguments.get("timeout").and_then(Value::as_u64);
                                let env = arguments.get("env");
                                if let Err(stale_msg) = self.check_stale_context(is_context_dep).await {
                                    return Ok(ToolCallResult::text(stale_msg, true));
                                }
                                let output = crate::run_proc::run_foreground(
                                    &prog,
                                    &args,
                                    Some(&cwd_str),
                                    timeout,
                                    env,
                                )
                                .await;
                                return Ok(render_proc_output_as_terminal_result(
                                    output,
                                    &command,
                                    &cwd_str,
                                    description.as_deref(),
                                ));
                            }
                            ResolvedShell::Background { .. } => unreachable!(),
                        }
                    } else {
                        // Default shell foreground
                        if let Some(cwd) = explicit_cwd {
                            if let Some(obj) = arguments.as_object_mut() {
                                obj.insert(
                                    "command".to_string(),
                                    json!(wrap_background_cwd(command.clone(), Some(&cwd))),
                                );
                                obj.remove("cwd");
                                obj.remove("workdir");
                            }
                        }
                        if let Some(obj) = arguments.as_object_mut() {
                            obj.remove("execution_mode");
                            obj.remove("yield_after_ms");
                        }
                        let call_id = format!("mcp-{}", self.call_seq.fetch_add(1, Ordering::Relaxed));
                        let bridge = self.bridge().await?;
                        if let Err(stale_msg) = self.check_stale_context(is_context_dep).await {
                            return Ok(ToolCallResult::text(stale_msg, true));
                        }
                        match bridge.call(name, arguments, &call_id).await {
                            Ok(result) => {
                                let mut prompt_text = result.prompt_text;
                                let structured = shape_structured_output(&result.output, &mut prompt_text);
                                let is_error = result.output.is_error();
                                return Ok(ToolCallResult {
                                    content: vec![ToolContent::Text { text: prompt_text }],
                                    structured: Some(structured),
                                    is_error,
                                });
                            }
                            Err(e) => return Ok(ToolCallResult::text(e.to_string(), true)),
                        }
                    }
                }
            }
        }

        let call_id = format!("mcp-{}", self.call_seq.fetch_add(1, Ordering::Relaxed));
        let bridge = self.bridge().await?;
        if let Err(stale_msg) = self.check_stale_context(is_context_dep).await {
            return Ok(ToolCallResult::text(stale_msg, true));
        }
        match bridge.call(name, arguments, &call_id).await {
            Ok(result) => {
                let mut prompt_text = result.prompt_text;
                let structured = shape_structured_output(&result.output, &mut prompt_text);
                let is_error = result.output.is_error();
                Ok(ToolCallResult {
                    content: vec![ToolContent::Text { text: prompt_text }],
                    structured: Some(structured),
                    is_error,
                })
            }
            Err(e) => Ok(ToolCallResult::text(e.to_string(), true)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedExecutionMode {
    Foreground,
    Background,
    Auto { yield_after_ms: u64 },
}

pub fn resolve_execution_mode(arguments: &Value) -> Result<ResolvedExecutionMode, String> {
    let mode_str = match arguments.get("execution_mode") {
        Some(v) => match v.as_str() {
            Some(s) => Some(s.trim().to_ascii_lowercase()),
            None => {
                return Err(
                    "execution_mode must be a string ('foreground', 'background', or 'auto')"
                        .to_string(),
                );
            }
        },
        None => None,
    };

    let yield_after_ms = match arguments.get("yield_after_ms") {
        Some(v) => {
            if let Some(ms) = v.as_u64() {
                Some(ms)
            } else if let Some(s) = v.as_str() {
                if let Ok(ms) = s.parse::<u64>() {
                    Some(ms)
                } else {
                    return Err("yield_after_ms must be an integer (milliseconds)".to_string());
                }
            } else {
                return Err("yield_after_ms must be an integer (milliseconds)".to_string());
            }
        }
        None => None,
    };

    let is_background = arguments
        .get("is_background")
        .and_then(|v| v.as_bool().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .unwrap_or(false);

    match mode_str.as_deref() {
        Some("background") => Ok(ResolvedExecutionMode::Background),
        Some("foreground") => Ok(ResolvedExecutionMode::Foreground),
        Some("auto") => {
            let budget = yield_after_ms.unwrap_or(10_000);
            Ok(ResolvedExecutionMode::Auto {
                yield_after_ms: budget,
            })
        }
        Some(other) => Err(format!(
            "unsupported execution_mode: '{other}'; must be 'foreground', 'background', or 'auto'"
        )),
        None => {
            if is_background {
                Ok(ResolvedExecutionMode::Background)
            } else if let Some(budget) = yield_after_ms {
                Ok(ResolvedExecutionMode::Auto {
                    yield_after_ms: budget,
                })
            } else {
                Ok(ResolvedExecutionMode::Foreground)
            }
        }
    }
}

enum ResolvedShell {
    Foreground { prog: String, args: Vec<String> },
    Background { command: String },
}

fn wrap_background_cwd(command: String, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd else {
        return command;
    };
    #[cfg(windows)]
    {
        let escaped_cwd = cwd.replace('\'', "''");
        format!("Set-Location -LiteralPath '{escaped_cwd}' -ErrorAction Stop; {command}")
    }
    #[cfg(not(windows))]
    {
        let escaped_cwd = cwd.replace('\'', "'\\''");
        format!("cd -- '{escaped_cwd}' && {command}")
    }
}

/// Resolve explicit shell selectors (`powershell`, `cmd`, `git-bash`) for `run_terminal_cmd`.
///
/// Foreground commands are launched directly via `run_proc::run_foreground` with argv-level
/// isolation bypassing outer shell layers.
///
/// Background commands must preserve `xai_grok_tools`'s async task tracking, output streaming,
/// and cancellation lifecycle owned by `ToolBridge`'s command string API. To preserve this
/// lifecycle without introducing a second task manager, background commands wrap the payload
/// with the explicit shell executable and use single-quote escaping (`replace(''', "''")`
/// under PowerShell host backend on Windows) so the outer launcher unquotes the command string
/// verbatim and hands the unaltered payload to the selected shell's `-Command`, `/C`, or `-c`.
fn resolve_shell_command(
    shell_str: &str,
    command: &str,
    is_background: bool,
    background_cwd: Option<&str>,
) -> Result<ResolvedShell, String> {
    match shell_str {
        "powershell" => {
            #[cfg(windows)]
            {
                if is_background {
                    let escaped = command.replace('\'', "''");
                    let bridge_cmd = format!(
                        "powershell.exe -NoProfile -NonInteractive -Command '{escaped}'"
                    );
                    Ok(ResolvedShell::Background {
                        command: wrap_background_cwd(bridge_cmd, background_cwd),
                    })
                } else {
                    let prog = "powershell.exe".to_string();
                    let args = vec![
                        "-NoProfile".to_string(),
                        "-NonInteractive".to_string(),
                        "-Command".to_string(),
                        command.to_string(),
                    ];
                    Ok(ResolvedShell::Foreground { prog, args })
                }
            }
            #[cfg(not(windows))]
            {
                let pwsh = crate::service::which("pwsh")
                    .or_else(|| crate::service::which("powershell"))
                    .ok_or_else(|| "powershell is not installed on this system".to_string())?;
                let prog = pwsh.to_string_lossy().to_string();
                let args = vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-Command".to_string(),
                    command.to_string(),
                ];
                if is_background {
                    let escaped = command.replace('\'', "''");
                    let bridge_cmd = format!("'{prog}' -NoProfile -NonInteractive -Command '{escaped}'");
                    Ok(ResolvedShell::Background {
                        command: wrap_background_cwd(bridge_cmd, background_cwd),
                    })
                } else {
                    Ok(ResolvedShell::Foreground { prog, args })
                }
            }
        }
        "cmd" => {
            #[cfg(windows)]
            {
                let native_cmd = host::native_cmd_exe()?;
                let prog = native_cmd.to_string_lossy().to_string();
                if is_background {
                    let escaped = command.replace('\'', "''");
                    let bridge_cmd = format!("& '{prog}' /D /S /C '{escaped}'");
                    Ok(ResolvedShell::Background {
                        command: wrap_background_cwd(bridge_cmd, background_cwd),
                    })
                } else {
                    let args = vec![
                        "/D".to_string(),
                        "/S".to_string(),
                        "/C".to_string(),
                        command.to_string(),
                    ];
                    Ok(ResolvedShell::Foreground { prog, args })
                }
            }
            #[cfg(not(windows))]
            {
                Err("cmd shell is only supported on Windows".to_string())
            }
        }
        "git-bash" => {
            #[cfg(windows)]
            {
                let git_bash = host::find_git_bash()?;
                let prog = git_bash.to_string_lossy().to_string();
                if is_background {
                    let escaped = command.replace('\'', "''");
                    let bridge_cmd = format!("& '{prog}' -c '{escaped}'");
                    Ok(ResolvedShell::Background {
                        command: wrap_background_cwd(bridge_cmd, background_cwd),
                    })
                } else {
                    let args = vec!["-c".to_string(), command.to_string()];
                    Ok(ResolvedShell::Foreground { prog, args })
                }
            }
            #[cfg(not(windows))]
            {
                let git_bash = host::find_git_bash()?;
                let prog = git_bash.to_string_lossy().to_string();
                let args = vec!["-c".to_string(), command.to_string()];
                if is_background {
                    let escaped = command.replace('\'', "'\''");
                    let bridge_cmd = format!("'{prog}' -c '{escaped}'");
                    Ok(ResolvedShell::Background {
                        command: wrap_background_cwd(bridge_cmd, background_cwd),
                    })
                } else {
                    Ok(ResolvedShell::Foreground { prog, args })
                }
            }
        }
        other => {
            Err(format!(
                "unsupported shell selector: '{other}'; supported on Windows: powershell, cmd, git-bash"
            ))
        }
    }
}

fn is_path_absolute(path_str: &str) -> bool {
    let p = Path::new(path_str);
    if p.is_absolute() {
        return true;
    }
    let bytes = path_str.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == 92 || bytes[2] == 47)
    {
        return true;
    }
    false
}

fn is_context_dependent_call(tool_name: &str, arguments: &Value) -> bool {
    match tool_name {
        "workspace_info" | "get_task_output" | "kill_task" => false,
        "run_command" => {
            if let Some(workdir) = arguments.get("workdir").and_then(Value::as_str) {
                !is_path_absolute(workdir)
            } else {
                true
            }
        }
        "run_terminal_cmd" => {
            if let Some(cwd) = arguments
                .get("cwd")
                .or_else(|| arguments.get("workdir"))
                .and_then(Value::as_str)
            {
                !is_path_absolute(cwd)
            } else {
                true
            }
        }
        "read_file" => {
            if let Some(target) = arguments
                .get("target_file")
                .or_else(|| arguments.get("file_path"))
                .or_else(|| arguments.get("path"))
                .and_then(Value::as_str)
            {
                !is_path_absolute(target)
            } else {
                true
            }
        }
        "list_dir" => {
            if let Some(dir_path) = arguments
                .get("target_directory")
                .or_else(|| arguments.get("dir_path"))
                .or_else(|| arguments.get("path"))
                .and_then(Value::as_str)
            {
                !is_path_absolute(dir_path)
            } else {
                true
            }
        }
        "grep" => {
            if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                !is_path_absolute(path)
            } else {
                true
            }
        }
        "glob" => {
            if let Some(path) = arguments
                .get("path")
                .or_else(|| arguments.get("directory"))
                .and_then(Value::as_str)
            {
                !is_path_absolute(path)
            } else {
                true
            }
        }
        "write" | "search_replace" | "open_code_write" => {
            if let Some(file_path) = arguments
                .get("file_path")
                .or_else(|| arguments.get("path"))
                .and_then(Value::as_str)
            {
                !is_path_absolute(file_path)
            } else {
                true
            }
        }
        "apply_patch" => {
            if let Some(patch) = arguments.get("patch").and_then(Value::as_str) {
                let mut has_paths = false;
                let mut all_abs = true;
                for line in patch.lines() {
                    let path = [
                        "*** Add File: ",
                        "*** Update File: ",
                        "*** Delete File: ",
                        "*** Move to: ",
                    ]
                    .iter()
                    .find_map(|prefix| line.strip_prefix(prefix));
                    if let Some(path) = path {
                        let path = path.trim();
                        has_paths = true;
                        if !is_path_absolute(path) {
                            all_abs = false;
                            break;
                        }
                    }
                }
                !(has_paths && all_abs)
            } else {
                true
            }
        }
        "todo_write" => false,
        _ => true,
    }
}

fn shape_structured_output(output: &ToolOutput, prompt_text: &mut String) -> Value {
    let mut structured = serde_json::to_value(output).unwrap_or_else(|_| json!({ "type": "unknown" }));

    // Enrich specific tool variant structured outputs
    if let Some(bash) = structured.as_object_mut() {
        if bash.get("type").and_then(Value::as_str) == Some("Bash") {
            let total_bytes = bash.get("total_bytes").and_then(Value::as_u64).unwrap_or(0);
            let has_output = total_bytes > 0 || !prompt_text.is_empty();
            bash.insert("has_output".to_string(), json!(has_output));
        } else if bash.get("type").and_then(Value::as_str) == Some("TaskOutput") {
            if let Some(res) = bash.get_mut("Result").and_then(Value::as_object_mut) {
                let raw_bytes = res.get("raw_output_bytes").and_then(Value::as_u64).unwrap_or(0);
                let truncated = res.get("truncated").and_then(Value::as_bool).unwrap_or(false);
                let is_output_empty = res.get("output").and_then(Value::as_str).map_or(true, str::is_empty);
                let has_output = !is_output_empty || raw_bytes > 0;
                res.insert("total_bytes".to_string(), json!(raw_bytes));
                res.insert("has_output".to_string(), json!(has_output));

                // Fix contradictory text when retained output exists but inline text was empty
                if is_output_empty && (truncated || raw_bytes > 0) {
                    if prompt_text.contains("(no output)") {
                        *prompt_text = prompt_text.replace(
                            "(no output)",
                            "(output truncated - use read_file on output_file for full content)",
                        );
                    }
                }
            } else if let Some(results) = bash
                .get_mut("MultiResult")
                .and_then(Value::as_object_mut)
                .and_then(|multi| multi.get_mut("results"))
                .and_then(Value::as_array_mut)
            {
                for result in results {
                    let Some(res) = result.as_object_mut() else {
                        continue;
                    };
                    let raw_bytes = res.get("raw_output_bytes").and_then(Value::as_u64).unwrap_or(0);
                    let is_output_empty = res.get("output").and_then(Value::as_str).map_or(true, str::is_empty);
                    res.insert("total_bytes".to_string(), json!(raw_bytes));
                    res.insert(
                        "has_output".to_string(),
                        json!(!is_output_empty || raw_bytes > 0),
                    );
                }
            }
        }
    }

    structured
}

fn render_proc_output_as_terminal_result(
    output: crate::run_proc::ProcOutput,
    command: &str,
    cwd: &str,
    description: Option<&str>,
) -> ToolCallResult {
    let mut combined = String::new();
    if !output.stdout.is_empty() {
        combined.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&output.stderr);
    }

    let has_output = !combined.is_empty();
    let total_bytes = combined.len();

    let (text, log_path) = if combined.len() > 40_000 {
        let log = crate::run_proc::write_temp_log(&output.stdout, &output.stderr);
        let preview_prefix = crate::run_proc::truncate_utf8(&combined, 20_000);
        let preview = format!(
            "{preview_prefix}

... (output truncated) ...

[truncated - full output at: {}]",
            log.as_deref().unwrap_or("")
        );
        (preview, log)
    } else {
        (combined, None)
    };

    let prompt_text = format!(
        "command: {command}
exit: {}{}

{text}",
        output.exit_code,
        if output.timed_out { " (timed out)" } else { "" }
    );

    let termination_reason = output.termination_reason.map(|r| r.as_str());

    let structured = json!({
        "command": command,
        "exit_code": output.exit_code,
        "timed_out": output.timed_out,
        "termination_reason": termination_reason,
        "current_dir": cwd,
        "truncated": output.capture_truncated || log_path.is_some(),
        "output_file": log_path.unwrap_or_default(),
        "total_bytes": total_bytes,
        "description": description.as_deref(),
        "has_output": has_output,
        "error": output.error,
    });

    ToolCallResult {
        content: vec![ToolContent::Text { text: prompt_text }],
        structured: Some(structured),
        is_error: output.error.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use crate::testenv::isolate_env;

    #[tokio::test]
    async fn test_tool_call_result_structured_serialization() {
        let structured_data = json!({
            "exit_code": 0,
            "timed_out": false,
            "has_output": true,
            "total_bytes": 42
        });
        let res = ToolCallResult::structured("hello world", structured_data.clone(), false);
        let val = res.to_value();
        assert_eq!(val["isError"], false);
        assert_eq!(val["content"][0]["type"], "text");
        assert_eq!(val["content"][0]["text"], "hello world");
        assert_eq!(val["structuredContent"], structured_data);
        assert!(val.get("structured").is_none());

        let roundtrip = ToolCallResult::from_value(val);
        assert_eq!(roundtrip, res);

        let err_res = ToolCallResult::structured("something broke", json!({"error": "broken"}), true);
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
        assert!(names.contains(&"run_terminal_cmd"), "must include run_terminal_cmd");
        assert!(names.contains(&"read_file"), "must include read_file");
        assert!(names.contains(&"grep"), "must include grep");
        assert!(names.contains(&"list_dir"), "must include list_dir");

        let ws_tool = tools.iter().find(|t| t["name"] == "workspace_info").unwrap();
        assert_eq!(ws_tool["annotations"]["readOnlyHint"], true);
        assert_eq!(ws_tool["annotations"]["destructiveHint"], false);

        // Verify shell selector parameter exists in run_terminal_cmd schema
        let term_tool = tools.iter().find(|t| t["name"] == "run_terminal_cmd").unwrap();
        assert!(
            term_tool["inputSchema"]["properties"]["shell"].is_object(),
            "run_terminal_cmd inputSchema must include shell selector: {term_tool:?}"
        );
    }

    #[tokio::test]
    async fn test_list_tools_cli_parameters_contract() {
        let (_lock, _guard) = isolate_env("list_cli");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");

        let engine = ToolEngine::new(ws_dir);
        let cli_tools = engine.list_tools_cli().await.expect("list_tools_cli should succeed");

        let names: Vec<&str> = cli_tools
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();

        assert!(names.contains(&"workspace_info"), "CLI must include workspace_info");
        assert!(names.contains(&"run_command"), "CLI must include run_command");
        assert!(names.contains(&"read_file"), "CLI must include read_file");

        for tool in &cli_tools {
            assert!(
                tool.get("parameters").is_some(),
                "CLI tool definition must have 'parameters' field: {tool:?}"
            );
            assert!(
                tool.get("inputSchema").is_none(),
                "CLI tool definition must NOT have 'inputSchema' field: {tool:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_cli_and_mcp_listing_tool_names_parity() {
        let (_lock, _guard) = isolate_env("parity_list");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");

        let engine = ToolEngine::new(ws_dir);
        let mcp_tools = engine.list_tools().await.expect("list_tools should succeed");
        let cli_tools = engine.list_tools_cli().await.expect("list_tools_cli should succeed");

        assert_eq!(mcp_tools.len(), cli_tools.len(), "Tool count must match between MCP and CLI");
        for (mcp, cli) in mcp_tools.iter().zip(cli_tools.iter()) {
            assert_eq!(mcp["name"], cli["name"], "Tool name parity check");
            assert_eq!(mcp["description"], cli["description"], "Tool description parity check");
            assert_eq!(mcp["inputSchema"], cli["parameters"], "Schema content parity check");
        }
    }

    #[tokio::test]
    async fn test_virtual_tool_workspace_info_call() {
        let (_lock, _guard) = isolate_env("ws_info");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");

        let engine = ToolEngine::new(ws_dir.clone());
        let res = engine
            .call_tool("workspace_info", json!({}))
            .await
            .expect("call_tool should succeed");

        assert!(!res.is_error);
        let text = match &res.content[0] {
            ToolContent::Text { text } => text,
        };
        assert!(text.contains("workspace:"));
        assert!(text.contains("workspace_generation:"));
        assert!(text.contains("default context"));
        assert!(text.contains("source_git_sha:"));
        assert!(text.contains(crate::build_provenance::SOURCE_GIT_SHA));

        let structured = res.structured.expect("workspace_info must return structured data");
        assert_eq!(structured["workspace"], dunce::canonicalize(&ws_dir).unwrap_or(ws_dir).to_string_lossy().as_ref());
        assert!(structured["workspace_generation"].as_str().is_some());
        assert_eq!(structured["is_default_context"], true);
    }

    #[tokio::test]
    async fn test_stale_workspace_context_guard_rejects_relative_call_and_allows_after_workspace_info() {
        let (_lock, _guard) = isolate_env("stale_guard");
        let ws1 = _guard.root.join("repo1");
        let ws2 = _guard.root.join("repo2");
        fs::create_dir_all(&ws1).expect("create repo1");
        fs::create_dir_all(&ws2).expect("create repo2");

        let pinned1 = host::pin_workspace(&ws1).expect("pin repo1");
        let engine = ToolEngine::new(pinned1);

        // 1. Session acknowledges Workspace 1
        let ws_info_res = engine.call_tool("workspace_info", json!({})).await.expect("workspace_info 1");
        assert!(!ws_info_res.is_error);

        // Relative call succeeds
        fs::write(ws1.join("file1.txt"), "hello repo1").unwrap();
        let read1 = engine.call_tool("read_file", json!({ "target_file": "file1.txt" })).await.unwrap();
        assert!(!read1.is_error);
        assert!(read1.content[0].text().contains("hello repo1"));

        // 2. User switches pin to Workspace 2
        let _pinned2 = host::pin_workspace(&ws2).expect("pin repo2");

        // 3. Stale session makes relative call -> fails closed!
        let stale_read = engine.call_tool("read_file", json!({ "target_file": "file1.txt" })).await.unwrap();
        assert!(stale_read.is_error, "stale relative call must fail closed");
        assert!(
            stale_read.content[0].text().contains("Workspace context changed"),
            "stale error must explain generation mismatch: {}",
            stale_read.content[0].text()
        );
        assert!(
            stale_read.content[0].text().contains("workspace_info"),
            "stale error must direct caller to workspace_info"
        );

        // 4. Explicit absolute path still works without repinning!
        let abs_file = ws1.join("file1.txt");
        let explicit_read = engine.call_tool("read_file", json!({ "target_file": abs_file.to_string_lossy().to_string() })).await.unwrap();
        assert!(!explicit_read.is_error, "explicit absolute path must work even with unacknowledged generation");
        assert!(explicit_read.content[0].text().contains("hello repo1"));

        // 5. Calling workspace_info acknowledges new generation
        let ack_res = engine.call_tool("workspace_info", json!({})).await.unwrap();
        assert!(!ack_res.is_error);

        // 6. Subsequent relative calls now proceed against repo2
        fs::write(ws2.join("file2.txt"), "hello repo2").unwrap();
        let read2 = engine.call_tool("read_file", json!({ "target_file": "file2.txt" })).await.unwrap();
        assert!(!read2.is_error);
        assert!(read2.content[0].text().contains("hello repo2"));
    }

    #[tokio::test]
    async fn test_workspace_generation_switch_a_b_a_detected() {
        let (_lock, _guard) = isolate_env("gen_aba");
        let ws_a = _guard.root.join("repoA");
        let ws_b = _guard.root.join("repoB");
        fs::create_dir_all(&ws_a).expect("create repoA");
        fs::create_dir_all(&ws_b).expect("create repoB");

        // Pin A -> Gen 1
        host::pin_workspace(&ws_a).expect("pin A");
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();
        let gen1 = engine.acknowledged_generation().await.unwrap();

        // Pin B -> Gen 2
        host::pin_workspace(&ws_b).expect("pin B");

        // Pin A again -> Gen 3 (different generation even though same path!)
        host::pin_workspace(&ws_a).expect("pin A again");
        let gen3 = host::current_workspace_generation();

        assert_ne!(gen1, gen3, "A -> B -> A must allocate a new generation");

        // Stale session with Gen 1 fails closed
        let stale_call = engine.call_tool("read_file", json!({ "target_file": "nonexistent.txt" })).await.unwrap();
        assert!(stale_call.is_error);
        assert!(stale_call.content[0].text().contains("Workspace context changed"));
    }

    #[tokio::test]
    async fn test_multi_repo_explicit_absolute_paths_and_cwds() {
        let (_lock, _guard) = isolate_env("multi_repo");
        let ws_a = _guard.root.join("repoA");
        let ws_b = _guard.root.join("repoB");
        let ws_c = _guard.root.join("repoC");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();
        fs::create_dir_all(&ws_c).unwrap();

        host::pin_workspace(&ws_a).unwrap();
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();

        // Write file in Repo B
        let file_b = ws_b.join("b.txt");
        fs::write(&file_b, "content in repo B").unwrap();

        // Read explicit absolute path in Repo B
        let read_b = engine.call_tool("read_file", json!({ "target_file": file_b.to_string_lossy().to_string() })).await.unwrap();
        assert!(!read_b.is_error);
        assert!(read_b.content[0].text().contains("content in repo B"));

        // Execute command with explicit CWD in Repo C
        let proc_c = engine.call_tool("run_command", json!({
            "command": if cfg!(windows) { "cmd.exe" } else { "sh" },
            "args": if cfg!(windows) { vec!["/c", "echo", "HELLO_REPO_C"] } else { vec!["-c", "echo HELLO_REPO_C"] },
            "workdir": ws_c.to_string_lossy().to_string()
        })).await.unwrap();
        assert!(!proc_c.is_error);
        assert!(proc_c.content[0].text().contains("HELLO_REPO_C"));
    }

    #[tokio::test]
    async fn test_multi_repo_keeps_default_a_while_editing_b_and_running_c() {
        let (_lock, _guard) = isolate_env("multi_repo_exact_contract");
        let ws_a = _guard.root.join("repoA");
        let ws_b = _guard.root.join("repoB");
        let ws_c = _guard.root.join("repoC");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();
        fs::create_dir_all(&ws_c).unwrap();

        let pinned_a = host::pin_workspace(&ws_a).unwrap();
        let generation_a = host::current_workspace_generation();
        let engine = ToolEngine::new(pinned_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();

        let file_b = ws_b.join("edited-in-b.txt");
        let write_b = engine
            .call_tool(
                "write",
                json!({
                    "file_path": file_b.to_string_lossy().to_string(),
                    "content": "multi-repo edit"
                }),
            )
            .await
            .unwrap();
        assert!(!write_b.is_error);

        let read_b = engine
            .call_tool(
                "read_file",
                json!({ "target_file": file_b.to_string_lossy().to_string() }),
            )
            .await
            .unwrap();
        assert!(!read_b.is_error);
        assert!(read_b.content[0].text().contains("multi-repo edit"));

        let proc_c = engine
            .call_tool(
                "run_command",
                json!({
                    "command": if cfg!(windows) { "cmd.exe" } else { "sh" },
                    "args": if cfg!(windows) {
                        vec!["/c", "echo", "MULTI_REPO_C"]
                    } else {
                        vec!["-c", "echo MULTI_REPO_C"]
                    },
                    "workdir": ws_c.to_string_lossy().to_string()
                }),
            )
            .await
            .unwrap();
        assert!(!proc_c.is_error);
        assert!(proc_c.content[0].text().contains("MULTI_REPO_C"));

        assert_eq!(host::read_pinned_workspace(), Some(pinned_a));
        assert_eq!(host::current_workspace_generation(), generation_a);
    }
    #[tokio::test]
    async fn test_stale_context_rejection_performs_no_mutation_or_process_execution() {
        let (_lock, _guard) = isolate_env("stale_no_mutation");
        let ws_a = _guard.root.join("repoA");
        let ws_b = _guard.root.join("repoB");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();

        host::pin_workspace(&ws_a).unwrap();
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();
        host::pin_workspace(&ws_b).unwrap();

        let relative_write = engine
            .call_tool(
                "write",
                json!({
                    "file_path": "must-not-exist.txt",
                    "content": "stale mutation"
                }),
            )
            .await
            .unwrap();
        assert!(relative_write.is_error);
        assert!(!ws_b.join("must-not-exist.txt").exists());

        let marker = ws_b.join("process-must-not-run.txt");
        let process = if cfg!(windows) {
            engine
                .call_tool(
                    "run_command",
                    json!({
                        "command": "cmd.exe",
                        "args": ["/c", "echo stale-process>process-must-not-run.txt"]
                    }),
                )
                .await
                .unwrap()
        } else {
            engine
                .call_tool(
                    "run_command",
                    json!({
                        "command": "sh",
                        "args": ["-c", "echo stale-process > process-must-not-run.txt"]
                    }),
                )
                .await
                .unwrap()
        };
        assert!(process.is_error);
        assert!(!marker.exists(), "stale rejection must happen before process spawn");
    }
    #[tokio::test]
    async fn test_stale_context_allows_explicit_absolute_run_command_workdir() {
        let (_lock, _guard) = isolate_env("stale_absolute_workdir");
        let ws_a = _guard.root.join("repoA");
        let ws_b = _guard.root.join("repoB");
        let ws_c = _guard.root.join("repoC");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();
        fs::create_dir_all(&ws_c).unwrap();

        host::pin_workspace(&ws_a).unwrap();
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();
        host::pin_workspace(&ws_b).unwrap();

        let result = engine
            .call_tool(
                "run_command",
                json!({
                    "command": if cfg!(windows) { "cmd.exe" } else { "sh" },
                    "args": if cfg!(windows) {
                        vec!["/c", "echo", "STALE_EXPLICIT_CWD_OK"]
                    } else {
                        vec!["-c", "echo STALE_EXPLICIT_CWD_OK"]
                    },
                    "workdir": ws_c.to_string_lossy().to_string()
                }),
            )
            .await
            .unwrap();

        assert!(
            !result.is_error,
            "absolute workdir must remain valid while default Workspace generation is stale: {}",
            result.content[0].text()
        );
        assert!(result.content[0].text().contains("STALE_EXPLICIT_CWD_OK"));
    }
    #[tokio::test]
    async fn test_stale_context_allows_explicit_absolute_write() {
        let (_lock, _guard) = isolate_env("stale_absolute_write");
        let ws_a = _guard.root.join("repoA");
        let ws_b = _guard.root.join("repoB");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();

        host::pin_workspace(&ws_a).unwrap();
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();

        host::pin_workspace(&ws_b).unwrap();
        let target = ws_a.join("explicit-write.txt");
        let result = engine
            .call_tool(
                "write",
                json!({
                    "file_path": target.to_string_lossy().to_string(),
                    "content": "absolute write survives stale default context"
                }),
            )
            .await
            .unwrap();

        assert!(
            !result.is_error,
            "absolute write must not be blocked by stale default workspace: {}",
            result.content[0].text()
        );
        assert_eq!(
            fs::read_to_string(target).unwrap(),
            "absolute write survives stale default context"
        );
    }

    #[tokio::test]
    async fn test_stale_context_allows_explicit_absolute_apply_patch() {
        let (_lock, _guard) = isolate_env("stale_absolute_patch");
        let ws_a = _guard.root.join("repoA");
        let ws_b = _guard.root.join("repoB");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();

        let target = ws_a.join("explicit-patch.txt");
        fs::write(&target, "before\n").unwrap();
        host::pin_workspace(&ws_a).unwrap();
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();

        host::pin_workspace(&ws_b).unwrap();
        let patch = format!(
            "*** Begin Patch\n*** Update File: {}\n@@\n-before\n+after\n*** End Patch\n",
            target.display()
        );
        let result = engine
            .call_tool("apply_patch", json!({ "patch": patch }))
            .await
            .unwrap();

        assert!(
            !result.is_error,
            "absolute apply_patch must not be blocked by stale default workspace: {}",
            result.content[0].text()
        );
        assert_eq!(fs::read_to_string(target).unwrap(), "after\n");
    }
    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_schema_exposes_shell_and_cwd() {
        let (_lock, _guard) = isolate_env("shell_schema_cwd");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();
        let engine = ToolEngine::new(ws_dir);

        let tools = engine.list_tools().await.unwrap();
        let terminal = tools
            .iter()
            .find(|tool| tool["name"] == "run_terminal_cmd")
            .expect("run_terminal_cmd definition");
        let properties = terminal["inputSchema"]["properties"]
            .as_object()
            .expect("terminal properties");
        assert!(properties.contains_key("shell"));
        assert!(properties.contains_key("cwd"), "explicit cwd must be model-visible");
        assert!(properties.contains_key("execution_mode"), "execution_mode must be model-visible");
        assert!(properties.contains_key("yield_after_ms"), "yield_after_ms must be model-visible");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_explicit_shell_cwd_foreground_and_background_spaces_unicode() {
        let (_lock, _guard) = isolate_env("shell_cwd_unicode");
        let ws_a = _guard.root.join("defaultA");
        let ws_b = _guard.root.join("Other Repo 測試 With Spaces");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();

        host::pin_workspace(&ws_a).unwrap();
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();

        let fg = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "[IO.File]::WriteAllText('fg-cwd-marker.txt', 'FG_CWD_OK'); Write-Output 'FG_CWD_OK'",
                    "description": "foreground explicit cwd",
                    "shell": "powershell",
                    "cwd": ws_b.to_string_lossy().to_string()
                }),
            )
            .await
            .unwrap();
        assert!(!fg.is_error, "foreground explicit cwd failed: {}", fg.content[0].text());
        assert_eq!(
            dunce::canonicalize(fg.structured.as_ref().unwrap()["current_dir"].as_str().unwrap()).unwrap(),
            dunce::canonicalize(&ws_b).unwrap()
        );
        assert!(fg.content[0].text().contains("FG_CWD_OK"));
        assert_eq!(fs::read_to_string(ws_b.join("fg-cwd-marker.txt")).unwrap(), "FG_CWD_OK");

        let bg = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "[IO.File]::WriteAllText('bg-cwd-marker.txt', 'BG_CWD_OK'); Write-Output 'BG_CWD_OK'",
                    "description": "background explicit cwd",
                    "shell": "powershell",
                    "cwd": ws_b.to_string_lossy().to_string(),
                    "is_background": true
                }),
            )
            .await
            .unwrap();
        assert!(!bg.is_error, "background explicit cwd start failed: {}", bg.content[0].text());
        let task_id = bg.structured.as_ref().unwrap()["task_id"].as_str().unwrap().to_string();
        let out = engine
            .call_tool(
                "get_task_output",
                json!({ "task_id": task_id, "timeout_ms": 15000 }),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let result = &out.structured.as_ref().unwrap()["Result"];
        assert_eq!(result["status"], "completed");
        assert_eq!(result["exit_code"], 0);
        assert!(
            result["output"].as_str().unwrap_or("").contains("BG_CWD_OK"),
            "background command must complete successfully; got: {}",
            result["output"]
        );
        assert_eq!(
            fs::read_to_string(ws_b.join("bg-cwd-marker.txt")).unwrap(),
            "BG_CWD_OK",
            "background task must execute in explicit cwd"
        );
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_explicit_shell_background_invalid_cwd_fails_closed() {
        let (_lock, _guard) = isolate_env("shell_invalid_cwd");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();
        let engine = ToolEngine::new(ws_dir.clone());

        let missing_cwd = ws_dir.join("missing-dir");
        let marker = ws_dir.join("must-not-run.txt");
        let bg = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "[IO.File]::WriteAllText('must-not-run.txt', 'BAD')",
                    "description": "invalid cwd must fail closed",
                    "shell": "powershell",
                    "cwd": missing_cwd.to_string_lossy().to_string(),
                    "is_background": true
                }),
            )
            .await
            .unwrap();
        assert!(!bg.is_error);
        let task_id = bg.structured.as_ref().unwrap()["task_id"].as_str().unwrap().to_string();
        let out = engine
            .call_tool(
                "get_task_output",
                json!({ "task_id": task_id, "timeout_ms": 15000 }),
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        let result = &out.structured.as_ref().unwrap()["Result"];
        assert_eq!(result["status"], "failed", "invalid cwd must fail the task");
        assert!(!marker.exists(), "command must not run in fallback/default cwd");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_default_shell_explicit_cwd_survives_stale_workspace() {
        let (_lock, _guard) = isolate_env("default_shell_explicit_cwd");
        let ws_a = _guard.root.join("defaultA");
        let ws_b = _guard.root.join("defaultB");
        let ws_c = _guard.root.join("Explicit Repo 測試 C");
        fs::create_dir_all(&ws_a).unwrap();
        fs::create_dir_all(&ws_b).unwrap();
        fs::create_dir_all(&ws_c).unwrap();

        host::pin_workspace(&ws_a).unwrap();
        let engine = ToolEngine::new(ws_a.clone());
        let _ = engine.call_tool("workspace_info", json!({})).await.unwrap();
        host::pin_workspace(&ws_b).unwrap();

        let result = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "Set-Content -LiteralPath 'default-cwd-marker.txt' -Value 'DEFAULT_CWD_OK' -NoNewline; Write-Output 'DEFAULT_CWD_OK'",
                    "description": "default shell explicit cwd",
                    "cwd": ws_c.to_string_lossy().to_string()
                }),
            )
            .await
            .unwrap();

        assert!(
            !result.is_error,
            "explicit cwd must remain usable without selecting a shell: {}",
            result.content[0].text()
        );
        assert_eq!(
            fs::read_to_string(ws_c.join("default-cwd-marker.txt")).unwrap(),
            "DEFAULT_CWD_OK"
        );
        assert!(!ws_b.join("default-cwd-marker.txt").exists());
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_explicit_shell_cwd_cmd_and_git_bash_foreground_background() {
        let (_lock, _guard) = isolate_env("shell_cwd_cmd_git_bash");
        let ws_dir = _guard.root.join("default");
        let cwd = _guard.root.join("Other Shell Repo With Spaces");
        fs::create_dir_all(&ws_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();
        let engine = ToolEngine::new(ws_dir);
        let cwd_arg = cwd.to_string_lossy().to_string();

        let cmd_fg = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo CMD_FG_CWD_OK>cmd-fg-cwd-marker.txt",
                    "description": "cmd foreground cwd",
                    "shell": "cmd",
                    "cwd": cwd_arg
                }),
            )
            .await
            .unwrap();
        assert!(!cmd_fg.is_error);
        assert!(cwd.join("cmd-fg-cwd-marker.txt").is_file());

        let cmd_bg = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo CMD_BG_CWD_OK>cmd-bg-cwd-marker.txt",
                    "description": "cmd background cwd",
                    "shell": "cmd",
                    "cwd": cwd_arg,
                    "is_background": true
                }),
            )
            .await
            .unwrap();
        assert!(!cmd_bg.is_error);
        let cmd_task_id = cmd_bg.structured.as_ref().unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_string();
        let cmd_out = engine
            .call_tool(
                "get_task_output",
                json!({ "task_id": cmd_task_id, "timeout_ms": 15000 }),
            )
            .await
            .unwrap();
        assert_eq!(cmd_out.structured.as_ref().unwrap()["Result"]["status"], "completed");
        assert!(cwd.join("cmd-bg-cwd-marker.txt").is_file());

        if host::find_git_bash().is_ok() {
            let git_fg = engine
                .call_tool(
                    "run_terminal_cmd",
                    json!({
                        "command": "printf GIT_FG_CWD_OK > git-fg-cwd-marker.txt",
                        "description": "git bash foreground cwd",
                        "shell": "git-bash",
                        "cwd": cwd_arg
                    }),
                )
                .await
                .unwrap();
            assert!(!git_fg.is_error);
            assert!(cwd.join("git-fg-cwd-marker.txt").is_file());

            let git_bg = engine
                .call_tool(
                    "run_terminal_cmd",
                    json!({
                        "command": "printf GIT_BG_CWD_OK > git-bg-cwd-marker.txt",
                        "description": "git bash background cwd",
                        "shell": "git-bash",
                        "cwd": cwd_arg,
                        "is_background": true
                    }),
                )
                .await
                .unwrap();
            assert!(!git_bg.is_error);
            let git_task_id = git_bg.structured.as_ref().unwrap()["task_id"]
                .as_str()
                .unwrap()
                .to_string();
            let git_out = engine
                .call_tool(
                    "get_task_output",
                    json!({ "task_id": git_task_id, "timeout_ms": 15000 }),
                )
                .await
                .unwrap();
            assert_eq!(git_out.structured.as_ref().unwrap()["Result"]["status"], "completed");
            assert!(cwd.join("git-bg-cwd-marker.txt").is_file());
        }
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_shell_selectors_on_windows() {
        let (_lock, _guard) = isolate_env("shell_sel");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // 1. powershell selector
        let ps_res = engine.call_tool("run_terminal_cmd", json!({
            "command": "$val = 'POWERSHELL_SYNTAX_OK'; Write-Output $val",
            "description": "test powershell selector",
            "shell": "powershell"
        })).await.unwrap();
        assert!(!ps_res.is_error);
        assert!(ps_res.content[0].text().contains("POWERSHELL_SYNTAX_OK"));
        let ps_struct = ps_res.structured.expect("must return structured output");
        assert_eq!(ps_struct["exit_code"], 0);
        assert_eq!(ps_struct["has_output"], true);

        // 2. cmd selector
        let cmd_res = engine.call_tool("run_terminal_cmd", json!({
            "command": "echo CMD_SYNTAX_OK",
            "description": "test cmd selector",
            "shell": "cmd"
        })).await.unwrap();
        assert!(!cmd_res.is_error);
        assert!(cmd_res.content[0].text().contains("CMD_SYNTAX_OK"));
        let cmd_struct = cmd_res.structured.expect("must return structured output");
        assert_eq!(cmd_struct["exit_code"], 0);

        // 3. invalid shell selector fails deterministically
        let invalid_res = engine.call_tool("run_terminal_cmd", json!({
            "command": "echo test",
            "description": "test invalid shell",
            "shell": "nonexistent_shell_xyz"
        })).await.unwrap();
        assert!(invalid_res.is_error);
        assert!(invalid_res.content[0].text().contains("unsupported shell selector"));

        // 4. git-bash selector (if installed) or deterministic error
        let git_bash_res = engine.call_tool("run_terminal_cmd", json!({
            "command": "echo GIT_BASH_OK",
            "description": "test git-bash selector",
            "shell": "git-bash"
        })).await.unwrap();
        if host::find_git_bash().is_ok() {
            assert!(!git_bash_res.is_error);
            assert!(git_bash_res.content[0].text().contains("GIT_BASH_OK"));
        } else {
            assert!(git_bash_res.is_error);
            assert!(git_bash_res.content[0].text().contains("Git for Windows Bash not found"));
        }
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

    #[tokio::test]
    async fn test_structured_output_for_bridge_read_file_and_list_dir() {
        let (_lock, _guard) = isolate_env("struct_read");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws dir");
        fs::write(ws_dir.join("demo.txt"), "hello structured tool call").unwrap();

        let engine = ToolEngine::new(ws_dir.clone());
        let res = engine
            .call_tool("read_file", json!({ "target_file": "demo.txt" }))
            .await
            .expect("read_file succeeds");

        assert!(!res.is_error);
        assert!(res.content[0].text().contains("hello structured tool call"));
        assert!(res.structured.is_some(), "read_file must return structured output");

        let list_res = engine
            .call_tool("list_dir", json!({ "target_directory": ws_dir.to_string_lossy().to_string() }))
            .await
            .expect("list_dir succeeds");
        assert!(!list_res.is_error);
        assert!(list_res.structured.is_some(), "list_dir must return structured output");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_get_task_output_structured_output_and_no_contradictory_text() {
        let (_lock, _guard) = isolate_env("task_out_struct");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // Start a real background task producing substantial output exceeding inline surface
        let bg_res = engine.call_tool("run_terminal_cmd", json!({
            "command": "powershell.exe -NoProfile -NonInteractive -Command \"[Console]::Out.Write(('BULK_A:' + [string][char]0x41) * 60000); Write-Output 'TRUNC_TAIL_SENTINEL'\"",
            "description": "large retained output test",
            "is_background": true
        })).await.unwrap();

        assert!(!bg_res.is_error);
        let bg_struct = bg_res.structured.expect("background task start must return structured output");
        let task_id = bg_struct["task_id"].as_str().expect("must have task_id");
        assert_eq!(bg_struct["status"], "running");
        assert!(!task_id.is_empty());

        // Wait with timeout_ms for background execution
        let out_res = engine.call_tool("get_task_output", json!({
            "task_id": task_id,
            "timeout_ms": 30000
        })).await.unwrap();

        assert!(!out_res.is_error);
        let out_text = out_res.content[0].text();
        assert!(out_text.contains("... (output truncated) ..."));
        assert!(!out_text.contains("(no output)"), "must not report contradictory '(no output)'");
        assert!(out_text.len() <= 60_000, "rendered output must stay bounded");

        let out_struct = out_res.structured.expect("get_task_output must return structured output");
        let result_obj = &out_struct["Result"];
        assert_eq!(result_obj["task_id"], task_id);
        assert_eq!(result_obj["has_output"], true);
        assert_eq!(result_obj["truncated"], true);
        let total_bytes = result_obj["total_bytes"].as_u64().unwrap_or(0);
        assert!(total_bytes > out_text.len() as u64, "total_bytes ({total_bytes}) must exceed inline length ({})", out_text.len());

        let output_file = result_obj["output_file"]
            .as_str()
            .expect("Result must include output_file path when truncated");
        assert!(!output_file.is_empty(), "output_file path must not be empty");
        let log_path = PathBuf::from(output_file);
        assert!(log_path.is_file(), "output_file must point to an existing retained log file: {}", log_path.display());
        let log_content = fs::read_to_string(&log_path).expect("retained log file must be readable");
        assert!(
            log_content.contains("TRUNC_TAIL_SENTINEL"),
            "retained log must contain full-output tail sentinel"
        );
        assert!(
            log_content.contains("BULK_A:A"),
            "retained log must contain bulk data payload"
        );

        let _ = engine.call_tool("kill_task", json!({ "task_id": task_id })).await;
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_explicit_shell_foreground_nonzero_exit_is_not_mcp_error() {
        let (_lock, _guard) = isolate_env("fg_nonzero");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // 1. Foreground cmd with non-zero exit code
        let cmd_res = engine.call_tool("run_terminal_cmd", json!({
            "command": "exit /b 42",
            "description": "nonzero exit test",
            "shell": "cmd"
        })).await.unwrap();

        assert!(!cmd_res.is_error, "exit_code != 0 must NOT become MCP isError");
        let cmd_struct = cmd_res.structured.expect("must return structured output");
        assert_eq!(cmd_struct["exit_code"], 42);
        assert_eq!(cmd_struct["timed_out"], false);

        // 2. Foreground powershell with non-zero exit code
        let ps_res = engine.call_tool("run_terminal_cmd", json!({
            "command": "exit 7",
            "description": "nonzero exit powershell",
            "shell": "powershell"
        })).await.unwrap();

        assert!(!ps_res.is_error, "exit_code != 0 must NOT become MCP isError");
        let ps_struct = ps_res.structured.expect("must return structured output");
        assert_eq!(ps_struct["exit_code"], 7);
    }

    #[test]
    fn test_render_proc_output_multibyte_utf8_truncation() {
        let unit = "Tiếng Việt 🦀 測 ";
        let repeats = (50_000 / unit.len()) + 1;
        let large_stdout = unit.repeat(repeats);

        let proc_output = crate::run_proc::ProcOutput {
            stdout: large_stdout,
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            capture_truncated: false,
            error: None,
            termination_reason: None,
        };

        // This must not panic on non-char-boundary slice
        let result = render_proc_output_as_terminal_result(
            proc_output,
            "echo test",
            "C:/ws",
            None,
        );

        assert!(!result.is_error);
        let text = result.content[0].text();
        assert!(text.contains("... (output truncated) ..."));
        let structured = result.structured.unwrap();
        assert_eq!(structured["truncated"], true);
        assert!(structured["total_bytes"].as_u64().unwrap() > 40_000);
        let out_file = structured["output_file"].as_str().unwrap();
        assert!(!out_file.is_empty());
        let _ = std::fs::remove_file(out_file);
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_background_shell_selectors_and_path_shadow_protection() {
        let (_lock, _guard) = isolate_env("bg_shell_shadow");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // 1. Background powershell selector with valid syntax & quote/metacharacter sensitivity
        let ps_bg = engine.call_tool("run_terminal_cmd", json!({
            "command": "$v = 'BG_POWERSHELL_OK''_QUOTE_&_<SPECIAL>'; Write-Output \"OUT:$v\"",
            "description": "bg powershell",
            "shell": "powershell",
            "is_background": true
        })).await.unwrap();

        assert!(!ps_bg.is_error);
        let ps_task_id = ps_bg.structured.as_ref().unwrap()["task_id"].as_str().unwrap().to_string();
        assert!(!ps_task_id.is_empty());

        let ps_out = engine.call_tool("get_task_output", json!({
            "task_id": ps_task_id,
            "timeout_ms": 15000
        })).await.unwrap();
        assert!(!ps_out.is_error);
        let ps_struct = ps_out.structured.expect("must return structured output");
        let ps_res = &ps_struct["Result"];
        assert_eq!(ps_res["status"], "completed");
        assert_eq!(ps_res["exit_code"], 0);
        let ps_output_text = ps_res["output"].as_str().unwrap().trim();
        assert_eq!(ps_output_text, "OUT:BG_POWERSHELL_OK'_QUOTE_&_<SPECIAL>");
        assert!(ps_out.content[0].text().contains("OUT:BG_POWERSHELL_OK'_QUOTE_&_<SPECIAL>"));

        // 2. Background cmd selector with PATH shadowed by fake cmd.cmd + quote/special payload
        let shadow_dir = _guard.root.join("fake_bin");
        fs::create_dir_all(&shadow_dir).unwrap();
        fs::write(shadow_dir.join("cmd.cmd"), "@echo off
echo SHADOWED_CMD
exit /b 1
").unwrap();
        fs::write(shadow_dir.join("cmd.ps1"), "Write-Error 'SHADOWED_CMD'
").unwrap();

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let mut entries = vec![shadow_dir.clone()];
        entries.extend(std::env::split_paths(&orig_path));
        let shadowed_path = std::env::join_paths(entries).unwrap();
        unsafe {
            std::env::set_var("PATH", &shadowed_path);
        }

        let cmd_bg = engine.call_tool("run_terminal_cmd", json!({
            "command": "echo BG_CMD_NATIVE_OK_QUOTE_\"TEST\"_SPECIAL",
            "description": "bg cmd with shadow",
            "shell": "cmd",
            "is_background": true
        })).await.unwrap();

        assert!(!cmd_bg.is_error);
        let cmd_task_id = cmd_bg.structured.as_ref().unwrap()["task_id"].as_str().unwrap().to_string();
        assert!(!cmd_task_id.is_empty());

        let cmd_out = engine.call_tool("get_task_output", json!({
            "task_id": cmd_task_id,
            "timeout_ms": 15000
        })).await.unwrap();
        assert!(!cmd_out.is_error);
        let cmd_struct = cmd_out.structured.expect("must return structured output");
        let cmd_res = &cmd_struct["Result"];
        assert_eq!(cmd_res["status"], "completed");
        assert_eq!(cmd_res["exit_code"], 0);
        assert!(cmd_res["output"].as_str().unwrap().contains("BG_CMD_NATIVE_OK_QUOTE_\"TEST\"_SPECIAL"));
        assert!(!cmd_out.content[0].text().contains("SHADOWED_CMD"));

        // 3. Background git-bash selector (if installed) or deterministic error
        let git_bash_bg = engine.call_tool("run_terminal_cmd", json!({
            "command": "VAR='BG_GIT_BASH_OK'; echo \"$VAR:POSIX_QUOTE_TEST_&\"",
            "description": "bg git-bash",
            "shell": "git-bash",
            "is_background": true
        })).await.unwrap();

        if host::find_git_bash().is_ok() {
            assert!(!git_bash_bg.is_error);
            let gb_task_id = git_bash_bg.structured.as_ref().unwrap()["task_id"].as_str().unwrap().to_string();
            assert!(!gb_task_id.is_empty());

            let gb_out = engine.call_tool("get_task_output", json!({
                "task_id": gb_task_id,
                "timeout_ms": 15000
            })).await.unwrap();
            assert!(!gb_out.is_error);
            let gb_struct = gb_out.structured.expect("must return structured output");
            let gb_res = &gb_struct["Result"];
            assert_eq!(gb_res["status"], "completed");
            assert_eq!(gb_res["exit_code"], 0);
            assert!(gb_res["output"].as_str().unwrap().contains("BG_GIT_BASH_OK:POSIX_QUOTE_TEST_&"));
        } else {
            assert!(git_bash_bg.is_error);
            assert!(git_bash_bg.content[0].text().contains("Git for Windows Bash not found"));
        }

        // 4. Reject aliases like \"pwsh\" and \"bash\" deterministically
        let pwsh_err = engine.call_tool("run_terminal_cmd", json!({
            "command": "echo test",
            "shell": "pwsh"
        })).await.unwrap();
        assert!(pwsh_err.is_error);
        assert!(pwsh_err.content[0].text().contains("unsupported shell selector: 'pwsh'"));

        let bash_err = engine.call_tool("run_terminal_cmd", json!({
            "command": "echo test",
            "shell": "bash"
        })).await.unwrap();
        assert!(bash_err.is_error);
        assert!(bash_err.content[0].text().contains("unsupported shell selector: 'bash'"));
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_short_completes_foreground_shape() {
        let (_lock, _guard) = isolate_env("auto_short");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "powershell.exe -NoProfile -NonInteractive -Command \"Write-Output 'AUTO_SHORT_PAYLOAD'\"",
                    "description": "auto mode short command",
                    "execution_mode": "auto",
                    "yield_after_ms": 10000
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let text = res.content[0].text();
        assert!(text.contains("exit: 0"));
        assert!(text.contains("AUTO_SHORT_PAYLOAD"));

        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["exit_code"], 0);
        assert_eq!(structured["status"], "completed");
        assert_eq!(structured["yielded"], false);
        assert_eq!(structured["timed_out"], false);
        assert_eq!(structured["execution_mode"], "auto");
        assert_eq!(structured["has_output"], true);
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_long_yields_to_background_and_resumes() {
        let (_lock, _guard) = isolate_env("auto_yield");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // Yield budget 400ms while command sleeps 2.5s
        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "powershell.exe -NoProfile -NonInteractive -Command \"Start-Sleep -Milliseconds 2500; Write-Output 'AUTO_YIELD_COMPLETED'\"",
                    "description": "auto mode long command",
                    "execution_mode": "auto",
                    "yield_after_ms": 400
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let text = res.content[0].text();
        assert!(text.contains("[Command yielded to background after 400ms"));
        assert!(text.contains("get_task_output"));

        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        assert_eq!(structured["backgrounded"], true);
        assert_eq!(structured["execution_mode"], "auto");
        let task_id = structured["task_id"].as_str().expect("task_id must be present").to_string();
        assert!(!task_id.is_empty());

        // Inspect and wait on task output via get_task_output
        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({
                    "task_id": task_id,
                    "timeout_ms": 15000
                }),
            )
            .await
            .unwrap();

        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("get_task_output must return structured output");
        let result_obj = &poll_struct["Result"];
        assert_eq!(result_obj["status"], "completed");
        assert_eq!(result_obj["exit_code"], 0);
        assert!(result_obj["output"].as_str().unwrap().contains("AUTO_YIELD_COMPLETED"));
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_yield_after_ms_without_mode_opts_in() {
        let (_lock, _guard) = isolate_env("auto_opt_in");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // Passing yield_after_ms without execution_mode opts into auto mode
        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "powershell.exe -NoProfile -NonInteractive -Command \"Start-Sleep -Milliseconds 2500; Write-Output 'IMPLICIT_AUTO_OK'\"",
                    "description": "yield_after_ms opt in test",
                    "yield_after_ms": 400
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        assert_eq!(structured["backgrounded"], true);
        let task_id = structured["task_id"].as_str().unwrap().to_string();

        let _ = engine.call_tool("kill_task", json!({ "task_id": task_id })).await;
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_partial_output_continuity() {
        let (_lock, _guard) = isolate_env("auto_continuity");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "powershell.exe -NoProfile -NonInteractive -Command \"Write-Output 'PARTIAL_HEAD_SENTINEL'; Start-Sleep -Milliseconds 2000; Write-Output 'PARTIAL_TAIL_SENTINEL'\"",
                    "description": "partial output continuity test",
                    "execution_mode": "auto",
                    "yield_after_ms": 500
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        let task_id = structured["task_id"].as_str().unwrap().to_string();

        // Wait for subsequent output and completion
        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({
                    "task_id": task_id,
                    "timeout_ms": 15000
                }),
            )
            .await
            .unwrap();

        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("must return structured output");
        let result_obj = &poll_struct["Result"];
        assert_eq!(result_obj["status"], "completed");
        let output = result_obj["output"].as_str().unwrap();
        assert!(output.contains("PARTIAL_HEAD_SENTINEL"), "output must retain head produced before yield");
        assert!(output.contains("PARTIAL_TAIL_SENTINEL"), "output must contain tail produced after yield");
        assert_eq!(output.matches("PARTIAL_HEAD_SENTINEL").count(), 1, "head must not be duplicated");
        assert_eq!(output.matches("PARTIAL_TAIL_SENTINEL").count(), 1, "tail must not be duplicated");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_yield_and_kill_lifecycle() {
        let (_lock, _guard) = isolate_env("auto_kill");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "powershell.exe -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 60\"",
                    "description": "auto mode kill test",
                    "execution_mode": "auto",
                    "yield_after_ms": 300
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        let task_id = structured["task_id"].as_str().unwrap().to_string();

        // Kill the yielded task
        let kill_res = engine
            .call_tool(
                "kill_task",
                json!({ "task_id": &task_id }),
            )
            .await
            .unwrap();
        assert!(!kill_res.is_error);

        // Verify task state after kill
        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({ "task_id": &task_id }),
            )
            .await
            .unwrap();
        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("must return structured output");
        let status = poll_struct["Result"]["status"].as_str().unwrap();
        assert!(status == "cancelled" || status == "killed" || status == "completed" || status == "failed");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_with_shell_selectors() {
        let (_lock, _guard) = isolate_env("auto_shell");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // 1. cmd short auto
        let cmd_short = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo CMD_AUTO_SHORT_OK",
                    "description": "cmd auto short",
                    "shell": "cmd",
                    "execution_mode": "auto",
                    "yield_after_ms": 5000
                }),
            )
            .await
            .unwrap();
        assert!(!cmd_short.is_error);
        assert!(cmd_short.content[0].text().contains("CMD_AUTO_SHORT_OK"));
        let cmd_struct = cmd_short.structured.unwrap();
        assert_eq!(cmd_struct["status"], "completed");
        assert_eq!(cmd_struct["yielded"], false);

        // 2. cmd long auto yields
        let cmd_long = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "ping -n 5 127.0.0.1 > nul && echo CMD_YIELD_DONE",
                    "description": "cmd auto long",
                    "shell": "cmd",
                    "execution_mode": "auto",
                    "yield_after_ms": 400
                }),
            )
            .await
            .unwrap();
        assert!(!cmd_long.is_error);
        let cmd_long_struct = cmd_long.structured.unwrap();
        assert_eq!(cmd_long_struct["status"], "running");
        assert_eq!(cmd_long_struct["yielded"], true);
        let cmd_task_id = cmd_long_struct["task_id"].as_str().unwrap().to_string();

        let _ = engine.call_tool("kill_task", json!({ "task_id": cmd_task_id })).await;
    }

    #[tokio::test]
    async fn test_run_terminal_cmd_invalid_execution_mode_and_yield_after_ms() {
        let (_lock, _guard) = isolate_env("auto_invalid");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let invalid_mode = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo test",
                    "execution_mode": "unsupported_mode"
                }),
            )
            .await
            .unwrap();
        assert!(invalid_mode.is_error);
        assert!(invalid_mode.content[0].text().contains("unsupported execution_mode: 'unsupported_mode'"));

        let invalid_yield = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo test",
                    "execution_mode": "auto",
                    "yield_after_ms": "not-a-number"
                }),
            )
            .await
            .unwrap();
        assert!(invalid_yield.is_error);
        assert!(invalid_yield.content[0].text().contains("yield_after_ms must be an integer"));
    }
    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_nonzero_exit_is_not_mcp_error() {
        let (_lock, _guard) = isolate_env("auto_nonzero");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "exit /b 42",
                    "description": "auto nonzero exit",
                    "shell": "cmd",
                    "execution_mode": "auto",
                    "yield_after_ms": 10000
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error, "exit_code != 0 must NOT become MCP isError in auto mode");
        let structured = res.structured.expect("must return structured output");
        assert_ne!(structured["exit_code"], 0);
        assert_eq!(structured["status"], "failed");
        assert_eq!(structured["yielded"], false);
        assert_eq!(structured["timed_out"], false);
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_run_terminal_cmd_auto_mode_single_process_no_double_spawn_and_pid_identity() {
        let (_lock, _guard) = isolate_env("auto_single_proc");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let marker_file = ws_dir.join("marker.log");
        let marker_path = marker_file.to_string_lossy().to_string();

        let engine = ToolEngine::new(ws_dir.clone());

        // 1. Long running command that appends lines to a file with delay
        let cmd = format!(
            "echo PROC_START>> \"{marker_path}\" && ping -n 4 127.0.0.1 > nul && echo PROC_END>> \"{marker_path}\""
        );
        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": cmd,
                    "description": "single process identity test",
                    "shell": "cmd",
                    "execution_mode": "auto",
                    "yield_after_ms": 400
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        assert_eq!(structured["execution_mode"], "auto");
        let task_id = structured["task_id"].as_str().expect("task_id must exist").to_string();
        let initial_pid = structured.get("pid").and_then(Value::as_u64);

        // At yield time, PROC_START was written once; PROC_END is not yet written
        let mid_content = fs::read_to_string(&marker_file).unwrap_or_default();
        assert!(mid_content.contains("PROC_START"));
        assert_eq!(mid_content.matches("PROC_START").count(), 1);
        assert!(!mid_content.contains("PROC_END"));

        // Wait for completion via get_task_output
        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({
                    "task_id": &task_id,
                    "timeout_ms": 15000
                }),
            )
            .await
            .unwrap();

        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("must return structured output");
        let result_obj = &poll_struct["Result"];
        assert_eq!(result_obj["status"], "completed");
        assert_eq!(result_obj["exit_code"], 0);

        // Verify file content: PROC_START must appear EXACTLY ONCE (proving no double spawn / restart)
        let final_content = fs::read_to_string(&marker_file).expect("read final marker");
        assert_eq!(
            final_content.matches("PROC_START").count(),
            1,
            "PROC_START must not be re-executed by a duplicate spawn"
        );
        assert_eq!(
            final_content.matches("PROC_END").count(),
            1,
            "PROC_END must execute exactly once"
        );

        if let Some(pid) = initial_pid {
            assert!(pid > 0, "PID must be a valid positive integer");
        }

        // 2. Short command completing before yield budget also executes exactly once and preserves PID field
        let short_marker = ws_dir.join("short_marker.log");
        let short_path = short_marker.to_string_lossy().to_string();
        let short_cmd = format!("echo SHORT_ONCE>> \"{short_path}\"");
        let short_res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": short_cmd,
                    "description": "short single execution test",
                    "shell": "cmd",
                    "execution_mode": "auto",
                    "yield_after_ms": 10000
                }),
            )
            .await
            .unwrap();

        assert!(!short_res.is_error);
        let short_struct = short_res.structured.unwrap();
        assert_eq!(short_struct["status"], "completed");
        assert_eq!(short_struct["yielded"], false);
        assert!(short_struct.get("pid").is_some(), "PID field must be present in structured output");
        let short_content = fs::read_to_string(&short_marker).expect("read short marker");
        assert_eq!(short_content.matches("SHORT_ONCE").count(), 1);
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_short_completes_foreground_shape_unix() {
        let (_lock, _guard) = isolate_env("auto_short_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo 'AUTO_SHORT_PAYLOAD_UNIX'",
                    "description": "auto mode short command unix",
                    "execution_mode": "auto",
                    "yield_after_ms": 10000
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let text = res.content[0].text();
        assert!(text.contains("AUTO_SHORT_PAYLOAD_UNIX"));

        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["exit_code"], 0);
        assert_eq!(structured["status"], "completed");
        assert_eq!(structured["yielded"], false);
        assert_eq!(structured["timed_out"], false);
        assert_eq!(structured["execution_mode"], "auto");
        assert_eq!(structured["has_output"], true);
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_long_yields_to_background_and_resumes_unix() {
        let (_lock, _guard) = isolate_env("auto_yield_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "sleep 2; echo 'AUTO_YIELD_COMPLETED_UNIX'",
                    "description": "auto mode long command unix",
                    "execution_mode": "auto",
                    "yield_after_ms": 300
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let text = res.content[0].text();
        assert!(text.contains("[Command yielded to background after 300ms"));
        assert!(text.contains("get_task_output"));

        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        assert_eq!(structured["backgrounded"], true);
        assert_eq!(structured["execution_mode"], "auto");
        let task_id = structured["task_id"].as_str().expect("task_id must be present").to_string();
        assert!(!task_id.is_empty());

        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({
                    "task_id": task_id,
                    "timeout_ms": 15000
                }),
            )
            .await
            .unwrap();

        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("get_task_output must return structured output");
        let result_obj = &poll_struct["Result"];
        assert_eq!(result_obj["status"], "completed");
        assert_eq!(result_obj["exit_code"], 0);
        assert!(result_obj["output"].as_str().unwrap().contains("AUTO_YIELD_COMPLETED_UNIX"));
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_yield_after_ms_without_mode_opts_in_unix() {
        let (_lock, _guard) = isolate_env("auto_opt_in_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "sleep 2; echo 'IMPLICIT_AUTO_OK_UNIX'",
                    "description": "yield_after_ms opt in test unix",
                    "yield_after_ms": 300
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        assert_eq!(structured["backgrounded"], true);
        let task_id = structured["task_id"].as_str().unwrap().to_string();

        let _ = engine.call_tool("kill_task", json!({ "task_id": task_id })).await;
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_partial_output_continuity_unix() {
        let (_lock, _guard) = isolate_env("auto_continuity_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo 'PARTIAL_HEAD_SENTINEL_UNIX'; sleep 2; echo 'PARTIAL_TAIL_SENTINEL_UNIX'",
                    "description": "partial output continuity test unix",
                    "execution_mode": "auto",
                    "yield_after_ms": 400
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        let task_id = structured["task_id"].as_str().unwrap().to_string();

        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({
                    "task_id": task_id,
                    "timeout_ms": 15000
                }),
            )
            .await
            .unwrap();

        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("must return structured output");
        let result_obj = &poll_struct["Result"];
        assert_eq!(result_obj["status"], "completed");
        let output = result_obj["output"].as_str().unwrap();
        assert!(output.contains("PARTIAL_HEAD_SENTINEL_UNIX"), "output must retain head produced before yield");
        assert!(output.contains("PARTIAL_TAIL_SENTINEL_UNIX"), "output must contain tail produced after yield");
        assert_eq!(output.matches("PARTIAL_HEAD_SENTINEL_UNIX").count(), 1, "head must not be duplicated");
        assert_eq!(output.matches("PARTIAL_TAIL_SENTINEL_UNIX").count(), 1, "tail must not be duplicated");
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_yield_and_kill_lifecycle_unix() {
        let (_lock, _guard) = isolate_env("auto_kill_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "sleep 60",
                    "description": "auto mode kill test unix",
                    "execution_mode": "auto",
                    "yield_after_ms": 300
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        let task_id = structured["task_id"].as_str().unwrap().to_string();

        let kill_res = engine
            .call_tool(
                "kill_task",
                json!({ "task_id": &task_id }),
            )
            .await
            .unwrap();
        assert!(!kill_res.is_error);

        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({ "task_id": &task_id }),
            )
            .await
            .unwrap();
        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("must return structured output");
        let status = poll_struct["Result"]["status"].as_str().unwrap();
        assert!(status == "cancelled" || status == "killed" || status == "completed" || status == "failed");
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_nonzero_exit_is_not_mcp_error_unix() {
        let (_lock, _guard) = isolate_env("auto_nonzero_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "sh -c 'exit 42'",
                    "description": "auto nonzero exit unix",
                    "execution_mode": "auto",
                    "yield_after_ms": 10000
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error, "exit_code != 0 must NOT become MCP isError in auto mode");
        let structured = res.structured.expect("must return structured output");
        assert_ne!(structured["exit_code"], 0);
        assert_eq!(structured["status"], "failed");
        assert_eq!(structured["yielded"], false);
        assert_eq!(structured["timed_out"], false);
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_with_shell_selectors_unix() {
        let (_lock, _guard) = isolate_env("auto_shell_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let engine = ToolEngine::new(ws_dir.clone());

        // 1. git-bash selector short auto on Unix
        let git_bash_short = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo GIT_BASH_AUTO_SHORT_OK",
                    "description": "git-bash auto short unix",
                    "shell": "git-bash",
                    "execution_mode": "auto",
                    "yield_after_ms": 5000
                }),
            )
            .await
            .unwrap();

        if host::find_git_bash().is_ok() {
            assert!(!git_bash_short.is_error);
            assert!(git_bash_short.content[0].text().contains("GIT_BASH_AUTO_SHORT_OK"));
            let gb_struct = git_bash_short.structured.unwrap();
            assert_eq!(gb_struct["status"], "completed");
            assert_eq!(gb_struct["yielded"], false);
        } else {
            assert!(git_bash_short.is_error);
        }

        // 2. cmd selector fails deterministically on non-Windows
        let cmd_res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": "echo test",
                    "shell": "cmd",
                    "execution_mode": "auto"
                }),
            )
            .await
            .unwrap();
        assert!(cmd_res.is_error);
        assert!(cmd_res.content[0].text().contains("cmd shell is only supported on Windows"));
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_run_terminal_cmd_auto_mode_single_process_no_double_spawn_and_pid_identity_unix() {
        let (_lock, _guard) = isolate_env("auto_single_proc_unix");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).unwrap();

        let marker_file = ws_dir.join("marker_unix.log");
        let marker_path = marker_file.to_string_lossy().to_string();

        let engine = ToolEngine::new(ws_dir.clone());

        // 1. Long running command that appends lines to a file with delay
        let cmd = format!(
            "echo 'UNIX_PROC_START' >> '{marker_path}'; sleep 2; echo 'UNIX_PROC_END' >> '{marker_path}'"
        );
        let res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": cmd,
                    "description": "unix single process identity test",
                    "execution_mode": "auto",
                    "yield_after_ms": 400
                }),
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let structured = res.structured.expect("must return structured output");
        assert_eq!(structured["status"], "running");
        assert_eq!(structured["yielded"], true);
        assert_eq!(structured["execution_mode"], "auto");
        let task_id = structured["task_id"].as_str().expect("task_id must exist").to_string();
        let initial_pid = structured.get("pid").and_then(Value::as_u64);

        // At yield time, UNIX_PROC_START was written once; UNIX_PROC_END is not yet written
        let mid_content = fs::read_to_string(&marker_file).unwrap_or_default();
        assert!(mid_content.contains("UNIX_PROC_START"));
        assert_eq!(mid_content.matches("UNIX_PROC_START").count(), 1);
        assert!(!mid_content.contains("UNIX_PROC_END"));

        // Wait for completion via get_task_output
        let poll_res = engine
            .call_tool(
                "get_task_output",
                json!({
                    "task_id": &task_id,
                    "timeout_ms": 15000
                }),
            )
            .await
            .unwrap();

        assert!(!poll_res.is_error);
        let poll_struct = poll_res.structured.expect("must return structured output");
        let result_obj = &poll_struct["Result"];
        assert_eq!(result_obj["status"], "completed");
        assert_eq!(result_obj["exit_code"], 0);

        // Verify file content: UNIX_PROC_START must appear EXACTLY ONCE (proving no double spawn / restart)
        let final_content = fs::read_to_string(&marker_file).expect("read final marker");
        assert_eq!(
            final_content.matches("UNIX_PROC_START").count(),
            1,
            "UNIX_PROC_START must not be re-executed by a duplicate spawn"
        );
        assert_eq!(
            final_content.matches("UNIX_PROC_END").count(),
            1,
            "UNIX_PROC_END must execute exactly once"
        );

        if let Some(pid) = initial_pid {
            assert!(pid > 0, "PID must be a valid positive integer");
        }

        // 2. Short command completing before yield budget also executes exactly once and preserves PID field
        let short_marker = ws_dir.join("short_marker_unix.log");
        let short_path = short_marker.to_string_lossy().to_string();
        let short_cmd = format!("echo 'SHORT_UNIX_ONCE' >> '{short_path}'");
        let short_res = engine
            .call_tool(
                "run_terminal_cmd",
                json!({
                    "command": short_cmd,
                    "description": "unix short single execution test",
                    "execution_mode": "auto",
                    "yield_after_ms": 10000
                }),
            )
            .await
            .unwrap();

        assert!(!short_res.is_error);
        let short_struct = short_res.structured.unwrap();
        assert_eq!(short_struct["status"], "completed");
        assert_eq!(short_struct["yielded"], false);
        assert!(short_struct.get("pid").is_some(), "PID field must be present in structured output");
        let short_content = fs::read_to_string(&short_marker).expect("read short marker");
        assert_eq!(short_content.matches("SHORT_UNIX_ONCE").count(), 1);
    }

}
