//! Keep `tunnel-client` running: login start + restart if the MCP child dies.
//!
//! Foreground `tunnel-client run` exits when its stdio MCP child is killed.
//! A LaunchAgent / systemd user unit with KeepAlive is the actual client.

use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::host;

pub const HEALTH_LISTEN: &str = "127.0.0.1:18780";
pub const HEALTH_BASE: &str = "http://127.0.0.1:18780";
pub const PROFILE: &str = "hands";
#[cfg(target_os = "macos")]
const LABEL: &str = "dev.hands.tunnel";
#[cfg(target_os = "macos")]
const WATCH_LABEL: &str = "dev.hands.watch";
#[cfg(target_os = "macos")]
const LEGACY_LABEL: &str = "ai.grok.harness.tunnel";
#[cfg(windows)]
const TASK_NAME: &str = "dev.hands.tunnel";
#[cfg(windows)]
const WATCH_TASK_NAME: &str = "dev.hands.watch";
const LEGACY_PROFILE: &str = "grok-harness";

pub fn profile_file() -> PathBuf {
    host::tunnel_client_dir().join(format!("{PROFILE}.yaml"))
}

fn legacy_profile_file() -> PathBuf {
    host::tunnel_client_dir().join(format!("{LEGACY_PROFILE}.yaml"))
}

pub fn ready() -> bool {
    ureq_get(&format!("{HEALTH_BASE}/readyz"))
        .ok()
        .is_some_and(|s| s == "ready")
}

pub fn wait_ready(timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    ready()
}

pub fn status_line() -> String {
    let health = if ready() {
        format!("ready  {HEALTH_BASE}/ui")
    } else {
        "down".into()
    };
    let svc = if installed() {
        "enabled (login + restart)"
    } else {
        "off — hands setup"
    };
    format!("{health}\nservice    {svc}")
}

/// Pin-time helper: start the supervised client if it is down.
pub fn ensure() -> Result<bool, String> {
    if ready() {
        if !installed() && can_enable() {
            enable()?;
        }
        return Ok(ready());
    }
    if installed() {
        start()?;
    } else if can_enable() {
        enable()?;
    } else {
        return Ok(false);
    }
    Ok(wait_ready(Duration::from_secs(12)))
}

pub fn enable() -> Result<(), String> {
    host::migrate_from_legacy();
    let key = persist_key()?;
    let tunnel_id = resolve_tunnel_id()?;
    let harness = harness_bin()?;
    let client = tunnel_client_bin()?;
    write_profile(&key, &harness, &tunnel_id)?;
    write_wrapper(&client)?;
    install_supervisor()?;
    if let Err(e) = install_watch() {
        eprintln!("warning: Hands watch task not installed: {e}");
    }
    if wait_ready(Duration::from_secs(15)) {
        eprintln!("tunnel on. login start + restart. config: hands config");
        eprintln!("admin  {HEALTH_BASE}/ui");
        Ok(())
    } else {
        Err(format!(
            "service installed but /readyz is not up yet. logs: {}",
            host::config_dir().join("logs").display()
        ))
    }
}

pub fn disable() -> Result<(), String> {
    uninstall_supervisor()?;
    eprintln!("tunnel auto-start removed.");
    Ok(())
}

pub fn start() -> Result<(), String> {
    if !installed() {
        return enable();
    }
    start_supervisor()?;
    if wait_ready(Duration::from_secs(15)) {
        eprintln!("tunnel ready  {HEALTH_BASE}/ui");
        Ok(())
    } else {
        Err("tunnel did not become ready".into())
    }
}

pub fn stop() -> Result<(), String> {
    stop_supervisor()?;
    eprintln!("tunnel stopped (will start again at next login if enabled).");
    Ok(())
}

pub fn has_key() -> bool {
    host::migrate_from_legacy();
    crate::secrets::get().is_some()
}

pub fn tunnel_id_opt() -> Option<String> {
    resolve_tunnel_id().ok()
}

/// Save credentials from the config UI. Empty strings are ignored.
pub fn save_connect(key: Option<&str>, tunnel_id: Option<&str>) -> Result<(), String> {
    host::migrate_from_legacy();
    if let Some(key) = key.map(str::trim).filter(|s| !s.is_empty()) {
        crate::secrets::set(key)?;
    }
    if let Some(id) = tunnel_id.map(str::trim).filter(|s| !s.is_empty()) {
        set_tunnel_id(id)?;
    }
    if can_enable() {
        enable()?;
    }
    Ok(())
}

