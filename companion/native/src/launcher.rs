use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use crate::journal::AttemptEvidence;

#[derive(Debug)]
pub enum LauncherError {
    Io(std::io::Error),
    Json(serde_json::Error),
    OrcaSpawnFailed(String),
    OrcaExecutionUncertain(String),
    UnsupportedPolicy(String),
    PreflightFailed(String),
}

impl From<std::io::Error> for LauncherError {
    fn from(e: std::io::Error) -> Self {
        LauncherError::Io(e)
    }
}

impl From<serde_json::Error> for LauncherError {
    fn from(e: serde_json::Error) -> Self {
        LauncherError::Json(e)
    }
}

impl std::fmt::Display for LauncherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LauncherError::Io(e) => write!(f, "IO error: {}", e),
            LauncherError::Json(e) => write!(f, "JSON error: {}", e),
            LauncherError::OrcaSpawnFailed(e) => write!(f, "Orca spawn failed (pre-spawn): {}", e),
            LauncherError::OrcaExecutionUncertain(e) => write!(f, "Orca execution outcome uncertain: {}", e),
            LauncherError::UnsupportedPolicy(e) => write!(f, "Unsupported policy: {}", e),
            LauncherError::PreflightFailed(e) => write!(f, "Preflight check failed: {}", e),
        }
    }
}

impl std::error::Error for LauncherError {}

/// Pinned companion adapter revision identifier
pub const COMPANION_ADAPTER_REVISION: &str = "v1";

/// The embedded TypeScript adapter loaded explicitly via `-e` for owned OMP turns
pub const ADAPTER_TS_CONTENT: &str = r#"// Return Bridge companion OMP adapter (revision: v1)
export default function (pi) {
  // Scoped initialization for owned execution
  if (process.env.HANDS_RETURN_BRIDGE_EXECUTION_ID) {
    // Registered for owned execution
  }
}
"#;

pub fn ensure_adapter_file(state_dir: &Path) -> Result<PathBuf, LauncherError> {
    std::fs::create_dir_all(state_dir)?;
    let adapter_path = state_dir.join("adapter.ts");
    std::fs::write(&adapter_path, ADAPTER_TS_CONTENT)?;
    let content = std::fs::read_to_string(&adapter_path)?;
    if content != ADAPTER_TS_CONTENT {
        return Err(LauncherError::UnsupportedPolicy(
            "Adapter content on disk failed integrity check".to_string(),
        ));
    }
    Ok(adapter_path)
}

/// Resolves the native OMP binary token or path based on local authority
pub fn resolve_omp_binary() -> String {
    if let Ok(bin) = std::env::var("HANDS_RETURN_BRIDGE_OMP_BIN") {
        let trimmed = bin.trim();
        if !trimmed.is_empty() {
            if trimmed.contains(' ') && !trimmed.starts_with('"') {
                return format!("\"{}\"", trimmed.replace('\\', "/"));
            } else {
                return trimmed.to_string();
            }
        }
    }
    "omp".to_string()
}

