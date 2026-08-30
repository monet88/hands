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
    const PROCESS_SUSPEND_RESUME: u32 = 0x0800;
    const THREAD_SUSPEND_RESUME: u32 = 0x0002;
    const TH32CS_SNAPTHREAD: u32 = 0x00000004;
    const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

    #[repr(C)]
    #[allow(non_snake_case)]
    struct THREADENTRY32 {
        dwSize: u32,
        cntUsage: u32,
        th32ThreadID: u32,
        th32OwnerProcessID: u32,
        tpBasePri: i32,
        tpDeltaPri: i32,
        dwFlags: u32,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateJobObjectW(lpJobAttributes: *const std::ffi::c_void, lpName: *const u16)
        -> HANDLE;
        fn AssignProcessToJobObject(hJob: HANDLE, hProcess: HANDLE) -> BOOL;
        fn TerminateJobObject(hJob: HANDLE, uExitCode: u32) -> BOOL;
        fn CloseHandle(hObject: HANDLE) -> BOOL;
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: BOOL, dwProcessId: u32) -> HANDLE;
        fn OpenThread(dwDesiredAccess: u32, bInheritHandle: BOOL, dwThreadId: u32) -> HANDLE;
        fn ResumeThread(hThread: HANDLE) -> u32;
        fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> HANDLE;
        fn Thread32First(hSnapshot: HANDLE, lpte: *mut THREADENTRY32) -> BOOL;
        fn Thread32Next(hSnapshot: HANDLE, lpte: *mut THREADENTRY32) -> BOOL;
    }

    // Job object handles are thread-safe; the raw HANDLE pointer is
    // only passed to kernel32 functions that accept it from any thread.
    unsafe impl Send for JobObject {}
    unsafe impl Sync for JobObject {}

    pub struct JobObject(HANDLE);

    impl JobObject {
        pub fn create() -> Option<Self> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle == INVALID_HANDLE_VALUE || handle.is_null() {
                return None;
            }
            Some(Self(handle))
        }

        pub fn assign_process(&self, pid: u32) -> bool {
            let process = unsafe {
                OpenProcess(
                    PROCESS_TERMINATE | PROCESS_SET_QUOTA | PROCESS_SUSPEND_RESUME,
                    0,
                    pid,
                )
            };
            if process == INVALID_HANDLE_VALUE || process.is_null() {
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

    /// Resume the suspended initial thread of `pid` (created via
    /// CREATE_SUSPENDED) so the process starts executing.
    pub fn resume_process(pid: u32) -> bool {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return false;
        }
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            cntUsage: 0,
            th32ThreadID: 0,
            th32OwnerProcessID: 0,
            tpBasePri: 0,
            tpDeltaPri: 0,
            dwFlags: 0,
        };
        let mut resumed = false;
        let mut has_entry = unsafe { Thread32First(snapshot, &mut entry) != 0 };
        while has_entry {
            if entry.th32OwnerProcessID == pid {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread != INVALID_HANDLE_VALUE && !thread.is_null() {
                    let prev_count = unsafe { ResumeThread(thread) };
                    unsafe { CloseHandle(thread) };
                    if prev_count != u32::MAX {
                        resumed = true;
                        break;
                    }
                }
            }
            has_entry = unsafe { Thread32Next(snapshot, &mut entry) != 0 };
        }
        unsafe { CloseHandle(snapshot) };
        resumed
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

    impl JobObject {
        pub fn terminate(&self) -> bool {
            false
        }
    }
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

/// Resume a suspended Windows process (created via CREATE_SUSPENDED)
/// without a Job Object handle. Used when job creation/assignment failed
/// but the child was spawned suspended and must not stay frozen forever.
#[cfg(windows)]
fn resume_suspended_child(pid: u32) -> bool {
    job_object::resume_process(pid)
}

#[cfg(not(windows))]
fn create_job_for_child(_pid: u32) -> Option<job_object::JobObject> {
    None
}

#[derive(Clone)]
struct PipeCapture {
    bytes: Vec<u8>,
    truncated: bool,
    /// Some(read error) when the pipe read failed before EOF. A successful
    /// child can still produce partial output; treating a read failure as
    /// clean EOF hides that the output may be incomplete.
    read_error: Option<String>,
}

impl PipeCapture {
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
            read_error: None,
        }
    }
}

