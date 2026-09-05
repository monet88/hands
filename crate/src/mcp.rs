//! MCP JSON-RPC over stdio (newline-delimited) and Streamable HTTP POST /mcp.
//! No extra crates: ChatGPT tunnel-client speaks stdio; Inspector can use HTTP.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::types::output::{ToolOutput, ToolRunResult};

use crate::edit;
use crate::host;
use crate::plugin;
use crate::ui;
use crate::run_command;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "Hands";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct McpHost {
    fallback_cwd: PathBuf,
    cached: Mutex<HashMap<(String, PathBuf), ToolBridge>>,
    call_seq: AtomicU64,
}

impl McpHost {
    pub fn new(fallback_cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            fallback_cwd,
            cached: Mutex::new(HashMap::new()),
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
        let mut cache = self.cached.lock().await;
        if let Some(bridge) = cache.get(&key) {
            return Ok(bridge.clone());
        }
        let bridge = host::build_bridge(cwd).await?;
        cache.insert(key, bridge.clone());
        Ok(bridge)
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

    async fn switch_workspace(
        &self,
        session: Option<&str>,
        raw: &str,
    ) -> Result<PathBuf, String> {
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
