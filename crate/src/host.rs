//! Workspace pin + ToolBridge. Unofficial; runtime from xai-org/grok-build.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

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
pub const UPSTREAM_BASE_COMMIT: &str = "c059e0d";
pub const DEV_GIT_REV: &str = env!("DEV_GIT_REV");
const SESSION_TTL_SECS: u64 = 7 * 24 * 60 * 60;

thread_local! {
    static CONFIG_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

fn hands_config_dir_override() -> Option<PathBuf> {
    match std::env::var("HANDS_CONFIG_DIR") {
        Ok(dir) if !dir.trim().is_empty() => Some(PathBuf::from(dir.trim())),
        _ => None,
    }
}

/// Config dir: scoped test override, then $HANDS_CONFIG_DIR if set, else XDG on Unix (`~/.config/hands`), `%APPDATA%\hands` on Windows.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = CONFIG_OVERRIDE.with(|c| c.borrow().clone()) {
        return dir;
    }
    if let Some(dir) = hands_config_dir_override() {
        return dir;
    }
    #[cfg(windows)]
    let base = dirs::config_dir()
        .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
        .join(APP);
    #[cfg(not(windows))]
    let base = home_dir().join(".config").join(APP);
    base
}

pub fn tunnel_client_dir() -> PathBuf {
    if let Some(dir) = hands_config_dir_override() {
        return dir.join("tunnel-client");
    }
    #[cfg(windows)]
    let base = dirs::config_dir()
        .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
        .join("tunnel-client");
    #[cfg(not(windows))]
    let base = home_dir().join(".config/tunnel-client");
    base
}

pub fn workspace_file() -> PathBuf {
    config_dir().join("workspace")
}

pub fn mcp_socket() -> PathBuf {
    config_dir().join("mcp.sock")
}

