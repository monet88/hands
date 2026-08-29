//! Direct foreground process execution. No shell.
//! Spawns an executable with an argv vector, captures stdout+stderr,
//! supports timeout, working directory, and env overrides.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

/// Maximum foreground timeout (10 minutes).
const MAX_TIMEOUT_MS: u64 = 600_000;

/// Default foreground timeout (120 seconds).
const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// Timeout for draining pipe reads after process termination. Descendants
/// may inherit pipe handles, so we cannot wait indefinitely.
const BOUNDED_DRAIN_TIMEOUT_MS: u64 = 500;

/// Output character cap (bytes). Matching the terminal tool's bounded-output
/// rule: the combined stdout+stderr response is bounded, and oversized
/// output is persisted to a temp log file for later retrieval.
const OUTPUT_BOUND: usize = 40_000;

/// Hard cap on raw bytes buffered per stream before truncation. Prevents a
/// runaway child from exhausting process memory while still capturing
/// enough to persist a useful log.
const MAX_RAW_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Job Object for Windows process-tree lifetime ownership. When a process
/// is killed, all descendants inherit the pipe handles and keep the read
/// ends open indefinitely. The Job Object ensures the entire tree is
/// terminated on timeout, so pipe reads complete promptly.
#[cfg(windows)]
mod job_object {
    type HANDLE = *mut std::ffi::c_void;
    type BOOL = i32;

    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_SET_QUOTA: u32 = 0x0100;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(lpJobAttributes: *const std::ffi::c_void, lpName: *const u16)
        -> HANDLE;
        fn AssignProcessToJobObject(hJob: HANDLE, hProcess: HANDLE) -> BOOL;
        fn TerminateJobObject(hJob: HANDLE, uExitCode: u32) -> BOOL;
        fn CloseHandle(hObject: HANDLE) -> BOOL;
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: BOOL, dwProcessId: u32) -> HANDLE;
    }

    // Job object handles are thread-safe; the raw HANDLE pointer is
    // only passed to kernel32 functions that accept it from any thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    pub struct JobObject(HANDLE);

    impl JobObject {
        pub fn create() -> Option<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            Some(Self(handle))
        }

        pub fn assign_process(&self, pid: u32) -> bool {
            let process = unsafe { OpenProcess(PROCESS_TERMINATE | PROCESS_SET_QUOTA, 0, pid) };
            if process.is_null() {
                return false;
            }
            let ok = unsafe { AssignProcessToJobObject(self.0, process) != 0 };
            unsafe { CloseHandle(process) };
            ok
        }

        pub fn terminate(&self) -> bool {
            unsafe { TerminateJobObject(self.0, 1) != 0 }
        }
    }

    impl Drop for JobObject {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[cfg(not(windows))]
mod job_object {
    pub struct JobObject;
}

/// Create a Job Object and assign the child process to it.
/// Returns `None` on non-Windows or if creation/assignment fails.
#[cfg(windows)]
fn create_job_for_child(pid: u32) -> Option<job_object::JobObject> {
    let j = job_object::JobObject::create()?;
    // If assignment fails, the process is not owned by the job; returning
    // None means no tree-termination guarantee is claimed, so the caller
    // falls back to root-only kill + bounded drain.
    if !j.assign_process(pid) {
        return None;
    }
    Some(j)
}

#[cfg(not(windows))]
fn create_job_for_child(_pid: u32) -> Option<job_object::JobObject> {
    None
}

struct PipeCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

impl PipeCapture {
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
        }
    }
}

/// Keep a bounded in-memory prefix while continuing to drain the pipe to EOF.
/// Closing the read end at the memory cap would surface as BrokenPipe/EPIPE in
/// the child and incorrectly change its exit status, so excess bytes are
/// deliberately discarded rather than stopping the read.
async fn read_capped_pipe<R>(mut reader: R) -> PipeCapture
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => break,
        };
        let remaining = MAX_RAW_OUTPUT_BYTES.saturating_sub(bytes.len());
        if remaining > 0 {
            let keep = remaining.min(read);
            bytes.extend_from_slice(&chunk[..keep]);
            if keep < read {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    PipeCapture { bytes, truncated }
}

/// Result of a foreground process execution.
pub struct ProcOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    /// True when raw stdout/stderr exceeded the in-memory capture cap. The
    /// child was still fully drained so its exit semantics were preserved.
    pub capture_truncated: bool,
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

/// Truncate `s` to at most `max_bytes` bytes at a UTF-8 char boundary.
/// `String::truncate` panics if the index is not on a char boundary, so
/// oversized multibyte output must be cut with this helper.
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let end = s.floor_char_boundary(max_bytes);
    s[..end].to_string()
}

