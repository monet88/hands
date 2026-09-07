//! MCP JSON-RPC over stdio (newline-delimited) and Streamable HTTP POST /mcp.
//! No extra crates: ChatGPT tunnel-client speaks stdio; Inspector can use HTTP.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::computer::local::LocalTerminalBackend;
use xai_grok_tools::computer::types::TerminalBackend;
use xai_grok_tools::types::output::{ToolOutput, ToolRunResult};
use crate::edit;
use crate::host;
use crate::plugin;
use crate::ui;
use crate::run_command;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "Hands";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bound on retained session terminal backends. A backend owns its session's
/// live/completed tasks, so eviction only ever targets *inactive* sessions:
/// never the session being served, never a session with an in-flight call,
/// never a session with a live background task. Victims lose cached bridges
/// and backend together; their next call rebuilds cold (history loss for idle
/// sessions past the cap is the accepted cost). Temporary over-cap while
/// sessions are active is fine; permanent unbounded growth is what this
/// prevents.
const MAX_SESSION_BACKENDS: usize = 64;
pub struct McpHost {
    fallback_cwd: PathBuf,
    cached: Mutex<HashMap<(String, PathBuf), ToolBridge>>,
    backends: Mutex<HashMap<String, (Arc<LocalTerminalBackend>, Instant)>>,
    inflight: std::sync::Mutex<HashMap<String, usize>>,
    call_seq: AtomicU64,
}

/// RAII in-flight marker: a session holding a live guard is never evicted.
/// Blocking mutex by design: plain counters, never held across `.await`.
struct InflightGuard<'a> {
    counts: &'a std::sync::Mutex<HashMap<String, usize>>,
    session: String,
}

impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(n) = counts.get_mut(&self.session) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(&self.session);
            }
        }
    }
}

/// Oldest-inactive-first eviction pick, returning at most `need` victims.
/// Pure for testability. Immune: the session being served and any session with
/// an in-flight call. Sessions holding cached bridges are NOT immune.
fn session_eviction_order(
    sessions: Vec<(String, Instant)>,
    inflight: &HashMap<String, usize>,
    current: &str,
    need: usize,
) -> Vec<String> {
    let mut idle: Vec<(String, Instant)> = sessions
        .into_iter()
        .filter(|(s, _)| *s != current && inflight.get(s).copied().unwrap_or(0) == 0)
        .collect();
    idle.sort_by_key(|(_, t)| *t);
    idle.into_iter().take(need).map(|(s, _)| s).collect()
}