fn sessions_file() -> PathBuf {
    config_dir().join("sessions.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// ChatGPT plugin host id: `_meta["openai/session"]` on `tools/call`.
pub fn openai_session(params: &Value) -> Option<String> {
    let meta = params.get("_meta")?;
    let raw = meta
        .get("openai/session")
        .or_else(|| meta.get("openai/conversationId"))
        .and_then(Value::as_str)?
        .trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

struct SessionPin {
    path: PathBuf,
    at: u64,
}

fn load_sessions() -> HashMap<String, SessionPin> {
    let Ok(text) = std::fs::read_to_string(sessions_file()) else {
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    let Some(obj) = v.get("sessions").and_then(Value::as_object) else {
        return HashMap::new();
    };
    let cutoff = now_secs().saturating_sub(SESSION_TTL_SECS);
    let mut out = HashMap::new();
    for (id, entry) in obj {
        let Some(path) = entry.get("path").and_then(Value::as_str) else {
            continue;
        };
        let at = entry.get("at").and_then(Value::as_u64).unwrap_or(0);
        let p = PathBuf::from(path);
        if at >= cutoff && p.is_dir() {
            out.insert(id.clone(), SessionPin { path: p, at });
        }
    }
    out
}

fn save_sessions(map: &HashMap<String, SessionPin>) {
    let mut sessions = Map::new();
    for (id, pin) in map {
        sessions.insert(
            id.clone(),
            json!({
                "path": pin.path.display().to_string(),
                "at": pin.at,
            }),
        );
    }
    let body = json!({ "sessions": sessions });
    if let Some(parent) = sessions_file().parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&body) {
        let _ = std::fs::write(sessions_file(), text);
    }
}

fn with_sessions<T>(f: impl FnOnce(&mut HashMap<String, SessionPin>) -> T) -> T {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut map = load_sessions();
    let out = f(&mut map);
    save_sessions(&map);
    out
}

pub fn get_session_workspace(session: &str) -> Option<PathBuf> {
    if session.is_empty() {
        return None;
    }
    with_sessions(|map| map.get(session).map(|p| p.path.clone()))
}

fn put_session_workspace(session: &str, cwd: &Path) {
    let path = cwd.to_path_buf();
    let at = now_secs();
    with_sessions(|map| {
        map.insert(
            session.to_string(),
            SessionPin { path, at },
        );
    });
}

/// `set_workspace` from ChatGPT: pin this conversation only.
/// No `openai/session` → write the CLI pin file (old behavior).
pub fn pin_for_chat(session: Option<&str>, dir: &Path) -> Result<PathBuf, String> {
    match session {
        Some(id) if !id.is_empty() => {
            if !dir.is_dir() {
                return Err(format!("not a directory: {}", dir.display()));
            }
            let cwd = dunce::canonicalize(dir).map_err(|e| format!("canonicalize: {e}"))?;
            push_recent(&cwd);
            put_session_workspace(id, &cwd);
            Ok(cwd)
        }
        _ => pin_workspace(dir),
    }
}

/// Cwd for one MCP tool call: `workspace` arg → chat session map → CLI pin.
pub fn resolve_call_workspace(
    fallback: &Path,
    session: Option<&str>,
    workspace_arg: Option<&str>,
) -> Result<PathBuf, String> {
    if let Some(raw) = workspace_arg {
        return resolve_project(raw);
    }
    if let Some(id) = session.filter(|s| !s.is_empty()) {
        if let Some(p) = get_session_workspace(id) {
            return Ok(p);
        }
    }
    Ok(resolve_workspace(fallback))
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
    push_recent(&cwd);
    Ok(cwd)
}

fn recent_file() -> PathBuf {
    config_dir().join("recent")
}

pub fn read_recent() -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(recent_file()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let p = PathBuf::from(line.trim());
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
        if out.len() >= 20 {
            break;
        }
    }
    out
}

fn push_recent(cwd: &Path) {
    let mut items = read_recent();
    items.retain(|p| p != cwd);
    items.insert(0, cwd.to_path_buf());
    items.truncate(20);
    let body: String = items
        .iter()
        .map(|p| format!("{}\n", p.display()))
        .collect();
    let _ = std::fs::write(recent_file(), body);
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if raw == "~" {
        return home_dir();
    }
    PathBuf::from(raw)
}

/// Resolve a path or short name (`bunko`, `~/Dev/bunko`) to an existing directory.
pub fn resolve_project(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty path".into());
    }
    let expanded = expand_tilde(raw);
    if expanded.is_dir() {
        return dunce::canonicalize(&expanded).map_err(|e| e.to_string());
    }
    let mut tried = vec![expanded.clone()];
    if !expanded.is_absolute() {
        let name = Path::new(raw);
        tried.push(home_dir().join(name));
        tried.push(home_dir().join("Dev").join(name));
        tried.push(home_dir().join("dev").join(name));
        if let Some(pin) = read_pinned_workspace() {
            tried.push(pin.join(name));
            if let Some(parent) = pin.parent() {
                tried.push(parent.join(name));
            }
        }
    }
    for c in &tried {
        if c.is_dir() {
            return dunce::canonicalize(c).map_err(|e| e.to_string());
        }
    }
    Err(format!(
        "not a directory: {raw}. Try an absolute path or a folder name under ~/Dev."
    ))
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
            ToolConfig::from(&BashTool)
                .with_param("enabled_background", true)
                .with_param("auto_background_on_timeout", true),
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

#[cfg(test)]
fn with_test_config<T>(dir: &Path, f: impl FnOnce() -> T) -> T {
    CONFIG_OVERRIDE.with(|c| *c.borrow_mut() = Some(dir.to_path_buf()));
    let out = f();
    CONFIG_OVERRIDE.with(|c| *c.borrow_mut() = None);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_session_reads_host_meta() {
        let params = json!({
            "name": "read_file",
            "arguments": {},
            "_meta": { "openai/session": " conv-aaa " }
        });
        assert_eq!(openai_session(&params).as_deref(), Some("conv-aaa"));
        assert!(openai_session(&json!({ "name": "x" })).is_none());
    }

    #[test]
    fn two_chats_do_not_share_pin() {
        let tmp = std::env::temp_dir().join(format!("hands-session-{}", std::process::id()));
        let cfg = tmp.join("cfg");
        let a = tmp.join("repo-a");
        let b = tmp.join("repo-b");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        with_test_config(&cfg, || {
            pin_workspace(&a).unwrap();
            let wa = pin_for_chat(Some("chat-a"), &a).unwrap();
            let wb = pin_for_chat(Some("chat-b"), &b).unwrap();
            assert_eq!(wa, dunce::canonicalize(&a).unwrap());
            assert_eq!(wb, dunce::canonicalize(&b).unwrap());
            assert_eq!(
                resolve_call_workspace(&a, Some("chat-a"), None).unwrap(),
                wa
            );
            assert_eq!(
                resolve_call_workspace(&a, Some("chat-b"), None).unwrap(),
                wb
            );
            let pin = read_pinned_workspace().unwrap();
            assert_eq!(pin, dunce::canonicalize(&a).unwrap());
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn workspace_arg_wins_over_session() {
        let tmp = std::env::temp_dir().join(format!("hands-wsarg-{}", std::process::id()));
        let cfg = tmp.join("cfg");
        let a = tmp.join("repo-a");
        let b = tmp.join("repo-b");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        with_test_config(&cfg, || {
            pin_for_chat(Some("chat-a"), &a).unwrap();
            let got = resolve_call_workspace(&a, Some("chat-a"), Some(b.to_str().unwrap())).unwrap();
            assert_eq!(got, dunce::canonicalize(&b).unwrap());
        });
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
