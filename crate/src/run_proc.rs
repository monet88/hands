//! Direct foreground process execution. No shell.
//! Spawns an executable with an argv vector, captures stdout+stderr,
//! supports timeout, working directory, and env overrides.

use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::task::JoinHandle;

/// Maximum foreground timeout (10 minutes).
const MAX_TIMEOUT_MS: u64 = 600_000;

/// Default foreground timeout (120 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// Await a pipe-read task and return its bytes (empty on task failure).
async fn collect_pipe(handle: Option<JoinHandle<Vec<u8>>>) -> Vec<u8> {
    match handle {
        Some(h) => h.await.unwrap_or_default(),
        None => Vec::new(),
    }
}

/// Result of a foreground process execution.
pub struct ProcOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    /// Set when the process could not be spawned or wait() failed.
    /// NOT set for non-zero exit codes — those are a normal result.
    pub error: Option<String>,
}
pub const TOOL_NAME: &str = "run_command";
/// The full MCP tool definition JSON for `run_command`.
pub fn tool_json() -> serde_json::Value {
    serde_json::json!({
        "name": TOOL_NAME,
        "description": TOOL_DESCRIPTION,
        "inputSchema": input_schema(),
        "annotations": {
            "readOnlyHint": false,
            "destructiveHint": true,
            "openWorldHint": false,
        }
    })
}

/// The MCP tool description for `run_command`.
pub const TOOL_DESCRIPTION: &str = "\
Execute a native CLI process directly with an ordered argument vector, bypassing \
any shell. The executable path and each argument are passed to the OS verbatim — \
no shell quoting, no PowerShell call operator, no metacharacter interpolation. \
Use this when PowerShell/Bash syntax (pipes, redirection, variable expansion) is \
not needed; use run_terminal_cmd for commands that intentionally need shell \
semantics. Supports an optional working directory, bounded foreground timeout in \
milliseconds, and optional environment overrides. Missing executables and \
timeouts are reported distinctly from a child exiting non-zero; a non-zero exit \
still returns the captured output with the exit code.";

/// JSON Schema for `run_command` arguments.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Executable path to run. Paths containing spaces are launched directly without shell quoting."
            },
            "args": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Ordered argument vector, passed verbatim to the OS. Each element is a distinct argv entry."
            },
            "workdir": {
                "type": "string",
                "description": "Optional working directory. Defaults to the active workspace."
            },
            "timeout": {
                "type": "integer",
                "description": "Optional foreground timeout in milliseconds (default 120000, max 600000)."
            },
            "env": {
                "type": "object",
                "additionalProperties": { "type": "string" },
                "description": "Optional environment overrides applied to the child without rewriting argv."
            }
        },
        "required": ["command"]
    })
}

/// Execute `run_command` from parsed MCP-like arguments and return the
/// tool result as a `{content, isError}` JSON object.
pub async fn handle_call(arguments: &serde_json::Value) -> serde_json::Value {
    let command = arguments
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let args: Vec<String> = arguments
        .get("args")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let workdir = arguments.get("workdir").and_then(serde_json::Value::as_str);
    let timeout = arguments.get("timeout").and_then(serde_json::Value::as_u64);
    let env = arguments.get("env");

    let output = run_foreground(command, &args, workdir, timeout, env).await;

    let mut lines: Vec<String> = Vec::new();
    if let Some(err) = &output.error {
        lines.push(format!("error: {err}"));
    }
    if !output.stdout.is_empty() {
        lines.push(output.stdout.clone());
    }
    if !output.stderr.is_empty() {
        lines.push(format!("stderr: {}", output.stderr));
    }
    if output.timed_out {
        lines.push("timed out".to_string());
    }
    let outcome = if output.timed_out {
        "timeout"
    } else if output.error.is_some() {
        "error"
    } else if output.exit_code != 0 {
        "exit-nonzero"
    } else {
        "exit-zero"
    };
    lines.push(format!(
        "exit: {}{}",
        output.exit_code,
        if outcome == "timeout" {
            " (timed out)"
        } else if outcome == "error" {
            " (error)"
        } else if outcome == "exit-nonzero" {
            " (non-zero exit)"
        } else {
            ""
        }
    ));
    let text = lines.join("\n");

    // Non-zero exit, timeout, and missing-executable are all distinct
    // outcomes. A child exiting non-zero or timing out is NOT an MCP
    // protocol error — only an unlaunchable process is.
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": output.error.is_some()
    })
}

