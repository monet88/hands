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

/// Pinned supported OMP CLI revision
pub const SUPPORTED_OMP_REVISION: &str = "18.1.16";
/// Exact verified OMP CLI version output shape
pub const SUPPORTED_OMP_CLI_SHAPE: &str = "omp/18.1.16";
/// Keep each Windows CreateProcess argv payload comfortably below the ~32K command-line ceiling.
pub const ORCA_PROMPT_CHUNK_MAX_BYTES: usize = 8 * 1024;

/// Validates exact verified OMP CLI version output shape ("omp/18.1.16")
pub fn is_exact_supported_omp_version(output: &str) -> bool {
    output.trim() == SUPPORTED_OMP_CLI_SHAPE
}

/// Pinned companion adapter revision identifier
pub const COMPANION_ADAPTER_REVISION: &str = "v3";

/// The embedded TypeScript adapter loaded explicitly via `-e` for owned OMP turns
pub const ADAPTER_TS_CONTENT: &str = r#"// Return Bridge companion OMP adapter (revision: v3)
import * as fs from "node:fs";
import * as path from "node:path";
import * as crypto from "node:crypto";
import { Database } from "bun:sqlite";

function computeCanonicalReceiptDigest(params: {
  turnIndex: number;
  stopReason: string;
  assistantMessageId?: string | null;
  assistantText: string;
  toolCallCount: number;
  sessionId: string;
  adapterInstanceId: string;
}): string {
  const hasher = crypto.createHash("sha256");
  hasher.update("hands_rb_receipt_v2:");
  hasher.update(`${params.turnIndex}:`);
  hasher.update(`${params.stopReason}:`);
  hasher.update(`${params.assistantMessageId ?? ""}:`);
  hasher.update(`${params.toolCallCount}:`);
  hasher.update(`${params.sessionId}:`);
  hasher.update(`${params.adapterInstanceId}:`);
  hasher.update(params.assistantText);
  return hasher.digest("hex");
}

function generateRandomHex(bytes: number): string {
  return crypto.randomBytes(bytes).toString("hex");
}

interface MessageContentItem {
  type: string;
  text?: string;
}

interface AssistantMessageLike {
  id?: string;
  role?: string;
  stopReason?: string;
  content?: MessageContentItem[];
}

interface SessionStopEventLike {
  turn_id?: number;
  messages?: unknown[];
  last_assistant_message?: AssistantMessageLike;
  session_id?: string;
  stop_hook_active?: boolean;
}

interface AgentEndEventLike {
  messages?: unknown[];
  willContinue?: boolean;
}

interface ExtensionContextLike {
  sessionManager?: {
    getSessionId?: () => string;
    getSessionFile?: () => string;
  };
}

