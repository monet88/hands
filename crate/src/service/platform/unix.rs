//! Shared Unix supervisor helpers used by macOS and Linux backends.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::host;

pub fn wrapper_path() -> PathBuf {
    host::config_dir().join("run-tunnel.sh")
}

pub fn render_wrapper_script(client: &Path) -> String {
    let client = client.display();
    let profile = super::super::PROFILE;
    format!(
        r#"#!/bin/sh
export PATH="$HOME/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
CLIENT="{client}"
set -- "$CLIENT" run --profile {profile} --log.level=warn --control-plane.poll-timeout=60s
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
    )
}

pub fn write_wrapper(client: &Path) -> Result<(), String> {
    let dir = host::config_dir();
    fs::create_dir_all(dir.join("logs")).map_err(|e| format!("mkdir logs: {e}"))?;
    let path = wrapper_path();
    let body = render_wrapper_script(client);
    fs::write(&path, body).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(())
}

pub fn stop_unmanaged() {
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