/// Keep a bounded in-memory prefix while continuing to drain the pipe to EOF.
/// Closing the read end at the memory cap would surface as BrokenPipe/EPIPE in
/// the child and incorrectly change its exit status, so excess bytes are
/// deliberately discarded rather than stopping the read.
///
/// Output is written incrementally into `capture` so that if the read task is
/// aborted or times out, whatever partial bytes were buffered before abort
/// are preserved.
async fn read_capped_pipe_shared<R>(
    mut reader: R,
    capture: std::sync::Arc<std::sync::Mutex<PipeCapture>>,
) -> PipeCapture
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 8192];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) => {
                if let Ok(mut cap) = capture.lock() {
                    cap.read_error = Some(e.to_string());
                }
                break;
            }
        };
        if let Ok(mut cap) = capture.lock() {
            let remaining = MAX_RAW_OUTPUT_BYTES.saturating_sub(cap.bytes.len());
            if remaining > 0 {
                let keep = remaining.min(read);
                cap.bytes.extend_from_slice(&chunk[..keep]);
                if keep < read {
                    cap.truncated = true;
                }
            } else {
                cap.truncated = true;
            }
        }
    }
    capture
        .lock()
        .map(|c| c.clone())
        .unwrap_or_else(|_| PipeCapture::empty())
}

#[cfg(test)]
async fn read_capped_pipe<R>(reader: R) -> PipeCapture
where
    R: AsyncRead + Unpin,
{
    let capture = std::sync::Arc::new(std::sync::Mutex::new(PipeCapture::empty()));
    read_capped_pipe_shared(reader, capture).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminationReason {
    Timeout,
    SpawnFailure,
    WaitFailure,
    OutputReadFailure,
}

impl TerminationReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminationReason::Timeout => "timeout",
            TerminationReason::SpawnFailure => "spawn_failure",
            TerminationReason::WaitFailure => "wait_failure",
            TerminationReason::OutputReadFailure => "output_read_failure",
        }
    }
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
    /// Machine-readable termination reason when process execution did not exit normally.
    pub termination_reason: Option<TerminationReason>,
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
pub fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let end = s.floor_char_boundary(max_bytes);
    s[..end].to_string()
}

/// Persist the full captured output to a temp log file so the operator can
/// retrieve what was truncated from the bounded tool response. Returns the
/// log path on success.
pub fn write_temp_log(stdout: &str, stderr: &str) -> Option<String> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let dir = std::env::temp_dir().join("hands").join("run-command");
    std::fs::create_dir_all(&dir).ok()?;
    // Prune stale logs before writing a new one; without this the
    // persistent service accumulates output-<pid>-<ns>.log files without
    // bound on disk.
    prune_temp_logs(&dir);
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