impl McpHost {
    pub fn new(fallback_cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            fallback_cwd,
            cached: Mutex::new(HashMap::new()),
            backends: Mutex::new(HashMap::new()),
            inflight: std::sync::Mutex::new(HashMap::new()),
            call_seq: AtomicU64::new(1),
        })
    }

    fn workspace(&self) -> PathBuf {
        host::resolve_workspace(&self.fallback_cwd)
    }

    fn cwd_for(&self, session: Option<&str>, workspace_arg: Option<&str>) -> Result<PathBuf, String> {
        host::resolve_call_workspace(&self.fallback_cwd, session, workspace_arg)
    }

    async fn bridge(&self) -> Result<ToolBridge, String> {
        self.bridge_for("", self.workspace()).await
    }
    async fn bridge_for(&self, session: &str, cwd: PathBuf) -> Result<ToolBridge, String> {
        let key = (session.to_string(), cwd.clone());
        {
            let cache = self.cached.lock().await;
            if let Some(bridge) = cache.get(&key) {
                let bridge = bridge.clone();
                drop(cache);
                self.touch_session(session).await;
                return Ok(bridge);
            }
        }
        let (backend, is_new) = {
            let mut backends = self.backends.lock().await;
            let exists = backends.contains_key(session);
            let entry = backends
                .entry(session.to_string())
                .or_insert_with(|| (Arc::new(LocalTerminalBackend::new()), Instant::now()));
            entry.1 = Instant::now();
            (entry.0.clone(), !exists)
        };
        let bridge = match host::build_bridge_with_backend(cwd, backend).await {
            Ok(b) => b,
            Err(e) => {
                if is_new {
                    let mut backends = self.backends.lock().await;
                    backends.remove(session);
                }
                return Err(e);
            }
        };
        {
            let mut cache = self.cached.lock().await;
            cache.insert(key, bridge.clone());
        }
        self.evict_sessions_over_cap(session).await;
        Ok(bridge)
    }

    async fn touch_session(&self, session: &str) {
        let mut backends = self.backends.lock().await;
        if let Some(entry) = backends.get_mut(session) {
            entry.1 = Instant::now();
        }
    }

    fn enter_inflight(&self, session: &str) -> InflightGuard<'_> {
        let mut counts = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
        *counts.entry(session.to_string()).or_insert(0) += 1;
        InflightGuard {
            counts: &self.inflight,
            session: session.to_string(),
        }
    }

    fn is_inflight(&self, session: &str) -> bool {
        self.inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session)
            .copied()
            .unwrap_or(0)
            > 0
    }

    /// True when the session backend still owns a running background task.

    /// Reclaim idle sessions past the cap, oldest-inactive first. Removes the
    /// victim's cached bridges and backend together. Immune: the session being
    /// served, any session with an in-flight call, and any session with a live
    /// background task. Holding a cached bridge is deliberately NOT immunity —
    /// idle cached sessions are what the cap reclaims.
    /// Neither `cached` nor `backends` mutex is held across `list_tasks().await`.
    async fn evict_sessions_over_cap(&self, current: &str) {
        let victim_candidates = {
            let backends = self.backends.lock().await;
            if backends.len() <= MAX_SESSION_BACKENDS {
                return;
            }
            let inflight = self.inflight.lock().unwrap_or_else(|e| e.into_inner());
            let victims = session_eviction_order(
                backends.iter().map(|(s, (_, t))| (s.clone(), *t)).collect(),
                &inflight,
                current,
                backends.len() - MAX_SESSION_BACKENDS,
            );
            victims
                .into_iter()
                .filter_map(|v| backends.get(&v).map(|(b, _)| (v, b.clone())))
                .collect::<Vec<_>>()
        };
        for (victim, backend) in victim_candidates {
            if victim == current || self.is_inflight(&victim) {
                continue;
            }
            if backend.list_tasks().await.iter().any(|t| !t.completed) {
                continue;
            }
            if victim == current || self.is_inflight(&victim) {
                continue;
            }
            let mut cache = self.cached.lock().await;
            let mut backends = self.backends.lock().await;
            if backends.len() <= MAX_SESSION_BACKENDS {
                break;
            }
            if victim == current || self.is_inflight(&victim) {
                continue;
            }
            cache.retain(|(s, _), _| s != &victim);
            backends.remove(&victim);
        }
    }

    async fn drop_session_cache(&self, session: &str) {
        let mut cache = self.cached.lock().await;
        cache.retain(|(s, _), _| s != session);
    }

    fn workspace_info_result(&self, session: Option<&str>, workspace_arg: Option<&str>) -> Value {
        let cwd = match self.cwd_for(session, workspace_arg) {
            Ok(p) => p,
            Err(e) => {
                return json!({
                    "content": [{ "type": "text", "text": e }],
                    "isError": true
                });
            }
        };
        let mut lines = vec![format!("default workspace: {}", cwd.display())];
        match session {
            Some(id) => lines.push(format!("session: {id} (this chat only)")),
            None => lines.push(
                "session: (none - this chat shares the CLI pin; pass workspace on later calls)"
                    .into(),
            ),
        }
        lines.push(
            "note: Explicit absolute targets or commands with an explicit workdir execute in their specified target without changing this default workspace.".into(),
        );
        let recent: Vec<String> = host::read_recent()
            .into_iter()
            .filter(|p| p != &cwd)
            .map(|p| p.display().to_string())
            .collect();
        if recent.is_empty() {
            lines.push("recent: (none)".into());
        } else {
            lines.push("recent:".into());
            for p in &recent {
                lines.push(format!("  {p}"));
            }
        }
        lines.push(
            "Switch from chat with set_workspace({path}). Short names resolve under ~/Dev.".into(),
        );
        json!({
            "content": [{ "type": "text", "text": lines.join("\n") }],
            "structuredContent": {
                "workspace": cwd.display().to_string(),
                "default_workspace": cwd.display().to_string(),
                "is_default": true,
                "session": session,
                "recent": recent,
            },
            "isError": false
        })
    }

    async fn switch_workspace(&self, session: Option<&str>, raw: &str) -> Result<PathBuf, String> {
        // The whole switch runs under the session in-flight marker: between
        // dropping the cached bridges and returning, an over-cap eviction from
        // another session must not reclaim this session's backend, or running
        // tasks and completed history would be lost mid-switch (Issue #62
        // stories 6-8). Same lifecycle as normal terminal/tool calls.
        let _inflight = self.enter_inflight(session.unwrap_or(""));
        let path = host::resolve_project(raw)?;
        let cwd = host::pin_for_chat(session, &path)?;
        self.drop_session_cache(session.unwrap_or("")).await;
        Ok(cwd)
    }

    pub async fn serve_stdio(self: Arc<Self>) -> Result<(), String> {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines
            .next_line()
            .await
            .map_err(|e| format!("stdin: {e}"))?
        {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let msg: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(e) => {
                    let err = rpc_error(Value::Null, -32700, format!("parse error: {e}"));
                    write_line(&mut stdout, &err).await?;
                    continue;
                }
            };
            if let Some(resp) = self.handle_rpc(msg).await {
                write_line(&mut stdout, &resp).await?;
            }
        }
        Ok(())
    }

    pub async fn serve_http(self: Arc<Self>, addr: SocketAddr) -> Result<(), String> {
        let warm = Arc::clone(&self);
        tokio::spawn(async move {
            if let Err(e) = warm.bridge().await {
                eprintln!("warmup: {e}");
            }
        });
        #[cfg(unix)]
        {
            let sock = host::mcp_socket();
            if let Some(parent) = sock.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::remove_file(&sock);
            let uds = UnixListener::bind(&sock)
                .map_err(|e| format!("bind {}: {e}", sock.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
            }
            eprintln!("MCP uds  {}", sock.display());
            let host_u = Arc::clone(&self);
            tokio::spawn(async move {
                loop {
                    match uds.accept().await {
                        Ok((stream, _)) => {
                            let host = Arc::clone(&host_u);
                            tokio::spawn(async move {
                                let (r, w) = stream.into_split();
                                if let Err(e) =
                                    handle_connection(BufReader::new(r), w, host).await
                                {
                                    eprintln!("uds: {e}");
                                }
                            });
                        }
                        Err(e) => eprintln!("uds accept: {e}"),
                    }
                }
            });
        }
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind {addr}: {e}"))?;
        eprintln!("Hands UI  http://{addr}/");
        eprintln!("MCP       http://{addr}/mcp");
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("accept: {e}"))?;
            let host = Arc::clone(&self);
            tokio::spawn(async move {
                let (r, w) = stream.into_split();
                if let Err(e) = handle_connection(BufReader::new(r), w, host).await {
                    eprintln!("http: {e}");
                }
            });
        }
    }

    pub async fn handle_rpc(&self, msg: Value) -> Option<Value> {
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let Some(id) = msg.get("id").cloned() else {
            return None;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        let result = match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => self.tools_list().await,
            "tools/call" => self.tools_call(params).await,
            "skills/list" => Ok(plugin::skills_list()),
            "skills/get" => plugin::skills_get(&params),
            "resources/list" => Ok(plugin::resources_list()),
            "resources/read" => plugin::resources_read(&params),
            other => Err((
                -32601,
                format!("method not found: {other}"),
                Value::Null,
            )),
        };

        Some(match result {
            Ok(value) => json!({"jsonrpc": "2.0", "id": id, "result": value}),
            Err((code, message, data)) => rpc_error_with_data(id, code, message, data),
        })
    }

    fn initialize(&self, params: Value) -> Value {
        let client_version = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(PROTOCOL_VERSION);
        json!({
            "protocolVersion": client_version,
            "capabilities": plugin::initialize_capabilities(),
            "serverInfo": {
                "name": SERVER_NAME,
                "version": format!("{}+{}.{}", SERVER_VERSION, host::UPSTREAM_BASE_COMMIT, host::DEV_GIT_REV),
            },
            "instructions": plugin::initialize_instructions(
                &self.workspace().display().to_string()
            ),
        })
    }

    async fn tools_list(&self) -> Result<Value, (i64, String, Value)> {
        let mut tools = vec![
            plugin::tool_descriptor(
                "workspace_info",
                "Use this to inspect the default Workspace root and recently used folders. Relative operations resolve against this default Workspace; explicit paths/workdirs target their specified location.",
                json!({ "type": "object", "properties": {} }),
            ),
            plugin::tool_descriptor(
                "set_workspace",
                "Use this when the user wants another repo, including while they are not at the machine. Pins the workspace for THIS ChatGPT conversation only. Other chats keep their folder. Accepts an absolute path, ~/path, or a short name resolved under ~/Dev (e.g. bunko).",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Directory to pin: absolute, ~/…, or folder name under ~/Dev"
                        }
                    },
                    "required": ["path"]
                }),
            ),
            plugin::tool_descriptor(
                "list_terminal_tasks",
                "List all running and completed background terminal tasks in the current session. Returns task IDs, commands, status, exit codes, and output metadata.",
                json!({
                    "type": "object",
                    "properties": {}
                }),
            ),
            run_command::tool_descriptor(),
        ];
        let defs = self
            .bridge()
            .await
            .map_err(|e| (-32603, e, Value::Null))?
            .tool_definitions()
            .await;
        tools.extend(defs.into_iter().map(|d| {
            let name = d.function.name;
            let description = d.function.description.unwrap_or_default();
            plugin::tool_descriptor(&name, &description, d.function.parameters)
        }));
        Ok(json!({ "tools": tools }))
    }

    async fn tools_call(&self, params: Value) -> Result<Value, (i64, String, Value)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call requires name".into(), Value::Null))?;
        let session = host::openai_session(&params);
        note_chat_session(session.as_deref());
        let mut arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let workspace_arg = take_workspace_arg(&mut arguments);

        if name == "workspace_info" {
            return Ok(self.workspace_info_result(session.as_deref(), workspace_arg.as_deref()));
        }
        if name == "set_workspace" {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .ok_or((-32602, "set_workspace requires path".into(), Value::Null))?;
            return match self.switch_workspace(session.as_deref(), path).await {
                Ok(cwd) => {
                    let isolated = session.is_some();
                    let extra = if isolated {
                        "Pinned for this ChatGPT conversation only. Other chats keep their folder."
                    } else {
                        "Host sent no openai/session — pinned globally. Pass workspace on later calls so other chats do not share this folder."
                    };
                    Ok(json!({
                        "content": [{
                            "type": "text",
                            "text": format!("workspace pinned: {}\n{extra}\nRelative operations resolve against this default workspace; explicit paths and workdirs target their specified locations.", cwd.display())
                        }],
                        "structuredContent": {
                            "workspace": cwd.display().to_string(),
                            "default_workspace": cwd.display().to_string(),
                            "session": session,
                            "isolated": isolated
                        },
                        "isError": false
                    }))
                }
                Err(e) => Ok(json!({
                    "content": [{ "type": "text", "text": e }],
                    "isError": true
                })),
            };
        }
        let cwd = self
            .cwd_for(session.as_deref(), workspace_arg.as_deref())
            .map_err(|e| (-32602, e, Value::Null))?;
        // Every path below may use the session terminal backend (list/get/kill
        // tasks, run_terminal_cmd). Hold the in-flight marker across the whole
        // dispatch so an over-cap scan never evicts this session mid-call.
        let _inflight = self.enter_inflight(session.as_deref().unwrap_or(""));

        if name == "list_terminal_tasks" {
            let bridge = self
                .bridge_for(session.as_deref().unwrap_or(""), cwd.clone())
                .await
                .map_err(|e| (-32603, e, Value::Null))?;
            let tasks = bridge.list_background_tasks().await;

            let mut projected = Vec::new();
            let mut summary_lines = Vec::new();

            for t in tasks {
                let status = if t.completed {
                    if t.explicitly_killed {
                        "cancelled"
                    } else if t.signal.as_deref() == Some("timeout") {
                        "timed_out"
                    } else if t.exit_code == Some(0) {
                        "completed"
                    } else {
                        "failed"
                    }
                } else {
                    "running"
                };
                let raw_summary = if let Some(desc) = t.description.as_deref().filter(|d| !d.trim().is_empty()) {
                    desc
                } else if let Some(display) = t.display_command.as_deref().filter(|d| !d.trim().is_empty()) {
                    display
                } else {
                    &t.command
                };
                let bounded_summary = if raw_summary.len() > 120 {
                    let boundary = raw_summary.floor_char_boundary(117);
                    format!("{}...", &raw_summary[..boundary])
                } else {
                    raw_summary.to_string()
                };

                summary_lines.push(format!(
                    "- ID: {}\n  Status: {}\n  Command: {}\n  Exit Code: {:?}",
                    t.task_id, status, bounded_summary, t.exit_code
                ));

                projected.push(json!({
                    "task_id": t.task_id,
                    "status": status,
                    "command": bounded_summary,
                    "cwd": t.cwd,
                    "exit_code": t.exit_code,
                    "output_file": t.output_file.display().to_string(),
                    "duration_secs": t.duration_secs(),
                    "completed": t.completed,
                    "truncated": t.truncated,
                    "total_bytes": t.output_total_bytes,
                }));
            }

            let text = if projected.is_empty() {
                "Total tasks: 0".to_string()
            } else {
                format!("Total tasks: {}\n{}", projected.len(), summary_lines.join("\n"))
            };

            return Ok(json!({
                "content": [{ "type": "text", "text": text }],
                "structuredContent": {
                    "tasks": projected
                },
                "isError": false
            }));
        }
        if name == run_command::TOOL_NAME {
            let res = run_command::execute(&arguments, &cwd).await;
            return Ok(res);
        }
        let call_id = format!(
            "mcp-{}",
            self.call_seq.fetch_add(1, Ordering::Relaxed)
        );
        let bridge = self
            .bridge_for(session.as_deref().unwrap_or(""), cwd.clone())
            .await
            .map_err(|e| (-32603, e, Value::Null))?;
        match bridge.call(name, arguments, &call_id).await {
            Ok(result) => {
                let mut edit_result = edit::mcp_result(
                    &result.output,
                    &result.prompt_text,
                    &cwd,
                );
                if let Some(edit_structured) = edit_result.get("structuredContent").cloned() {
                    let (mut structured, _) = shape_tool_result(&result);
                    if let (Some(dst), Some(src)) =
                        (structured.as_object_mut(), edit_structured.as_object())
                    {
                        for (key, value) in src {
                            dst.insert(key.clone(), value.clone());
                        }
                    } else {
                        structured = edit_structured;
                    }
                    edit_result["structuredContent"] =
                        enrich_context_metadata(structured, &result, &cwd);
                    return Ok(edit_result);
                }
                let is_error = result.output.is_error();
                let (structured, summary_text) = shape_tool_result(&result);
                let structured = enrich_context_metadata(structured, &result, &cwd);
                Ok(json!({
                    "content": [{ "type": "text", "text": summary_text }],
                    "structuredContent": structured,
                    "isError": is_error
                }))
            }
            Err(e) => Ok(json!({
                "content": [{ "type": "text", "text": e.to_string() }],
                "isError": true
            })),
        }
    }
}

