//! Shared `#[cfg(test)]`-only test support: hermetic env isolation for
//! Workspace-dependent tests (CLI, MCP host, ToolEngine).
//!
//! Never referenced by production code; wired via `#[cfg(test)] mod testenv;`.

use std::fs;
use std::path::PathBuf;
use parking_lot::Mutex;

/// Serializes every test that mutates process-global environment variables.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Restores `HANDS_CONFIG_DIR`, `HANDS_WORKSPACE`, and
/// `GROK_HARNESS_WORKSPACE` on drop and removes the temp config root.
pub struct EnvGuard {
    saved_config_dir: Option<std::ffi::OsString>,
    saved_workspace: Option<std::ffi::OsString>,
    saved_legacy: Option<std::ffi::OsString>,
    pub root: PathBuf,
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.saved_config_dir {
                Some(v) => std::env::set_var("HANDS_CONFIG_DIR", v),
                None => std::env::remove_var("HANDS_CONFIG_DIR"),
            }
            match &self.saved_workspace {
                Some(v) => std::env::set_var("HANDS_WORKSPACE", v),
                None => std::env::remove_var("HANDS_WORKSPACE"),
            }
            match &self.saved_legacy {
                Some(v) => std::env::set_var("GROK_HARNESS_WORKSPACE", v),
                None => std::env::remove_var("GROK_HARNESS_WORKSPACE"),
            }
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Point `HANDS_CONFIG_DIR` at a fresh temp dir and clear Workspace env vars,
/// returning the process-global test lock plus an `EnvGuard` restoring the
/// environment on drop. `name` namespaces the temp dir for failure forensics.
pub fn isolate_env(name: &str) -> (parking_lot::MutexGuard<'static, ()>, EnvGuard) {
    let guard = TEST_LOCK.lock();
    let root = std::env::temp_dir().join(format!(
        "hands_test_{}_{}_{}",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create isolated test config dir");
    let env_guard = EnvGuard {
        saved_config_dir: std::env::var_os("HANDS_CONFIG_DIR"),
        saved_workspace: std::env::var_os("HANDS_WORKSPACE"),
        saved_legacy: std::env::var_os("GROK_HARNESS_WORKSPACE"),
        root: root.clone(),
    };
    unsafe {
        std::env::set_var("HANDS_CONFIG_DIR", &root);
        std::env::remove_var("HANDS_WORKSPACE");
        std::env::remove_var("GROK_HARNESS_WORKSPACE");
    }
    (guard, env_guard)
}