/// Delete output logs older than 1 day. These are diagnostic artifacts for
/// operator retrieval, not durable records; pruning keeps the directory
/// bounded without removing logs the operator may still be reading.
/// Only prunes `output-*.log` files owned by this feature.
fn prune_temp_logs(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|now| now.as_secs().saturating_sub(86_400))
        .unwrap_or(0);
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.starts_with("output-") || !name.ends_with(".log") {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH) else {
            continue;
        };
        if age.as_secs() < cutoff {
            let _ = std::fs::remove_file(path);
        }
    }
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
    // Reject non-string argv elements up front instead of silently dropping
    // them: a dropped argument changes the command the caller asked for.
    let args: Result<Vec<String>, String> = arguments
        .get("args")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_string)
                        .ok_or_else(|| "args must contain only strings".to_string())
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(Vec::new()));
    let args = match args {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({
                "content": [{ "type": "text", "text": format!("error: {e}") }],
                "isError": true
            });
        }
    };
    let explicit_workdir = arguments.get("workdir").and_then(serde_json::Value::as_str);
    let workdir = explicit_workdir.or(workspace);
    let timeout = arguments.get("timeout").and_then(serde_json::Value::as_u64);
    let env = match arguments.get("env") {
        None => None,
        Some(value) => {
            let Some(obj) = value.as_object() else {
                return serde_json::json!({
                    "content": [{ "type": "text", "text": "error: env must be an object" }],
                    "isError": true
                });
            };
            if obj.values().any(|value| !value.is_string()) {
                return serde_json::json!({
                    "content": [{ "type": "text", "text": "error: env must contain only string values" }],
                    "isError": true
                });
            }
            Some(value)
        }
    };

    let output = run_foreground(command, &args, workdir, timeout, env).await;
    let unbounded = render_tool_text(&output);
    let (text, log_path) = if unbounded.as_bytes().len() > OUTPUT_BOUND {
        let log_path = write_temp_log(&output.stdout, &output.stderr);
        (render_bounded_tool_text(&output, log_path.as_deref()), log_path)
    } else {
        (unbounded, None)
    };

    let has_output = !output.stdout.is_empty() || !output.stderr.is_empty();
    let total_bytes = output.stdout.len() + output.stderr.len();

    let termination_reason = output.termination_reason.map(|r| r.as_str());

    let structured = serde_json::json!({
        "command": command,
        "args": args,
        "workdir": workdir,
        "exit_code": output.exit_code,
        "timed_out": output.timed_out,
        "termination_reason": termination_reason,
        "truncated": output.capture_truncated || log_path.is_some(),
        "has_output": has_output,
        "total_bytes": total_bytes,
        "output_file": log_path,
        "error": output.error,
    });

    // Non-zero exit, timeout, and missing-executable are all distinct
    // outcomes. A child exiting non-zero or timing out is NOT an MCP
    // protocol error — only an unlaunchable process is.
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
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
    reason: Option<TerminationReason>,
) -> ProcOutput {
    // A pipe read failure means the captured output may be incomplete even
    // when the child itself succeeded. Surface it rather than reporting a
    // clean run with truncated-looking output.
    let mut error = error;
    let mut termination_reason = reason;
    if let Some(e) = stdout.read_error.or_else(|| stderr.read_error.clone()) {
        let msg = format!("output read failed: {e}");
        error = Some(match error {
            Some(existing) => format!("{existing}; {msg}"),
            None => msg,
        });
        if termination_reason.is_none() && !timed_out {
            termination_reason = Some(TerminationReason::OutputReadFailure);
        }
    }
    if timed_out && termination_reason.is_none() {
        termination_reason = Some(TerminationReason::Timeout);
    }
    ProcOutput {
        stdout: String::from_utf8_lossy(&stdout.bytes).to_string(),
        stderr: String::from_utf8_lossy(&stderr.bytes).to_string(),
        exit_code,
        timed_out,
        capture_truncated: stdout.truncated || stderr.truncated,
        error,
        termination_reason,
    }
}

/// Snapshot whatever partial output the shared capture buffers hold.
/// Called only after the drain task's `JoinHandle` has been polled to
/// completion — a completed handle must never be polled again (Tokio
/// panics with "JoinHandle polled after completion").
fn drain_captures_from_shares(
    stdout_capture: &std::sync::Arc<std::sync::Mutex<PipeCapture>>,
    stderr_capture: &std::sync::Arc<std::sync::Mutex<PipeCapture>>,
) -> (PipeCapture, PipeCapture) {
    let stdout = stdout_capture
        .lock()
        .map(|c| c.clone())
        .unwrap_or_else(|_| PipeCapture::empty());
    let stderr = stderr_capture
        .lock()
        .map(|c| c.clone())
        .unwrap_or_else(|_| PipeCapture::empty());
    (stdout, stderr)
}

/// Rejoin the pipe-drain task after the process tree is terminated.
/// Killing the tree closes every pipe read end, so awaiting the drain
/// normally completes immediately. If the drain times out or errors,
/// whatever partial output was buffered into `stdout_capture` and
/// `stderr_capture` is preserved rather than dropped.
async fn bounded_rejoin(
    drain: &mut JoinHandle<(PipeCapture, PipeCapture)>,
    stdout_capture: &std::sync::Arc<std::sync::Mutex<PipeCapture>>,
    stderr_capture: &std::sync::Arc<std::sync::Mutex<PipeCapture>>,
    drain_timeout_ms: u64,
) -> (PipeCapture, PipeCapture) {
    let drain_timeout = Duration::from_millis(drain_timeout_ms);
    match tokio::time::timeout(drain_timeout, &mut *drain).await {
        Ok(Ok(captures)) => captures,
        Ok(Err(_)) => drain_captures_from_shares(stdout_capture, stderr_capture),
        Err(_) => {
            drain.abort();
            let _ = drain.await;
            let stdout = stdout_capture
                .lock()
                .map(|c| c.clone())
                .unwrap_or_else(|_| PipeCapture::empty());
            let stderr = stderr_capture
                .lock()
                .map(|c| c.clone())
                .unwrap_or_else(|_| PipeCapture::empty());
            (stdout, stderr)
        }
    }
}

