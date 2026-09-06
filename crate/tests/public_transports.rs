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

    // 8. tools/call run_command over stdio with literal argv
    let python_cmd = if std::process::Command::new("python3").arg("--version").output().is_ok() {
        "python3"
    } else {
        "python"
    };
    let run_cmd_req = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": python_cmd,
                "args": [
                    "-c",
                    "import sys, json; print(json.dumps(sys.argv[1:]))",
                    "space arg",
                    "\"quotes\"",
                    "$PATH",
                    "{\"a\":1}",
                    "unicode-✓",
                    "line1\nline2",
                    "--leading",
                    "&|<>"
                ]
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
    let expected_stdio_ws = dunce::canonicalize(switched_ws.path()).unwrap().display().to_string();
    assert_eq!(
        resp["result"]["structuredContent"]["default_workspace"].as_str().unwrap(),
        expected_stdio_ws,
        "stdio run_command must report default_workspace"
    );
    assert_eq!(
        resp["result"]["structuredContent"]["cwd"].as_str().unwrap(),
        expected_stdio_ws,
        "stdio run_command must report effective execution cwd"
    );
    let stdout = resp["result"]["structuredContent"]["stdout"].as_str().expect("stdout");
    let parsed: Vec<String> = serde_json::from_str(stdout.trim()).expect("parse stdout json");
    assert_eq!(
        parsed,
        vec![
            "space arg",
            "\"quotes\"",
            "$PATH",
            "{\"a\":1}",
            "unicode-✓",
            "line1\nline2",
            "--leading",
            "&|<>"
        ]
    );

    // 9. tools/call run_command over stdio with explicit workdir outside default workspace
    let outside_stdio_dir = TempDir::new().expect("outside stdio dir");
    let outside_stdio_path = dunce::canonicalize(outside_stdio_dir.path()).unwrap().display().to_string();
    let outside_cmd_req = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": python_cmd,
                "args": ["-c", "import os; print('OUTSIDE_STDIO_OK')"],
                "workdir": outside_stdio_path
            }
        }
    });
    line = serde_json::to_string(&outside_cmd_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read outside run_command response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse outside run_command json");
    assert_eq!(resp["id"], 9);
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(
        resp["result"]["structuredContent"]["default_workspace"].as_str().unwrap(),
        expected_stdio_ws,
        "default_workspace must remain Repo A over stdio"
    );
    assert_eq!(
        resp["result"]["structuredContent"]["cwd"].as_str().unwrap(),
        outside_stdio_path,
        "effective cwd must report Repo B over stdio"
    );

    // 10. tools/call run_command over stdio with process timeout
    let timeout_cmd_req = json!({
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": python_cmd,
                "args": ["-c", "import time; time.sleep(5)"],
                "timeout_ms": 100
            }
        }
    });
    line = serde_json::to_string(&timeout_cmd_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();

    resp_line.clear();
    reader.read_line(&mut resp_line).expect("read timeout run_command response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse timeout run_command json");
    assert_eq!(resp["id"], 10);
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(
        resp["result"]["structuredContent"]["execution_state"], "timed_out",
        "terminal process timeout must report execution_state 'timed_out'"
    );
    assert_eq!(resp["result"]["structuredContent"]["command_started"], true);
    assert_eq!(resp["result"]["structuredContent"]["command_completed"], false);
    assert_eq!(resp["result"]["structuredContent"]["timed_out"], true);
    assert_eq!(resp["result"]["structuredContent"]["exit_code"], -1);
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
    let python_cmd = if std::process::Command::new("python3").arg("--version").output().is_ok() {
        "python3"
    } else {
        "python"
    };
    let run_cmd_req = json!({
        "jsonrpc": "2.0",
        "id": 15,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": python_cmd,
                "args": [
                    "-c",
                    "import sys, json; print(json.dumps(sys.argv[1:]))",
                    "space arg",
                    "\"quotes\"",
                    "$PATH",
                    "{\"a\":1}",
                    "unicode-✓",
                    "line1\nline2",
                    "--leading",
                    "&|<>"
                ]
            }
        }
    });
    let resp = http_post_rpc(port, &run_cmd_req).expect("http run_command rpc");
    assert_eq!(resp["id"], 15);
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(resp["result"]["structuredContent"]["execution_state"], "completed");
    assert_eq!(resp["result"]["structuredContent"]["exit_code"], 0);
    let expected_http_ws = dunce::canonicalize(switched_ws.path()).unwrap().display().to_string();
    assert_eq!(
        resp["result"]["structuredContent"]["default_workspace"].as_str().unwrap(),
        expected_http_ws,
        "http run_command must report default_workspace"
    );
    assert_eq!(
        resp["result"]["structuredContent"]["cwd"].as_str().unwrap(),
        expected_http_ws,
        "http run_command must report effective execution cwd"
    );
    let stdout = resp["result"]["structuredContent"]["stdout"].as_str().expect("stdout");
    let parsed: Vec<String> = serde_json::from_str(stdout.trim()).expect("parse stdout json");
    assert_eq!(
        parsed,
        vec![
            "space arg",
            "\"quotes\"",
            "$PATH",
            "{\"a\":1}",
            "unicode-✓",
            "line1\nline2",
            "--leading",
            "&|<>"
        ]
    );

    // 6. tools/call run_command over HTTP with explicit workdir outside default workspace
    let outside_http_dir = TempDir::new().expect("outside http dir");
    let outside_http_path = dunce::canonicalize(outside_http_dir.path()).unwrap().display().to_string();
    let outside_http_req = json!({
        "jsonrpc": "2.0",
        "id": 16,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": python_cmd,
                "args": ["-c", "import os; print('OUTSIDE_HTTP_OK')"],
                "workdir": outside_http_path
            }
        }
    });
    let resp = http_post_rpc(port, &outside_http_req).expect("http run_command outside rpc");
    assert_eq!(resp["id"], 16);
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(
        resp["result"]["structuredContent"]["default_workspace"].as_str().unwrap(),
        expected_http_ws,
        "default_workspace must remain Repo A over HTTP"
    );
    assert_eq!(
        resp["result"]["structuredContent"]["cwd"].as_str().unwrap(),
        outside_http_path,
        "effective cwd must report Repo B over HTTP"
    );

    // 7. tools/call run_command over HTTP with process timeout
    let timeout_http_req = json!({
        "jsonrpc": "2.0",
        "id": 17,
        "method": "tools/call",
        "params": {
            "name": "run_command",
            "arguments": {
                "command": python_cmd,
                "args": ["-c", "import time; time.sleep(5)"],
                "timeout_ms": 100
            }
        }
    });
    let resp = http_post_rpc(port, &timeout_http_req).expect("http run_command timeout rpc");
    assert_eq!(resp["id"], 17);
    assert_eq!(resp["result"]["isError"], false);
    assert_eq!(
        resp["result"]["structuredContent"]["execution_state"], "timed_out",
        "http run_command process timeout must report execution_state 'timed_out'"
    );
    assert_eq!(resp["result"]["structuredContent"]["command_started"], true);
    assert_eq!(resp["result"]["structuredContent"]["command_completed"], false);
    assert_eq!(resp["result"]["structuredContent"]["timed_out"], true);
    assert_eq!(resp["result"]["structuredContent"]["exit_code"], -1);
}

