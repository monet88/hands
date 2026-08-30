//! Tunnel profile rendering, credential inspection, daemon execution, and environment scrubbing.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::health::HEALTH_LISTEN;
use crate::host;

pub const PROFILE: &str = "hands";
pub const LEGACY_PROFILE: &str = "grok-harness";

pub const TUNNEL_CHILD_ENV_REMOVE: &[&str] = &[
    "CONTROL_PLANE_URL_PATH",
    "LOG_HTTP_RAW_UNSAFE",
    "MCP_SERVER_URL",
    "MCP_COMMAND",
    "TUNNEL_CLIENT_CONFIG",
    "TUNNEL_CLIENT_PROFILE",
    "TUNNEL_CLIENT_PROFILE_FILE",
    "TUNNEL_CLIENT_PROFILE_DIR",
    "XDG_CONFIG_HOME",
    "HEALTH_LISTEN_ADDR",
    "HEALTH_UNIX_SOCKET",
    "HEALTH_URL_FILE",
    "CONTROL_PLANE_BASE_URL",
];

pub fn profile_file() -> PathBuf {
    host::tunnel_client_dir().join(format!("{PROFILE}.yaml"))
}

pub fn legacy_profile_file() -> PathBuf {
    host::tunnel_client_dir().join(format!("{LEGACY_PROFILE}.yaml"))
}

pub fn has_key() -> bool {
    host::migrate_from_legacy();
    crate::secrets::get().is_some()
}

pub fn tunnel_id_opt() -> Option<String> {
    resolve_tunnel_id().ok()
}

pub fn valid_tunnel_id(id: &str) -> bool {
    id.starts_with("tunnel_")
}

pub fn set_tunnel_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if !valid_tunnel_id(id) {
        return Err("tunnel id should look like tunnel_…".into());
    }
    let dir = host::config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    write_secret(&dir.join("tunnel_id"), id)
}

pub fn save_connect(key: Option<&str>, tunnel_id: Option<&str>) -> Result<(), String> {
    host::migrate_from_legacy();
    if let Some(key) = key.map(str::trim).filter(|s| !s.is_empty()) {
        crate::secrets::set(key)?;
    }
    if let Some(id) = tunnel_id.map(str::trim).filter(|s| !s.is_empty()) {
        set_tunnel_id(id)?;
    }
    if super::can_enable() {
        super::enable()?;
    }
    Ok(())
}

pub fn status_json(workspace: &Path) -> serde_json::Value {
    host::migrate_from_legacy();
    let pin = host::read_pinned_workspace();
    serde_json::json!({
        "name": host::DISPLAY,
        "unofficial": true,
        "version": env!("CARGO_PKG_VERSION"),
        "source_git_sha": crate::build_provenance::SOURCE_GIT_SHA,
        "workspace": workspace.display().to_string(),
        "pin": pin.as_ref().map(|p| p.display().to_string()),
        "tunnel_ready": super::ready(),
        "tunnel_admin": format!("{}/ui", super::HEALTH_BASE),
        "service": if super::installed() { "enabled" } else { "off" },
        "has_key": has_key(),
        "tunnel_id": tunnel_id_opt(),
        "chatgpt": "https://chatgpt.com/plugins",
    })
}

pub fn write_secret(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(path, format!("{}\n", contents.trim()))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn resolve_tunnel_id() -> Result<String, String> {
    if let Ok(id) = std::env::var("CONTROL_PLANE_TUNNEL_ID") {
        let id = id.trim();
        if valid_tunnel_id(id) {
            return Ok(id.to_string());
        }
    }
    if let Ok(id) = fs::read_to_string(host::config_dir().join("tunnel_id")) {
        let id = id.trim();
        if valid_tunnel_id(id) {
            return Ok(id.to_string());
        }
    }
    for path in [profile_file(), legacy_profile_file()] {
        if let Ok(text) = fs::read_to_string(path) {
            for line in text.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("tunnel_id:") {
                    let id = rest.trim().trim_matches('"').trim();
                    if valid_tunnel_id(id) {
                        return Ok(id.to_string());
                    }
                }
            }
        }
    }
    Err("missing tunnel id. paste it in the config UI (hands config) or export CONTROL_PLANE_TUNNEL_ID".into())
}

#[cfg_attr(windows, allow(unused_variables))]
pub fn write_profile(key: &Path, harness: &Path, tunnel_id: &str) -> Result<(), String> {
    write_profile_at(&profile_file(), key, harness, tunnel_id)
}