async fn terminate_tree(
    child: &mut tokio::process::Child,
    job: Option<&job_object::JobObject>,
    _child_pid: Option<u32>,
) {
    #[cfg(unix)]
    if let Some(pid) = _child_pid {
        // Kill the whole process group FIRST before killing/reaping the root.
        // Child was spawned with process_group(0) and has NOT been
        // reaped yet: its PGID == its PID and is active/unrecycled in
        // the kernel. killpg(pid, SIGKILL) terminates every descendant
        // in the group before the root is reaped.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    if let Some(j) = job {
        j.terminate();
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
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
    // Null stdin: the child must never inherit Hands' own stdin (the MCP
    // JSON-RPC transport when running as a stdio server). A child reading
    // stdin could consume protocol traffic or block forever.
    cmd.stdin(std::process::Stdio::null());
    #[cfg(unix)]
    {
        // Place the child in its own process group so a timeout can kill
        // the whole tree (killpg) instead of only the root process.
        // PGID == child PID, so killpg(child_pid, SIGKILL) reaches all.
        cmd.process_group(0);
    }

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

    #[cfg(windows)]
    {
        // Spawn the child suspended so the Job Object can be assigned
        // BEFORE the child's first instruction runs. This closes the
        // spawn-to-assign race: without CREATE_SUSPENDED, a child that
        // spawns a descendant before assignment leaves that descendant
        // outside the job, and a timeout would orphan it with the pipe
        // handles still open.
        // CREATE_SUSPENDED (0x4). tokio always ORs CREATE_UNICODE_ENVIRONMENT
        // (0x400) itself, so passing only 0x4 is correct.
        cmd.creation_flags(0x0000_0004);
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
                termination_reason: Some(TerminationReason::SpawnFailure),
            };
        }
    };

    // Capture the PID immediately after spawn. Tokio's Child::id() returns
    // None after the child is reaped (kill().await / wait().await), but the
    // process group ID equals the original PID on Unix, so the tree kill
    // must use the value captured here, before any reap.
    let child_pid = child.id();

    // Create Job Object for process-tree ownership (Windows). On timeout,
    // terminating the job kills all descendants, closing pipe handles.
    // The child is suspended at this point (CREATE_SUSPENDED), so
    // assigning the job before resuming guarantees the entire tree is
    // owned by the job from the child's first instruction onward.
    #[cfg(windows)]
    let job = child_pid.and_then(create_job_for_child);
    #[cfg(not(windows))]
    let job: Option<job_object::JobObject> = None;

    // The child is suspended at this point; the Job Object was assigned
    // before any of its code ran. Resume the initial thread so the child
    // starts executing inside the job's ownership. If resume fails,
    // terminate and reap the child immediately rather than letting it hang
    // until timeout.
    #[cfg(windows)]
    {
        let resumed = if job.is_some() {
            job_object::resume_process(child_pid.unwrap_or(0))
        } else if let Some(pid) = child_pid {
            resume_suspended_child(pid)
        } else {
            false
        };
        if !resumed {
            let _ = child.kill().await;
            if let Some(j) = &job {
                j.terminate();
            }
            let _ = child.wait().await;
            return ProcOutput {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: -1,
                timed_out: false,
                capture_truncated: false,
                error: Some("failed to resume suspended process".to_string()),
                termination_reason: Some(TerminationReason::SpawnFailure),
            };
        }
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_capture = std::sync::Arc::new(std::sync::Mutex::new(PipeCapture::empty()));
    let stderr_capture = std::sync::Arc::new(std::sync::Mutex::new(PipeCapture::empty()));

    let stdout_cap_clone = stdout_capture.clone();
    let stderr_cap_clone = stderr_capture.clone();

    // Drain both pipes inside one cancellable task so the deadline covers
    // them jointly. Each reader keeps only a bounded prefix but continues
    // consuming excess bytes to EOF so the child never sees a broken pipe.
    // Shared capture buffers preserve buffered partial output across
    // cancellation or timeouts.
    let mut drain = tokio::spawn(async move {
        let stdout_read = async {
            match stdout {
                Some(pipe) => read_capped_pipe_shared(pipe, stdout_cap_clone).await,
                None => PipeCapture::empty(),
            }
        };
        let stderr_read = async {
            match stderr {
                Some(pipe) => read_capped_pipe_shared(pipe, stderr_cap_clone).await,
                None => PipeCapture::empty(),
            }
        };
        tokio::join!(stdout_read, stderr_read)
    });

    // The overall deadline bounds the whole execution lifecycle.
    //
    // Lifecycle design:
    // We drain stdout/stderr pipes up to the deadline BEFORE reaping the
    // root process. While the child is unreaped (running or zombie), its PID
    // and PGID are guaranteed un-recycled in the kernel's process table.
    // If the drain reaches EOF within deadline, all holders of the pipes
    // have terminated, and awaiting child.wait() reaps the root immediately.
    // If the drain hits deadline (a runaway child or descendant holding the
    // pipe open), terminate_tree terminates the tree and process group while the
    // PGID is still 100% active and un-recycled, then reaps the root.
    let deadline = tokio::time::Instant::now() + timeout;

    let drain_result = tokio::time::timeout_at(deadline, &mut drain).await;

    resolve_drain_result(
        drain_result,
        deadline,
        &mut child,
        job.as_ref(),
        child_pid,
        &stdout_capture,
        &stderr_capture,
        &mut drain,
    )
    .await
}