/// Explicit compatibility preflight verifying Orca and OMP CLI capabilities before marking attempt
pub fn verify_launch_preflight(omp_bin_override: Option<&str>) -> Result<(), LauncherError> {
    // 1. Verify orca terminal subcommands
    let orca_help = Command::new("orca")
        .args(["terminal", "--help"])
        .output()
        .map_err(|e| LauncherError::PreflightFailed(format!("Failed to execute 'orca terminal --help': {}", e)))?;
    if !orca_help.status.success() {
        return Err(LauncherError::PreflightFailed(format!(
            "'orca terminal --help' failed with exit code: {:?}",
            orca_help.status.code()
        )));
    }
    let orca_text = String::from_utf8_lossy(&orca_help.stdout);
    if !orca_text.contains("create") || !orca_text.contains("wait") || !orca_text.contains("send") {
        return Err(LauncherError::PreflightFailed(
            "Orca CLI missing required terminal subcommands (create, wait, send)".to_string(),
        ));
    }

    // 2. Verify orca terminal wait --for tui-idle
    let orca_wait_help = Command::new("orca")
        .args(["terminal", "wait", "--help"])
        .output()
        .map_err(|e| LauncherError::PreflightFailed(format!("Failed to execute 'orca terminal wait --help': {}", e)))?;
    let wait_text = String::from_utf8_lossy(&orca_wait_help.stdout);
    if !wait_text.contains("--for") || !wait_text.contains("tui-idle") {
        return Err(LauncherError::PreflightFailed(
            "Orca terminal wait missing required '--for tui-idle' capability".to_string(),
        ));
    }

    // 3. Verify orca terminal send --text and --enter
    let orca_send_help = Command::new("orca")
        .args(["terminal", "send", "--help"])
        .output()
        .map_err(|e| LauncherError::PreflightFailed(format!("Failed to execute 'orca terminal send --help': {}", e)))?;
    let send_text = String::from_utf8_lossy(&orca_send_help.stdout);
    if !send_text.contains("--text") || !send_text.contains("--enter") {
        return Err(LauncherError::PreflightFailed(
            "Orca terminal send missing required '--text' or '--enter' capability".to_string(),
        ));
    }

    // 4. Verify OMP security and policy flags
    let raw_bin = omp_bin_override
        .map(str::to_string)
        .unwrap_or_else(resolve_omp_binary);
    let clean_bin = raw_bin.trim_matches('"');
    let omp_help = Command::new(clean_bin)
        .arg("--help")
        .output()
        .map_err(|e| LauncherError::PreflightFailed(format!("Failed to execute '{} --help': {}", clean_bin, e)))?;
    if !omp_help.status.success() {
        return Err(LauncherError::PreflightFailed(format!(
            "OMP CLI '{} --help' failed with exit code: {:?}",
            clean_bin,
            omp_help.status.code()
        )));
    }
    let omp_text = String::from_utf8_lossy(&omp_help.stdout);
    if !omp_text.contains("--no-extensions")
        || !omp_text.contains("--approval-mode")
        || !omp_text.contains("--tools")
        || !omp_text.contains("--no-skills")
        || !omp_text.contains("--no-rules")
        || !omp_text.contains("--no-prewalk")
    {
        return Err(LauncherError::PreflightFailed(format!(
            "OMP CLI '{}' missing required flags (--no-extensions, --approval-mode, --tools, --no-skills, --no-rules, --no-prewalk)",
            clean_bin
        )));
    }

    Ok(())
}

/// Fixed companion launcher building native-owned OMP startup command (NO user prompt)
pub fn build_omp_startup_command(
    adapter_path: &Path,
    tool_policy: &str,
    approval_policy: &str,
) -> Result<String, LauncherError> {
    // Fail-closed explicit tool set mapping
    let tool_flag = match tool_policy.to_lowercase().as_str() {
        "standard" | "all" => "--tools=read,edit,write,bash,grep,glob,lsp,todo",
        "read_only" => "--tools=read,grep,glob,lsp",
        "none" | "no_tools" => "--no-tools",
        other => return Err(LauncherError::UnsupportedPolicy(format!("Unknown tool policy: {}", other))),
    };

    // Map approval_policy
    let approval_flag = match approval_policy.to_lowercase().as_str() {
        "prompt" | "ask" => "--approval-mode=always-ask",
        "write" => "--approval-mode=write",
        "auto" | "yolo" => "--approval-mode=yolo",
        other => return Err(LauncherError::UnsupportedPolicy(format!("Unknown approval policy: {}", other))),
    };

    let omp_bin = resolve_omp_binary();
    let mut parts = Vec::new();
    parts.push(omp_bin);
    parts.push("--no-extensions".to_string());
    parts.push("-e".to_string());
    parts.push(format!("\"{}\"", adapter_path.to_string_lossy().replace('\\', "/")));
    parts.push("--no-prewalk".to_string());
    parts.push("--no-skills".to_string());
    parts.push("--no-rules".to_string());
    parts.push(tool_flag.to_string());
    parts.push(approval_flag.to_string());

    Ok(parts.join(" "))
}

