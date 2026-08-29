//! MCP JSON-RPC over stdio (newline-delimited) and Streamable HTTP POST /mcp.
//! No extra crates: ChatGPT tunnel-client speaks stdio; Inspector can use HTTP.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::tool_engine::ToolEngine;
use crate::ui;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SERVER_NAME: &str = "Hands";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

pub struct McpHost {
    engine: Arc<ToolEngine>,
}

impl McpHost {
    pub fn new(fallback_cwd: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            engine: Arc::new(ToolEngine::new(fallback_cwd)),
        })
    }

    pub async fn serve_stdio(self: Arc<Self>) -> Result<(), String> {
        let stdin = BufReader::new(tokio::io::stdin());
        let mut lines = stdin.lines();
        let mut stdout = tokio::io::stdout();
        while let Some(line) = lines.next_line().await.map_err(|e| format!("stdin: {e}"))? {
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
        eprintln!("Hands UI  http://{addr}/");
        eprintln!("MCP       http://{addr}/mcp");
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
            other => Err((-32601, format!("method not found: {other}"), Value::Null)),
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
                "Hands: unofficial local coding tools for ChatGPT. No model. \
                 Workspace is set on the machine with `hands use` or the config UI. \
                 Call workspace_info first (now {}). \
                 Then read_file/grep/glob/list_dir, write/search_replace/apply_patch to edit, \
                 todo_write for plans, run_terminal_cmd to test (background + kill_task/get_task_output). \
                 After each edit, rerun the failing check.",
                self.engine.workspace().display()
            ),
        })
    }

    async fn tools_list(&self) -> Result<Value, (i64, String, Value)> {
        let tools = self
            .engine
            .list_tools()
            .await
            .map_err(|e| (-32603, e, Value::Null))?;
        Ok(json!({ "tools": tools }))
    }

    async fn tools_call(&self, params: Value) -> Result<Value, (i64, String, Value)> {
        let name = params.get("name").and_then(Value::as_str).ok_or((
            -32602,
            "tools/call requires name".into(),
            Value::Null,
        ))?;
        let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
        let result = self
            .engine
            .call_tool(name, arguments)
            .await
            .map_err(|e| (-32603, e, Value::Null))?;
        Ok(result.to_value())
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

    let path_only = path.split('?').next().unwrap_or(path);
    if method == "GET" && (path_only == "/health" || path_only == "/healthz") {
        return write_http(&mut writer, 200, "text/plain", b"ok").await;
    }
    if let Some((status, ctype, payload)) = ui::route(method, path_only, &body) {
        return write_http(&mut writer, status, ctype, &payload).await;
    }
    if method != "POST" || path_only != "/mcp" {
        return write_http(&mut writer, 404, "text/plain", b"not found").await;
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
        400 => "Bad Request",
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
            "hands_mcp_test_{}_{}_{}",
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
    async fn test_mcp_host_initialize_and_ping() {
        let (_lock, _guard) = isolate_env("init_ping");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let ping_msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "ping"
        });
        let ping_resp = host.handle_rpc(ping_msg).await.expect("ping response");
        assert_eq!(ping_resp["id"], 1);
        assert_eq!(ping_resp["result"], json!({}));

        let init_msg = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18"
            }
        });
        let init_resp = host.handle_rpc(init_msg).await.expect("init response");
        assert_eq!(init_resp["id"], 2);
        assert_eq!(init_resp["result"]["serverInfo"]["name"], "Hands");
        assert_eq!(init_resp["result"]["protocolVersion"], "2025-06-18");
    }

    #[tokio::test]
    async fn test_mcp_host_tools_list_and_call() {
        let (_lock, _guard) = isolate_env("list_call");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let list_msg = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list"
        });
        let list_resp = host.handle_rpc(list_msg).await.expect("tools/list response");
        assert_eq!(list_resp["id"], 3);
        assert!(list_resp["result"]["tools"].as_array().is_some());

        let call_msg = json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "workspace_info"
            }
        });
        let call_resp = host.handle_rpc(call_msg).await.expect("tools/call response");
        assert_eq!(call_resp["id"], 4);
        assert_eq!(call_resp["result"]["isError"], false);
    }

    #[tokio::test]
    async fn test_mcp_host_tools_call_missing_name_returns_error_code() {
        let (_lock, _guard) = isolate_env("missing_name");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let call_msg = json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {}
        });
        let call_resp = host.handle_rpc(call_msg).await.expect("tools/call response");
        assert_eq!(call_resp["id"], 5);
        assert_eq!(call_resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn test_mcp_host_tools_call_unknown_tool_returns_is_error() {
        let (_lock, _guard) = isolate_env("unknown_tool");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let call_msg = json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "unknown_tool_test"
            }
        });
        let call_resp = host.handle_rpc(call_msg).await.expect("tools/call response");
        assert_eq!(call_resp["id"], 6);
        assert_eq!(call_resp["result"]["isError"], true);
    }

    #[tokio::test]
    async fn test_mcp_http_serve_smoke() {
        let (_lock, _guard) = isolate_env("http_smoke");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");

        let host = McpHost::new(ws_dir);
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let host_clone = Arc::clone(&host);
        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept connection");
            handle_http(stream, host_clone).await.expect("handle http");
        });

        let mut client_stream = TcpStream::connect(addr).await.expect("connect to test server");
        let req_body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/list"
        }))
        .expect("serialize req");

        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            req_body.len()
        );
        client_stream
            .write_all(request.as_bytes())
            .await
            .expect("write req header");
        client_stream
            .write_all(&req_body)
            .await
            .expect("write req body");

        let mut response = Vec::new();
        client_stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        let resp_str = String::from_utf8_lossy(&response);

        assert!(resp_str.contains("HTTP/1.1 200 OK"));
        assert!(resp_str.contains("\"tools\""));

        let _ = server_task.await;
    }
}
