//! Shared `#[cfg(test)]`-only test support: hermetic env isolation for
//! Workspace-dependent tests (CLI, MCP host, ToolEngine).
//!
//! Never referenced by production code; wired via `#[cfg(test)] mod testenv;`.

use parking_lot::Mutex;
use std::fs;
use std::path::PathBuf;

/// Serializes every test that mutates process-global environment variables.
static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Environment state shared tests may mutate while holding `TEST_LOCK`.
const SERIALIZED_ENV_VARS: &[&str] = &[
    "HANDS_CONFIG_DIR",
    "HANDS_WORKSPACE",
    "GROK_HARNESS_WORKSPACE",
    "HANDS_TEST_CRED_NAMESPACE",
    "CONTROL_PLANE_API_KEY",
    "CONTROL_PLANE_TUNNEL_ID",
    "PATH",
];

/// Restores serialized process-global env state on drop and removes the temp
/// config root.
pub struct EnvGuard {
    saved_env: Vec<(&'static str, Option<std::ffi::OsString>)>,
    pub root: PathBuf,
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            for (name, value) in self.saved_env.iter().rev() {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
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
        saved_env: SERIALIZED_ENV_VARS
            .iter()
            .map(|&name| (name, std::env::var_os(name)))
            .collect(),
        root: root.clone(),
    };
    unsafe {
        std::env::set_var("HANDS_CONFIG_DIR", &root);
        std::env::remove_var("HANDS_WORKSPACE");
        std::env::remove_var("GROK_HARNESS_WORKSPACE");
    }
    (guard, env_guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_isolate_env_restores_serialized_env_state() {
        let (_env_lock, env) = isolate_env("restore_serialized_state");
        let expected_env = env.saved_env.clone();
        unsafe {
            std::env::set_var("CONTROL_PLANE_API_KEY", "sk-test-override");
            std::env::set_var("PATH", "hands-test-path-override");
        }
        drop(env);

        for (name, expected) in expected_env {
            assert_eq!(std::env::var_os(name), expected, "env restore mismatch: {name}");
        }
    }
}
