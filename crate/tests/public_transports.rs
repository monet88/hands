mod common;
use serde_json::{json, Value};
use serial_test::serial;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use tempfile::TempDir;

fn hands_bin() -> std::path::PathBuf {
    if let Ok(bin) = std::env::var("CARGO_BIN_EXE_hands") {
        let p = std::path::PathBuf::from(bin);
        if p.exists() {
            return p;
        }
    }
    // Fallback: search relative to target dir / grok-build
    let target = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/debug/hands.exe");
    if target.exists() {
        return dunce::canonicalize(target).unwrap();
    }
    let target_unix = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/debug/hands");
    if target_unix.exists() {
        return dunce::canonicalize(target_unix).unwrap();
    }
    panic!("cannot locate hands test binary");
}

struct ProcessGuard(Child);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[serial]
fn test_public_mcp_stdio_process_boundary() {
    let bin = hands_bin();
    let config_dir = TempDir::new().expect("config dir");
    let workspace = TempDir::new().expect("workspace dir");

    // Write a test file in the workspace
    let test_file = workspace.path().join("hello.txt");
    std::fs::write(&test_file, "Hello from stdio process!").expect("write test file");
    // Pre-pin initial workspace in config_dir so HANDS_WORKSPACE env does not override set_workspace
    std::fs::write(config_dir.path().join("workspace"), format!("{}\n", workspace.path().display())).unwrap();
    let mut child = Command::new(&bin)
        .env("HANDS_CONFIG_DIR", config_dir.path())
        .env_remove("HANDS_WORKSPACE")
        .env_remove("GROK_HARNESS_WORKSPACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hands in stdio mode");

    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);
    let _guard = ProcessGuard(child);

    // 1. initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "clientInfo": { "name": "test-stdio", "version": "1.0" }
        }
    });
    let mut line = serde_json::to_string(&init_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    let mut resp_line = String::new();
    reader.read_line(&mut resp_line).expect("read init response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse init json");
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["serverInfo"]["name"], "Hands");

    // 2. tools/list
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });
    line = serde_json::to_string(&list_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read list response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse list json");
    assert_eq!(resp["id"], 2);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"run_terminal_cmd"));
    assert!(names.contains(&"run_command"));
    assert!(names.contains(&"workspace_info"));

    // 3. tools/call workspace_info
    let ws_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "workspace_info",
            "arguments": {}
        }
    });
    line = serde_json::to_string(&ws_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read ws response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse ws json");
    assert_eq!(resp["id"], 3);
    assert_eq!(resp["result"]["isError"], false);

    // 4. tools/call read_file
    let read_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "read_file",
            "arguments": { "target_file": "hello.txt" }
        }
    });
    line = serde_json::to_string(&read_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read read_file response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse read_file json");
    assert_eq!(resp["id"], 4);
    assert_eq!(resp["result"]["isError"], false);
    let content = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(content.contains("Hello from stdio process!"));
    // 5. tools/call set_workspace to switched workspace and read switched file
    let switched_ws = TempDir::new().expect("switched workspace");
    let switched_file = switched_ws.path().join("switched_hello.txt");
    std::fs::write(&switched_file, "Switched stdio workspace content!").expect("write switched file");

    let set_ws_req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "set_workspace",
            "arguments": { "path": switched_ws.path().to_str().unwrap() }
        }
    });
    line = serde_json::to_string(&set_ws_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read set_workspace response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse set_workspace json");
    assert_eq!(resp["id"], 5);
    assert_eq!(resp["result"]["isError"], false);

    let read_switched_req = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "read_file",
            "arguments": { "target_file": "switched_hello.txt" }
        }
    });
    line = serde_json::to_string(&read_switched_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read read_file switched response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse read_file switched json");
    assert_eq!(resp["id"], 6);
    assert_eq!(resp["result"]["isError"], false);
    let content = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(content.contains("Switched stdio workspace content!"));

    // 6. tools/call run_terminal_cmd over stdio
    #[cfg(windows)]
    let cmd = "powershell -NoProfile -Command \"Write-Output 'STDIO_TERMINAL_OK'\"";
    #[cfg(not(windows))]
    let cmd = "echo 'STDIO_TERMINAL_OK'";

    let term_req = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "run_terminal_cmd",
            "arguments": {
                "command": cmd,
                "description": "Verify terminal execution over public stdio transport"
            }
        }
    });
    line = serde_json::to_string(&term_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read terminal response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse terminal json");
    assert_eq!(resp["id"], 7);
    assert_eq!(resp["result"]["isError"], false);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("STDIO_TERMINAL_OK"), "output must contain STDIO_TERMINAL_OK: {text}");

    // 8. tools/call run_command over stdio
    let run_cmd_req = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": "git",
                "args": ["--version"]
            }
        }
    });
    line = serde_json::to_string(&run_cmd_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read run_command response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse run_command json");
    assert_eq!(resp["id"], 8);
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(resp["result"]["structuredContent"]["execution_state"], "completed");
    assert_eq!(resp["result"]["structuredContent"]["exit_code"], 0);
}

fn pick_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    port
}