pub fn set_tunnel_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if !id.starts_with("tunnel_") {
        return Err("tunnel id should look like tunnel_…".into());
    }
    let dir = host::config_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    write_secret(&dir.join("tunnel_id"), id)
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
        "tunnel_ready": ready(),
        "tunnel_admin": format!("{HEALTH_BASE}/ui"),
        "service": if installed() { "enabled" } else { "off" },
        "has_key": has_key(),
        "tunnel_id": tunnel_id_opt(),
        "chatgpt": "https://chatgpt.com/plugins",
    })
}

fn can_enable() -> bool {
    persist_key().is_ok() && resolve_tunnel_id().is_ok() && tunnel_client_bin().is_ok()
}

fn persist_key() -> Result<PathBuf, String> {
    let k = crate::secrets::get().ok_or_else(|| {
        "missing runtime key. run hands setup, or export CONTROL_PLANE_API_KEY".to_string()
    })?;
    #[cfg(windows)]
    {
        crate::secrets::win_cred_set(&k)?;
        Ok(crate::secrets::key_file())
    }
    #[cfg(not(windows))]
    {
        crate::secrets::ensure_file(&k)
    }
}

fn write_secret(path: &Path, contents: &str) -> Result<(), String> {
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

fn resolve_tunnel_id() -> Result<String, String> {
    if let Ok(id) = std::env::var("CONTROL_PLANE_TUNNEL_ID") {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    if let Ok(id) = fs::read_to_string(host::config_dir().join("tunnel_id")) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    for path in [profile_file(), legacy_profile_file()] {
        if let Ok(text) = fs::read_to_string(path) {
            for line in text.lines() {
                let t = line.trim();
                if let Some(rest) = t.strip_prefix("tunnel_id:") {
                    let id = rest.trim().trim_matches('"').trim();
                    if !id.is_empty() {
                        return Ok(id.to_string());
                    }
                }
            }
        }
    }
    Err("missing tunnel id. paste it in the config UI (hands config) or export CONTROL_PLANE_TUNNEL_ID".into())
}

// `key` is only referenced on Unix (the 0600 key file); Windows carries no
// plaintext key, so silence the unused-parameter warning there.
#[cfg_attr(windows, allow(unused_variables))]
fn write_profile(key: &Path, harness: &Path, tunnel_id: &str) -> Result<(), String> {
    write_profile_at(&profile_file(), key, harness, tunnel_id)
}

#[cfg_attr(windows, allow(unused_variables))]
fn write_profile_at(
    profile_path: &Path,
    key: &Path,
    harness: &Path,
    tunnel_id: &str,
) -> Result<(), String> {
    if let Some(parent) = profile_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let harness = harness.display().to_string();
    // Windows: profile references the env var; the key itself lives only in
    // Credential Manager. Unix: profile references the 0600 key file.
    #[cfg(windows)]
    let api_key_entry = "api_key: \"env:CONTROL_PLANE_API_KEY\"".to_string();
    #[cfg(not(windows))]
    let api_key_entry = format!("api_key: \"file:{}\"", key.display());

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

fn wrapper_path() -> PathBuf {
    host::config_dir().join("run-tunnel.sh")
}

fn write_wrapper(client: &Path) -> Result<(), String> {
    let dir = host::config_dir();
    fs::create_dir_all(dir.join("logs")).map_err(|e| format!("mkdir logs: {e}"))?;
    let path = wrapper_path();
    let client = client.display();
    // Long-poll is the wait-for-request. Do not busy-loop. Only block idle
    // sleep so the radio stays up. On AC also block system sleep (clamshell).
    // On battery, lid-close may sleep — no VPS/iPhone required.
    let body = format!(
        r#"#!/bin/sh
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
CLIENT="{client}"
set -- "$CLIENT" run --profile {PROFILE} --log.level=warn --control-plane.poll-timeout=60s
mode="${{HANDS_CAFFEINATE:-${{GROK_HARNESS_CAFFEINATE:-auto}}}}"
if [ "$mode" = "auto" ]; then
  if command -v pmset >/dev/null 2>&1 && pmset -g ps 2>/dev/null | grep -q "AC Power"; then
    mode=is
  else
    mode=i
  fi
fi
if [ -x /usr/bin/caffeinate ] && [ "$mode" != "off" ]; then
  exec /usr/bin/caffeinate -"$mode" -- "$@"
fi
if command -v systemd-inhibit >/dev/null 2>&1; then
  exec systemd-inhibit --what=idle --who=hands --why="ChatGPT MCP tunnel" --mode=block "$@"
fi
exec "$@"
"#
    );
    fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    }
    Ok(())
}

fn harness_bin() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Ok(dunce::canonicalize(&exe).unwrap_or(exe))
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn log_dir() -> PathBuf {
    host::config_dir().join("logs")
}

#[cfg(target_os = "macos")]
fn plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn gui_target() -> String {
    let uid = Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "501".into());
    format!("gui/{uid}")
}

#[cfg(target_os = "macos")]
pub fn installed() -> bool {
    plist_path().is_file()
}

#[cfg(target_os = "linux")]
pub fn installed() -> bool {
    unit_path().is_file()
}

#[cfg(windows)]
pub fn installed() -> bool {
    let out = Command::new("schtasks")
        .args(["/query", "/tn", TASK_NAME])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
    out.map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub fn installed() -> bool {
    false
}

#[cfg(target_os = "macos")]
fn install_supervisor() -> Result<(), String> {
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let wrapper = wrapper_path();
    let out = log_dir().join("tunnel.out");
    let err = log_dir().join("tunnel.err");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>2</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>LowPriorityIO</key><false/>
  <key>Nice</key><integer>0</integer>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml_escape(&wrapper.display().to_string()),
        xml_escape(&out.display().to_string()),
        xml_escape(&err.display().to_string()),
    );
    fs::write(&plist, xml).map_err(|e| format!("write {}: {e}", plist.display()))?;
    stop_unmanaged();
    let target = gui_target();
    let _ = launchctl(&["bootout", &target, LEGACY_LABEL]);
    let legacy_plist = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents")
        .join(format!("{LEGACY_LABEL}.plist"));
    let _ = fs::remove_file(legacy_plist);
    let _ = launchctl(&["bootout", &target, LABEL]);
    let boot = launchctl(&["bootstrap", &target, &plist.display().to_string()]);
    if !boot.status.success() {
        let msg = String::from_utf8_lossy(&boot.stderr);
        if !msg.contains("already") && !msg.contains("37") {
            // try enable + kickstart anyway
        }
    }
    let _ = launchctl(&["enable", &format!("{target}/{LABEL}")]);
    let kick = launchctl(&["kickstart", "-k", &format!("{target}/{LABEL}")]);
    if !kick.status.success() {
        return Err(format!(
            "launchctl kickstart failed: {}",
            String::from_utf8_lossy(&kick.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn start_supervisor() -> Result<(), String> {
    let plist = plist_path();
    let target = gui_target();
    let _ = launchctl(&["bootstrap", &target, &plist.display().to_string()]);
    let _ = launchctl(&["enable", &format!("{target}/{LABEL}")]);
    let kick = launchctl(&["kickstart", "-k", &format!("{target}/{LABEL}")]);
    if !kick.status.success() {
        return Err(format!(
            "launchctl kickstart failed: {}",
            String::from_utf8_lossy(&kick.stderr).trim()
        ));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn stop_supervisor() -> Result<(), String> {
    let target = gui_target();
    let _ = launchctl(&["bootout", &target, LABEL]);
    stop_unmanaged();
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_supervisor() -> Result<(), String> {
    stop_supervisor()?;
    let _ = uninstall_watch();
    let plist = plist_path();
    if plist.exists() {
        fs::remove_file(&plist).map_err(|e| format!("rm {}: {e}", plist.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn watch_plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents")
        .join(format!("{WATCH_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn install_watch() -> Result<(), String> {
    let hands = harness_bin()?;
    let plist = watch_plist_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let out = log_dir().join("watch.out");
    let err = log_dir().join("watch.err");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{WATCH_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>watch</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml_escape(&hands.display().to_string()),
        xml_escape(&out.display().to_string()),
        xml_escape(&err.display().to_string()),
    );
    fs::write(&plist, xml).map_err(|e| format!("write {}: {e}", plist.display()))?;
    let target = gui_target();
    let _ = launchctl(&["bootout", &target, WATCH_LABEL]);
    let _ = launchctl(&["bootstrap", &target, &plist.display().to_string()]);
    let _ = launchctl(&["enable", &format!("{target}/{WATCH_LABEL}")]);
    let _ = launchctl(&["kickstart", "-k", &format!("{target}/{WATCH_LABEL}")]);
    Ok(())
}

#[cfg(target_os = "macos")]
fn uninstall_watch() -> Result<(), String> {
    let target = gui_target();
    let _ = launchctl(&["bootout", &target, WATCH_LABEL]);
    let plist = watch_plist_path();
    if plist.exists() {
        fs::remove_file(&plist).map_err(|e| format!("rm {}: {e}", plist.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/systemd/user/hands-tunnel.service")
}

#[cfg(target_os = "linux")]
fn install_supervisor() -> Result<(), String> {
    let unit = unit_path();
    if let Some(parent) = unit.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let wrapper_buf = wrapper_path();
    let wrapper = wrapper_buf.display();
    let body = format!(
        r#"[Unit]
Description=Hands ChatGPT tunnel
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={wrapper}
Restart=always
RestartSec=2
Nice=0

[Install]
WantedBy=default.target
"#
    );
    fs::write(&unit, body).map_err(|e| format!("write {}: {e}", unit.display()))?;
    stop_unmanaged();
    run_ok("systemctl", &["--user", "daemon-reload"])?;
    run_ok(
        "systemctl",
        &["--user", "enable", "--now", "hands-tunnel.service"],
    )?;
    let _ = install_watch();
    Ok(())
}

#[cfg(target_os = "linux")]
fn watch_unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/systemd/user/hands-watch.service")
}

#[cfg(target_os = "linux")]
fn install_watch() -> Result<(), String> {
    let hands = harness_bin()?;
    let unit = watch_unit_path();
    if let Some(parent) = unit.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let body = format!(
        r#"[Unit]
Description=Hands tunnel down notifier
After=hands-tunnel.service

[Service]
Type=simple
ExecStart={} watch
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
"#,
        hands.display()
    );
    fs::write(&unit, body).map_err(|e| format!("write {}: {e}", unit.display()))?;
    run_ok("systemctl", &["--user", "daemon-reload"])?;
    run_ok(
        "systemctl",
        &["--user", "enable", "--now", "hands-watch.service"],
    )?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_watch() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "hands-watch.service"])
        .status();
    let unit = watch_unit_path();
    if unit.exists() {
        fs::remove_file(&unit).map_err(|e| format!("rm {}: {e}", unit.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn start_supervisor() -> Result<(), String> {
    run_ok("systemctl", &["--user", "start", "hands-tunnel.service"])
}

#[cfg(target_os = "linux")]
fn stop_supervisor() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "hands-tunnel.service"])
        .status();
    stop_unmanaged();
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_supervisor() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "hands-tunnel.service"])
        .status();
    let _ = uninstall_watch();
    stop_unmanaged();
    let unit = unit_path();
    if unit.exists() {
        fs::remove_file(&unit).map_err(|e| format!("rm {}: {e}", unit.display()))?;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    Ok(())
}

#[cfg(windows)]
fn task_xml(exec_path: &Path, args: &str, description: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>{description}</Description>
    <Author>Hands</Author>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT10S</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{}</Command>
      <Arguments>{}</Arguments>
    </Exec>
  </Actions>
</Task>
"#,
        xml_escape(&exec_path.display().to_string()),
        xml_escape(args),
    )
}

#[cfg(windows)]
fn install_supervisor() -> Result<(), String> {
    let hands = harness_bin()?;
    let xml = task_xml(&hands, "run-tunnel", "Hands ChatGPT tunnel supervisor");
    let xml_path = host::config_dir().join("tunnel-task.xml");
    if let Some(parent) = xml_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&xml_path, &xml).map_err(|e| format!("write {}: {e}", xml_path.display()))?;

    // A previous scheduled `hands run-tunnel` may be inside its own retry
    // loop. End the owning Task Scheduler instance before replacing the task
    // definition, then clean up only its recorded tunnel-client tree.
    stop_supervisor()?;

    let out = Command::new("schtasks")
        .args([
            "/create",
            "/tn",
            TASK_NAME,
            "/xml",
            &xml_path.display().to_string(),
            "/f",
        ])
        .output()
        .map_err(|e| format!("schtasks /create: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "schtasks /create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    start_supervisor()?;
    Ok(())
}

#[cfg(windows)]
fn start_supervisor() -> Result<(), String> {
    let out = Command::new("schtasks")
        .args(["/run", "/tn", TASK_NAME])
        .output()
        .map_err(|e| format!("schtasks /run: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.contains("already running") && !err.contains("2147750562") {
            return Err(format!("schtasks /run failed: {}", err.trim()));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn stop_supervisor() -> Result<(), String> {
    let _ = Command::new("schtasks")
        .args(["/end", "/tn", TASK_NAME])
        .output();
    stop_unmanaged();
    Ok(())
}

#[cfg(windows)]
fn uninstall_supervisor() -> Result<(), String> {
    stop_supervisor()?;
    let _ = uninstall_watch();
    let _ = Command::new("schtasks")
        .args(["/delete", "/tn", TASK_NAME, "/f"])
        .output();
    let xml_path = host::config_dir().join("tunnel-task.xml");
    if xml_path.is_file() {
        let _ = fs::remove_file(xml_path);
    }
    Ok(())
}

#[cfg(windows)]
fn install_watch() -> Result<(), String> {
    let hands = harness_bin()?;
    let xml = task_xml(&hands, "watch", "Hands tunnel down notifier");
    let xml_path = host::config_dir().join("watch-task.xml");
    if let Some(parent) = xml_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    fs::write(&xml_path, &xml).map_err(|e| format!("write {}: {e}", xml_path.display()))?;

    let out = Command::new("schtasks")
        .args([
            "/create",
            "/tn",
            WATCH_TASK_NAME,
            "/xml",
            &xml_path.display().to_string(),
            "/f",
        ])
        .output()
        .map_err(|e| format!("schtasks /create watch: {e}"))?;

    if !out.status.success() {
        return Err(format!(
            "schtasks /create watch failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let _ = Command::new("schtasks")
        .args(["/run", "/tn", WATCH_TASK_NAME])
        .output();
    Ok(())
}

#[cfg(windows)]
fn uninstall_watch() -> Result<(), String> {
    let _ = Command::new("schtasks")
        .args(["/end", "/tn", WATCH_TASK_NAME])
        .output();
    let _ = Command::new("schtasks")
        .args(["/delete", "/tn", WATCH_TASK_NAME, "/f"])
        .output();
    let xml_path = host::config_dir().join("watch-task.xml");
    if xml_path.is_file() {
        let _ = fs::remove_file(xml_path);
    }
    Ok(())
}

/// PID/state file for the Hands-owned tunnel-client tree. Written by the
/// supervisor path (`run_tunnel_daemon`) before spawn; `stop_unmanaged` only
/// ever stops PIDs proven by this file (live check + name + command line +
/// creation time to defeat PID recycling).
#[cfg(windows)]
fn tunnel_pid_file() -> PathBuf {
    host::config_dir().join("tunnel-pid.json")
}

#[cfg(windows)]
fn query_process_creation(pid: u32) -> Option<String> {
    let out = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "(Get-CimInstance Win32_Process -Filter \"ProcessId={pid}\" -ErrorAction SilentlyContinue).CreationDate | Out-String"
            ),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s.contains("CreationDate") {
        None
    } else {
        Some(s)
    }
}

#[cfg(windows)]
fn write_tunnel_pid(pid: u32) {
    let path = tunnel_pid_file();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let creation = query_process_creation(pid).unwrap_or_default();
    let json = if creation.is_empty() {
        format!(r#"{{"pid":{pid},"profile":"{PROFILE}"}}"#)
    } else {
        // Escape backslashes/quotes in creation string
        let esc = creation.replace('\\', r"\\").replace('"', r#"\""#);
        format!(r#"{{"pid":{pid},"creation":"{esc}","profile":"{PROFILE}"}}"#)
    };
    let _ = fs::write(&path, json);
}

#[cfg(windows)]
fn read_tunnel_pid() -> Option<(u32, Option<String>)> {
    let raw = fs::read_to_string(tunnel_pid_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let pid = v.get("pid")?.as_u64()? as u32;
    let creation = v
        .get("creation")
        .and_then(|c| c.as_str())
        .map(|s| s.to_string());
    Some((pid, creation))
}

/// Stop only the Hands-owned tunnel tree: the PID recorded by our supervisor
/// (validated against executable name + managed profile marker + creation
/// time), plus any child processes recursively. Unrelated `tunnel-client.exe`
/// processes are never touched. Cleans stale PID records.
#[cfg(windows)]
fn stop_unmanaged() {
    let Some((pid, stored_creation)) = read_tunnel_pid() else {
        return;
    };
    // Validate ownership before killing; use creation time to defeat PID recycling.
    if let Some(stored) = stored_creation.as_ref().filter(|s| !s.is_empty()) {
        if let Some(live) = query_process_creation(pid) {
            if live.trim() != stored.trim() {
                let _ = fs::remove_file(tunnel_pid_file());
                return;
            }
        } else {
            // Process gone: clean stale record.
            let _ = fs::remove_file(tunnel_pid_file());
            return;
        }
    }
    let pid_file_esc = tunnel_pid_file().display().to_string().replace('\'', "''");
    let script = format!(
        r#"
$owned = {pid}
if (-not $owned) {{ exit 0 }}
$p = Get-CimInstance Win32_Process -Filter "ProcessId=$owned" -ErrorAction SilentlyContinue
if (-not $p) {{
    Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
    exit 0
}}
$isOurs = ($p.Name -ieq 'tunnel-client.exe') -and ($p.CommandLine -match '--profile hands')
if (-not $isOurs) {{
    # PID was recycled or state file is stale: do not kill an unknown owner.
    Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
    exit 0
}}
# Additional creation-time check from PowerShell when Rust-stored creation exists
$storedCreation = '{stored_creation_esc}'
if ($storedCreation) {{
    $liveCreation = $p.CreationDate
    if ($liveCreation -and "$liveCreation" -ne $storedCreation) {{
        Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
        exit 0
    }}
}}
# Terminate the complete owned descendant tree recursively (handles grandchildren)
try {{ taskkill.exe /PID $owned /T /F | Out-Null }} catch {{}}
try {{ Stop-Process -Id $owned -Force -ErrorAction SilentlyContinue }} catch {{}}
Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
"#,
        pid = pid,
        pid_file_esc = pid_file_esc,
        stored_creation_esc = stored_creation.as_deref().unwrap_or("").replace('\'', "''")
    );
    let _ = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    std::thread::sleep(Duration::from_millis(300));
}

pub fn run_tunnel_daemon() -> Result<(), String> {
    host::migrate_from_legacy();
    let harness = harness_bin()?;
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
        cmd.args(["run", "--profile", PROFILE, "--log.level=warn"]);
        cmd.env("CONTROL_PLANE_API_KEY", &key);
        cmd.env("CONTROL_PLANE_TUNNEL_ID", &tunnel_id);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn tunnel-client: {e}"))?;
        #[cfg(windows)]
        write_tunnel_pid(child.id());
        let status = child
            .wait()
            .map_err(|e| format!("wait tunnel-client: {e}"))?;
        #[cfg(windows)]
        {
            let _ = fs::remove_file(tunnel_pid_file());
        }
        if status.success() {
            return Ok(());
        }
        eprintln!("tunnel-client exited with status: {status}; restarting in 5s...");
        std::thread::sleep(Duration::from_secs(5));
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn install_supervisor() -> Result<(), String> {
    Err("auto-start is not implemented for this platform".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn start_supervisor() -> Result<(), String> {
    Err("auto-start is not implemented for this platform".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn stop_supervisor() -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn uninstall_supervisor() -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn install_watch() -> Result<(), String> {
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn stop_unmanaged() {
    let Ok(out) = Command::new("ps").args(["-axo", "pid=,command="]).output() else {
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        let Some((pid, cmd)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        if !cmd.contains("tunnel-client") {
            continue;
        }
        let ours =
            cmd.contains("run --profile hands") || cmd.contains("run --profile grok-harness");
        if !ours || cmd.contains("pkill") {
            continue;
        }
        let _ = Command::new("kill").arg(pid.trim()).status();
    }
    std::thread::sleep(Duration::from_millis(300));
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> std::process::Output {
    Command::new("launchctl")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            use std::os::unix::process::ExitStatusExt;
            std::process::Output {
                status: std::process::ExitStatus::from_raw(1),
                stdout: Vec::new(),
                stderr: e.to_string().into_bytes(),
            }
        })
}

#[cfg(target_os = "linux")]
fn run_ok(bin: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(bin)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{bin}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{bin} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

#[cfg(any(target_os = "macos", windows))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn ureq_get(url: &str) -> Result<String, ()> {
    let mut child = Command::new("curl")
        .args(["-fsS", "--max-time", "1", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let mut buf = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut buf);
    }
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    if ok {
        Ok(buf.trim().to_string())
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn test_task_xml_structure_and_least_privilege() {
        let exe = PathBuf::from(r"C:\Program Files\Hands\hands.exe");
        let xml = task_xml(&exe, "run-tunnel", "Hands test task");
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<RestartOnFailure>"));
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<Arguments>run-tunnel</Arguments>"));
        assert!(xml.contains(r"C:\Program Files\Hands\hands.exe"));
        assert!(!xml.contains("sk-"));
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