async fn resolve_drain_result(
    drain_result: Result<
        Result<(PipeCapture, PipeCapture), tokio::task::JoinError>,
        tokio::time::error::Elapsed,
    >,
    deadline: tokio::time::Instant,
    child: &mut tokio::process::Child,
    job: Option<&job_object::JobObject>,
    child_pid: Option<u32>,
    stdout_capture: &std::sync::Arc<std::sync::Mutex<PipeCapture>>,
    stderr_capture: &std::sync::Arc<std::sync::Mutex<PipeCapture>>,
    drain: &mut JoinHandle<(PipeCapture, PipeCapture)>,
) -> ProcOutput {
    match drain_result {
        Ok(Ok((stdout_cap, stderr_cap))) => {
            // Pipes reached EOF cleanly (all write ends closed).
            // Await the root process exit status with whatever time remains.
            match tokio::time::timeout_at(deadline, child.wait()).await {
                Ok(Ok(status)) => proc_output_from_captures(
                    stdout_cap,
                    stderr_cap,
                    status.code().unwrap_or(-1),
                    false,
                    None,
                    None,
                ),
                Ok(Err(e)) => {
                    let wait_error = format!("process wait failed: {e}");
                    terminate_tree(child, job, child_pid).await;
                    proc_output_from_captures(
                        stdout_cap,
                        stderr_cap,
                        -1,
                        false,
                        Some(wait_error),
                        Some(TerminationReason::WaitFailure),
                    )
                }
                Err(_elapsed) => {
                    // Root process closed pipes but did not exit before deadline.
                    terminate_tree(child, job, child_pid).await;
                    proc_output_from_captures(
                        stdout_cap,
                        stderr_cap,
                        -1,
                        true,
                        None,
                        Some(TerminationReason::Timeout),
                    )
                }
            }
        }
        Ok(Err(_)) => {
            // Drain task failed (e.g. panicked). The JoinHandle was already
            // polled to completion by the timeout_at above, so it must NOT be
            // polled again (Tokio 1.52.3 panics: "JoinHandle polled after
            // completion"). Snapshot whatever the shared captures hold.
            terminate_tree(child, job, child_pid).await;
            let (stdout_cap, stderr_cap) =
                drain_captures_from_shares(&stdout_capture, &stderr_capture);
            proc_output_from_captures(
                stdout_cap,
                stderr_cap,
                -1,
                false,
                Some("pipe drain task failed".to_string()),
                Some(TerminationReason::OutputReadFailure),
            )
        }
        Err(_elapsed) => {
            // Deadline hit while draining (long-running process or descendant
            // holding inherited pipe). Terminate tree while PGID is valid,
            // then collect what was captured.
            terminate_tree(child, job, child_pid).await;
            let (stdout_cap, stderr_cap) = bounded_rejoin(
                drain,
                &stdout_capture,
                &stderr_capture,
                BOUNDED_DRAIN_TIMEOUT_MS,
            )
            .await;
            proc_output_from_captures(
                stdout_cap,
                stderr_cap,
                -1,
                true,
                None,
                Some(TerminationReason::Timeout),
            )
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
    async fn test_pipe_read_error_is_reported_not_eof() {
        // A reader that fails after delivering one chunk must surface the
        // failure: treating it as clean EOF would hide incomplete output
        // from a successful child.
        struct FailingReader {
            delivered: bool,
        }
        impl AsyncRead for FailingReader {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if self.delivered {
                    return std::task::Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "simulated pipe break",
                    )));
                }
                self.delivered = true;
                buf.put_slice(b"partial");
                std::task::Poll::Ready(Ok(()))
            }
        }
        let capture = read_capped_pipe(FailingReader { delivered: false }).await;
        assert_eq!(
            capture.bytes, b"partial",
            "bytes before the error must survive"
        );
        let err = capture.read_error.expect("read error must be recorded");
        assert!(
            err.contains("simulated pipe break"),
            "unexpected read error: {err}"
        );

        // A clean EOF carries no error.
        struct EofReader;
        impl AsyncRead for EofReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _cx: &mut std::task::Context<'_>,
                _buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let clean = read_capped_pipe(EofReader).await;
        assert!(
            clean.read_error.is_none(),
            "clean EOF must not be reported as a read error"
        );
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

    #[cfg(unix)]
    #[tokio::test]
    async fn test_timeout_terminates_descendant_process_unix() {
        // Unix regression: the process group kill (killpg) must terminate
        // descendants, not just the root. The root spawns a grandchild that
        // writes its PID to a file and keeps the inherited stdout pipe open;
        // on timeout the whole group must die and the PID must no longer
        // be alive.
        let python = "python3";
        let root = std::env::temp_dir().join(format!(
            "hands_run_proc_tree_unix_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create unix tree-kill fixture directory");
        let script = root.join("spawn_child.py");
        let pid_file = root.join("child.pid");
        std::fs::write(
            &script,
            "import pathlib, subprocess, sys, time\n\
             child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n\
             pathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\n\
             time.sleep(30)\n",
        )
        .expect("write unix tree-kill fixture");

        let result = run_foreground(
            python,
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

        // killpg must have terminated the descendant (it was in the same
        // process group as the root). Read /proc/<pid>/stat: an absent file
        // or a zombie state 'Z' both mean terminated. kill -0 wrongly
        // succeeds on zombies, so it cannot prove termination.
        // /proc/<pid>/stat is "pid (comm) state ...". comm may contain
        // spaces/parens, so the state is the first token after the LAST
        // ')'. Treat a missing file or state 'Z'/'X' as terminated.
        let still_alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let idx = stat.rfind(')')?;
                stat[idx + 1..]
                    .split_whitespace()
                    .next()
                    .map(str::to_string)
            })
            .map(|state| state != "Z" && state != "X")
            .unwrap_or(false);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            !still_alive,
            "Unix timeout must terminate descendant PID {pid} via process group kill"
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
    async fn test_handle_call_rejects_non_string_env_values() {
        let arguments = serde_json::json!({
            "command": if cfg!(windows) { "cmd.exe" } else { "sh" },
            "args": if cfg!(windows) {
                serde_json::json!(["/c", "echo SHOULD_NOT_RUN"])
            } else {
                serde_json::json!(["-c", "echo SHOULD_NOT_RUN"])
            },
            "env": { "HANDS_TEST_ENV": 123 }
        });

        let result = handle_call(&arguments, None).await;
        assert_eq!(result["isError"], serde_json::Value::Bool(true));
        let text = result["content"][0]["text"].as_str().expect("content text");
        assert!(
            text.contains("env must contain only string values"),
            "unexpected validation error: {text}"
        );
        assert!(
            !text.contains("SHOULD_NOT_RUN"),
            "malformed env must be rejected before the child executes"
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

    #[tokio::test]
    async fn test_bounded_rejoin_preserves_partial_output_on_abort() {
        let stdout_capture = std::sync::Arc::new(std::sync::Mutex::new(PipeCapture::empty()));
        let stderr_capture = std::sync::Arc::new(std::sync::Mutex::new(PipeCapture::empty()));

        {
            let mut cap = stdout_capture.lock().unwrap();
            cap.bytes.extend_from_slice(b"partial diagnostic stdout");
        }
        {
            let mut cap = stderr_capture.lock().unwrap();
            cap.bytes.extend_from_slice(b"partial diagnostic stderr");
        }

        let mut drain = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            (PipeCapture::empty(), PipeCapture::empty())
        });

        let (stdout, stderr) =
            bounded_rejoin(&mut drain, &stdout_capture, &stderr_capture, 50).await;

        assert_eq!(
            stdout.bytes, b"partial diagnostic stdout",
            "bounded_rejoin must preserve buffered stdout on abort"
        );
        assert_eq!(
            stderr.bytes, b"partial diagnostic stderr",
            "bounded_rejoin must preserve buffered stderr on abort"
        );
    }

    fn set_age(path: &std::path::Path, age: std::time::Duration) {
        let target = std::time::SystemTime::now()
            .checked_sub(age)
            .unwrap_or(std::time::UNIX_EPOCH);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(target)
            .expect("set mtime");
    }

    #[tokio::test]
    async fn test_drain_error_returns_error_without_repolling_consumed_handle() {
        let stdout_capture = std::sync::Arc::new(std::sync::Mutex::new(PipeCapture::empty()));
        let stderr_capture = std::sync::Arc::new(std::sync::Mutex::new(PipeCapture::empty()));

        // A real child so terminate_tree has a valid process to kill/reap.
        let mut child = if cfg!(windows) {
            Command::new("cmd.exe")
                .args(["/c", "echo hi"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn cmd")
        } else {
            Command::new("sh")
                .args(["-c", "echo hi"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("spawn sh")
        };

        // Drain task that panics => JoinHandle yields Err(JoinError), matching
        // the timeout_at result shape run_foreground produces.
        let mut drain = tokio::spawn(async {
            panic!("simulated pipe drain failure");
        });
        let res = tokio::time::timeout(Duration::from_secs(5), &mut drain).await;
        let join_err = match res {
            Ok(Err(e)) => e,
            _ => panic!("drain should fail with JoinError"),
        };

        // The Ok(Err) arm must snapshot the shared captures (never re-poll the
        // consumed `drain`) and surface the drain failure as an error. If the
        // arm is ever changed to call bounded_rejoin on the completed handle,
        // this test panics exactly like the old run_foreground did.
        let out = resolve_drain_result(
            Ok(Err(join_err)),
            tokio::time::Instant::now(),
            &mut child,
            None,
            None,
            &stdout_capture,
            &stderr_capture,
            &mut drain,
        )
        .await;

        assert!(
            out.error
                .as_deref()
                .unwrap_or_default()
                .contains("pipe drain task failed"),
            "drain failure must surface as an error, got {:?}",
            out.error
        );
        assert_eq!(out.exit_code, -1);
        assert!(!out.timed_out);
    }

    #[tokio::test]
    async fn test_prune_temp_logs_only_deletes_feature_logs() {
        let dir = std::env::temp_dir().join(format!(
            "hands_prune_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create test dir");

        let old_other_file = dir.join("important-file.txt");
        let old_feature_log = dir.join("output-999-888.log");
        let recent_feature_log = dir.join("output-999-889.log");

        std::fs::write(&old_other_file, "don't delete").unwrap();
        std::fs::write(&old_feature_log, "old log").unwrap();
        std::fs::write(&recent_feature_log, "new log").unwrap();

        // Age both the unrelated file and one feature log past the 1-day
        // cutoff. A regression that removed ANY file older than a day would
        // delete old_other_file; a regression that ignored the age would
        // delete recent_feature_log.
        set_age(&old_other_file, std::time::Duration::from_secs(2 * 86_400));
        set_age(&old_feature_log, std::time::Duration::from_secs(2 * 86_400));

        prune_temp_logs(&dir);

        assert!(
            old_other_file.exists(),
            "old non-feature file must NOT be deleted"
        );
        assert!(!old_feature_log.exists(), "old feature log must be deleted");
        assert!(
            recent_feature_log.exists(),
            "recent feature log must NOT be deleted"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn test_timeout_terminates_descendant_when_root_exits() {
        let python =
            crate::service::which("python").expect("Python is required by the Windows test gate");
        let root = std::env::temp_dir().join(format!(
            "hands_run_proc_root_exit_win_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create tree-kill fixture directory");
        let script = root.join("spawn_child_exit.py");
        let pid_file = root.join("child.pid");
        std::fs::write(
            &script,
            "import pathlib, subprocess, sys, time\n\
             child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n\
             pathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\n\
             sys.exit(0)\n",
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
        assert!(
            result.timed_out,
            "fixture should time out because descendant holds pipe"
        );

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("child PID fixture should be written before exit")
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
            "timeout when root exits must terminate descendant PID {pid}; tasklist={listing}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_timeout_terminates_descendant_when_root_exits_unix() {
        let python = "python3";
        let root = std::env::temp_dir().join(format!(
            "hands_run_proc_root_exit_unix_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create unix tree-kill fixture directory");
        let script = root.join("spawn_child_exit.py");
        let pid_file = root.join("child.pid");
        std::fs::write(
            &script,
            "import pathlib, subprocess, sys, time\n\
             child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n\
             pathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='ascii')\n\
             sys.exit(0)\n",
        )
        .expect("write unix tree-kill fixture");

        let result = run_foreground(
            python,
            &[
                script.to_string_lossy().to_string(),
                pid_file.to_string_lossy().to_string(),
            ],
            None,
            Some(700),
            None,
        )
        .await;
        assert!(
            result.timed_out,
            "fixture should time out because descendant holds pipe"
        );

        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("child PID fixture should be written before exit")
            .trim()
            .parse()
            .expect("child PID must be numeric");
        tokio::time::sleep(Duration::from_millis(100)).await;

        let still_alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                let idx = stat.rfind(')')?;
                stat[idx + 1..]
                    .split_whitespace()
                    .next()
                    .map(str::to_string)
            })
            .map(|state| state != "Z" && state != "X")
            .unwrap_or(false);
        let _ = std::fs::remove_dir_all(&root);
        assert!(
            !still_alive,
            "Unix timeout when root exits must terminate descendant PID {pid} via process group kill"
        );
    }

    fn extract_full_output_log_path(text: &str) -> Option<std::path::PathBuf> {
        let marker = "[truncated - full output: ";
        let start = text.find(marker)? + marker.len();
        let rest = &text[start..];
        let end = rest.find("; use read_file]")?;
        Some(std::path::PathBuf::from(&rest[..end]))
    }
    #[tokio::test]
    async fn test_termination_reason_structured_outcomes() {
        let (echo_cmd, echo_args) = platform_echo(&["TERMINATION_REASON_OK"]);
        let normal_call = serde_json::json!({
            "command": echo_cmd,
            "args": echo_args
        });
        let normal_res = handle_call(&normal_call, None).await;
        assert_eq!(normal_res["isError"], false);
        assert_eq!(normal_res["structuredContent"]["exit_code"], 0);
        assert_eq!(normal_res["structuredContent"]["timed_out"], false);
        assert_eq!(normal_res["structuredContent"]["termination_reason"], serde_json::Value::Null);

        // Non-zero child exit remains a normal outcome (termination_reason: null, isError: false)
        let (exit_cmd, exit_args) = if cfg!(windows) {
            ("cmd.exe".to_string(), vec!["/c".to_string(), "exit 42".to_string()])
        } else {
            ("sh".to_string(), vec!["-c".to_string(), "exit 42".to_string()])
        };
        let nonzero_call = serde_json::json!({
            "command": exit_cmd,
            "args": exit_args
        });
        let nonzero_res = handle_call(&nonzero_call, None).await;
        assert_eq!(nonzero_res["isError"], false);
        assert_eq!(nonzero_res["structuredContent"]["exit_code"], 42);
        assert_eq!(nonzero_res["structuredContent"]["timed_out"], false);
        assert_eq!(nonzero_res["structuredContent"]["termination_reason"], serde_json::Value::Null);

        // Spawn failure -> termination_reason: "spawn_failure", isError: true
        let spawn_fail_call = serde_json::json!({
            "command": "nonexistent_executable_12345_xyz"
        });
        let spawn_fail_res = handle_call(&spawn_fail_call, None).await;
        assert_eq!(spawn_fail_res["isError"], true);
        assert_eq!(spawn_fail_res["structuredContent"]["exit_code"], -1);
        assert_eq!(spawn_fail_res["structuredContent"]["termination_reason"], "spawn_failure");

        // Timeout -> termination_reason: "timeout", timed_out: true, isError: false
        let (sleep_cmd, sleep_args) = if cfg!(windows) {
            ("powershell.exe".to_string(), vec!["-NoProfile".to_string(), "-Command".to_string(), "Start-Sleep -Seconds 5".to_string()])
        } else {
            ("sleep".to_string(), vec!["5".to_string()])
        };
        let timeout_call = serde_json::json!({
            "command": sleep_cmd,
            "args": sleep_args,
            "timeout": 300
        });
        let timeout_res = handle_call(&timeout_call, None).await;
        assert_eq!(timeout_res["isError"], false);
        assert_eq!(timeout_res["structuredContent"]["timed_out"], true);
        assert_eq!(timeout_res["structuredContent"]["termination_reason"], "timeout");
    }

}