/// Invokes public Orca CLI to create terminal in canonical target workspace
pub fn launch_orca_terminal(
    canonical_target_path: &str,
    startup_command: &str,
    execution_id: &str,
) -> Result<AttemptEvidence, LauncherError> {
    let title = format!("Hands-Task-{}", &execution_id[..std::cmp::min(12, execution_id.len())]);
    let clean_path = if canonical_target_path.starts_with(r"\\?\") {
        &canonical_target_path[4..]
    } else {
        canonical_target_path
    };
    let worktree_selector = format!("path:{}", clean_path.replace('\\', "/"));

    let output = Command::new("orca")
        .args([
            "terminal",
            "create",
            "--worktree",
            &worktree_selector,
            "--title",
            &title,
            "--command",
            startup_command,
            "--json",
        ])
        .output()
        .map_err(|e| LauncherError::OrcaSpawnFailed(format!("Failed to spawn orca CLI binary: {}", e)))?;

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca CLI returned exit code {:?}: {}",
            output.status.code(),
            err_msg.trim()
        )));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let val: Value = serde_json::from_str(&stdout_str).map_err(|e| {
        LauncherError::OrcaExecutionUncertain(format!("Failed to parse Orca CLI JSON output: {}. Output was: {}", e, stdout_str))
    })?;

    if val.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(LauncherError::OrcaExecutionUncertain(format!("Orca response not ok: {}", stdout_str)));
    }

    let term_obj = val.get("result").and_then(|r| r.get("terminal"));
    let handle = term_obj.and_then(|t| t.get("handle")).and_then(|h| h.as_str()).map(String::from);
    let tab_id = term_obj.and_then(|t| t.get("tabId")).and_then(|t| t.as_str()).map(String::from);
    let pane_key = term_obj.and_then(|t| t.get("paneKey")).and_then(|p| p.as_str()).map(String::from);
    let pty_id = term_obj.and_then(|t| t.get("ptyId")).and_then(|p| p.as_str()).map(String::from);

    if handle.is_none() {
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca response reported ok but omitted terminal handle: {}",
            stdout_str
        )));
    }

    Ok(AttemptEvidence {
        orca_terminal_handle: handle,
        orca_tab_id: tab_id,
        orca_pane_key: pane_key,
        orca_pty_id: pty_id,
    })
}

/// Waits for terminal to become tui-idle
pub fn wait_orca_terminal_idle(terminal_handle: &str, timeout_ms: u64) -> Result<(), LauncherError> {
    let timeout_str = timeout_ms.to_string();
    let output = Command::new("orca")
        .args([
            "terminal",
            "wait",
            "--terminal",
            terminal_handle,
            "--for",
            "tui-idle",
            "--timeout-ms",
            &timeout_str,
            "--json",
        ])
        .output()
        .map_err(|e| LauncherError::OrcaExecutionUncertain(format!("Failed to spawn orca terminal wait: {}", e)))?;

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        let out_msg = String::from_utf8_lossy(&output.stdout);
        let detail = if !err_msg.trim().is_empty() {
            err_msg.trim()
        } else {
            out_msg.trim()
        };
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca terminal wait returned exit code {:?}: {}",
            output.status.code(),
            detail
        )));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let val: Value = serde_json::from_str(&stdout_str).map_err(|e| {
        LauncherError::OrcaExecutionUncertain(format!("Failed to parse Orca terminal wait JSON: {}. Output was: {}", e, stdout_str))
    })?;

    if val.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(LauncherError::OrcaExecutionUncertain(format!("Orca terminal wait response not ok: {}", stdout_str)));
    }

    Ok(())
}

/// Sends exact bounded task prompt as literal text data to terminal via Command argv
pub fn send_orca_terminal_prompt(terminal_handle: &str, literal_prompt: &str) -> Result<(), LauncherError> {
    let text_arg = format!("--text={}", literal_prompt);
    let output = Command::new("orca")
        .args([
            "terminal",
            "send",
            "--terminal",
            terminal_handle,
            &text_arg,
            "--enter",
            "--json",
        ])
        .output()
        .map_err(|e| LauncherError::OrcaExecutionUncertain(format!("Failed to spawn orca terminal send: {}", e)))?;

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        let out_msg = String::from_utf8_lossy(&output.stdout);
        let detail = if !err_msg.trim().is_empty() {
            err_msg.trim()
        } else {
            out_msg.trim()
        };
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca terminal send returned exit code {:?}: {}",
            output.status.code(),
            detail
        )));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let val: Value = serde_json::from_str(&stdout_str).map_err(|e| {
        LauncherError::OrcaExecutionUncertain(format!("Failed to parse Orca terminal send JSON: {}. Output was: {}", e, stdout_str))
    })?;

    if val.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(LauncherError::OrcaExecutionUncertain(format!("Orca terminal send response not ok: {}", stdout_str)));
    }

    Ok(())
}