export default function (pi: { on: (event: string, handler: (event: unknown, ctx?: unknown) => Promise<unknown> | unknown) => void }) {
  const executionId = process.env.HANDS_RETURN_BRIDGE_EXECUTION_ID?.trim();
  if (!executionId) {
    return;
  }

  const stateDir = process.env.HANDS_RETURN_BRIDGE_STATE_DIR?.trim() || import.meta.dir;
  const dbPath = path.join(stateDir, "journal.sqlite");

  const adapterInstanceId = "adp_" + generateRandomHex(16);
  let initialSessionId: string | null = null;
  let isOwner: boolean | null = null;
  let completionCandidate: {
    turnId: number;
    text: string;
    messageId?: string;
    stopReason: string;
    toolCallCount: number;
    sessionId: string;
    stopHookActive: boolean;
  } | null = null;

  function countToolCalls(messages: unknown): number {
    let count = 0;
    if (Array.isArray(messages)) {
      for (const msg of messages) {
        if (msg && typeof msg === "object" && "content" in msg) {
          const content = (msg as { content: unknown }).content;
          if (Array.isArray(content)) {
            for (const item of content) {
              if (item && typeof item === "object" && "type" in item) {
                if ((item as { type: unknown }).type === "toolCall") {
                  count++;
                }
              }
            }
          }
        }
      }
    }
    return count;
  }

  function claimExecutionOwnership(sid: string): boolean {
    if (isOwner !== null) {
      return isOwner;
    }
    try {
      if (!fs.existsSync(dbPath)) {
        isOwner = false;
        return false;
      }
      const db = new Database(dbPath);
      db.run("PRAGMA foreign_keys = ON;");
      const timeoutMs = parseInt(process.env.HANDS_RETURN_BRIDGE_BUSY_TIMEOUT_MS || "5000", 10) || 5000;
      db.run(`PRAGMA busy_timeout = ${timeoutMs};`);

      const nowSecs = Math.floor(Date.now() / 1000);
      try {
        db.run(
          `INSERT INTO execution_adapter_claims (execution_id, adapter_instance_id, session_id, claimed_at)
           VALUES (?, ?, ?, ?)`,
          [executionId, adapterInstanceId, sid, nowSecs]
        );
        isOwner = true;
      } catch (_err) {
        const row = db
          .query(
            "SELECT adapter_instance_id, session_id FROM execution_adapter_claims WHERE execution_id = ?"
          )
          .get(executionId) as
          | { adapter_instance_id: string; session_id: string }
          | undefined;
        isOwner = row?.adapter_instance_id === adapterInstanceId && row?.session_id === sid;
      } finally {
        db.close();
      }
    } catch (_err) {
      isOwner = false;
    }
    return isOwner;
  }

  pi.on("agent_start", (_event: unknown, ctxRaw: unknown) => {
    const ctx = ctxRaw as ExtensionContextLike | undefined;
    const sid = ctx?.sessionManager?.getSessionId?.();
    if (typeof sid !== "string" || !sid.trim()) {
      return;
    }
    if (initialSessionId === null) {
      initialSessionId = sid;
      claimExecutionOwnership(sid);
    }
  });

  pi.on("session_before_switch", () => {
    completionCandidate = null;
    isOwner = false;
  });

  pi.on("session_switch", () => {
    completionCandidate = null;
    isOwner = false;
  });
  pi.on("session_stop", (eventRaw: unknown, ctxRaw: unknown) => {
    const ctx = ctxRaw as ExtensionContextLike | undefined;
    const sid = ctx?.sessionManager?.getSessionId?.();
    if (typeof sid !== "string" || !sid.trim()) {
      completionCandidate = null;
      return;
    }
    if (initialSessionId === null) {
      initialSessionId = sid;
      claimExecutionOwnership(sid);
    }
    if (sid !== initialSessionId || !isOwner) {
      completionCandidate = null;
      return;
    }

    const event = eventRaw as SessionStopEventLike | undefined;
    if (typeof event?.turn_id !== "number" || !Number.isInteger(event.turn_id) || event.turn_id < 0) {
      completionCandidate = null;
      return;
    }
    // Fail closed if stop_hook_active is missing or not strictly boolean false (OMP 18.1.16 guarantees stop_hook_active: boolean)
    if (typeof event?.stop_hook_active !== "boolean" || event.stop_hook_active !== false) {
      completionCandidate = null;
      return;
    }
    const lastMsg = event?.last_assistant_message;
    if (!lastMsg) {
      completionCandidate = null;
      return;
    }

    const stopReason = lastMsg.stopReason;
    if (stopReason !== "stop" && stopReason !== "end_turn") {
      completionCandidate = null;
      return;
    }

    const content = lastMsg.content;
    if (!Array.isArray(content) || content.length === 0) {
      completionCandidate = null;
      return;
    }

    // Must not end mid-tool-use
    const hasToolCalls = content.some(c => c && c.type === "toolCall");
    if (hasToolCalls) {
      completionCandidate = null;
      return;
    }

    // Extract text parts
    const textParts = content
      .filter(c => c && c.type === "text" && typeof c.text === "string")
      .map(c => c.text!.trim())
      .filter(Boolean);

    const fullText = textParts.join("\n").trim();
    if (!fullText) {
      completionCandidate = null;
      return;
    }

    completionCandidate = {
      turnId: event.turn_id,
      text: fullText,
      messageId: typeof lastMsg.id === "string" ? lastMsg.id : undefined,
      stopReason,
      toolCallCount: countToolCalls(event?.messages),
      sessionId: sid,
      stopHookActive: event.stop_hook_active,
    };
  });

  pi.on("agent_end", (eventRaw: unknown, ctxRaw: unknown) => {
    const ctx = ctxRaw as ExtensionContextLike | undefined;
    const sid = ctx?.sessionManager?.getSessionId?.();
    if (typeof sid !== "string" || !sid.trim() || sid !== initialSessionId || !isOwner) {
      return;
    }

    const event = eventRaw as AgentEndEventLike | undefined;
    // Explicit terminal check: willContinue must be strictly boolean false,
    // OR under verified OMP 18.1.16 provider lifecycle contract where terminal agent_end
    // emits options?: { willContinue?: boolean } as undefined on terminal completion,
    // undefined is accepted ONLY when paired with a completionCandidate whose session_stop
    // event verified stop_hook_active === false and all required authority fields.
    const isExplicitFalse = event?.willContinue === false;
    const isOmp18Terminal = event?.willContinue === undefined && completionCandidate?.stopHookActive === false;
    if (!isExplicitFalse && !isOmp18Terminal) {
      completionCandidate = null;
      return;
    }
    if (event?.willContinue === true) {
      completionCandidate = null;
      return;
    }
    if (!completionCandidate || completionCandidate.sessionId !== sid) {
      return;
    }

    const candidate = completionCandidate;
    completionCandidate = null;

    try {
      if (!fs.existsSync(dbPath)) {
        return;
      }

      const db = new Database(dbPath);
      try {
        db.run("PRAGMA foreign_keys = ON;");
        const timeoutMs = parseInt(process.env.HANDS_RETURN_BRIDGE_BUSY_TIMEOUT_MS || "5000", 10) || 5000;
        db.run(`PRAGMA busy_timeout = ${timeoutMs};`);

        // Test-only fault hook simulating SQLite SQLITE_FULL without filling physical disk
        if (process.env.HANDS_RETURN_BRIDGE_FAULT_INJECT === "disk_full") {
          db.run("PRAGMA max_page_count = 1;");
        }
        const tx = db.transaction(() => {
        // Verify durable ownership claim in SQLite matches this adapter instance
        const claim = db
          .query(
            "SELECT adapter_instance_id, session_id FROM execution_adapter_claims WHERE execution_id = ?"
          )
          .get(executionId) as
          | { adapter_instance_id: string; session_id: string }
          | undefined;

        if (!claim || claim.adapter_instance_id !== adapterInstanceId || claim.session_id !== sid) {
          return;
        }

        // 1. Verify launch request exists
        const req = db
          .query(
            "SELECT pairing_id, return_token, origin_conversation_id FROM launch_requests WHERE execution_id = ?"
          )
          .get(executionId) as
          | { pairing_id: string; return_token: string; origin_conversation_id: string }
          | undefined;

        if (!req) {
          return;
        }

        const digest = computeCanonicalReceiptDigest({
          turnIndex: candidate.turnId,
          stopReason: candidate.stopReason,
          assistantMessageId: candidate.messageId,
          assistantText: candidate.text,
          toolCallCount: candidate.toolCallCount,
          sessionId: candidate.sessionId,
          adapterInstanceId,
        });

        // 2. Check existing receipt for deterministic duplicate or conflict
        const existing = db
          .query(
            "SELECT receipt_id, content_digest FROM completion_receipts WHERE execution_id = ?"
          )
          .get(executionId) as { receipt_id: string; content_digest: string } | undefined;

        if (existing) {
          if (existing.content_digest === digest) {
            return;
          } else {
            // Conflict: different digest for same execution -> reject explicitly and never overwrite
            console.error(
              `[ReturnBridge] Conflict: execution ${executionId} already has receipt ${existing.receipt_id} with conflicting digest ${existing.content_digest} vs ${digest}`
            );
            throw new Error(`payload_conflict: conflicting completion digest for execution ${executionId}`);
          }
        }

        const receiptId = "rcpt_" + generateRandomHex(16);
        const nowSecs = Math.floor(Date.now() / 1000);

        db.run(
          `INSERT INTO completion_receipts (
            receipt_id, execution_id, pairing_id, return_token,
            origin_conversation_id, turn_index, stop_reason,
            assistant_message_id, assistant_text, content_digest,
            tool_call_count, state, committed_at
          ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'completed', ?)`,
          [
            receiptId,
            executionId,
            req.pairing_id,
            req.return_token,
            req.origin_conversation_id,
            candidate.turnId,
            candidate.stopReason,
            candidate.messageId ?? null,
            candidate.text,
            digest,
            candidate.toolCallCount,
            nowSecs,
          ]
        );

        db.run(
          "UPDATE launch_requests SET state = 'completed', updated_at = ? WHERE execution_id = ?",
          [nowSecs, executionId]
        );

        db.run(
          "UPDATE launch_attempts SET state = 'completed' WHERE execution_id = ?",
          [executionId]
        );
        });

        tx.immediate();
      } finally {
        db.close();
      }
    } catch (err) {
      if (err instanceof Error && err.message.startsWith("payload_conflict")) {
        throw err;
      }
      // Storage/disk/lock failure: fail closed without false acknowledgement
    }
  });
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
            return trimmed.trim_matches('"').trim_matches('\'').to_string();
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
    if !orca_wait_help.status.success() {
        return Err(LauncherError::PreflightFailed(format!(
            "'orca terminal wait --help' failed with exit code: {:?}",
            orca_wait_help.status.code()
        )));
    }
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
    if !orca_send_help.status.success() {
        return Err(LauncherError::PreflightFailed(format!(
            "'orca terminal send --help' failed with exit code: {:?}",
            orca_send_help.status.code()
        )));
    }
    let send_text = String::from_utf8_lossy(&orca_send_help.stdout);
    if !send_text.contains("--text") || !send_text.contains("--enter") {
        return Err(LauncherError::PreflightFailed(
            "Orca terminal send missing required '--text' or '--enter' capability".to_string(),
        ));
    }

    // 4. Verify exact supported OMP CLI revision
    let raw_bin = omp_bin_override
        .map(str::to_string)
        .unwrap_or_else(resolve_omp_binary);
    let clean_bin = raw_bin.trim_matches('"');
    let omp_version = Command::new(clean_bin)
        .arg("--version")
        .output()
        .map_err(|e| LauncherError::PreflightFailed(format!("Failed to execute '{} --version': {}", clean_bin, e)))?;
    if !omp_version.status.success() {
        return Err(LauncherError::PreflightFailed(format!(
            "OMP CLI '{} --version' failed with exit code: {:?}",
            clean_bin,
            omp_version.status.code()
        )));
    }
    let ver_text = String::from_utf8_lossy(&omp_version.stdout);
    let trimmed_ver = ver_text.trim();
    if !is_exact_supported_omp_version(trimmed_ver) {
        return Err(LauncherError::PreflightFailed(format!(
            "Unsupported OMP revision: '{}'. Return Bridge requires exact supported revision '{}'",
            trimmed_ver,
            SUPPORTED_OMP_CLI_SHAPE
        )));
    }

    // 5. Verify basic OMP CLI responsiveness
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
    Ok(())
}

