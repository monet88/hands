//! macOS LaunchAgent supervisor backend.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::host;

pub use super::unix::{render_wrapper_script, stop_unmanaged, wrapper_path, write_wrapper};

pub const LABEL: &str = "dev.hands.tunnel";
pub const WATCH_LABEL: &str = "dev.hands.watch";
pub const LEGACY_LABEL: &str = "ai.grok.harness.tunnel";

pub fn supervisor_name() -> &'static str {
    "LaunchAgent"
}

pub fn log_dir() -> PathBuf {
    host::config_dir().join("logs")
}

pub fn plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist"))
}

pub fn watch_plist_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/LaunchAgents")
        .join(format!("{WATCH_LABEL}.plist"))
}

pub fn gui_target() -> String {
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

pub fn installed() -> bool {
    plist_path().is_file()
}

pub fn launchctl(args: &[&str]) -> std::process::Output {
    Command::new("launchctl")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap_or_else(|e| {
            #[cfg(unix)]
            use std::os::unix::process::ExitStatusExt;
            #[cfg(unix)]
            let status = std::process::ExitStatus::from_raw(1);
            #[cfg(not(unix))]
            let status = std::process::ExitStatus::default();
            std::process::Output {
                status,
                stdout: Vec::new(),
                stderr: e.to_string().into_bytes(),
            }
        })
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn render_service_plist(wrapper: &Path, out: &Path, err: &Path) -> String {
    format!(
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
    )
}

pub fn render_watch_plist(hands: &Path, out: &Path, err: &Path) -> String {
    format!(
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
    )
}

pub fn install_supervisor() -> Result<(), String> {
    let plist = plist_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let wrapper = wrapper_path();
    let out = log_dir().join("tunnel.out");
    let err = log_dir().join("tunnel.err");
    let xml = render_service_plist(&wrapper, &out, &err);
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

pub fn start_supervisor() -> Result<(), String> {
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

pub fn stop_supervisor() -> Result<(), String> {
    let target = gui_target();
    let _ = launchctl(&["bootout", &target, LABEL]);
    stop_unmanaged();
    Ok(())
}

pub fn uninstall_supervisor() -> Result<(), String> {
    stop_supervisor()?;
    let _ = uninstall_watch();
    let plist = plist_path();
    if plist.exists() {
        fs::remove_file(&plist).map_err(|e| format!("rm {}: {e}", plist.display()))?;
    }
    Ok(())
}

pub fn install_watch() -> Result<(), String> {
    let hands = super::super::harness_bin()?;
    let plist = watch_plist_path();
    if let Some(parent) = plist.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let out = log_dir().join("watch.out");
    let err = log_dir().join("watch.err");
    let xml = render_watch_plist(&hands, &out, &err);
    fs::write(&plist, xml).map_err(|e| format!("write {}: {e}", plist.display()))?;
    let target = gui_target();
    let _ = launchctl(&["bootout", &target, WATCH_LABEL]);
    let _ = launchctl(&["bootstrap", &target, &plist.display().to_string()]);
    let _ = launchctl(&["enable", &format!("{target}/{WATCH_LABEL}")]);
    let _ = launchctl(&["kickstart", "-k", &format!("{target}/{WATCH_LABEL}")]);
    Ok(())
}

pub fn uninstall_watch() -> Result<(), String> {
    let target = gui_target();
    let _ = launchctl(&["bootout", &target, WATCH_LABEL]);
    let plist = watch_plist_path();
    if plist.exists() {
        fs::remove_file(&plist).map_err(|e| format!("rm {}: {e}", plist.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_xml_escape() {
        assert_eq!(
            xml_escape("a & b < c > d ' e \""),
            "a &amp; b &lt; c &gt; d &apos; e &quot;"
        );
    }

    #[test]
    fn test_macos_supervisor_name() {
        assert_eq!(supervisor_name(), "LaunchAgent");
    }
}