fn spawn_stdio_hands(
    env_overrides: &[(&str, &str)],
) -> (
    ProcessGuard,
    std::process::ChildStdin,
    BufReader<std::process::ChildStdout>,
    TempDir,
    TempDir,
) {
    let config_dir = TempDir::new().expect("config tempdir");
    let workspace = TempDir::new().expect("workspace tempdir");
    let probe = workspace.path().join("hello.txt");
    std::fs::write(&probe, "Hello from stdio process!").unwrap();
    std::fs::write(
        config_dir.path().join("workspace"),
        format!("{}
", workspace.path().display()),
    )
    .unwrap();
    let bin = hands_bin();
    let mut cmd = Command::new(&bin);
    cmd.env("HANDS_CONFIG_DIR", config_dir.path())
        .env_remove("HANDS_WORKSPACE")
        .env_remove("GROK_HARNESS_WORKSPACE");
    for (k, v) in env_overrides {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hands over stdio");
    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "seam1-client", "version": "0.0"} }
    });
    let mut line = serde_json::to_string(&init_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut resp_line = String::new();
    reader
        .read_line(&mut resp_line)
        .expect("read init response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse init json");
    assert_eq!(resp["id"], 1);

    (ProcessGuard(child), stdin, reader, config_dir, workspace)
}

fn stdio_call_with_meta(
    stdin: &mut std::process::ChildStdin,
    reader: &mut BufReader<std::process::ChildStdout>,
    id: u64,
    name: &str,
    arguments: Value,
    meta: Option<Value>,
) -> Value {
    let mut params = json!({
        "name": name,
        "arguments": arguments,
    });
    if let Some(m) = meta {
        params["_meta"] = m;
    }
    let req = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": params,
    });
    let mut line = serde_json::to_string(&req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
    let mut resp_line = String::new();
    reader
        .read_line(&mut resp_line)
        .expect("read tools/call response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse tools/call json");
    assert_eq!(resp["id"], id);
    resp
}

