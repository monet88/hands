//! Direct foreground process execution. No shell.
//! Spawns an executable with an ordered argv vector, captures stdout+stderr,
//! supports bounded timeout, optional working directory, stdin, and env overrides.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::plugin;

pub const TOOL_NAME: &str = "run_command";

pub const TOOL_DESCRIPTION: &str = "\
Execute a native CLI process directly with an ordered argument vector, bypassing \
any shell. The executable path and each argument are passed to the OS verbatim — \
no shell quoting, no shell metacharacter expansion, and no shell interpretation. \
Use run_terminal_cmd when shell semantics (pipes, globs, redirection, batch scripts) \
are required. Shell scripts (.cmd, .bat) are rejected. Pre-spawn validation failures \
report execution_state=not_started. Completed commands report execution_state=completed. \
Timed out commands report execution_state=timed_out once process cleanup is proven.";
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const MAX_STDIN_BYTES: usize = 1024 * 1024; // 1 MB
pub const MAX_OUTPUT_BYTES: usize = 40_000; // 40 KB
pub const MAX_RAW_OUTPUT_BYTES: usize = 8 * 1024 * 1024; // 8 MB

pub fn tool_descriptor() -> Value {
    plugin::tool_descriptor(TOOL_NAME, TOOL_DESCRIPTION, input_schema())
}

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Executable path or binary name to run. Launched directly without shell routing."
            },
            "args": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Ordered argument vector, passed verbatim to the OS. Each element is a distinct literal argv entry."
            },
            "workdir": {
                "type": "string",
                "description": "Optional working directory. Defaults to the active workspace."
            },
            "stdin": {
                "type": "string",
                "description": "Optional bounded input string piped to the child process stdin."
            },
            "timeout_ms": {
                "type": "integer",
                "description": "Optional total process runtime deadline in milliseconds (default 120000, max 600000). Distinct from foreground wait budgets: this deadline applies to the process runtime."
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

pub struct ValidatedInput {
    pub command: String,
    pub args: Vec<String>,
    pub workdir: Option<PathBuf>,
    pub stdin: Option<String>,
    pub timeout: Duration,
    pub env: HashMap<String, String>,
}

fn validate_input(params: &Value, default_workdir: &Path) -> Result<ValidatedInput, String> {
    let cmd_val = match params.get("command") {
        Some(Value::String(s)) => s.trim(),
        Some(_) => return Err("command must be a string".into()),
        None => return Err("command must not be empty".into()),
    };
    if cmd_val.is_empty() {
        return Err("command must not be empty".into());
    }
    if cmd_val.len() > 1024 {
        return Err("command name exceeds maximum length of 1024 characters".into());
    }

    // Reject shell-only scripts on Windows / cross-platform
    let lower = cmd_val.to_ascii_lowercase();
    if lower.ends_with(".cmd") || lower.ends_with(".bat") {
        return Err(format!(
            "Shell script '{}' cannot be launched directly with run_command. Use run_terminal_cmd when shell semantics are required.",
            cmd_val
        ));
    }

    let mut args = Vec::new();
    if let Some(args_val) = params.get("args") {
        if !args_val.is_null() {
            let arr = args_val.as_array().ok_or("args must be an array of strings")?;
            if arr.len() > 2000 {
                return Err("args array exceeds maximum length of 2000 items".into());
            }
            for item in arr {
                let s = item.as_str().ok_or("all args items must be strings")?;
                args.push(s.to_string());
            }
        }
    }

    let workdir = match params.get("workdir") {
        Some(Value::Null) | None => None,
        Some(Value::String(wd_val)) => {
            let s = wd_val.trim();
            if s.is_empty() {
                return Err("workdir must not be empty".into());
            }
            let p = PathBuf::from(s);
            let resolved = if p.is_absolute() {
                p
            } else {
                default_workdir.join(p)
            };
            if !resolved.is_dir() {
                return Err(format!("workdir '{}' is not a valid directory", resolved.display()));
            }
            Some(dunce::canonicalize(&resolved).unwrap_or(resolved))
        }
        Some(_) => return Err("workdir must be a string".into()),
    };

    let stdin = match params.get("stdin") {
        Some(Value::Null) | None => None,
        Some(Value::String(stdin_val)) => {
            if stdin_val.len() > MAX_STDIN_BYTES {
                return Err(format!("stdin exceeds maximum size limit of {} bytes", MAX_STDIN_BYTES));
            }
            Some(stdin_val.clone())
        }
        Some(_) => return Err("stdin must be a string".into()),
    };

    let timeout_val = params.get("timeout_ms").or_else(|| params.get("timeout"));
    let timeout_ms = match timeout_val {
        Some(Value::Null) | None => DEFAULT_TIMEOUT_MS,
        Some(v) => {
            let n = v.as_u64().ok_or("timeout_ms must be a positive integer")?;
            if n == 0 {
                return Err("timeout_ms must be greater than 0".into());
            }
            if n > MAX_TIMEOUT_MS {
                return Err(format!("timeout_ms exceeds maximum limit of {} ms", MAX_TIMEOUT_MS));
            }
            n
        }
    };
    let timeout = Duration::from_millis(timeout_ms);

    let mut env = HashMap::new();
    if let Some(env_val) = params.get("env") {
        if !env_val.is_null() {
            let obj = env_val.as_object().ok_or("env must be a JSON object of key-value string pairs")?;
            for (k, v) in obj {
                let v_str = v.as_str().ok_or(format!("env value for key '{}' must be a string", k))?;
                env.insert(k.clone(), v_str.to_string());
            }
        }
    }

    Ok(ValidatedInput {
        command: cmd_val.to_string(),
        args,
        workdir,
        stdin,
        timeout,
        env,
    })
}

async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    max_bytes: usize,
    capture: Arc<Mutex<(Vec<u8>, bool)>>,
) {
    let mut chunk = [0u8; 65536];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let mut guard = capture.lock().unwrap();
                let remaining = max_bytes.saturating_sub(guard.0.len());
                if remaining > 0 {
                    let to_take = n.min(remaining);
                    guard.0.extend_from_slice(&chunk[..to_take]);
                    if n > remaining {
                        guard.1 = true;
                    }
                } else {
                    guard.1 = true;
                }
            }
            Err(_) => break,
        }
    }
}

