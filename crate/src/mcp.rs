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
                 Workspace pin is the default context and safety anchor for relative paths and default commands, not a filesystem sandbox. \
                 Explicit absolute paths and explicit working directories may be used across multiple repositories without repinning. \
                 Call workspace_info first (now {}). \
                 Then read_file/grep/glob/list_dir, write/search_replace/apply_patch to edit, \
                 todo_write for plans, run_terminal_cmd (supports optional shell selector powershell/cmd/git-bash) or run_command to test. \
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
    use crate::testenv::isolate_env;

    async fn post_mcp(addr: SocketAddr, body: &Value) -> String {
        let mut client = TcpStream::connect(addr).await.expect("connect to test server");
        let req_body = serde_json::to_vec(body).expect("serialize req");
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            req_body.len()
        );
        client
            .write_all(request.as_bytes())
            .await
            .expect("write req header");
        client
            .write_all(&req_body)
            .await
            .expect("write req body");

        let mut response = Vec::new();
        client
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
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

        let req_body = json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/list"
        });
        let resp_str = post_mcp(addr, &req_body).await;

        assert!(resp_str.contains("HTTP/1.1 200 OK"));
        assert!(resp_str.contains("\"tools\""));

        let _ = server_task.await;
    }

    /// Locks HTTP-facing dispatch parity: a raw POST /mcp tools/call must
    /// produce the same JSON-RPC result as the shared `handle_rpc` dispatch
    /// (stdio serves the exact same function per line, so both transports
    /// route to the same ToolEngine behavior).
    #[tokio::test]
    async fn test_mcp_http_dispatch_matches_handle_rpc_parity() {
        let (_lock, _guard) = isolate_env("http_parity");
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

        let call_msg = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "workspace_info" }
        });
        let resp_str = post_mcp(addr, &call_msg).await;
        assert!(resp_str.contains("HTTP/1.1 200 OK"), "got: {resp_str}");
        let body_start = resp_str.find("\r\n\r\n").expect("header terminator") + 4;
        let http_resp: Value =
            serde_json::from_str(resp_str[body_start..].trim()).expect("parse http json-rpc body");

        let rpc_resp = host.handle_rpc(call_msg).await.expect("handle_rpc response");

        assert_eq!(http_resp["id"], rpc_resp["id"]);
        assert_eq!(
            http_resp["result"], rpc_resp["result"],
            "HTTP dispatch must produce identical engine behavior as shared dispatch"
        );
        assert_eq!(http_resp["result"]["isError"], false);

        let _ = server_task.await;
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_mcp_tools_call_multi_task_structured_output_contract() {
        let (_lock, _guard) = isolate_env("mcp_multi_task_structured");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let start_large = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 20,
                "method": "tools/call",
                "params": {
                    "name": "run_terminal_cmd",
                    "arguments": {
                        "command": "powershell.exe -NoProfile -NonInteractive -Command \"[Console]::Out.Write(('MCP_MULTI_A:' + [string][char]0x41) * 60000); Write-Output 'MCP_MULTI_TAIL'\"",
                        "description": "MCP multi-result large output",
                        "is_background": true
                    }
                }
            }))
            .await
            .expect("large background start response");
        assert_eq!(start_large["result"]["isError"], false);
        let large_task_id = start_large["result"]["structuredContent"]["task_id"]
            .as_str()
            .expect("large task id")
            .to_string();

        let start_small = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 21,
                "method": "tools/call",
                "params": {
                    "name": "run_terminal_cmd",
                    "arguments": {
                        "command": "powershell.exe -NoProfile -NonInteractive -Command \"Write-Output 'MCP_MULTI_SMALL'\"",
                        "description": "MCP multi-result small output",
                        "is_background": true
                    }
                }
            }))
            .await
            .expect("small background start response");
        assert_eq!(start_small["result"]["isError"], false);
        let small_task_id = start_small["result"]["structuredContent"]["task_id"]
            .as_str()
            .expect("small task id")
            .to_string();

        let output = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 22,
                "method": "tools/call",
                "params": {
                    "name": "get_task_output",
                    "arguments": {
                        "task_ids": [large_task_id, small_task_id],
                        "timeout_ms": 30000
                    }
                }
            }))
            .await
            .expect("multi task output response");

        let result = &output["result"];
        assert_eq!(result["isError"], false);
        let text = result["content"][0]["text"]
            .as_str()
            .expect("backward-compatible text content");
        assert!(text.contains("=== Multi-wait"));

        let results = result["structuredContent"]["MultiResult"]["results"]
            .as_array()
            .expect("structured MultiResult results");
        assert_eq!(results.len(), 2);
        for task in results {
            assert_eq!(task["has_output"], true, "task must explicitly report retained output");
            assert!(
                task["total_bytes"].as_u64().unwrap_or(0) > 0,
                "task must expose total output bytes"
            );
        }

        let large = results
            .iter()
            .find(|task| task["task_id"] == large_task_id)
            .expect("large task result");
        assert_eq!(large["truncated"], true);
        assert!(
            !large["output_file"].as_str().unwrap_or("").is_empty(),
            "truncated task must expose retrievable output_file"
        );
    }
    #[tokio::test]
    async fn test_mcp_tools_call_foreground_terminal_structured_output_contract() {
        let (_lock, _guard) = isolate_env("mcp_foreground_structured");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let output = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 23,
                "method": "tools/call",
                "params": {
                    "name": "run_terminal_cmd",
                    "arguments": {
                        "command": "echo MCP_FOREGROUND_STRUCTURED",
                        "description": "MCP foreground structured contract"
                    }
                }
            }))
            .await
            .expect("foreground terminal response");

        let result = &output["result"];
        assert_eq!(result["isError"], false);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .contains("MCP_FOREGROUND_STRUCTURED"),
            "existing model-facing text must be preserved"
        );

        let structured = &result["structuredContent"];
        assert_eq!(structured["type"], "Bash");
        assert_eq!(structured["exit_code"], 0);
        assert_eq!(structured["timed_out"], false);
        assert_eq!(structured["has_output"], true);
        assert!(
            structured["output"]
                .as_str()
                .unwrap_or("")
                .contains("MCP_FOREGROUND_STRUCTURED"),
            "ChatGPT-visible structured output must include the foreground sentinel"
        );
        assert!(structured["total_bytes"].as_u64().unwrap_or(0) > 0);
        assert!(structured["current_dir"].as_str().is_some());
        assert!(structured.get("truncated").is_some());
        assert!(structured.get("output_file").is_some());
        assert!(structured.get("signal").is_some());
    }

    #[tokio::test]
    async fn test_mcp_tools_call_run_command_structured_output_is_model_visible() {
        let (_lock, _guard) = isolate_env("mcp_run_command_structured_output");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let (command, args) = if cfg!(windows) {
            ("cmd.exe", json!(["/d", "/c", "echo MCP_RUN_COMMAND_STRUCTURED"]))
        } else {
            ("sh", json!(["-c", "printf 'MCP_RUN_COMMAND_STRUCTURED\\n'"]))
        };

        let output = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 24,
                "method": "tools/call",
                "params": {
                    "name": "run_command",
                    "arguments": {
                        "command": command,
                        "args": args
                    }
                }
            }))
            .await
            .expect("run_command response");

        let result = &output["result"];
        assert_eq!(result["isError"], false);
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .contains("MCP_RUN_COMMAND_STRUCTURED"),
            "backward-compatible content text must keep the native sentinel"
        );
        let structured_output = result["structuredContent"]["output"]
            .as_str()
            .unwrap_or("");
        assert!(
            structured_output.contains("MCP_RUN_COMMAND_STRUCTURED"),
            "ChatGPT-visible structured output must include the native sentinel"
        );
        assert!(
            !structured_output.contains("exit:"),
            "structured output should expose command stream text only; exit metadata already has dedicated fields"
        );
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_mcp_tools_call_cancellation_is_structured() {
        let (_lock, _guard) = isolate_env("mcp_cancellation_structured");
        let ws_dir = _guard.root.join("ws");
        fs::create_dir_all(&ws_dir).expect("create ws");
        let host = McpHost::new(ws_dir);

        let started = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 24,
                "method": "tools/call",
                "params": {
                    "name": "run_terminal_cmd",
                    "arguments": {
                        "command": "powershell.exe -NoProfile -NonInteractive -Command \"Start-Sleep -Seconds 30\"",
                        "description": "MCP cancellation structured contract",
                        "is_background": true
                    }
                }
            }))
            .await
            .expect("background start response");
        assert_eq!(started["result"]["isError"], false);
        let task_id = started["result"]["structuredContent"]["task_id"]
            .as_str()
            .expect("background task id")
            .to_string();

        let killed = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 25,
                "method": "tools/call",
                "params": {
                    "name": "kill_task",
                    "arguments": { "task_id": task_id }
                }
            }))
            .await
            .expect("kill response");
        assert_eq!(killed["result"]["isError"], false);
        assert_eq!(
            killed["result"]["structuredContent"]["Result"]["outcome"],
            "killed"
        );

        let after = host
            .handle_rpc(json!({
                "jsonrpc": "2.0",
                "id": 26,
                "method": "tools/call",
                "params": {
                    "name": "get_task_output",
                    "arguments": { "task_id": task_id, "timeout_ms": 5000 }
                }
            }))
            .await
            .expect("post-cancel output response");
        assert_eq!(after["result"]["isError"], false);
        assert_eq!(
            after["result"]["structuredContent"]["Result"]["status"],
            "cancelled",
            "caller must distinguish cancellation without parsing prompt text"
        );
    }
}