/// Persist the full captured output to a temp log file so the operator can
/// retrieve what was truncated from the bounded tool response. Returns the
/// log path on success.
fn write_temp_log(stdout: &str, stderr: &str) -> Option<String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let dir = std::env::temp_dir().join("hands").join("run-command");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("output-{}-{ts}.log", std::process::id()));
    let mut file = std::fs::File::create(&path).ok()?;
    use std::io::Write as _;
    file.write_all(b"== stdout ==\n").ok()?;
    file.write_all(stdout.as_bytes()).ok()?;
    if !stderr.is_empty() {
        file.write_all(b"\n== stderr ==\n").ok()?;
        file.write_all(stderr.as_bytes()).ok()?;
    }
    Some(path.to_string_lossy().to_string())
}

fn exit_line(output: &ProcOutput) -> String {
    let outcome = if output.timed_out {
        "timeout"
    } else if output.error.is_some() {
        "error"
    } else if output.exit_code != 0 {
        "exit-nonzero"
    } else {
        "exit-zero"
    };
    format!(
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
    )
}

fn render_tool_text(output: &ProcOutput) -> String {
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
    lines.push(exit_line(output));
    lines.join("\n")
}

fn render_bounded_tool_text(output: &ProcOutput, log_path: Option<&str>) -> String {
    let full = render_tool_text(output);
    if full.as_bytes().len() <= OUTPUT_BOUND {
        return full;
    }

    let footer = match (log_path, output.capture_truncated) {
        (Some(path), false) => {
            format!("... (output truncated) ...\n[truncated - full output: {path}; use read_file]")
        }
        (Some(path), true) => format!(
            "... (output truncated) ...\n[truncated - captured output: {path}; raw capture capped at {MAX_RAW_OUTPUT_BYTES} bytes; use read_file]"
        ),
        (None, _) => {
            "... (output truncated) ...\n[truncated - full output log unavailable]".to_string()
        }
    };

    let mut fixed_before = Vec::new();
    if let Some(err) = &output.error {
        fixed_before.push(format!("error: {err}"));
    }
    let mut fixed_after = vec![footer];
    if output.timed_out {
        fixed_after.push("timed out".to_string());
    }
    fixed_after.push(exit_line(output));

    let fixed_bytes: usize = fixed_before
        .iter()
        .chain(fixed_after.iter())
        .map(|part| part.as_bytes().len())
        .sum();
    // Reserve separators between all fixed parts plus the preview. Eight
    // bytes is intentionally conservative and keeps the final response
    // strictly within OUTPUT_BOUND even when every optional part is present.
    let preview_budget = OUTPUT_BOUND.saturating_sub(fixed_bytes + 8);

    let mut stream_text = String::new();
    if !output.stdout.is_empty() {
        stream_text.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !stream_text.is_empty() {
            stream_text.push('\n');
        }
        stream_text.push_str("stderr: ");
        stream_text.push_str(&output.stderr);
    }
    let preview = truncate_utf8(&stream_text, preview_budget);

    let mut parts = fixed_before;
    if !preview.is_empty() {
        parts.push(preview);
    }
    parts.extend(fixed_after);
    let text = parts.join("\n");
    debug_assert!(text.as_bytes().len() <= OUTPUT_BOUND);
    text
}