fn enrich_context_metadata(mut structured: Value, result: &ToolRunResult, default_ws: &Path) -> Value {
    if let Some(obj) = structured.as_object_mut() {
        let ws_str = default_ws.display().to_string();
        obj.insert("default_workspace".to_string(), Value::String(ws_str));

        match &result.output {
            ToolOutput::Bash(b) => {
                obj.insert("cwd".to_string(), Value::String(b.current_dir.clone()));
            }
            ToolOutput::ReadFile(rf) => {
                if let xai_grok_tools::types::output::ReadFileOutput::FileContent(fc) = rf {
                    obj.insert(
                        "target_path".to_string(),
                        Value::String(fc.absolute_path.display().to_string()),
                    );
                }
            }
            ToolOutput::SearchReplace(xai_grok_tools::types::output::SearchReplaceOutput::EditsApplied(ea)) => {
                obj.insert(
                    "target_path".to_string(),
                    Value::String(ea.absolute_path.display().to_string()),
                );
            }
            ToolOutput::ListDir(xai_grok_tools::types::output::ListDirOutput::Content(ldc)) => {
                obj.insert(
                    "target_path".to_string(),
                    Value::String(ldc.absolute_root_path.display().to_string()),
                );
            }
            _ => {}
        }
    }
    structured
}