fn stdio_call(
    stdin: &mut std::process::ChildStdin,
    reader: &mut BufReader<std::process::ChildStdout>,
    id: u64,
    name: &str,
    arguments: Value,
) -> Value {
    stdio_call_with_meta(stdin, reader, id, name, arguments, None)
}

// Issue #62 Seam 1, public acceptance: search_replace over real hands.exe MCP
// stdio, one process, four sequential cases. Asserts actual file mutation plus
// line-separated unified diffs in authoritative structuredContent — the same
// contract the in-process output_visibility tests cover, but through the real
// process boundary the spec requires.
#[test]
#[serial]
fn test_public_stdio_search_replace_all_four_cases() {
    let config_dir = TempDir::new().expect("config tempdir");
    let workspace = TempDir::new().expect("workspace tempdir");
    let probe = workspace.path().join("hello.txt");
    std::fs::write(&probe, "Hello from stdio process!").unwrap();
    // Pre-pin initial workspace in config_dir so HANDS_WORKSPACE env does not override set_workspace
    std::fs::write(
        config_dir.path().join("workspace"),
        format!("{}\n", workspace.path().display()),
    )
    .unwrap();
    let bin = hands_bin();
    let mut child = Command::new(&bin)
        .env("HANDS_CONFIG_DIR", config_dir.path())
        .env_remove("HANDS_WORKSPACE")
        .env_remove("GROK_HARNESS_WORKSPACE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hands over stdio");
    let mut stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);
    let _guard = ProcessGuard(child);
    let mut resp_line = String::new();

    // 1. initialize
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": { "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "sr-seam1", "version": "0.0"} }
    });
    let mut line = serde_json::to_string(&init_req).unwrap();
    line.push('\n');
    stdin.write_all(line.as_bytes()).unwrap();
    stdin.flush().unwrap();
    reader
        .read_line(&mut resp_line)
        .expect("read init response");
    let resp: Value = serde_json::from_str(&resp_line).expect("parse init json");
    assert_eq!(resp["id"], 1);

    // 2. set_workspace to the fixture dir
    let ws: Value = stdio_call(
        &mut stdin,
        &mut reader,
        2,
        "set_workspace",
        json!({ "path": workspace.path().to_str().unwrap() }),
    );
    assert_eq!(ws["result"]["isError"], false);

    // Case 1: single replacement. Full lines; diff must be line-separated.
    let f1 = workspace.path().join("sr_single.txt");
    std::fs::write(&f1, "alpha\nbeta\ngamma\n").unwrap();
    let r = stdio_call(
        &mut stdin,
        &mut reader,
        10,
        "search_replace",
        json!({ "file_path": f1.to_str().unwrap(), "old_string": "beta", "new_string": "beta2" }),
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(
        std::fs::read_to_string(&f1).unwrap(),
        "alpha\nbeta2\ngamma\n"
    );
    let structured = &r["result"]["structuredContent"];
    assert_eq!(structured["kind"], "edited");
    let diff = structured["diff"].as_str().expect("structured diff");
    assert!(
        diff.contains("-beta\n"),
        "diff must carry full old line: {diff}"
    );
    assert!(
        diff.contains("+beta2\n"),
        "diff must carry full new line: {diff}"
    );

    // Case 2: replace-all. Two lines change; counts must be exact.
    let f2 = workspace.path().join("sr_all.txt");
    std::fs::write(&f2, "one foo\ntwo foo\nkeep\n").unwrap();
    let r = stdio_call(
        &mut stdin,
        &mut reader,
        11,
        "search_replace",
        json!({ "file_path": f2.to_str().unwrap(), "old_string": "foo", "new_string": "bar", "replace_all": true }),
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(
        std::fs::read_to_string(&f2).unwrap(),
        "one bar\ntwo bar\nkeep\n"
    );
    let structured = &r["result"]["structuredContent"];
    assert_eq!(structured["kind"], "edited");
    assert_eq!(structured["removed"], 2);
    assert_eq!(structured["added"], 2);
    let diff = structured["diff"].as_str().expect("structured diff");
    let removed = diff
        .lines()
        .filter(|l| l.starts_with('-') && !l.starts_with("---"))
        .count();
    let added = diff
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .count();
    assert_eq!(
        removed, 2,
        "replace-all diff must show both removed lines: {diff}"
    );
    assert_eq!(
        added, 2,
        "replace-all diff must show both added lines: {diff}"
    );

    // Case 3: CRLF content. Mutation real; diff normalized to LF.
    let f3 = workspace.path().join("sr_crlf.txt");
    std::fs::write(&f3, b"x\r\nhello\r\nz\r\n").unwrap();
    let r = stdio_call(
        &mut stdin,
        &mut reader,
        12,
        "search_replace",
        json!({ "file_path": f3.to_str().unwrap(), "old_string": "hello", "new_string": "hello world" }),
    );
    assert_eq!(r["result"]["isError"], false);
    let raw = std::fs::read(&f3).unwrap();
    assert!(
        raw.windows(11).any(|w| w == b"hello world"),
        "file must contain the edit"
    );
    let diff = r["result"]["structuredContent"]["diff"]
        .as_str()
        .expect("structured diff");
    assert!(
        !diff.contains('\r'),
        "CRLF diff must be normalized: {diff:?}"
    );
    assert!(
        diff.contains("-hello\n"),
        "CRLF diff must show old line: {diff}"
    );
    assert!(
        diff.contains("+hello world\n"),
        "CRLF diff must show new line: {diff}"
    );

    // Case 4: inline same-line replacement stays one line, not two snippets.
    let f4 = workspace.path().join("sr_inline.txt");
    std::fs::write(&f4, "say hello now\n").unwrap();
    let r = stdio_call(
        &mut stdin,
        &mut reader,
        13,
        "search_replace",
        json!({ "file_path": f4.to_str().unwrap(), "old_string": "hello", "new_string": "hello world" }),
    );
    assert_eq!(r["result"]["isError"], false);
    assert_eq!(
        std::fs::read_to_string(&f4).unwrap(),
        "say hello world now\n"
    );
    let structured = &r["result"]["structuredContent"];
    assert_eq!(structured["kind"], "edited");
    let diff = structured["diff"].as_str().expect("structured diff");
    assert!(
        diff.contains("-say hello now\n"),
        "inline diff old line: {diff}"
    );
    assert!(
        diff.contains("+say hello world now\n"),
        "inline diff new line: {diff}"
    );
}

#[test]
#[serial]
fn test_public_stdio_run_command_descendant_timeout_and_captured_output() {
    let (_guard, mut stdin, mut reader, _config, workspace) = spawn_stdio_hands(&[]);

    let pid_file = workspace.path().join("child.pid");
    let pid_file_str = pid_file.display().to_string().replace('\\', "/");
    let python_cmd = if Command::new("python").arg("--version").output().is_ok() {
        "python"
    } else if Command::new("python3").arg("--version").output().is_ok() {
        "python3"
    } else {
        "python"
    };

    let script = format!(
        "import os, subprocess, sys, time\n\
         sys.stdout.write('CAPTURED_STDOUT_LINE\\n')\n\
         sys.stdout.flush()\n\
         sys.stderr.write('CAPTURED_STDERR_LINE\\n')\n\
         sys.stderr.flush()\n\
         p = subprocess.Popen(['{python_cmd}', '-c', 'import os, time; open(\"{pid_file_str}\", \"w\").write(str(os.getpid())); time.sleep(30)'])\n\
         time.sleep(30)\n"
    );

    let start = std::time::Instant::now();
    let r = stdio_call(
        &mut stdin,
        &mut reader,
        10,
        "run_command",
        json!({
            "command": python_cmd,
            "args": ["-c", script],
            "timeout_ms": 1200
        }),
    );
    let elapsed = start.elapsed();

    assert_eq!(r["result"]["isError"], false);
    let structured = &r["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "timed_out");
    assert_eq!(structured["command_started"], true);
    assert_eq!(structured["command_completed"], false);
    assert_eq!(structured["timed_out"], true);
    assert_eq!(structured["exit_code"], -1);
    assert!(
        elapsed < std::time::Duration::from_millis(5000),
        "call must return bounded within deadline, elapsed was {:?}",
        elapsed
    );

    let stdout = structured["stdout"].as_str().expect("stdout str");
    let stderr = structured["stderr"].as_str().expect("stderr str");
    assert!(
        stdout.contains("CAPTURED_STDOUT_LINE"),
        "captured stdout must survive timeout: {stdout}"
    );
    assert!(
        stderr.contains("CAPTURED_STDERR_LINE"),
        "captured stderr must survive timeout: {stderr}"
    );

    #[cfg(windows)]
    {
        assert!(
            pid_file.exists(),
            "descendant PID marker must exist before settlement assertion"
        );
        let pid_str = std::fs::read_to_string(&pid_file).expect("read child pid");
        let pid: u32 = pid_str.trim().parse().expect("parse child pid");

        let check = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {} -ErrorAction SilentlyContinue).Id", pid),
            ])
            .output()
            .expect("check descendant process");
        let out = String::from_utf8_lossy(&check.stdout).trim().to_string();
        assert!(
            out.is_empty(),
            "descendant PID {pid} must not be alive after run_command timed_out: found '{out}'"
        );
    }
}

