//! Shared workspace + ToolBridge construction for the global CLI.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
use xai_grok_tools::implementations::grok_build::{
    BashTool, GrepTool, ListDirTool, ReadFileTool, SearchReplaceTool,
};
use xai_grok_tools::notification::ToolNotificationHandle;
use xai_grok_tools::registry::types::{SessionContext, ToolConfig, ToolServerConfig};
use xai_grok_tools::reminders::DEFAULT_REMINDER_TAG;

pub fn config_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/grok-harness")
}

pub fn workspace_file() -> PathBuf {
    config_dir().join("workspace")
}

pub fn read_pinned_workspace() -> Option<PathBuf> {
    let raw = std::fs::read_to_string(workspace_file()).ok()?;
    let path = PathBuf::from(raw.trim());
    if path.is_dir() {
        dunce::canonicalize(&path).ok()
    } else {
        None
    }
}

pub fn pin_workspace(dir: &Path) -> Result<PathBuf, String> {
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
    if let Ok(env_path) = std::env::var("GROK_HARNESS_WORKSPACE") {
        let p = PathBuf::from(env_path);
        if let Ok(c) = dunce::canonicalize(&p) {
            if c.is_dir() {
                return c;
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
            ToolConfig::from(&SearchReplaceTool),
            ToolConfig::from(&BashTool).with_param("enabled_background", false),
        ],
        behavior_preset: None,
    }
}

fn session_context(cwd: PathBuf) -> SessionContext {
    let host_dir = std::env::temp_dir().join("grok-harness");
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