#[cfg_attr(windows, allow(unused_variables))]
pub fn write_profile_at(
    profile_path: &Path,
    key: &Path,
    harness: &Path,
    tunnel_id: &str,
) -> Result<(), String> {
    if let Some(parent) = profile_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let harness = yaml_double_quoted(&mcp_command_value(harness));
    let tunnel_id = yaml_double_quoted(tunnel_id);
    // Windows: profile references the env var; the key itself lives only in
    // Credential Manager. Unix: profile references the 0600 key file.
    #[cfg(windows)]
    let api_key_entry = "api_key: \"env:CONTROL_PLANE_API_KEY\"".to_string();
    #[cfg(not(windows))]
    let api_key_entry = format!(
        "api_key: \"file:{}\"",
        yaml_double_quoted(&key.display().to_string())
    );

    let yaml = format!(
        r#"config_version: 1
control_plane:
  base_url: "https://api.openai.com"
  tunnel_id: "{tunnel_id}"
  {api_key_entry}
health:
  listen_addr: "{HEALTH_LISTEN}"
admin_ui:
  open_browser: false
log:
  level: warn
  format: json
mcp:
  commands:
    - channel: main
      command: "{harness}"
"#
    );
    fs::write(profile_path, yaml).map_err(|e| format!("write {}: {e}", profile_path.display()))?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(profile_path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn yaml_double_quoted(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

pub fn mcp_command_value(executable: &Path) -> String {
    let raw = executable.display().to_string();
    #[cfg(windows)]
    {
        // tunnel-client parses the YAML command value with a shell-like lexer:
        // Windows backslashes are escape characters there. Forward slashes
        // are accepted by CreateProcess/Go exec and quoting preserves spaces.
        return format!("\"{}\"", raw.replace('\\', "/"));
    }
    #[cfg(not(windows))]
    {
        raw
    }
}

pub fn tunnel_client_bin() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        // The Windows installer owns a pinned, verified tunnel-client next to
        // hands.exe, including custom HANDS_PREFIX installs. Prefer that exact
        // sibling before consulting PATH so an unrelated earlier PATH entry
        // can never receive the Runtime Key in the normal installed flow.
        if let Ok(exe) = std::env::current_exe()
            && let Some(parent) = exe.parent()
        {
            let sibling = parent.join("tunnel-client.exe");
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
        // Preserve the standard install location as a fallback for callers
        // whose executable path cannot be resolved normally.
        if let Some(local) = dirs::data_local_dir() {
            let managed = local.join("Programs/hands/bin/tunnel-client.exe");
            if managed.is_file() {
                return Ok(managed);
            }
        }
    }
    which("tunnel-client").ok_or_else(|| {
        if cfg!(windows) {
            "tunnel-client.exe not found. Run install.ps1 or place tunnel-client.exe in PATH".into()
        } else {
            "tunnel-client not found. brew install openai/tools/tunnel-client".into()
        }
    })
}

pub fn which(name: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
        #[cfg(windows)]
        {
            dirs.push(home.join(".cargo/bin"));
        }
    }
    #[cfg(windows)]
    {
        if let Some(local) = dirs::data_local_dir() {
            dirs.push(local.join("Programs/hands/bin"));
            dirs.push(local.join("Programs/openai/tunnel-client"));
            dirs.push(local.join("Programs/orca/resources/bin"));
        }
    }
    #[cfg(not(windows))]
    {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
    }

    #[cfg(windows)]
    let extensions: Vec<String> = {
        if let Some(pathext) = std::env::var_os("PATHEXT") {
            std::env::split_paths(&pathext)
                .filter_map(|p| p.to_str().map(|s| s.to_ascii_lowercase()))
                .collect()
        } else {
            vec![".exe".into(), ".cmd".into(), ".bat".into()]
        }
    };

    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            if !name.contains('.') {
                for ext in &extensions {
                    let ext_candidate = dir.join(format!("{name}{ext}"));
                    if ext_candidate.is_file() {
                        return Some(ext_candidate);
                    }
                }
            }
        }
    }
    None
}

