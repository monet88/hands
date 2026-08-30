//! Tunnel profile rendering, health probing, and platform supervisor dispatch.

mod health;
pub mod platform;
mod profile;

use std::path::PathBuf;
use std::time::Duration;

use crate::host;

pub use health::{HEALTH_BASE, ready, wait_ready};
#[cfg(test)]
pub use profile::which;
pub use profile::{
    PROFILE, profile_file, run_tunnel_daemon, save_connect, set_tunnel_id, status_json,
    tunnel_client_bin, tunnel_id_opt, valid_tunnel_id,
};

pub fn supervisor_name() -> &'static str {
    platform::supervisor_name()
}

pub fn installed() -> bool {
    platform::installed()
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

#[derive(Debug, PartialEq, Eq)]
enum EnsureAction {
    ReturnReady,
    EnableThenReturnReady,
    StartThenWait,
    EnableThenWait,
    Unavailable,
}

fn decide_ensure(
    is_ready: bool,
    is_installed: bool,
    can_enable: impl FnOnce() -> bool,
) -> EnsureAction {
    if is_ready {
        if !is_installed && can_enable() {
            EnsureAction::EnableThenReturnReady
        } else {
            EnsureAction::ReturnReady
        }
    } else if is_installed {
        EnsureAction::StartThenWait
    } else if can_enable() {
        EnsureAction::EnableThenWait
    } else {
        EnsureAction::Unavailable
    }
}

/// Pin-time helper: start the supervised client if it is down.
pub fn ensure() -> Result<bool, String> {
    match decide_ensure(ready(), installed(), can_enable) {
        EnsureAction::ReturnReady => Ok(ready()),
        EnsureAction::EnableThenReturnReady => {
            enable()?;
            Ok(ready())
        }
        EnsureAction::StartThenWait => {
            start()?;
            Ok(wait_ready(Duration::from_secs(12)))
        }
        EnsureAction::EnableThenWait => {
            enable()?;
            Ok(wait_ready(Duration::from_secs(12)))
        }
        EnsureAction::Unavailable => Ok(false),
    }
}

pub fn enable() -> Result<(), String> {
    host::migrate_from_legacy();
    let key = persist_key()?;
    let tunnel_id = resolve_tunnel_id()?;
    let harness = harness_bin()?;
    let client = tunnel_client_bin()?;
    profile::write_profile(&key, &harness, &tunnel_id)?;
    platform::write_wrapper(&client)?;
    platform::install_supervisor()?;
    if let Err(e) = platform::install_watch() {
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
    platform::uninstall_supervisor()?;
    eprintln!("tunnel auto-start removed.");
    Ok(())
}

pub fn start() -> Result<(), String> {
    if !installed() {
        return enable();
    }
    platform::start_supervisor()?;
    if wait_ready(Duration::from_secs(15)) {
        eprintln!("tunnel ready  {HEALTH_BASE}/ui");
        Ok(())
    } else {
        Err("tunnel did not become ready".into())
    }
}

pub fn stop() -> Result<(), String> {
    platform::stop_supervisor()?;
    eprintln!("tunnel stopped (will start again at next login if enabled).");
    Ok(())
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

fn resolve_tunnel_id() -> Result<String, String> {
    profile::resolve_tunnel_id()
}

fn harness_bin() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    Ok(dunce::canonicalize(&exe).unwrap_or(exe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn test_valid_tunnel_id() {
        assert!(valid_tunnel_id("tunnel_123456789"));
        assert!(!valid_tunnel_id("invalid_tunnel_id"));
        assert!(!valid_tunnel_id(""));
    }

    #[test]
    fn test_supervisor_name_non_empty() {
        assert!(!supervisor_name().is_empty());
    }

    #[test]
    fn test_status_line_contains_expected_fields() {
        let line = status_line();
        assert!(line.contains("service"));
    }

    #[test]
    fn test_decide_ensure_preserves_lifecycle_policy() {
        assert_eq!(
            decide_ensure(true, false, || true),
            EnsureAction::EnableThenReturnReady
        );
        assert_eq!(
            decide_ensure(true, false, || false),
            EnsureAction::ReturnReady
        );
        assert_eq!(
            decide_ensure(false, true, || panic!(
                "installed path must not probe enableability"
            )),
            EnsureAction::StartThenWait
        );
        assert_eq!(
            decide_ensure(false, false, || true),
            EnsureAction::EnableThenWait
        );
        assert_eq!(
            decide_ensure(false, false, || false),
            EnsureAction::Unavailable
        );

        let probed = Cell::new(false);
        assert_eq!(
            decide_ensure(true, true, || {
                probed.set(true);
                true
            }),
            EnsureAction::ReturnReady
        );
        assert!(
            !probed.get(),
            "ready+installed must not probe credentials/config"
        );
    }
}