/// Execute `run_command` from parsed MCP-like arguments and return the
/// tool result as a `{content, isError}` JSON object.
///
/// `workspace` is the resolved active workspace path (the default cwd).
/// It is used when the caller does not provide an explicit `workdir`.
pub async fn handle_call(
    arguments: &serde_json::Value,
    workspace: Option<&str>,
) -> serde_json::Value {
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
    let explicit_workdir = arguments.get("workdir").and_then(serde_json::Value::as_str);
    let workdir = explicit_workdir.or(workspace);
    let timeout = arguments.get("timeout").and_then(serde_json::Value::as_u64);
    let env = arguments.get("env");

    let output = run_foreground(command, &args, workdir, timeout, env).await;
    let unbounded = render_tool_text(&output);
    let text = if unbounded.as_bytes().len() > OUTPUT_BOUND {
        let log_path = write_temp_log(&output.stdout, &output.stderr);
        render_bounded_tool_text(&output, log_path.as_deref())
    } else {
        unbounded
    };

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
fn proc_output_from_captures(
    stdout: PipeCapture,
    stderr: PipeCapture,
    exit_code: i32,
    timed_out: bool,
    error: Option<String>,
) -> ProcOutput {
    ProcOutput {
        stdout: String::from_utf8_lossy(&stdout.bytes).to_string(),
        stderr: String::from_utf8_lossy(&stderr.bytes).to_string(),
        exit_code,
        timed_out,
        capture_truncated: stdout.truncated || stderr.truncated,
        error,
    }
}

/// Rejoin the pipe-drain task with a bounded wait. After the tree is
/// killed the pipes close promptly; the timeout guards stragglers so the
/// request can never hang.
async fn bounded_rejoin(
    drain: &mut JoinHandle<(PipeCapture, PipeCapture)>,
    drain_timeout_ms: u64,
) -> (PipeCapture, PipeCapture) {
    let drain_timeout = Duration::from_millis(drain_timeout_ms);
    match tokio::time::timeout(drain_timeout, &mut *drain).await {
        Ok(Ok(captures)) => captures,
        Ok(Err(_)) => (PipeCapture::empty(), PipeCapture::empty()),
        Err(_) => {
            drain.abort();
            let _ = drain.await;
            (PipeCapture::empty(), PipeCapture::empty())
        }
    }
}

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
                capture_truncated: false,
                error: Some(format!("failed to spawn: {e}")),
            };
        }
    };

    // Create Job Object for process-tree ownership (Windows). On timeout,
    // terminating the job kills all descendants, closing pipe handles.
    let job = child.id().and_then(create_job_for_child);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Drain both pipes inside one cancellable task so the deadline covers
    // them jointly. Each reader keeps only a bounded prefix but continues
    // consuming excess bytes to EOF so the child never sees a broken pipe.
    let mut drain = tokio::spawn(async move {
        let stdout_read = async {
            match stdout {
                Some(pipe) => read_capped_pipe(pipe).await,
                None => PipeCapture::empty(),
            }
        };
        let stderr_read = async {
            match stderr {
                Some(pipe) => read_capped_pipe(pipe).await,
                None => PipeCapture::empty(),
            }
        };
        tokio::join!(stdout_read, stderr_read)
    });

    // The overall deadline bounds the whole lifecycle: wait for the root,
    // then drain stdout/stderr. A descendant may inherit the pipe handles
    // and keep them open after the root exits; if the drain outlives the
    // deadline, terminate the tree so the pipes close and the drain ends.
    let deadline = tokio::time::Instant::now() + timeout;
    let wait_result = tokio::time::timeout_at(deadline, child.wait()).await;

    // Kill the whole process tree: the root and (on Windows) all Job
    // Object members, then reap. On non-Windows only the root is killed;
    // the bounded drain below prevents an indefinite hang.
    let kill_tree = async {
        let _ = child.kill().await;
        if let Some(j) = &job {
            j.terminate();
        }
        let _ = child.wait().await;
    };

    let status = match wait_result {
        Ok(Ok(status)) => {
            match tokio::time::timeout_at(deadline, &mut drain).await {
                Ok(Ok((stdout_capture, stderr_capture))) => proc_output_from_captures(
                    stdout_capture,
                    stderr_capture,
                    status.code().unwrap_or(-1),
                    false,
                    None,
                ),
                Ok(Err(_)) => ProcOutput {
                    exit_code: status.code().unwrap_or(-1),
                    timed_out: false,
                    capture_truncated: false,
                    error: Some("pipe drain task failed".to_string()),
                    stdout: String::new(),
                    stderr: String::new(),
                },
                Err(_elapsed) => {
                    // Root exited but the drain hit the overall deadline
                    // (a descendant is holding a pipe open). Terminate the
                    // tree, then collect what the drain produced.
                    kill_tree.await;
                    let (stdout_capture, stderr_capture) =
                        bounded_rejoin(&mut drain, BOUNDED_DRAIN_TIMEOUT_MS).await;
                    proc_output_from_captures(stdout_capture, stderr_capture, -1, true, None)
                }
            }
        }
        Ok(Err(e)) => match tokio::time::timeout_at(deadline, &mut drain).await {
            Ok(Ok((stdout_capture, stderr_capture))) => proc_output_from_captures(
                stdout_capture,
                stderr_capture,
                -1,
                false,
                Some(format!("process wait failed: {e}")),
            ),
            Ok(Err(_)) => ProcOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                timed_out: false,
                capture_truncated: false,
                error: Some("pipe drain task failed".to_string()),
            },
            Err(_elapsed) => {
                kill_tree.await;
                let (stdout_capture, stderr_capture) =
                    bounded_rejoin(&mut drain, BOUNDED_DRAIN_TIMEOUT_MS).await;
                proc_output_from_captures(stdout_capture, stderr_capture, -1, true, None)
            }
        },
        Err(_elapsed) => {
            // Root did not exit before the deadline.
            kill_tree.await;
            let (stdout_capture, stderr_capture) =
                bounded_rejoin(&mut drain, BOUNDED_DRAIN_TIMEOUT_MS).await;
            proc_output_from_captures(stdout_capture, stderr_capture, -1, true, None)
        }
    };

    status
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
    async fn test_timeout_descendant() {
        // Verify that a timeout terminates the process tree (not just the
        // root process) and the pipe drain is bounded. Without Job Object
        // or bounded drain, a descendant holding stdout would hang.
        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec![
                    "/c".to_string(),
                    // Spawn a background child inheriting stdout, then sleep
                    "start /b cmd.exe /c ping -n 600 127.0.0.1 & ping -n 600 127.0.0.1".to_string(),
                ],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), "sleep 600 & sleep 600".to_string()],
            )
        };
        let start = std::time::Instant::now();
        let result = run_foreground(&exe, &args, None, Some(200), None).await;
        let elapsed = start.elapsed();

        assert!(result.timed_out, "should time out");
        assert_eq!(result.exit_code, -1, "exit code -1 on timeout");
        assert!(
            elapsed < Duration::from_secs(2),
            "descendant timeout must stay close to the requested bound, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn test_timeout_descendant_root_exits() {
        // The root process exits immediately, but a descendant holds the
        // inherited stdout pipe past the deadline. The overall deadline
        // must still fire: terminate the tree and report timed_out. This
        // is the exact regression the review reproduced (3195ms, exit 0).
        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec![
                    "/c".to_string(),
                    "start /b cmd.exe /c ping -n 600 127.0.0.1 & echo PARENT_DONE".to_string(),
                ],
            )
        } else {
            (
                "sh".to_string(),
                vec!["-c".to_string(), "sleep 600 & echo PARENT_DONE".to_string()],
            )
        };
        let start = std::time::Instant::now();
        let result = run_foreground(&exe, &args, None, Some(200), None).await;
        let elapsed = start.elapsed();

        // The descendant keeps the pipe open, so this MUST be reported as
        // a timeout even though the root exited cleanly.
        assert!(
            result.timed_out,
            "descendant holding pipe past deadline must be a timeout"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "overall deadline must bound pipe drain, took {elapsed:?}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_failed_job_assignment_is_not_reported_as_owned() {
        assert!(
            create_job_for_child(u32::MAX).is_none(),
            "an invalid PID must not produce a JobObject that appears to own the process"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_timeout_terminates_descendant_process() {
        let python =
            crate::service::which("python").expect("Python is required by the Windows test gate");
        let root = std::env::temp_dir().join(format!(
            "hands_run_proc_tree_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create tree-kill fixture directory");
        let script = root.join("spawn_child.py");
        let pid_file = root.join("child.pid");
        std::fs::write(
            &script,
            "import pathlib, subprocess, sys, time\n\
             time.sleep(0.2)\n\
             child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n\
             pathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\n\
             time.sleep(30)\n",
        )
        .expect("write tree-kill fixture");

        let result = run_foreground(
            python.to_str().expect("python path utf-8"),
            &[
                script.to_string_lossy().to_string(),
                pid_file.to_string_lossy().to_string(),
            ],
            None,
            Some(700),
            None,
        )
        .await;
        assert!(result.timed_out, "fixture root should time out");

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("child PID fixture should be written before timeout")
            .trim()
            .parse()
            .expect("child PID must be numeric");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let tasklist = std::process::Command::new("tasklist.exe")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist descendant probe");
        let listing = String::from_utf8_lossy(&tasklist.stdout);
        let still_alive = listing.contains(&format!(",\"{pid}\","));
        if still_alive {
            let _ = std::process::Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .output();
        }
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            !still_alive,
            "timeout must terminate descendant PID {pid}; tasklist={listing}"
        );
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
        // Use a relative path to verify cwd is actually applied.
        let tmp = std::env::temp_dir().join("hands_run_proc_cwd_test");
        let _ = std::fs::create_dir_all(&tmp);
        let marker_rel = "cwd_marker.txt";
        let marker_file = tmp.join(marker_rel);
        let _ = std::fs::remove_file(&marker_file);

        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec!["/c".to_string(), format!("echo marker > {}", marker_rel)],
            )
        } else {
            ("touch".to_string(), vec![marker_rel.to_string()])
        };
        let result = run_foreground(&exe, &args, Some(tmp.to_str().unwrap()), None, None).await;
        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        assert!(
            marker_file.exists(),
            "marker file should exist in cwd: {:?}",
            marker_file
        );
        let _ = std::fs::remove_file(&marker_file);
        let _ = std::fs::remove_dir(&tmp);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_spaces_in_path() {
        // Verify executable path with spaces works. Copy a real executable
        // into a temp dir with spaces and run it directly.
        let tmp = std::env::temp_dir().join("hands test dir with spaces");
        let _ = std::fs::create_dir_all(&tmp);
        let spaced_exe = tmp.join("cmd.exe");
        std::fs::copy(r"C:\Windows\System32\cmd.exe", &spaced_exe)
            .expect("fixture: copy cmd.exe into spaced dir");
        let result = run_foreground(
            spaced_exe.to_str().unwrap(),
            &[
                "/c".to_string(),
                "echo".to_string(),
                "PATH_SPACE_OK".to_string(),
            ],
            None,
            None,
            None,
        )
        .await;
        assert!(
            result.error.is_none(),
            "executable path with spaces: {:?}",
            result.error
        );
        assert!(
            result.stdout.contains("PATH_SPACE_OK"),
            "stdout: {}",
            result.stdout
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_cmd_exe_basic() {
        // Verify a known-good executable works through run_foreground
        // without shell wrapping.
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

    #[tokio::test]
    async fn test_argv_preservation() {
        // Verify literal argv preservation using Python as an argv oracle.
        // Each argument is passed verbatim to the OS — no shell quoting,
        // no metacharacter interpolation.
        let python = if cfg!(windows) { "python" } else { "python3" };

        // Python is part of the repository's Windows verification baseline;
        // fail closed instead of silently skipping argv coverage.
        let check = run_foreground(python, &["--version".to_string()], None, None, None).await;
        assert!(
            check.error.is_none(),
            "Python is required for deterministic argv regression coverage: {:?}",
            check.error
        );

        let code = "import sys, json; print(json.dumps(sys.argv[1:]))";
        let test_args = vec![
            r"$spec".to_string(),
            r#""double""#.to_string(),
            r"'single'".to_string(),
            r#"{"json":true}"#.to_string(),
            "Unicode éàü".to_string(),
            "spaces  in  here".to_string(),
            "-leading".to_string(),
            "& | < > ^ ;".to_string(),
            "line1\nline2\nline3".to_string(),
        ];
        let mut all_args = vec!["-c".to_string(), code.to_string()];
        all_args.extend(test_args.clone());

        let result = run_foreground(python, &all_args, None, None, None).await;
        assert!(
            result.error.is_none(),
            "argv test error: {:?}",
            result.error
        );
        assert_eq!(result.exit_code, 0, "exit: {}", result.exit_code);
        let stdout = result.stdout.trim();
        let argv: Vec<String> =
            serde_json::from_str(stdout).expect("JSON parse of argv oracle output");
        assert_eq!(argv, test_args, "argv mismatch");
    }

    #[tokio::test]
    async fn test_output_bounding() {
        // Verify that raw output exceeds bound, proving truncation is needed.
        let (exe, args) = if cfg!(windows) {
            (
                "cmd.exe".to_string(),
                vec![
                    "/c".to_string(),
                    "for /l %i in (1,1,5000) do @echo BIG_OUTPUT_LINE_%i".to_string(),
                ],
            )
        } else {
            (
                "sh".to_string(),
                vec![
                    "-c".to_string(),
                    "for i in $(seq 1 5000); do echo BIG_OUTPUT_LINE_$i; done".to_string(),
                ],
            )
        };
        let output = run_foreground(&exe, &args, None, None, None).await;
        assert!(
            output.error.is_none(),
            "unexpected error: {:?}",
            output.error
        );
        // raw output must be > bound
        assert!(
            output.stdout.len() > OUTPUT_BOUND,
            "raw output should exceed bound, len={}",
            output.stdout.len()
        );
    }

    #[tokio::test]
    async fn test_large_output_is_drained_without_breaking_child() {
        let python = if cfg!(windows) { "python" } else { "python3" };
        let bytes = MAX_RAW_OUTPUT_BYTES + 1024 * 1024;
        let code = format!("import sys; sys.stdout.write('X' * {bytes}); sys.stdout.flush()");
        let result =
            run_foreground(python, &["-c".to_string(), code], None, Some(30_000), None).await;

        assert!(
            result.error.is_none(),
            "unexpected error: {:?}",
            result.error
        );
        assert_eq!(
            result.exit_code, 0,
            "capture bounding must not close stdout early and change child exit semantics: {}",
            result.stderr
        );
        assert!(
            result.stdout.len() <= MAX_RAW_OUTPUT_BYTES,
            "in-memory stdout capture must remain bounded"
        );
    }

    #[tokio::test]
    async fn test_handle_call_output_bounding() {
        // The bound applies to the complete rendered response, not each
        // stream independently. 30KB stdout + 30KB stderr must therefore
        // stay under 40KB once metadata and the truncation footer are added.
        let python = if cfg!(windows) { "python" } else { "python3" };
        let arguments = serde_json::json!({
            "command": python,
            "args": [
                "-c",
                "import sys; sys.stdout.write('A' * 30000); sys.stderr.write('B' * 30000)"
            ]
        });
        let result = handle_call(&arguments, None).await;
        let text = result["content"][0]["text"].as_str().expect("content text");
        assert!(
            text.contains("... (output truncated) ..."),
            "must carry truncation marker"
        );
        assert!(
            text.as_bytes().len() <= OUTPUT_BOUND,
            "combined response must stay within {OUTPUT_BOUND} bytes, got {}",
            text.as_bytes().len()
        );

        let log_path = extract_full_output_log_path(text)
            .expect("truncation footer must expose a readable full-output log path");
        let full = std::fs::read_to_string(&log_path).expect("full output log must be readable");
        assert!(full.contains(&"A".repeat(1000)), "stdout missing from log");
        assert!(full.contains(&"B".repeat(1000)), "stderr missing from log");
        let _ = std::fs::remove_file(log_path);
    }

    #[tokio::test]
    async fn test_handle_call_unicode_output_is_utf8_safe_and_bounded() {
        let python = if cfg!(windows) { "python" } else { "python3" };
        let arguments = serde_json::json!({
            "command": python,
            "args": ["-c", "import sys; sys.stdout.write('測' * 20000)"]
        });

        let result = handle_call(&arguments, None).await;
        let text = result["content"][0]["text"].as_str().expect("content text");
        assert!(
            text.as_bytes().len() <= OUTPUT_BOUND,
            "UTF-8 response must stay within {OUTPUT_BOUND} bytes, got {}",
            text.as_bytes().len()
        );
        assert!(text.contains("... (output truncated) ..."));

        let log_path = extract_full_output_log_path(text)
            .expect("Unicode truncation must expose the full-output log path");
        let full = std::fs::read_to_string(&log_path).expect("Unicode full output log");
        assert!(full.contains('測'));
        assert!(full.as_bytes().len() > OUTPUT_BOUND);
        let _ = std::fs::remove_file(log_path);
    }

    #[test]
    fn test_truncate_utf8_multibyte() {
        // "測" is 3 bytes; truncating to a mid-char byte index must not
        // panic (String::truncate would). The result must be valid UTF-8
        // and within the byte bound.
        let s = "測".repeat(20_000);
        let cut = truncate_utf8(&s, OUTPUT_BOUND);
        assert!(cut.len() <= OUTPUT_BOUND, "cut len {}", cut.len());
        assert!(cut.is_char_boundary(cut.len()), "cut at char boundary");
        // Must be a multiple of 3 (each 測 is 3 bytes) unless truncated to
        // empty — floor_char_boundary guarantees char alignment.
        assert_eq!(cut.len() % 3, 0, "cut preserves whole chars");
    }

    fn extract_full_output_log_path(text: &str) -> Option<std::path::PathBuf> {
        let marker = "[truncated - full output: ";
        let start = text.find(marker)? + marker.len();
        let rest = &text[start..];
        let end = rest.find("; use read_file]")?;
        Some(std::path::PathBuf::from(&rest[..end]))
    }
}