pub fn run_tunnel_daemon() -> Result<(), String> {
    host::migrate_from_legacy();
    let harness = super::harness_bin()?;
    let client = tunnel_client_bin()?;
    let fake_key_path = crate::secrets::key_file();

    // Continuous supervision: if the tunnel-client exits unexpectedly, restart
    // with backoff so the Task Scheduler RestartOnFailure cap (now 999) is not
    // the sole recovery path. This satisfies the "reliable login/restart
    // supervisor" requirement without ever leaving Hands down permanently.
    loop {
        // Reload both values before every spawn. Runtime keys and Tunnel IDs
        // can be rotated while the supervisor remains alive; a retry must not
        // keep using stale credentials/configuration.
        let key = crate::secrets::get().ok_or_else(|| {
            "missing runtime key. run hands setup, or export CONTROL_PLANE_API_KEY".to_string()
        })?;
        let tunnel_id = resolve_tunnel_id()?;
        write_profile(&fake_key_path, &harness, &tunnel_id)?;

        let mut cmd = Command::new(&client);
        cmd.arg("run")
            .arg("--profile")
            .arg(PROFILE)
            .arg("--profile-dir")
            .arg(host::tunnel_client_dir())
            .arg("--log.level=warn");
        // Hands owns the MCP target and fixed health endpoint in its profile.
        // Do not let unrelated parent-shell/app environment overrides replace
        // that stdio target (for example MCP_SERVER_URL from a local bridge)
        // or move /readyz away from HEALTH_LISTEN.
        for name in TUNNEL_CHILD_ENV_REMOVE {
            cmd.env_remove(name);
        }
        cmd.env("CONTROL_PLANE_API_KEY", &key);
        cmd.env("CONTROL_PLANE_TUNNEL_ID", &tunnel_id);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn tunnel-client: {e}"))?;
        #[cfg(windows)]
        if let Err(e) = super::platform::write_tunnel_pid(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("record tunnel ownership: {e}"));
        }
        let status = child
            .wait()
            .map_err(|e| format!("wait tunnel-client: {e}"))?;
        #[cfg(windows)]
        {
            let _ = fs::remove_file(super::platform::tunnel_pid_file());
        }
        if status.success() {
            return Ok(());
        }
        eprintln!("tunnel-client exited with status: {status}; restarting in 5s...");
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_yaml_double_quoted_escapes_windows_path_characters() {
        let raw = "C:\\Program Files\\Hands\\say \"hello\".exe";
        assert_eq!(
            yaml_double_quoted(raw),
            "C:\\\\Program Files\\\\Hands\\\\say \\\"hello\\\".exe"
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_mcp_command_quotes_spaces_without_backslash_escapes() {
        let path = PathBuf::from(r"C:\Program Files\Hands\hands.exe");
        assert_eq!(
            mcp_command_value(&path),
            r#""C:/Program Files/Hands/hands.exe""#
        );
    }

    #[test]
    fn test_tunnel_child_env_scrubs_profile_owned_overrides() {
        for required in [
            "CONTROL_PLANE_URL_PATH",
            "LOG_HTTP_RAW_UNSAFE",
            "MCP_SERVER_URL",
            "MCP_COMMAND",
            "TUNNEL_CLIENT_CONFIG",
            "TUNNEL_CLIENT_PROFILE",
            "TUNNEL_CLIENT_PROFILE_FILE",
            "TUNNEL_CLIENT_PROFILE_DIR",
            "XDG_CONFIG_HOME",
            "HEALTH_LISTEN_ADDR",
            "HEALTH_UNIX_SOCKET",
            "HEALTH_URL_FILE",
        ] {
            assert!(
                TUNNEL_CHILD_ENV_REMOVE.contains(&required),
                "missing profile-owned env scrub: {required}"
            );
        }
    }

    #[test]
    fn test_tunnel_child_env_scrubs_control_plane_base_url() {
        assert!(
            TUNNEL_CHILD_ENV_REMOVE.contains(&"CONTROL_PLANE_BASE_URL"),
            "CONTROL_PLANE_BASE_URL must not override the Hands-owned control-plane endpoint"
        );
    }

    #[test]
    fn test_write_profile_hides_secrets_on_windows() {
        let temp_dir = std::env::temp_dir().join(format!(
            "hands_profile_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        let _ = fs::create_dir_all(&temp_dir);
        let key_file = temp_dir.join("control-plane.key");
        let harness = temp_dir.join("hands.exe");
        let tunnel_id = "tunnel_123456789";
        let profile_path = temp_dir.join("hands.yaml");
        write_profile_at(&profile_path, &key_file, &harness, tunnel_id)
            .expect("write_profile_at should succeed");
        let read = fs::read_to_string(&profile_path).expect("profile should be written");
        #[cfg(windows)]
        {
            assert!(
                read.contains("api_key: \"env:CONTROL_PLANE_API_KEY\""),
                "Windows profile must reference env var, got: {read}"
            );
            assert!(
                !read.contains("file:"),
                "Windows profile must not contain file: entry, got: {read}"
            );
            assert!(!read.contains("sk-"), "profile must not leak raw key");
        }
        #[cfg(not(windows))]
        {
            assert!(read.contains(&format!("file:{}", key_file.display())));
        }
        assert!(
            read.contains("tunnel_123456789"),
            "profile must contain tunnel id: {read}"
        );
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