#[inline]
fn kb(n: usize) -> usize {
    (n + 1023) / 1024
}

pub fn truncate_output_text(text: &str, max_bytes: usize, output_file: &str) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let half = max_bytes / 2;
    let head_boundary = text.floor_char_boundary(half);
    let head = &text[..head_boundary];
    let tail_start = text.ceil_char_boundary(text.len().saturating_sub(half));
    let tail = &text[tail_start..];
    let total_kb = kb(text.len());
    let head_kb = kb(head_boundary);
    let tail_kb = kb(text.len() - tail_start);
    let file_hint = if !output_file.is_empty() {
        format!(" Full output saved to {output_file}.")
    } else {
        String::new()
    };
    format!("{head}\n\n[Output truncated: showing first {head_kb}KB and last {tail_kb}KB of {total_kb}KB.{file_hint}]\n\n{tail}")
}

pub fn shape_tool_result(result: &ToolRunResult) -> (Value, String) {
    let mut structured = serde_json::to_value(&result.output).unwrap_or_else(|_| json!({}));
    let output_file: &str = match &result.output {
        ToolOutput::Bash(b) => {
            let output_str = String::from_utf8_lossy(&b.output).into_owned();
            if let Some(obj) = structured.as_object_mut() {
                obj.insert("output".to_string(), Value::String(output_str));
            }
            &b.output_file
        }
        ToolOutput::BackgroundTaskStarted(bg) => &bg.output_file,
        ToolOutput::TaskOutput(to) => match to {
            xai_tool_types::TaskOutputOutput::Result(r) => {
                structured = json!({
                    "type": "TaskOutput",
                    "task_id": r.task_id,
                    "command": r.command,
                    "status": r.status,
                    "exit_code": r.exit_code,
                    "duration_secs": r.duration_secs,
                    "output": r.output,
                    "output_file": r.output_file,
                    "truncated": r.truncated,
                    "raw_output_bytes": r.raw_output_bytes
                });
                &r.output_file
            }
            xai_tool_types::TaskOutputOutput::TaskNotFound(msg) => {
                structured = json!({
                    "type": "TaskOutput",
                    "error": msg
                });
                ""
            }
            _ => "",
        },
        _ => "",
    };
    let summary = if result.prompt_text.trim().is_empty() {
        match &result.output {
            ToolOutput::Bash(b) if b.total_bytes > 0 => {
                format!("(output captured in file, exit code: {})", b.exit_code)
            }
            ToolOutput::BackgroundTaskStarted(bg) => {
                format!("Background task started with ID: {}. Output streaming to {}.", bg.task_id, bg.output_file)
            }
            _ => result.prompt_text.clone(),
        }
    } else {
        truncate_output_text(&result.prompt_text, 256, output_file)
    };

    (structured, summary)
}

