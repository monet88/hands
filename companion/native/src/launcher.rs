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

    let mut parts = Vec::new();
    parts.push("omp".to_string());
    parts.push("--no-extensions".to_string());
    parts.push("-e".to_string());
    parts.push(format!("\"{}\"", adapter_path.to_string_lossy().replace('\\', "/")));
    parts.push("--no-prewalk".to_string());
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
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca terminal wait returned exit code {:?}: {}",
            output.status.code(),
            err_msg.trim()
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
    let output = Command::new("orca")
        .args([
            "terminal",
            "send",
            "--terminal",
            terminal_handle,
            "--text",
            literal_prompt,
            "--enter",
            "--json",
        ])
        .output()
        .map_err(|e| LauncherError::OrcaExecutionUncertain(format!("Failed to spawn orca terminal send: {}", e)))?;

    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca terminal send returned exit code {:?}: {}",
            output.status.code(),
            err_msg.trim()
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