pub async fn execute(params: &Value, active_workspace: &Path) -> Value {
    let validated = match validate_input(params, active_workspace) {
        Ok(v) => v,
        Err(err_msg) => {
            return json!({
                "content": [{ "type": "text", "text": format!("Validation failed: {err_msg}") }],
                "structuredContent": {
                    "execution_state": "not_started",
                    "command_started": false,
                    "command_completed": false,
                    "exit_code": Value::Null,
                    "error": err_msg,
                    "cwd": active_workspace.display().to_string(),
                    "default_workspace": active_workspace.display().to_string()
                },
                "isError": true
            });
        }
    };

    let cwd = validated.workdir.unwrap_or_else(|| active_workspace.to_path_buf());

    let mut cmd = Command::new(&validated.command);
    cmd.args(&validated.args)
        .current_dir(&cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if validated.stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }

    for (k, v) in &validated.env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let err_msg = format!("Failed to spawn executable '{}': {e}", validated.command);
            return json!({
                "content": [{ "type": "text", "text": err_msg }],
                "structuredContent": {
                    "execution_state": "not_started",
                    "command_started": false,
                    "command_completed": false,
                    "exit_code": Value::Null,
                    "error": err_msg,
                    "cwd": cwd.display().to_string(),
                    "default_workspace": active_workspace.display().to_string()
                },
                "isError": true
            });
        }
    };

    if let Some(input_text) = validated.stdin {
        if let Some(mut stdin_pipe) = child.stdin.take() {
            tokio::spawn(async move {
                let _ = stdin_pipe.write_all(input_text.as_bytes()).await;
                let _ = stdin_pipe.flush().await;
            });
        }
    }

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();

    let stdout_capture = Arc::new(Mutex::new((Vec::new(), false)));
    let stdout_capture_clone = Arc::clone(&stdout_capture);
    let stdout_handle = tokio::spawn(async move {
        if let Some(r) = stdout_pipe {
            read_bounded(r, MAX_RAW_OUTPUT_BYTES, stdout_capture_clone).await;
        }
    });

    let stderr_capture = Arc::new(Mutex::new((Vec::new(), false)));
    let stderr_capture_clone = Arc::clone(&stderr_capture);
    let stderr_handle = tokio::spawn(async move {
        if let Some(r) = stderr_pipe {
            read_bounded(r, MAX_RAW_OUTPUT_BYTES, stderr_capture_clone).await;
        }
    });

    let wait_res = tokio::time::timeout(validated.timeout, child.wait()).await;

    let (exit_code, timed_out, execution_state, command_completed) = match wait_res {
        Ok(Ok(status)) => (status.code().unwrap_or(-1), false, "completed", true),
        Ok(Err(e)) => {
            stdout_handle.abort();
            stderr_handle.abort();
            let err_msg = format!("Process wait error: {e}");
            return json!({
                "content": [{ "type": "text", "text": err_msg }],
                "structuredContent": {
                    "execution_state": "outcome_unknown",
                    "command_started": true,
                    "command_completed": false,
                    "exit_code": Value::Null,
                    "error": err_msg,
                    "cwd": cwd.display().to_string(),
                    "default_workspace": active_workspace.display().to_string()
                },
                "isError": true
            });
        }
        Err(_) => {
            let kill_res = child.kill().await;
            let wait_after_kill = child.wait().await;
            if kill_res.is_ok() && wait_after_kill.is_ok() {
                (-1, true, "timed_out", false)
            } else {
                (-1, true, "outcome_unknown", false)
            }
        }
    };

    if timed_out {
        stdout_handle.abort();
        stderr_handle.abort();
    } else {
        let join_timeout = Duration::from_millis(500);
        let _ = tokio::time::timeout(join_timeout, stdout_handle).await;
        let _ = tokio::time::timeout(join_timeout, stderr_handle).await;
    }

    let (stdout_raw, stdout_truncated) = {
        let guard = stdout_capture.lock().unwrap();
        (guard.0.clone(), guard.1)
    };
    let (stderr_raw, stderr_truncated) = {
        let guard = stderr_capture.lock().unwrap();
        (guard.0.clone(), guard.1)
    };

    let stdout_str = String::from_utf8_lossy(&stdout_raw).into_owned();
    let stderr_str = String::from_utf8_lossy(&stderr_raw).into_owned();

    let mut summary_lines = Vec::new();
    if timed_out {
        summary_lines.push(format!("Command timed out after {}ms", validated.timeout.as_millis()));
    }
    if !stdout_str.is_empty() {
        let text = if stdout_str.len() > MAX_OUTPUT_BYTES {
            crate::mcp::truncate_output_text(&stdout_str, MAX_OUTPUT_BYTES, "")
        } else {
            stdout_str.clone()
        };
        summary_lines.push(text);
    }
    if !stderr_str.is_empty() {
        let text = if stderr_str.len() > MAX_OUTPUT_BYTES {
            crate::mcp::truncate_output_text(&stderr_str, MAX_OUTPUT_BYTES, "")
        } else {
            stderr_str.clone()
        };
        summary_lines.push(format!("stderr: {text}"));
    }
    summary_lines.push(format!("exit: {exit_code}"));

    let combined = summary_lines.join("\n");
    let content_text = if combined.len() > MAX_OUTPUT_BYTES {
        crate::mcp::truncate_output_text(&combined, MAX_OUTPUT_BYTES, "")
    } else {
        combined
    };
    json!({
        "content": [{ "type": "text", "text": content_text }],
        "structuredContent": {
            "execution_state": execution_state,
            "command_started": true,
            "command_completed": command_completed,
            "exit_code": exit_code,
            "timed_out": timed_out,
            "stdout": stdout_str,
            "stderr": stderr_str,
            "stdout_truncated": stdout_truncated,
            "stderr_truncated": stderr_truncated,
            "cwd": cwd.display().to_string(),
            "default_workspace": active_workspace.display().to_string()
        },
        "isError": false
    })
}
