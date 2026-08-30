//! Linux systemd user unit supervisor backend.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub use super::unix::{render_wrapper_script, stop_unmanaged, wrapper_path, write_wrapper};

pub fn supervisor_name() -> &'static str {
    "systemd user unit"
}

pub fn unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/systemd/user/hands-tunnel.service")
}

pub fn watch_unit_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/systemd/user/hands-watch.service")
}

pub fn installed() -> bool {
    unit_path().is_file()
}

pub fn run_ok(bin: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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

pub fn render_service_unit(wrapper: &Path) -> String {
    let wrapper = wrapper.display();
    format!(
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
    )
}

pub fn render_watch_unit(hands: &Path) -> String {
    let hands = hands.display();
    format!(
        r#"[Unit]
Description=Hands tunnel down notifier
After=hands-tunnel.service

[Service]
Type=simple
ExecStart={hands} watch
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
"#
    )
}

pub fn install_supervisor() -> Result<(), String> {
    let unit = unit_path();
    if let Some(parent) = unit.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let wrapper_buf = wrapper_path();
    let body = render_service_unit(&wrapper_buf);
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

pub fn install_watch() -> Result<(), String> {
    let hands = super::super::harness_bin()?;
    let unit = watch_unit_path();
    if let Some(parent) = unit.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let body = render_watch_unit(&hands);
    fs::write(&unit, body).map_err(|e| format!("write {}: {e}", unit.display()))?;
    run_ok("systemctl", &["--user", "daemon-reload"])?;
    run_ok(
        "systemctl",
        &["--user", "enable", "--now", "hands-watch.service"],
    )?;
    Ok(())
}

pub fn uninstall_watch() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "hands-watch.service"])
        .status();
    let unit = watch_unit_path();
    if unit.exists() {
        fs::remove_file(&unit).map_err(|e| format!("rm {}: {e}", unit.display()))?;
    }
    Ok(())
}

pub fn start_supervisor() -> Result<(), String> {
    run_ok("systemctl", &["--user", "start", "hands-tunnel.service"])
}

pub fn stop_supervisor() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "hands-tunnel.service"])
        .status();
    stop_unmanaged();
    Ok(())
}

pub fn uninstall_supervisor() -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_linux_supervisor_name() {
        assert_eq!(supervisor_name(), "systemd user unit");
    }
}