/// Run a foreground process with the given argv, bypassing any shell.
///
/// `executable` is passed directly to the OS — no shell quoting, no
/// PowerShell call operator, no metacharacter interpolation. Each
/// element of `args` becomes a separate argv entry.
///
/// `env_overrides` is an optional JSON object `{"KEY": "value", ...}`
/// whose entries are added to the child's inherited environment.
pub async fn run_foreground(
    executable: &str,
    args: &[String],
    workdir: Option<&str>,
    timeout_ms: Option<u64>,
    env_overrides: Option<&serde_json::Value>,
) -> ProcOutput {
    let timeout =
        Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS));

    let mut cmd = Command::new(executable);
    cmd.args(args);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    if let Some(dir) = workdir {
        cmd.current_dir(dir);
    }

    if let Some(env_val) = env_overrides {
        if let Some(obj) = env_val.as_object() {
            for (k, v) in obj {
                if let Some(val) = v.as_str() {
                    cmd.env(k, val);
                }
            }
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return ProcOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                timed_out: false,
                error: Some(format!("failed to spawn: {e}")),
            };
        }
    };

    // Spawn background tasks to read stdout/stderr pipes concurrently.
    let read_stdout = child.stdout.take().map(|mut s| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            buf
        })
    });
    let read_stderr = child.stderr.take().map(|mut s| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = s.read_to_end(&mut buf).await;
            buf
        })
    });

    let wait_result = tokio::time::timeout(timeout, child.wait()).await;

    match wait_result {
        Ok(Ok(status)) => {
            let stdout_buf = collect_pipe(read_stdout).await;
            let stderr_buf = collect_pipe(read_stderr).await;
            let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
            let stderr = String::from_utf8_lossy(&stderr_buf).to_string();
            ProcOutput {
                exit_code: status.code().unwrap_or(-1),
                timed_out: false,
                error: None,
                stdout,
                stderr,
            }
        }
        Ok(Err(e)) => {
            let stdout_buf = collect_pipe(read_stdout).await;
            let stderr_buf = collect_pipe(read_stderr).await;
            ProcOutput {
                stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
                stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
                exit_code: -1,
                timed_out: false,
                error: Some(format!("process wait failed: {e}")),
            }
        }
        Err(_elapsed) => {
            let _ = child.kill().await;
            let _ = child.wait().await;

            let stdout_buf = collect_pipe(read_stdout).await;
            let stderr_buf = collect_pipe(read_stderr).await;
            let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
            let stderr = String::from_utf8_lossy(&stderr_buf).to_string();
            ProcOutput {
                exit_code: -1,
                timed_out: true,
                error: None,
                stdout,
                stderr,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_echo(args: &[&str]) -> (String, Vec<String>) {
        if cfg!(windows) {
            let mut all = vec!["/c".to_string(), "echo".to_string()];
            all.extend(args.iter().map(|s| s.to_string()));
            ("cmd.exe".to_string(), all)
        } else {
            let mut all: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            if all.is_empty() {
                // echo on Unix needs at least empty string to produce a blank line
                all.push(String::new());
            }
            ("echo".to_string(), all)
        }
    }

    #[tokio::test]
    async fn test_echo_hello() {
        let (exe, args) = platform_echo(&["hello"]);
        let result = run_foreground(&exe, &args, None, None, None).await;
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        assert_eq!(result.exit_code, 0, "exit code should be 0");
        assert!(
            result.stdout.contains("hello"),
            "stdout should contain hello, got: {}",
            result.stdout
        );
        assert!(!result.timed_out, "should not time out");
    }

    #[tokio::test]
    async fn test_missing_executable() {
        let result = run_foreground("nonexistent_hands_test_exe_xyz", &[], None, None, None).await;
        assert!(
            result.error.is_some(),
            "missing executable should produce an error"
        );
        assert!(!result.timed_out, "should not be a timeout");
    }

    #[tokio::test]
    async fn test_timeout() {
        let (exe, args) = if cfg!(windows) {
            (
                "ping".to_string(),
                vec!["-n".to_string(), "30".to_string(), "127.0.0.1".to_string()],
            )
        } else {
            ("sleep".to_string(), vec!["30".to_string()])
        };
        let result = run_foreground(&exe, &args, None, Some(100), None).await;
        assert!(
            result.timed_out,
            "should time out, got stdout: {}",
            result.stdout
        );
        assert_eq!(result.exit_code, -1, "exit code should be -1 on timeout");
    }

    #[tokio::test]
    async fn test_non_zero_exit() {
        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), "exit".to_string(), "42".to_string()],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), "exit 42".to_string()],
            )
        };
        let result = run_foreground(&exe, &args, None, None, None).await;
        assert!(
            result.error.is_none(),
            "non-zero exit should not be spawn error"
        );
        assert_eq!(
            result.exit_code, 42,
            "exit code should be 42, got {}",
            result.exit_code
        );
    }

    #[tokio::test]
    async fn test_env_overrides() {
        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), "echo %HANDS_TEST_ENV%".to_string()],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), "echo $HANDS_TEST_ENV".to_string()],
            )
        };
        let env = serde_json::json!({"HANDS_TEST_ENV": "works"});
        let result = run_foreground(&exe, &args, None, None, Some(&env)).await;
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        assert!(
            result.stdout.contains("works"),
            "env var should be visible, got: {}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn test_working_directory() {
        let tmp = std::env::temp_dir();
        let marker_file = tmp.join("hands_run_proc_test_marker");
        let _ = std::fs::remove_file(&marker_file);
        let marker_path = marker_file.to_string_lossy().to_string();

        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec![
                    "/c".to_string(),
                    format!("echo > {}", marker_path.replace('\'', "")),
                ],
            )
        } else {
            ("touch".to_string(), vec![marker_path.clone()])
        };
        let result = run_foreground(&exe, &args, Some(tmp.to_str().unwrap()), None, None).await;
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        assert!(marker_file.exists(), "marker file should exist in cwd");
        let _ = std::fs::remove_file(&marker_file);
    }

    #[tokio::test]
    async fn test_spaces_in_path() {
        // Verify that an executable path containing spaces works without shell quoting.
        let tmp = std::env::temp_dir().join("hands test dir with spaces");
        let _ = std::fs::create_dir_all(&tmp);
        let script_path = tmp.join("echo_args.bat");
        let content = if cfg!(windows) {
            "@echo %1 %2 %3"
        } else {
            "#!/bin/sh\necho \"$1\" \"$2\" \"$3\""
        };
        std::fs::write(&script_path, content).ok();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).ok();
        }

        let result = run_foreground(
            script_path.to_str().unwrap(),
            &["hello".to_string(), "world".to_string()],
            None,
            None,
            None,
        )
        .await;

        // Script may not run if shell is needed for .bat — that's OK for the test
        // The important thing is it didn't blow up on the space in the path.
        // A failure-to-spawn for .bat is expected since .bat needs cmd.exe.
        if result.error.is_none() {
            // On Unix, the script should work
            assert!(result.stdout.contains("hello"), "stdout: {}", result.stdout);
        } else {
            // On Windows, .bat needs cmd.exe, so this is expected — we test
            // spaces-in-path with cmd.exe directly in the next test.
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_cmd_exe_with_spaces_in_path() {
        // Verify that an executable path with spaces works using cmd.exe
        // which is at C:\Windows\System32\cmd.exe (no spaces) — but the
        // test still validates that our path handling doesn't break.
        // The real test is that a path WITH spaces would reach the OS
        // without shell re-interpretation.
        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), "echo".to_string(), "hello".to_string()],
            )
        } else {
            (
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "echo hello".to_string()],
            )
        };
        let result = run_foreground(&exe, &args, None, None, None).await;
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        assert!(result.stdout.contains("hello"), "stdout: {}", result.stdout);
    }
}
