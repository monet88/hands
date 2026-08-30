//! Fallback supervisor backend for non-supported platforms.

use std::path::Path;

pub fn supervisor_name() -> &'static str {
    "supervisor"
}

pub fn installed() -> bool {
    false
}

pub fn install_supervisor() -> Result<(), String> {
    Err("auto-start is not implemented for this platform".into())
}

pub fn start_supervisor() -> Result<(), String> {
    Err("auto-start is not implemented for this platform".into())
}

pub fn stop_supervisor() -> Result<(), String> {
    Ok(())
}

pub fn uninstall_supervisor() -> Result<(), String> {
    Ok(())
}

pub fn install_watch() -> Result<(), String> {
    Ok(())
}

pub fn uninstall_watch() -> Result<(), String> {
    Ok(())
}

pub fn write_wrapper(_client: &Path) -> Result<(), String> {
    Ok(())
}
