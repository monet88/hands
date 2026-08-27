//! MCP JSON-RPC over stdio (newline-delimited) and Streamable HTTP POST /mcp.
//! No extra crates: ChatGPT tunnel-client speaks stdio; Inspector can use HTTP.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use xai_grok_tools::bridge::ToolBridge;

use crate::host;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "grok-harness";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

const READ_ONLY: &[&str] = &[
    "workspace_info",
    "read_file",
    "grep",
    "list_dir",
    "glob",
    "get_task_output",
];

pub struct McpHost {
    fallback_cwd: PathBuf,
    cached: Mutex<Option<(PathBuf, ToolBridge)>>,
    call_seq: AtomicU64,
}

impl McpHost {
    pub fn new(fallback_cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            fallback_cwd,
            cached: Mutex::new(None),
            call_seq: AtomicU64::new(1),
        })
    }

    fn workspace(&self) -> PathBuf {
        host::resolve_workspace(&self.fallback_cwd)
    }

    async fn bridge(&self) -> Result<ToolBridge, String> {
        let cwd = self.workspace();
        let mut cache = self.cached.lock().await;
        if let Some((path, bridge)) = cache.as_ref()
            && path == &cwd
        {
            return Ok(bridge.clone());
        }
        let bridge = host::build_bridge(cwd.clone()).await?;
        *cache = Some((cwd, bridge.clone()));
        Ok(bridge)
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
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| format!("bind {addr}: {e}"))?;
        eprintln!("MCP HTTP listening on http://{addr}/mcp");
        loop {
            let (stream, _) = listener
                .accept()
                .await
                .map_err(|e| format!("accept: {e}"))?;
            let host = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(e) = handle_http(stream, host).await {
                    eprintln!("http: {e}");
                }
            });
        }
    }

    async fn handle_rpc(&self, msg: Value) -> Option<Value> {
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
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": SERVER_VERSION,
            },
            "instructions": format!(
                "Local Grok tool harness. No model. \
                 Active workspace is selected on the machine with `grok-harness use`. \
                 Call workspace_info first to see the current root (now {}). \
                 Then read_file/grep/glob/list_dir, write/search_replace/apply_patch to edit, \
                 todo_write for plans, run_terminal_cmd to test (background + kill_task/get_task_output). \
                 After each edit, rerun the failing check.",
                self.workspace().display()
            ),
        })
    }

    async fn tools_list(&self) -> Result<Value, (i64, String, Value)> {
        let mut tools = vec![json!({
            "name": "workspace_info",
            "description": "Return the active local workspace root. Call this before other tools if the user switched repos with grok-harness use.",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": {
                "readOnlyHint": true,
                "destructiveHint": false,
                "openWorldHint": false,
            }
        })];
        let defs = self
            .bridge()
            .await
            .map_err(|e| (-32603, e, Value::Null))?
            .tool_definitions()
            .await;
        tools.extend(defs.into_iter().map(|d| {
            let name = d.function.name;
            let read_only = READ_ONLY.contains(&name.as_str());
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
        Ok(json!({ "tools": tools }))
    }

    async fn tools_call(&self, params: Value) -> Result<Value, (i64, String, Value)> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or((-32602, "tools/call requires name".into(), Value::Null))?;
        if name == "workspace_info" {
            let cwd = self.workspace();
            return Ok(json!({
                "content": [{
                    "type": "text",
                    "text": format!("workspace: {}", cwd.display())
                }],
                "isError": false
            }));
        }
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let call_id = format!(
            "mcp-{}",
            self.call_seq.fetch_add(1, Ordering::Relaxed)
        );
        let bridge = self
            .bridge()
            .await
            .map_err(|e| (-32603, e, Value::Null))?;
        match bridge.call(name, arguments, &call_id).await {
            Ok(result) => Ok(json!({
                "content": [{ "type": "text", "text": result.prompt_text }],
                "isError": false
            })),
            Err(e) => Ok(json!({
                "content": [{ "type": "text", "text": e.to_string() }],
                "isError": true
            })),
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

async fn handle_http(mut stream: TcpStream, host: Arc<McpHost>) -> Result<(), String> {
    let (reader, mut writer) = stream.split();
    let mut reader = BufReader::new(reader);
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
            return write_http(&mut writer, 431, "text/plain", b"headers too large").await;
        }
    }
    let header_text = String::from_utf8_lossy(&header_buf);
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let mut content_length = 0usize;
    let mut accept = String::new();
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
        }
    }

    if method == "GET" && (path == "/health" || path == "/healthz") {
        return write_http(&mut writer, 200, "text/plain", b"ok").await;
    }
    if method != "POST" || path != "/mcp" {
        return write_http(&mut writer, 404, "text/plain", b"not found").await;
    }
    if content_length > 8 * 1024 * 1024 {
        return write_http(&mut writer, 413, "text/plain", b"body too large").await;
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader
            .read_exact(&mut body)
            .await
            .map_err(|e| format!("body: {e}"))?;
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
        write_http(&mut writer, 200, "text/event-stream", &sse).await
    } else {
        write_http(&mut writer, 200, "application/json", &payload).await
    }
}

async fn write_http(
    writer: &mut tokio::net::tcp::WriteHalf<'_>,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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