fn http_post_rpc(port: u16, req: &Value) -> Result<Value, String> {
    let body = serde_json::to_vec(req).map_err(|e| e.to_string())?;
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}"))
        .map_err(|e| format!("connect failed: {e}"))?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();

    let header = format!(
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(header.as_bytes()).map_err(|e| e.to_string())?;
    stream.write_all(&body).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut resp_bytes = Vec::new();
    stream.read_to_end(&mut resp_bytes).map_err(|e| e.to_string())?;
    let resp_str = String::from_utf8_lossy(&resp_bytes);
    let body_part = resp_str.split("\r\n\r\n").nth(1).ok_or("no http body")?;
    serde_json::from_str::<Value>(body_part).map_err(|e| format!("json parse error on '{body_part}': {e}"))
}

#[test]
#[serial]
fn test_public_mcp_http_process_boundary() {
    let bin = hands_bin();
    let config_dir = TempDir::new().expect("config dir");
    let workspace = TempDir::new().expect("workspace dir");
    let port = pick_free_port();
    // Pre-pin initial workspace in config_dir so HANDS_WORKSPACE env does not override set_workspace
    std::fs::write(config_dir.path().join("workspace"), format!("{}\n", workspace.path().display())).unwrap();

    let child = Command::new(&bin)
        .env("HANDS_CONFIG_DIR", config_dir.path())
        .env_remove("HANDS_WORKSPACE")
        .env_remove("GROK_HARNESS_WORKSPACE")
        .args(["--http", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hands in http mode");

    let _guard = ProcessGuard(child);

    // Poll until server is ready on ephemeral port
    let start = std::time::Instant::now();
    let mut ready = false;
    while start.elapsed() < std::time::Duration::from_secs(15) {
        if let Ok(mut stream) = std::net::TcpStream::connect(format!("127.0.0.1:{port}")) {
            let _ = stream.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            let mut buf = [0u8; 64];
            if let Ok(n) = stream.read(&mut buf) {
                if n > 0 && String::from_utf8_lossy(&buf[..n]).contains("200 OK") {
                    ready = true;
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(ready, "hands http server must respond on port {port}");

    // 1. initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "clientInfo": { "name": "test-http", "version": "1.0" }
        }
    });
    let resp = http_post_rpc(port, &init_req).expect("http init rpc");
    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 10);
    assert_eq!(resp["result"]["serverInfo"]["name"], "Hands");

    // 2. tools/list
    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/list",
        "params": {}
    });
    let resp = http_post_rpc(port, &list_req).expect("http tools/list rpc");
    assert_eq!(resp["id"], 11);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"workspace_info"));
    assert!(names.contains(&"run_terminal_cmd"));
    assert!(names.contains(&"run_command"));

    // 3. tools/call run_terminal_cmd
    #[cfg(windows)]
    let cmd = "powershell -NoProfile -Command \"Write-Output 'HTTP_PROCESS_OK'\"";
    #[cfg(not(windows))]
    let cmd = "echo 'HTTP_PROCESS_OK'";

    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "tools/call",
        "params": {
            "name": "run_terminal_cmd",
            "arguments": {
                "command": cmd,
                "description": "Verify terminal execution over public HTTP transport"
            }
        }
    });
    let resp = http_post_rpc(port, &call_req).expect("http tools/call rpc");
    assert_eq!(resp["id"], 12);
    assert_eq!(resp["result"]["isError"], false);
    let text = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("HTTP_PROCESS_OK"), "output must contain HTTP_PROCESS_OK: {text}");

    // 4. tools/call set_workspace to switched workspace and read file over HTTP
    let switched_ws = TempDir::new().expect("switched workspace");
    let switched_file = switched_ws.path().join("http_hello.txt");
    std::fs::write(&switched_file, "Switched HTTP workspace content!").expect("write switched http file");

    let set_ws_req = json!({
        "jsonrpc": "2.0",
        "id": 13,
        "method": "tools/call",
        "params": {
            "name": "set_workspace",
            "arguments": { "path": switched_ws.path().to_str().unwrap() }
        }
    });
    let resp = http_post_rpc(port, &set_ws_req).expect("http set_workspace rpc");
    assert_eq!(resp["id"], 13);
    assert_eq!(resp["result"]["isError"], false);

    let read_req = json!({
        "jsonrpc": "2.0",
        "id": 14,
        "method": "tools/call",
        "params": {
            "name": "read_file",
            "arguments": { "target_file": "http_hello.txt" }
        }
    });
    let resp = http_post_rpc(port, &read_req).expect("http read_file switched rpc");
    assert_eq!(resp["id"], 14);
    assert_eq!(resp["result"]["isError"], false);
    let content = resp["result"]["content"][0]["text"].as_str().unwrap();
    assert!(content.contains("Switched HTTP workspace content!"));

    // 5. tools/call run_command over HTTP
    let run_cmd_req = json!({
        "jsonrpc": "2.0",
        "id": 15,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": "git",
                "args": ["--version"]
            }
        }
    });
    let resp = http_post_rpc(port, &run_cmd_req).expect("http run_command rpc");
    assert_eq!(resp["id"], 15);
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(resp["result"]["structuredContent"]["execution_state"], "completed");
    assert_eq!(resp["result"]["structuredContent"]["exit_code"], 0);
}