fn take_workspace_arg(arguments: &mut Value) -> Option<String> {
    let obj = arguments.as_object_mut()?;
    let raw = obj.remove("workspace")?;
    let s = raw.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

fn note_chat_session(session: Option<&str>) {
    static LOGGED: AtomicBool = AtomicBool::new(false);
    if LOGGED.swap(true, Ordering::Relaxed) {
        return;
    }
    match session {
        Some(s) => {
            let show: String = s.chars().take(16).collect();
            eprintln!("Hands chat session {show} (per-conversation workspace)");
        }
        None => {
            eprintln!("Hands chat session: none — CLI pin / workspace arg");
        }
    }
}

fn rpc_error(id: Value, code: i64, message: String) -> Value {
    rpc_error_with_data(id, code, message, Value::Null)
}

fn rpc_error_with_data(id: Value, code: i64, message: String, data: Value) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if !data.is_null() {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

async fn write_line(stdout: &mut tokio::io::Stdout, value: &Value) -> Result<(), String> {
    let mut line = serde_json::to_string(value).map_err(|e| e.to_string())?;
    line.push('\n');
    stdout
        .write_all(line.as_bytes())
        .await
        .map_err(|e| format!("stdout: {e}"))?;
    stdout.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

async fn handle_connection<R, W>(
    mut reader: BufReader<R>,
    mut writer: W,
    host: Arc<McpHost>,
) -> Result<(), String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    loop {
        let mut header_buf = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader
                .read_line(&mut line)
                .await
                .map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                return Ok(());
            }
            header_buf.extend_from_slice(line.as_bytes());
            if line == "\r\n" || line == "\n" {
                break;
            }
            if header_buf.len() > 64 * 1024 {
                write_http(&mut writer, 431, "text/plain", b"headers too large", false)
                    .await?;
                return Ok(());
            }
        }
        let header_text = String::from_utf8_lossy(&header_buf);
        let mut lines = header_text.split("\r\n");
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("/");
        let version = parts.next().unwrap_or("HTTP/1.1");

        let mut content_length = 0usize;
        let mut accept = String::new();
        let mut connection = String::new();
        for line in lines {
            let Some((k, v)) = line.split_once(':') else {
                continue;
            };
            let k = k.trim();
            let v = v.trim();
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            } else if k.eq_ignore_ascii_case("accept") {
                accept = v.to_string();
            } else if k.eq_ignore_ascii_case("connection") {
                connection = v.to_string();
            }
        }
        let keep = if connection.eq_ignore_ascii_case("close") {
            false
        } else if connection.eq_ignore_ascii_case("keep-alive") {
            true
        } else {
            version.eq_ignore_ascii_case("HTTP/1.1")
        };

        if content_length > 8 * 1024 * 1024 {
            write_http(&mut writer, 413, "text/plain", b"body too large", false).await?;
            return Ok(());
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader
                .read_exact(&mut body)
                .await
                .map_err(|e| format!("body: {e}"))?;
        }

        let path_only = path.split('?').next().unwrap_or(path);
        if method == "GET" && (path_only == "/health" || path_only == "/healthz") {
            write_http(&mut writer, 200, "text/plain", b"ok", keep).await?;
            if !keep {
                return Ok(());
            }
            continue;
        }
        if path_only.contains("/.well-known/")
            || (method == "GET" && path_only == "/" && !accept.to_lowercase().contains("text/html"))
        {
            write_http(
                &mut writer,
                404,
                "application/json",
                br#"{"error":"not_found"}"#,
                keep,
            )
            .await?;
            if !keep {
                return Ok(());
            }
            continue;
        }
        if let Some((status, ctype, payload)) = ui::route(method, path_only, &body) {
            write_http(&mut writer, status, ctype, &payload, keep).await?;
            if !keep {
                return Ok(());
            }
            continue;
        }
        if method != "POST" || path_only != "/mcp" {
            write_http(&mut writer, 404, "text/plain", b"not found", keep).await?;
            if !keep {
                return Ok(());
            }
            continue;
        }
        let resp = match serde_json::from_slice::<Value>(&body) {
            Ok(msg) => host
                .handle_rpc(msg)
                .await
                .unwrap_or_else(|| json!({"jsonrpc": "2.0", "id": null, "result": {}})),
            Err(e) => rpc_error(Value::Null, -32700, format!("parse error: {e}")),
        };
        let payload = serde_json::to_vec(&resp).map_err(|e| e.to_string())?;
        if accept.contains("text/event-stream") && !accept.contains("application/json") {
            let mut sse = Vec::from("event: message\ndata: ");
            sse.extend_from_slice(&payload);
            sse.extend_from_slice(b"\n\n");
            write_http(&mut writer, 200, "text/event-stream", &sse, keep).await?;
        } else {
            write_http(&mut writer, 200, "application/json", &payload, keep).await?;
        }
        if !keep {
            return Ok(());
        }
    }
}

