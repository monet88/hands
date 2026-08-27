//! Keep `tunnel-client` running: login start + restart if the MCP child dies.
//!
//! Foreground `tunnel-client run` exits when its stdio MCP child is killed.
//! A LaunchAgent / systemd user unit with KeepAlive is the actual client.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::host;

pub const HEALTH_LISTEN: &str = "127.0.0.1:18780";
pub const HEALTH_BASE: &str = "http://127.0.0.1:18780";
pub const PROFILE: &str = "grok-harness";
const LABEL: &str = "ai.grok.harness.tunnel";

pub fn key_file() -> PathBuf {
    host::config_dir().join("control-plane.key")
}

pub fn profile_file() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/tunnel-client")
        .join(format!("{PROFILE}.yaml"))
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
        "off — grok-harness enable"
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
    let key = persist_key()?;
    let tunnel_id = resolve_tunnel_id()?;
    let harness = harness_bin()?;
    let client = tunnel_client_bin()?;
    write_profile(&key, &harness, &tunnel_id)?;
    write_wrapper(&client)?;
    install_supervisor()?;
    if wait_ready(Duration::from_secs(15)) {
        eprintln!("tunnel enabled. starts at login, restarts if it dies.");
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

fn can_enable() -> bool {
    persist_key().is_ok() && resolve_tunnel_id().is_ok() && tunnel_client_bin().is_ok()
}

fn persist_key() -> Result<PathBuf, String> {
    let dest = key_file();
    if let Ok(key) = std::env::var("CONTROL_PLANE_API_KEY") {
        let key = key.trim();
        if valid_runtime_key(key) {
            write_secret(&dest, key)?;
            return Ok(dest);
        }
    }
    if dest.is_file() {
        if let Ok(existing) = fs::read_to_string(&dest) {
            if valid_runtime_key(existing.trim()) {
                return Ok(dest);
            }
        }
    }
    Err(
        "missing runtime key. export CONTROL_PLANE_API_KEY then grok-harness enable".into(),
    )
}

fn valid_runtime_key(key: &str) -> bool {
    key.starts_with("sk-") && key.len() >= 32 && !key.contains(char::is_whitespace)
}

fn write_secret(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(path, format!("{}\n", contents.trim()))
        .map_err(|e| format!("write {}: {e}", path.display()))?;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(())
}

fn resolve_tunnel_id() -> Result<String, String> {
    if let Ok(id) = std::env::var("CONTROL_PLANE_TUNNEL_ID") {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    if let Ok(text) = fs::read_to_string(profile_file()) {
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
    Err("missing tunnel id. export CONTROL_PLANE_TUNNEL_ID or run tunnel-client init first".into())
}

fn write_profile(key: &Path, harness: &Path, tunnel_id: &str) -> Result<(), String> {
    let path = profile_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let key_path = key.display().to_string();
    let harness = harness.display().to_string();
    let yaml = format!(
        r#"config_version: 1
control_plane:
  base_url: "https://api.openai.com"
  tunnel_id: "{tunnel_id}"
  api_key: "file:{key_path}"
health:
  listen_addr: "{HEALTH_LISTEN}"
admin_ui:
  open_browser: false
log:
  level: info
  format: json
mcp:
  commands:
    - channel: main
      command: "{harness}"
"#
    );
    fs::write(&path, yaml).map_err(|e| format!("write {}: {e}", path.display()))?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    Ok(())
}

fn wrapper_path() -> PathBuf {
    host::config_dir().join("run-tunnel.sh")
}

fn write_wrapper(client: &Path) -> Result<(), String> {
    let dir = host::config_dir();
    fs::create_dir_all(dir.join("logs")).map_err(|e| format!("mkdir logs: {e}"))?;
    let path = wrapper_path();
    let body = format!(
        "#!/bin/sh\n\
         export PATH=\"$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin\"\n\
         exec \"{}\" run --profile {PROFILE}\n",
        client.display()
    );
    fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(())
}

fn harness_bin() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Ok(dunce::canonicalize(&exe).unwrap_or(exe))
}

fn tunnel_client_bin() -> Result<PathBuf, String> {
    which("tunnel-client").ok_or_else(|| {
        "tunnel-client not found. brew install openai/tools/tunnel-client".into()
    })
}

fn which(name: &str) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        dirs.extend(path.split(':').map(PathBuf::from));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".local/bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));
    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
  <key>ThrottleInterval</key><integer>5</integer>
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
    let plist = plist_path();
    if plist.exists() {
        fs::remove_file(&plist).map_err(|e| format!("rm {}: {e}", plist.display()))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/systemd/user/grok-harness-tunnel.service")
}

#[cfg(target_os = "linux")]
fn install_supervisor() -> Result<(), String> {
    let unit = unit_path();
    if let Some(parent) = unit.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let wrapper = wrapper_path();
    let body = format!(
        "[Unit]\n\
         Description=Grok harness ChatGPT tunnel\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={}\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        wrapper.display()
    );
    fs::write(&unit, body).map_err(|e| format!("write {}: {e}", unit.display()))?;
    stop_unmanaged();
    run_ok("systemctl", &["--user", "daemon-reload"])?;
    run_ok("systemctl", &["--user", "enable", "--now", "grok-harness-tunnel.service"])?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn start_supervisor() -> Result<(), String> {
    run_ok("systemctl", &["--user", "start", "grok-harness-tunnel.service"])
}

#[cfg(target_os = "linux")]
fn stop_supervisor() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "grok-harness-tunnel.service"])
        .status();
    stop_unmanaged();
    Ok(())
}

#[cfg(target_os = "linux")]
fn uninstall_supervisor() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "grok-harness-tunnel.service"])
        .status();
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

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn install_supervisor() -> Result<(), String> {
    Err("auto-start is implemented for macOS and Linux".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn start_supervisor() -> Result<(), String> {
    Err("auto-start is implemented for macOS and Linux".into())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn stop_supervisor() -> Result<(), String> {
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn uninstall_supervisor() -> Result<(), String> {
    Ok(())
}

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
        if !cmd.contains("tunnel-client") || !cmd.contains("run --profile grok-harness") {
            continue;
        }
        if cmd.contains("pkill") || cmd.contains("grok-harness") {
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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