#[test]
#[serial]
fn test_public_stdio_run_terminal_cmd_explicit_timeout_and_handoff_clocks() {
    // 1. Explicit short timeout before handoff (no background replacement)
    {
        let (_guard, mut stdin, mut reader, _config, _workspace) = spawn_stdio_hands(&[]);

        #[cfg(windows)]
        let cmd = r#"powershell -NoProfile -Command "Start-Sleep -Seconds 10""#;
        #[cfg(not(windows))]
        let cmd = "sleep 10";

        let start = std::time::Instant::now();
        let r = stdio_call(
            &mut stdin,
            &mut reader,
            10,
            "run_terminal_cmd",
            json!({
                "command": cmd,
                "description": "Short explicit timeout test",
                "timeout": 400
            }),
        );
        let elapsed = start.elapsed();
        // Terminal timeout produces isError: true
        assert_eq!(r["result"]["isError"], true);
        let structured = &r["result"]["structuredContent"];
        assert_eq!(structured["type"], "Bash");
        assert_eq!(structured["signal"], "timeout");
        assert!(elapsed < std::time::Duration::from_millis(3000));

        let list = stdio_call(&mut stdin, &mut reader, 11, "list_terminal_tasks", json!({}));
        let tasks = list["result"]["structuredContent"]["tasks"].as_array().unwrap();
        assert!(tasks.iter().all(|t| t["status"] != "running"), "no task should remain running");
    }

    // 2. Handoff before a longer explicit runtime deadline: task identity survives and settles to timed_out
    {
        let (_guard, mut stdin, mut reader, _config, _workspace) =
            spawn_stdio_hands(&[("GROK_FOREGROUND_BLOCK_BUDGET_MS", "300")]);

        #[cfg(windows)]
        let cmd = r#"powershell -NoProfile -Command "Start-Sleep -Seconds 20""#;
        #[cfg(not(windows))]
        let cmd = "sleep 20";

        let start = std::time::Instant::now();
        let r = stdio_call(
            &mut stdin,
            &mut reader,
            20,
            "run_terminal_cmd",
            json!({
                "command": cmd,
                "description": "Handoff with explicit deadline",
                "timeout": 1200
            }),
        );
        let initial_elapsed = start.elapsed();
        assert_eq!(r["result"]["isError"], false);
        let structured = &r["result"]["structuredContent"];
        assert_eq!(structured["status"], "running");
        assert!(initial_elapsed < std::time::Duration::from_millis(1000));
        let task_id = structured["task_id"].as_str().expect("task_id").to_string();

        let mut settled_to_timeout = false;
        while start.elapsed() < std::time::Duration::from_secs(8) {
            let list = stdio_call(&mut stdin, &mut reader, 21, "list_terminal_tasks", json!({}));
            if let Some(tasks) = list["result"]["structuredContent"]["tasks"].as_array() {
                if let Some(t) = tasks.iter().find(|t| t["task_id"] == task_id) {
                    if t["status"] == "timed_out" && t["completed"] == true {
                        settled_to_timeout = true;
                        break;
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        assert!(
            settled_to_timeout,
            "task must preserve explicit deadline and settle to timed_out after handoff"
        );
    }
}

#[test]
#[serial]
fn test_public_stdio_run_terminal_cmd_omitted_timeout_auto_yield_and_task_recovery() {
    let (_guard, mut stdin, mut reader, _config, workspace) =
        spawn_stdio_hands(&[("GROK_FOREGROUND_BLOCK_BUDGET_MS", "300")]);

    let marker_file = workspace.path().join("auto_yield_marker.txt");
    let marker_str = marker_file.display().to_string().replace('\\', "/");

    #[cfg(windows)]
    let cmd = format!(
        r#"powershell -NoProfile -Command "Set-Content -Path '{}' -Value 'ONE_RUN'; Start-Sleep -Seconds 20""#,
        marker_str
    );
    #[cfg(not(windows))]
    let cmd = format!("echo 'ONE_RUN' > '{}'; sleep 20", marker_str);

    let start = std::time::Instant::now();
    let r = stdio_call(
        &mut stdin,
        &mut reader,
        10,
        "run_terminal_cmd",
        json!({
            "command": cmd,
            "description": "Omitted timeout auto-yield test"
        }),
    );
    let elapsed = start.elapsed();
    assert_eq!(r["result"]["isError"], false);
    let structured = &r["result"]["structuredContent"];
    assert_eq!(structured["status"], "running");
    assert!(elapsed < std::time::Duration::from_millis(1200));
    let task_id = structured["task_id"].as_str().expect("task_id").to_string();

    let out = stdio_call(
        &mut stdin,
        &mut reader,
        11,
        "get_task_output",
        json!({ "task_id": task_id }),
    );
    assert_eq!(out["result"]["isError"], false);

    let list = stdio_call(&mut stdin, &mut reader, 12, "list_terminal_tasks", json!({}));
    assert_eq!(list["result"]["isError"], false);
    let tasks = list["result"]["structuredContent"]["tasks"].as_array().unwrap();
    assert!(tasks.iter().any(|t| t["task_id"] == task_id && t["status"] == "running"));

    // Bounded wait for marker file to appear before killing
    let start_wait = std::time::Instant::now();
    let mut marker_found = false;
    while start_wait.elapsed() < std::time::Duration::from_secs(10) {
        if marker_file.exists() && std::fs::read_to_string(&marker_file).map(|s| s.contains("ONE_RUN")).unwrap_or(false) {
            marker_found = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    assert!(marker_found, "marker file must have been written");

    let kill = stdio_call(
        &mut stdin,
        &mut reader,
        13,
        "kill_task",
        json!({ "task_id": task_id }),
    );
    assert_eq!(kill["result"]["isError"], false);

    let content = std::fs::read_to_string(&marker_file).unwrap();
    assert!(content.contains("ONE_RUN"));
    assert_eq!(content.matches("ONE_RUN").count(), 1, "must execute exactly once");
}

#[test]
#[serial]
fn test_public_stdio_session_tasks_survive_workspace_switch_and_roundtrip() {
    let (_guard, mut stdin, mut reader, _config, _workspace) = spawn_stdio_hands(&[]);

    let session = json!({ "openai/session": "seam1-session-workspace-test" });

    #[cfg(windows)]
    let long_cmd = r#"powershell -NoProfile -Command "Start-Sleep -Seconds 30""#;
    #[cfg(not(windows))]
    let long_cmd = "sleep 30";

    let bg = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        10,
        "run_terminal_cmd",
        json!({
            "command": long_cmd,
            "description": "Long background task",
            "is_background": true
        }),
        Some(session.clone()),
    );
    assert_eq!(bg["result"]["isError"], false);
    let task_id = bg["result"]["structuredContent"]["task_id"].as_str().unwrap().to_string();

    let new_ws = TempDir::new().unwrap();
    let sw = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        11,
        "set_workspace",
        json!({ "path": new_ws.path().to_str().unwrap() }),
        Some(session.clone()),
    );
    assert_eq!(sw["result"]["isError"], false);

    // Workspace switch recovery: get_task_output still works after switch
    let get_out_running = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        12,
        "get_task_output",
        json!({ "task_id": task_id }),
        Some(session.clone()),
    );
    assert_eq!(get_out_running["result"]["isError"], false);
    assert_eq!(get_out_running["result"]["structuredContent"]["status"], "running");

    let list1 = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        13,
        "list_terminal_tasks",
        json!({}),
        Some(session.clone()),
    );
    assert_eq!(list1["result"]["isError"], false);
    let tasks1 = list1["result"]["structuredContent"]["tasks"].as_array().unwrap();
    let found1 = tasks1.iter().find(|t| t["task_id"] == task_id).expect("task must survive switch");
    assert_eq!(found1["status"], "running");

    let kill = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        14,
        "kill_task",
        json!({ "task_id": task_id }),
        Some(session.clone()),
    );
    assert_eq!(kill["result"]["isError"], false);

    #[cfg(windows)]
    let short_cmd = r#"powershell -NoProfile -Command "Write-Output 'ROUNDTRIP_STDIO_OK'""#;
    #[cfg(not(windows))]
    let short_cmd = "echo ROUNDTRIP_STDIO_OK";

    let expected_short_cwd = dunce::canonicalize(new_ws.path()).unwrap().display().to_string();

    let short_bg = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        15,
        "run_terminal_cmd",
        json!({
            "command": short_cmd,
            "description": "Short task for roundtrip",
            "is_background": true
        }),
        Some(session.clone()),
    );
    assert_eq!(short_bg["result"]["isError"], false);
    let short_id = short_bg["result"]["structuredContent"]["task_id"].as_str().unwrap().to_string();

    let mut completed = false;
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(10) {
        let l = stdio_call_with_meta(
            &mut stdin,
            &mut reader,
            16,
            "list_terminal_tasks",
            json!({}),
            Some(session.clone()),
        );
        if let Some(arr) = l["result"]["structuredContent"]["tasks"].as_array() {
            if let Some(t) = arr.iter().find(|t| t["task_id"] == short_id) {
                if t["completed"] == true {
                    completed = true;
                    break;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    assert!(completed, "short task must complete");

    // Check get_task_output before roundtrip switch
    let get_out_before = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        17,
        "get_task_output",
        json!({ "task_id": short_id }),
        Some(session.clone()),
    );
    assert_eq!(get_out_before["result"]["isError"], false);
    assert_eq!(get_out_before["result"]["structuredContent"]["status"], "completed");
    assert_eq!(get_out_before["result"]["structuredContent"]["exit_code"], 0);
    assert!(get_out_before["result"]["structuredContent"]["output"].as_str().unwrap().contains("ROUNDTRIP_STDIO_OK"));

    let ws_a = TempDir::new().unwrap();
    let ws_b = TempDir::new().unwrap();
    let mut req_id = 18;
    for dir in [&ws_a, &ws_b] {
        let sw = stdio_call_with_meta(
            &mut stdin,
            &mut reader,
            req_id,
            "set_workspace",
            json!({ "path": dir.path().to_str().unwrap() }),
            Some(session.clone()),
        );
        assert_eq!(sw["result"]["isError"], false);
        req_id += 1;
    }

    // Workspace switch recovery: get_task_output still works after roundtrip switch
    let get_out_after = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        req_id,
        "get_task_output",
        json!({ "task_id": short_id }),
        Some(session.clone()),
    );
    req_id += 1;
    assert_eq!(get_out_after["result"]["isError"], false);
    assert_eq!(get_out_after["result"]["structuredContent"]["status"], "completed");
    assert_eq!(get_out_after["result"]["structuredContent"]["exit_code"], 0);
    assert!(get_out_after["result"]["structuredContent"]["output"].as_str().unwrap().contains("ROUNDTRIP_STDIO_OK"));

    let final_list = stdio_call_with_meta(
        &mut stdin,
        &mut reader,
        req_id,
        "list_terminal_tasks",
        json!({}),
        Some(session.clone()),
    );
    assert_eq!(final_list["result"]["isError"], false);
    let final_tasks = final_list["result"]["structuredContent"]["tasks"].as_array().unwrap();
    let completed_task = final_tasks.iter().find(|t| t["task_id"] == short_id).expect("completed task must survive");
    assert_eq!(completed_task["completed"], true);
    assert_eq!(completed_task["status"], "completed");
    assert_eq!(completed_task["exit_code"], 0);
    assert_eq!(
        dunce::canonicalize(completed_task["cwd"].as_str().unwrap()).unwrap().display().to_string(),
        expected_short_cwd,
        "completed history must retain exact original CWD across workspace switches"
    );
    assert!(completed_task["output_file"].is_string());
    assert!(completed_task["total_bytes"].is_number());
}

#[test]
#[serial]
fn test_public_stdio_run_command_large_output_bounding_and_drain() {
    let (_guard, mut stdin, mut reader, _config, _workspace) = spawn_stdio_hands(&[]);

    let python_cmd = if Command::new("python").arg("--version").output().is_ok() {
        "python"
    } else if Command::new("python3").arg("--version").output().is_ok() {
        "python3"
    } else {
        "python"
    };

    let script = "import sys\nchunk = b'X' * 65536\nfor _ in range(144):\n    sys.stdout.buffer.write(chunk)\nsys.stdout.buffer.flush()\n";
    let r = stdio_call(
        &mut stdin,
        &mut reader,
        10,
        "run_command",
        json!({
            "command": python_cmd,
            "args": ["-c", script]
        }),
    );
    assert_eq!(r["result"]["isError"], false);
    let structured = &r["result"]["structuredContent"];
    assert_eq!(structured["execution_state"], "completed");
    assert_eq!(structured["exit_code"], 0);
    assert_eq!(structured["stdout_truncated"], true);
    assert_eq!(structured["stderr_truncated"], false);
    let stdout = structured["stdout"].as_str().unwrap();
    assert!(stdout.len() <= hands::run_command::MAX_OUTPUT_BYTES);
    let total = structured["stdout_total_bytes"].as_u64().unwrap();
    assert_eq!(total, 144 * 65536);
    let c1 = r["result"]["content"][0]["text"].as_str().expect("c1 content text");
    assert!(
        c1.len() <= hands::run_command::MAX_FALLBACK_CONTENT_BYTES + 300,
        "content[0].text must be concise under MAX_FALLBACK_CONTENT_BYTES, got {}",
        c1.len()
    );
    assert!(c1.contains("[stdout truncated:"), "c1 must carry stdout truncation evidence: {c1}");
    assert!(c1.contains("exit: 0"), "c1 must carry exit status: {c1}");

    let script_both = "import sys; sys.stdout.write('A' * 30000); sys.stderr.write('B' * 30000)";
    let r2 = stdio_call(
        &mut stdin,
        &mut reader,
        11,
        "run_command",
        json!({
            "command": python_cmd,
            "args": ["-c", script_both]
        }),
    );
    assert_eq!(r2["result"]["isError"], false);
    let s2 = &r2["result"]["structuredContent"];
    assert_eq!(s2["stdout_truncated"], true);
    assert_eq!(s2["stderr_truncated"], true);
    assert_eq!(s2["stdout_total_bytes"], 30000);
    assert_eq!(s2["stderr_total_bytes"], 30000);
    let out_len = s2["stdout"].as_str().unwrap().len();
    let err_len = s2["stderr"].as_str().unwrap().len();
    assert!(out_len + err_len <= hands::run_command::MAX_OUTPUT_BYTES + 100);
    let c2 = r2["result"]["content"][0]["text"].as_str().expect("c2 content text");
    assert!(
        c2.len() <= hands::run_command::MAX_FALLBACK_CONTENT_BYTES + 300,
        "c2 content text must be concise under MAX_FALLBACK_CONTENT_BYTES, got {}",
        c2.len()
    );
    assert!(c2.contains("[stdout truncated:"), "c2 must carry stdout truncation evidence: {c2}");
    assert!(c2.contains("[stderr truncated:"), "c2 must carry stderr truncation evidence: {c2}");
    assert!(c2.contains("exit: 0"), "c2 must carry exit status: {c2}");

    // Stderr-only large output case
    let script_stderr = "import sys\nchunk = b'E' * 65536\nfor _ in range(144):\n    sys.stderr.buffer.write(chunk)\nsys.stderr.buffer.flush()\n";
    let r3 = stdio_call(
        &mut stdin,
        &mut reader,
        12,
        "run_command",
        json!({
            "command": python_cmd,
            "args": ["-c", script_stderr]
        }),
    );
    assert_eq!(r3["result"]["isError"], false);
    let s3 = &r3["result"]["structuredContent"];
    assert_eq!(s3["execution_state"], "completed");
    assert_eq!(s3["exit_code"], 0);
    assert_eq!(s3["stdout_truncated"], false);
    assert_eq!(s3["stderr_truncated"], true);
    assert_eq!(s3["stdout_total_bytes"], 0);
    assert_eq!(s3["stderr_total_bytes"], 144 * 65536);
    let err_str = s3["stderr"].as_str().unwrap();
    assert!(err_str.len() <= hands::run_command::MAX_OUTPUT_BYTES + 100);
    let c3 = r3["result"]["content"][0]["text"].as_str().expect("c3 content text");
    assert!(
        c3.len() <= hands::run_command::MAX_FALLBACK_CONTENT_BYTES + 300,
        "c3 content text must be concise under MAX_FALLBACK_CONTENT_BYTES, got {}",
        c3.len()
    );
    assert!(c3.contains("[stderr truncated:"), "c3 must carry stderr truncation evidence: {c3}");
    assert!(c3.contains("exit: 0"), "c3 must carry exit status: {c3}");
}

#[test]
#[serial]
fn test_public_stdio_run_command_pre_spawn_rejects_cmd_and_bat() {
    let (_guard, mut stdin, mut reader, _config, workspace) = spawn_stdio_hands(&[]);

    let cmd_marker = workspace.path().join("cmd_marker.txt");
    let bat_marker = workspace.path().join("bat_marker.txt");

    let cmd_file = workspace.path().join("evil.cmd");
    std::fs::write(&cmd_file, format!("echo EVIL > {}\n", cmd_marker.display())).unwrap();

    let bat_file = workspace.path().join("evil.bat");
    std::fs::write(&bat_file, format!("echo EVIL > {}\n", bat_marker.display())).unwrap();

    for script in [&cmd_file, &bat_file] {
        let r = stdio_call(
            &mut stdin,
            &mut reader,
            20,
            "run_command",
            json!({
                "command": script.to_str().unwrap(),
                "args": []
            }),
        );
        assert_eq!(r["result"]["isError"], true);
        let structured = &r["result"]["structuredContent"];
        assert_eq!(structured["execution_state"], "not_started");
        assert_eq!(structured["command_started"], false);
        assert_eq!(structured["command_completed"], false);
        assert_eq!(structured["exit_code"], Value::Null);
    }

    assert!(!cmd_marker.exists(), "cmd script must never be spawned");
    assert!(!bat_marker.exists(), "bat script must never be spawned");
}
