//! Workspace pin + ToolBridge. Unofficial; runtime from xai-org/grok-build.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
use xai_grok_tools::implementations::codex::ApplyPatchTool;
use xai_grok_tools::implementations::{
    BashTool, GrepTool, KillTaskTool, ListDirTool, OpenCodeGlobTool, OpenCodeWriteTool,
    ReadFileTool, SearchReplaceTool, TaskOutputTool, TodoWriteTool,
};
use xai_grok_tools::notification::ToolNotificationHandle;
use xai_grok_tools::registry::types::{SessionContext, ToolConfig, ToolServerConfig};
use xai_grok_tools::reminders::DEFAULT_REMINDER_TAG;

pub const APP: &str = "hands";
pub const DISPLAY: &str = "Hands";

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// XDG on Unix (`~/.config/hands`). `%APPDATA%\hands` on Windows.
pub fn config_dir() -> PathBuf {
    #[cfg(windows)]
    {
        return dirs::config_dir()
            .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
            .join(APP);
    }
    home_dir().join(".config").join(APP)
}

pub fn tunnel_client_dir() -> PathBuf {
    #[cfg(windows)]
    {
        return dirs::config_dir()
            .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
            .join("tunnel-client");
    }
    home_dir().join(".config/tunnel-client")
}

pub fn workspace_file() -> PathBuf {
    config_dir().join("workspace")
}

/// Copy `~/.config/grok-harness` once if the new dir is empty.
pub fn migrate_from_legacy() {
    let dest = config_dir();
    if dest.join("workspace").is_file() || dest.join("control-plane.key").is_file() {
        return;
    }
    let src = home_dir().join(".config/grok-harness");
    if !src.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(&dest);
    for name in ["workspace", "control-plane.key"] {
        let from = src.join(name);
        let to = dest.join(name);
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

pub fn read_pinned_workspace() -> Option<PathBuf> {
    migrate_from_legacy();
    let raw = std::fs::read_to_string(workspace_file()).ok()?;
    let path = PathBuf::from(raw.trim());
    if path.is_dir() {
        dunce::canonicalize(&path).ok()
    } else {
        None
    }
}

pub fn pin_workspace(dir: &Path) -> Result<PathBuf, String> {
    migrate_from_legacy();
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    let cwd = dunce::canonicalize(dir).map_err(|e| format!("canonicalize: {e}"))?;
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    std::fs::write(workspace_file(), format!("{}\n", cwd.display()))
        .map_err(|e| format!("write workspace pin: {e}"))?;
    Ok(cwd)
}

/// Active workspace: env → pin file → `--cwd`/process cwd.
pub fn resolve_workspace(fallback: &Path) -> PathBuf {
    migrate_from_legacy();
    for var in ["HANDS_WORKSPACE", "GROK_HARNESS_WORKSPACE"] {
        if let Ok(env_path) = std::env::var(var) {
            let p = PathBuf::from(env_path);
            if let Ok(c) = dunce::canonicalize(&p) {
                if c.is_dir() {
                    return c;
                }
            }
        }
    }
    if let Some(pinned) = read_pinned_workspace() {
        return pinned;
    }
    dunce::canonicalize(fallback).unwrap_or_else(|_| fallback.to_path_buf())
}

fn allowlist() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            ToolConfig::from(&ReadFileTool),
            ToolConfig::from(&GrepTool),
            ToolConfig::from(&ListDirTool),
            ToolConfig::from(&OpenCodeGlobTool),
            ToolConfig::from(&SearchReplaceTool),
            ToolConfig::from(&OpenCodeWriteTool),
            ToolConfig::from(&ApplyPatchTool),
            ToolConfig::from(&TodoWriteTool),
            ToolConfig::from(&BashTool),
            ToolConfig::from(&TaskOutputTool),
            ToolConfig::from(&KillTaskTool),
        ],
        behavior_preset: None,
    }
}

fn session_context(cwd: PathBuf) -> SessionContext {
    let host_dir = std::env::temp_dir().join(APP);
    let _ = std::fs::create_dir_all(&host_dir);
    SessionContext {
        backend: Arc::new(LocalTerminalBackend::new()),
        fs: Arc::new(LocalFs),
        cwd,
        session_folder: host_dir.join("session"),
        session_env: Arc::new(HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: host_dir.join("state.json"),
        memory_backend: None,
        web_search_config: Default::default(),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: DEFAULT_REMINDER_TAG,
    }
}

pub async fn build_bridge(cwd: PathBuf) -> Result<ToolBridge, String> {
    let mut builder = ToolBridge::get_builder();
    builder.set_system_reminders_enabled(false);
    ToolBridge::finalize_builder(builder, allowlist(), session_context(cwd))
        .await
        .map_err(|e| e.to_string())
}