async fn write_http<W: AsyncWrite + Unpin>(
    writer: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
    keep_alive: bool,
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let conn = if keep_alive {
        "keep-alive"
    } else {
        "close"
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: {conn}\r\n\r\n",
        body.len()
    );
    writer
        .write_all(header.as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    writer.write_all(body).await.map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::Duration;
    use tempfile::TempDir;

    struct EnvGuard {
        var: &'static str,
        prev: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(var: &'static str, val: impl AsRef<std::ffi::OsStr>) -> Self {
            let prev = std::env::var_os(var);
            unsafe {
                std::env::set_var(var, val);
            }
            Self { var, prev }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => unsafe {
                    std::env::set_var(self.var, v);
                },
                None => unsafe {
                    std::env::remove_var(self.var);
                },
            }
        }
    }
    fn aged_sessions(n: usize) -> Vec<(String, Instant)> {
        // s0 oldest, s{n-1} newest; deterministic without sleeps.
        let now = Instant::now();
        (0..n)
            .map(|i| {
                (
                    format!("s{i}"),
                    now.checked_sub(Duration::from_secs((n - i) as u64))
                        .unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn eviction_order_reclaims_oldest_inactive_first() {
        let sessions = aged_sessions(70);
        let victims = session_eviction_order(sessions, &HashMap::new(), "s69", 6);
        let expected: Vec<String> = (0..6).map(|i| format!("s{i}")).collect();
        assert_eq!(victims, expected);
    }

    #[test]
    fn eviction_order_spares_current_and_inflight_not_cached() {
        // Holding cached bridges is deliberately NOT immunity: only the served
        // session and sessions with an in-flight call are spared.
        let sessions = aged_sessions(10);
        let mut inflight = HashMap::new();
        inflight.insert("s0".to_string(), 1);
        let victims = session_eviction_order(sessions, &inflight, "s9", 3);
        let expected: Vec<String> = ["s1", "s2", "s3"].iter().map(|s| s.to_string()).collect();
        assert_eq!(victims, expected);
    }

    #[tokio::test]
    async fn evict_over_cap_reclaims_idle_keeps_guarded_and_current() {
        let host = McpHost::new(std::env::temp_dir());
        // Fabricate 70 idle backends directly: no bridge builds, same maps.
        {
            let mut backends = host.backends.lock().await;
            let now = Instant::now();
            for i in 0..70 {
                backends.insert(
                    format!("old{i}"),
                    (
                        Arc::new(LocalTerminalBackend::new()),
                        now.checked_sub(Duration::from_secs((70 - i) as u64))
                            .unwrap(),
                    ),
                );
            }
        }
        // Oldest session has an in-flight call; newest is being served.
        let _guard = host.enter_inflight("old0");
        host.evict_sessions_over_cap("old69").await;
        let backends = host.backends.lock().await;
        assert!(
            backends.len() <= MAX_SESSION_BACKENDS,
            "over-cap backends must be reclaimed, len={}",
            backends.len()
        );
        assert!(backends.contains_key("old0"), "in-flight oldest retained");
        assert!(backends.contains_key("old69"), "served newest retained");
        assert!(!backends.contains_key("old1"), "oldest unguarded evicted");
    }

    #[tokio::test]
    #[serial]
    async fn bridge_for_enforces_cap_across_many_sessions() {
        // End-to-end wiring: 70 real session bridges collapse to the cap, with
        // both maps bounded and the newest session retained.
        let cfg = TempDir::new().unwrap();
        let _env = EnvGuard::set("HANDS_CONFIG_DIR", cfg.path());
        let dir = TempDir::new().unwrap();
        let host = McpHost::new(dir.path().to_path_buf());
        for i in 0..70 {
            host.bridge_for(&format!("cap{i}"), dir.path().to_path_buf())
                .await
                .expect("session bridge build");
        }
        assert!(
            host.backends.lock().await.len() <= MAX_SESSION_BACKENDS,
            "backends must stay bounded"
        );
        assert!(
            host.cached.lock().await.len() <= MAX_SESSION_BACKENDS,
            "cached bridges must stay bounded"
        );
        assert!(
            host.backends.lock().await.contains_key("cap69"),
            "newest retained"
        );
        assert!(
            !host.backends.lock().await.contains_key("cap0"),
            "oldest reclaimed"
        );

    }

    #[tokio::test]
    #[serial]
    async fn switch_workspace_keeps_session_backend_identity() {
        // Issue #62 stories 6-8: set_workspace must not replace the session's
        // terminal backend. Identity proven by Arc pointer equality.
        let cfg = TempDir::new().unwrap();
        let _env = EnvGuard::set("HANDS_CONFIG_DIR", cfg.path());
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let host = McpHost::new(dir_a.path().to_path_buf());
        let before = host
            .bridge_for("sw", dir_a.path().to_path_buf())
            .await
            .expect("initial bridge");
        drop(before);
        let ptr_before = {
            let backends = host.backends.lock().await;
            Arc::as_ptr(&backends["sw"].0)
        };
        let pinned = host
            .switch_workspace(Some("sw"), dir_b.path().to_str().unwrap())
            .await
            .expect("switch workspace");
        assert_eq!(pinned, dunce::canonicalize(dir_b.path()).unwrap());
        let after = host
            .bridge_for("sw", pinned.clone())
            .await
            .expect("post-switch bridge");
        drop(after);
        let ptr_after = {
            let backends = host.backends.lock().await;
            Arc::as_ptr(&backends["sw"].0)
        };
        assert_eq!(
            ptr_before, ptr_after,
            "backend identity must survive set_workspace"
        );

    }

    fn set_old(
        backends: &mut HashMap<String, (Arc<LocalTerminalBackend>, Instant)>,
        s: &str,
        secs: u64,
    ) {
        let old = Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .unwrap();
        backends.get_mut(s).expect("session backend").1 = old;
    }

    fn fill_idle(
        backends: &mut HashMap<String, (Arc<LocalTerminalBackend>, Instant)>,
        n: usize,
        secs: u64,
    ) {
        let old = Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .unwrap();
        for i in 0..n {
            backends.insert(
                format!("fill{i}"),
                (Arc::new(LocalTerminalBackend::new()), old),
            );
        }
    }

    #[tokio::test]
    #[serial]
    async fn switch_window_eviction_hole_and_guard() {
        // Fix 1: `switch_workspace` holds the session in-flight marker across
        // `drop_session_cache`, closing the window where an over-cap scan
        // could reclaim the switching session's backend. Proved in three
        // phases: the unguarded window is genuinely catchable, the same guard
        // the fixed switch holds retains the backend, and release reclaims.
        let cfg = TempDir::new().unwrap();
        let _env = EnvGuard::set("HANDS_CONFIG_DIR", cfg.path());
        let dir_a = TempDir::new().unwrap();
        let host = McpHost::new(dir_a.path().to_path_buf());
        host.bridge_for("sw", dir_a.path().to_path_buf())
            .await
            .expect("build");
        let ptr = { Arc::as_ptr(&host.backends.lock().await["sw"].0) };
        {
            let mut backends = host.backends.lock().await;
            set_old(&mut backends, "sw", 7200);
            fill_idle(&mut backends, 70, 3600);
        }
        // Phase A: unguarded drop (what the old switch did) loses the backend.
        host.drop_session_cache("sw").await;
        host.evict_sessions_over_cap("fill0").await;
        assert!(
            !host.backends.lock().await.contains_key("sw"),
            "unguarded switch window must be catchable by over-cap eviction"
        );
        // Phase B: rebuild, then evict with the switch guard held — retained.
        host.bridge_for("sw", dir_a.path().to_path_buf())
            .await
            .expect("rebuild");
        let ptr_b = { Arc::as_ptr(&host.backends.lock().await["sw"].0) };
        {
            let mut backends = host.backends.lock().await;
            set_old(&mut backends, "sw", 7200);
            fill_idle(&mut backends, 70, 3600);
        }
        host.drop_session_cache("sw").await;
        let _switch_guard = host.enter_inflight("sw");
        host.evict_sessions_over_cap("fill0").await;
        {
            let backends = host.backends.lock().await;
            assert!(
                backends.contains_key("sw"),
                "guarded switch must retain backend"
            );
            assert_eq!(
                Arc::as_ptr(&backends["sw"].0),
                ptr_b,
                "guarded switch keeps identity"
            );
        }
        assert_ne!(ptr_b, ptr, "rebuild is a new backend (sanity)");
        // Phase C: guard released, session idle — reclamation still works.
        drop(_switch_guard);
        {
            let mut backends = host.backends.lock().await;
            fill_idle(&mut backends, 70, 3600);
        }
        host.evict_sessions_over_cap("fill0").await;
        assert!(
            !host.backends.lock().await.contains_key("sw"),
            "released idle session must be reclaimable"
        );
        let _ = dir_a;

    }

    #[tokio::test]
    #[serial]
    async fn switch_workspace_races_real_eviction_scans() {
        // Concurrency smoke: switches racing real over-cap scans must never
        // lose the switching backend mid-switch, panic, or deadlock. The
        // switching session stays newest here so between-switch idle eviction
        // cannot legitimately take it; the guard covers the switch itself.
        let cfg = TempDir::new().unwrap();
        let _env = EnvGuard::set("HANDS_CONFIG_DIR", cfg.path());
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        let host = McpHost::new(dir_a.path().to_path_buf());
        host.bridge_for("sw", dir_a.path().to_path_buf())
            .await
            .expect("build");
        let ptr = { Arc::as_ptr(&host.backends.lock().await["sw"].0) };
        {
            let mut backends = host.backends.lock().await;
            fill_idle(&mut backends, 70, 3600);
        }
        let dir_a_str = dir_a.path().to_str().unwrap().to_string();
        let dir_b_str = dir_b.path().to_str().unwrap().to_string();
        let host2 = Arc::clone(&host);
        let hammer = tokio::spawn(async move {
            for _ in 0..3 {
                host2.evict_sessions_over_cap("fill0").await;
            }
        });
        for i in 0..6 {
            let target = if i % 2 == 0 { &dir_b_str } else { &dir_a_str };
            let pinned = host
                .switch_workspace(Some("sw"), target)
                .await
                .expect("switch");
            // Every completed switch leaves a usable session: bridge + tasks.
            let _b = host
                .bridge_for("sw", pinned)
                .await
                .expect("post-switch bridge");
            let backends = host.backends.lock().await;
            assert_eq!(
                Arc::as_ptr(&backends["sw"].0),
                ptr,
                "identity across racing scans"
            );
        }
        hammer.await.expect("evict hammer joins");
        assert!(
            host.backends.lock().await.contains_key("sw"),
            "sw survives the race"
        );

    }

    #[tokio::test]
    #[serial]
    async fn over_cap_eviction_spares_live_task_then_reclaims() {
        // Fix 2: a session with a running background task is not inactive —
        // evicting it would drop the last backend senders and kill the task.
        // Once the task is killed/completed and the session is otherwise idle,
        // it becomes evictable and the cap converges.
        let cfg = TempDir::new().unwrap();
        let _env = EnvGuard::set("HANDS_CONFIG_DIR", cfg.path());
        let dir = TempDir::new().unwrap();
        let host = McpHost::new(dir.path().to_path_buf());
        let bridge = host
            .bridge_for("live", dir.path().to_path_buf())
            .await
            .expect("build");
        #[cfg(windows)]
        let cmd = "powershell -Command \"Start-Sleep -Seconds 60\"";
        #[cfg(not(windows))]
        let cmd = "sleep 60";
        let started = bridge
            .call(
                "run_terminal_cmd",
                json!({ "command": cmd, "description": "evict-guard probe", "is_background": true }),
                "evict-live-t1",
            )
            .await;
        assert!(started.is_ok(), "background task must start");
        let task_id = {
            let backends = host.backends.lock().await;
            let tasks = backends["live"].0.list_tasks().await;
            tasks
                .iter()
                .find(|t| !t.completed)
                .map(|t| t.task_id.clone())
                .expect("task running")
        };
        // "live" is the oldest candidate, so without immunity it would go first.
        {
            let mut backends = host.backends.lock().await;
            set_old(&mut backends, "live", 7200);
            fill_idle(&mut backends, 70, 3600);
        }
        host.evict_sessions_over_cap("fill0").await;
        {
            let backends = host.backends.lock().await;
            assert!(
                backends.contains_key("live"),
                "live-task session must survive"
            );
            assert_eq!(backends.len(), 65, "cap converges on idle fills only");
        }
        // Kill the task: completed-only history is evictable again.
        {
            let backends = host.backends.lock().await;
            backends["live"].0.kill_task(&task_id).await;
            let tasks = backends["live"].0.list_tasks().await;
            assert!(
                tasks
                    .iter()
                    .find(|t| t.task_id == task_id)
                    .is_some_and(|t| t.completed),
                "killed task must read back completed"
            );
        }
        host.evict_sessions_over_cap("fill0").await;
        {
            // Same lock order as production (cached, then backends).
            let cache = host.cached.lock().await;
            let backends = host.backends.lock().await;
            assert!(
                !backends.contains_key("live"),
                "quiet session reclaimed after task end"
            );
            assert!(backends.len() <= MAX_SESSION_BACKENDS, "cap converges");
            assert!(
                !cache.keys().any(|(s, _)| s == "live"),
                "victim bridges removed too"
            );
        }

    }

    #[tokio::test]
    #[serial]
    async fn eviction_does_not_hold_cached_lock_across_task_listing() {
        let cfg = TempDir::new().unwrap();
        let _env = EnvGuard::set("HANDS_CONFIG_DIR", cfg.path());
        let dir = TempDir::new().unwrap();
        let host = McpHost::new(dir.path().to_path_buf());
        let bridge = host
            .bridge_for("live", dir.path().to_path_buf())
            .await
            .expect("build");
        #[cfg(windows)]
        let cmd = "powershell -Command \"Start-Sleep -Seconds 60\"";
        #[cfg(not(windows))]
        let cmd = "sleep 60";
        let started = bridge
            .call(
                "run_terminal_cmd",
                json!({ "command": cmd, "description": "evict-guard probe", "is_background": true }),
                "evict-live-t2",
            )
            .await;
        assert!(started.is_ok(), "background task must start");
        let task_id = {
            let backends = host.backends.lock().await;
            let tasks = backends["live"].0.list_tasks().await;
            tasks
                .iter()
                .find(|t| !t.completed)
                .map(|t| t.task_id.clone())
                .expect("task running")
        };
        {
            let mut backends = host.backends.lock().await;
            set_old(&mut backends, "live", 7200);
            fill_idle(&mut backends, 70, 3600);
        }
        let host_clone = Arc::clone(&host);
        let evict_handle = tokio::spawn(async move {
            host_clone.evict_sessions_over_cap("fill0").await;
        });
        for _ in 0..10 {
            tokio::task::yield_now().await;
            let mut cache = host.cached.lock().await;
            cache.insert(("probe".to_string(), dir.path().to_path_buf()), bridge.clone());
        }
        evict_handle.await.expect("eviction completes");
        {
            let backends = host.backends.lock().await;
            backends["live"].0.kill_task(&task_id).await;
        }
    }
}