/// Builds native-owned OMP startup command for normal OMP execution with Return Bridge companion adapter (NO user prompt)
pub fn build_omp_startup_command(
    adapter_path: &Path,
) -> Result<String, LauncherError> {
    build_omp_startup_command_with_env(adapter_path, None, None)
}

/// Builds native-owned OMP startup command for normal OMP execution with Return Bridge companion adapter and optional execution environment
pub fn build_omp_startup_command_with_env(
    adapter_path: &Path,
    execution_id: Option<&str>,
    state_dir: Option<&Path>,
) -> Result<String, LauncherError> {
    build_omp_startup_command_with_env_and_bin(adapter_path, execution_id, state_dir, None)
}

/// Pure builder variant used by tests and callers that already resolved an OMP binary.
pub fn build_omp_startup_command_with_env_and_bin(
    adapter_path: &Path,
    execution_id: Option<&str>,
    state_dir: Option<&Path>,
    omp_bin_override: Option<&str>,
) -> Result<String, LauncherError> {
    let omp_bin = omp_bin_override
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .unwrap_or_else(resolve_omp_binary);
    let mut parts = Vec::new();

    if let Some(dir) = state_dir {
        parts.push(format!(
            "$env:HANDS_RETURN_BRIDGE_STATE_DIR={};",
            powershell_single_quoted(&dir.to_string_lossy().replace('\\', "/"))
        ));
    }
    if let Some(exec_id) = execution_id {
        parts.push(format!(
            "$env:HANDS_RETURN_BRIDGE_EXECUTION_ID={};",
            powershell_single_quoted(exec_id)
        ));
    }

    // PowerShell call operator '&' ensures executable paths (whether quoted with spaces or plain tokens) execute properly
    parts.push("&".to_string());
    parts.push(powershell_single_quoted(&omp_bin.replace('\\', "/")));
    parts.push("-e".to_string());
    parts.push(powershell_single_quoted(&adapter_path.to_string_lossy().replace('\\', "/")));

    Ok(parts.join(" "))
}

fn powershell_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
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

pub fn split_prompt_for_orca(literal_prompt: &str) -> Vec<&str> {
    if literal_prompt.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut start = 0;
    while start < literal_prompt.len() {
        let mut end = std::cmp::min(start + ORCA_PROMPT_CHUNK_MAX_BYTES, literal_prompt.len());
        while end > start && !literal_prompt.is_char_boundary(end) {
            end -= 1;
        }
        debug_assert!(end > start, "chunk bound is larger than any UTF-8 scalar");
        chunks.push(&literal_prompt[start..end]);
        start = end;
    }
    chunks
}

fn verify_orca_send_output(output: std::process::Output, operation: &str) -> Result<(), LauncherError> {
    if !output.status.success() {
        let err_msg = String::from_utf8_lossy(&output.stderr);
        let out_msg = String::from_utf8_lossy(&output.stdout);
        let detail = if !err_msg.trim().is_empty() {
            err_msg.trim()
        } else {
            out_msg.trim()
        };
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca terminal send ({operation}) returned exit code {:?}: {}",
            output.status.code(),
            detail
        )));
    }

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let val: Value = serde_json::from_str(&stdout_str).map_err(|e| {
        LauncherError::OrcaExecutionUncertain(format!(
            "Failed to parse Orca terminal send ({operation}) JSON: {}. Output was: {}",
            e, stdout_str
        ))
    })?;
    if val.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return Err(LauncherError::OrcaExecutionUncertain(format!(
            "Orca terminal send ({operation}) response not ok: {}",
            stdout_str
        )));
    }
    Ok(())
}

/// Sends exact bounded task prompt as small literal argv chunks, then submits one Enter.
pub fn send_orca_terminal_prompt(terminal_handle: &str, literal_prompt: &str) -> Result<(), LauncherError> {
    for chunk in split_prompt_for_orca(literal_prompt) {
        let text_arg = format!("--text={chunk}");
        let output = Command::new("orca")
            .args(["terminal", "send", "--terminal", terminal_handle, &text_arg, "--json"])
            .output()
            .map_err(|e| LauncherError::OrcaExecutionUncertain(format!("Failed to spawn orca terminal text send: {}", e)))?;
        verify_orca_send_output(output, "text chunk")?;
    }

    let output = Command::new("orca")
        .args(["terminal", "send", "--terminal", terminal_handle, "--enter", "--json"])
        .output()
        .map_err(|e| LauncherError::OrcaExecutionUncertain(format!("Failed to spawn orca terminal Enter send: {}", e)))?;
    verify_orca_send_output(output, "final Enter")
}
