use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::journal::{
    is_supported_approval_policy, is_supported_tool_policy, Journal, PolicyRecord, TargetRecord,
    ExplicitNotificationParams, ExplicitNotificationResult, ExplicitNotificationStatus,
    LOCAL_PAIRING_ID,
};
use crate::protocol::{
    ProtocolError, TRUST_NOTICE, handle_native_message, read_native_message, write_native_message,
};

pub const DEFAULT_HOST_NAME: &str = "com.hands.return_bridge";
pub const DEFAULT_EXTENSION_ID: &str = "mkkajdpmlmliildflmnnmfndboldnnfa";
const PUSH_ENDPOINT_FILE: &str = "push-endpoint.json";
const PUSH_CONNECT_TIMEOUT: Duration = Duration::from_millis(250);
const MAX_PUSH_SIGNAL_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushEndpoint {
    port: u16,
    token: String,
    pid: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushWakeSignal {
    token: String,
    receipt_id: String,
    execution_id: String,
    task_id: String,
    state: String,
}

#[derive(Debug)]
struct PushSubscription {
    endpoint_path: PathBuf,
    token: String,
}

#[derive(Debug)]
pub enum HostError {
    Io(std::io::Error),
    Storage(String),
    Crypto(String),
    Protocol(ProtocolError),
    InvalidTarget(String),
    Registry(String),
    Notification(String),
    Prepare(String),
}

impl From<std::io::Error> for HostError {
    fn from(e: std::io::Error) -> Self {
        HostError::Io(e)
    }
}

impl From<serde_json::Error> for HostError {
    fn from(e: serde_json::Error) -> Self {
        HostError::Protocol(ProtocolError::Json(e))
    }
}

impl From<ProtocolError> for HostError {
    fn from(e: ProtocolError) -> Self {
        HostError::Protocol(e)
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HostError::Io(e) => write!(f, "IO error: {}", e),
            HostError::Storage(s) => write!(f, "Storage error: {}", s),
            HostError::Crypto(c) => write!(f, "Cryptographic failure: {}", c),
            HostError::Protocol(p) => write!(f, "Protocol error: {}", p),
            HostError::InvalidTarget(t) => write!(f, "Invalid target path: {}", t),
            HostError::Registry(r) => write!(f, "Registry error: {}", r),
            HostError::Notification(n) => write!(f, "{}", n),
            HostError::Prepare(p) => write!(f, "{}", p),
        }
    }
}

impl std::error::Error for HostError {}

fn absolutize(path: PathBuf) -> Result<PathBuf, HostError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn resolve_default_state_dir() -> Result<PathBuf, HostError> {
    #[cfg(windows)]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            if !local_app_data.trim().is_empty() {
                return absolutize(
                    PathBuf::from(local_app_data.trim())
                        .join("Hands")
                        .join("return-bridge"),
                );
            }
        }
        Err(HostError::Storage(
            "Cannot resolve per-user state directory: LOCALAPPDATA environment variable is missing or empty".to_string(),
        ))
    }
    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            if !home.trim().is_empty() {
                return absolutize(
                    PathBuf::from(home.trim())
                        .join(".local")
                        .join("share")
                        .join("hands")
                        .join("return-bridge"),
                );
            }
        }
        Err(HostError::Storage(
            "Cannot resolve per-user state directory: HOME environment variable is missing or empty".to_string(),
        ))
    }
}

pub fn resolve_state_dir(override_opt: Option<&Path>) -> Result<PathBuf, HostError> {
    if let Some(p) = override_opt {
        return absolutize(p.to_path_buf());
    }
    if let Ok(val) = std::env::var("HANDS_RETURN_BRIDGE_STATE_DIR") {
        if !val.trim().is_empty() {
            return absolutize(PathBuf::from(val.trim()));
        }
    }
    if let Ok(val) = std::env::var("RETURN_BRIDGE_STATE_DIR") {
        if !val.trim().is_empty() {
            return absolutize(PathBuf::from(val.trim()));
        }
    }
    resolve_default_state_dir()
}

fn atomic_replace_file(temp_path: &Path, destination: &Path) -> Result<(), HostError> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;
        const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;
        unsafe extern "system" {
            fn MoveFileExW(
                lp_existing_file_name: *const u16,
                lp_new_file_name: *const u16,
                dw_flags: u32,
            ) -> i32;
        }

        let from: Vec<u16> = temp_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let to: Vec<u16> = destination.as_os_str().encode_wide().chain(Some(0)).collect();
        let ok = unsafe {
            MoveFileExW(
                from.as_ptr(),
                to.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(HostError::Io(std::io::Error::last_os_error()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(temp_path, destination)?;
        Ok(())
    }
}

fn write_manifest_atomic(path: &Path, bytes: &[u8]) -> Result<(), HostError> {
    let parent = path.parent().ok_or_else(|| {
        HostError::Storage(format!("Manifest path has no parent: {}", path.display()))
    })?;
    let temp_name = format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or(DEFAULT_HOST_NAME),
        generate_random_id("write", 6)?
    );
    let temp_path = parent.join(temp_name);
    let result = (|| -> Result<(), HostError> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        atomic_replace_file(&temp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn generate_random_id(prefix: &str, num_bytes: usize) -> Result<String, HostError> {
    let mut bytes = vec![0u8; num_bytes];
    getrandom::fill(&mut bytes)
        .map_err(|e| HostError::Crypto(format!("OS CSPRNG failure: {}", e)))?;
    let hex_part: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    Ok(format!("{}_{}", prefix, hex_part))
}

pub fn verify_and_canonicalize_git_target(raw_path: &Path) -> Result<PathBuf, HostError> {
    if !raw_path.exists() || !raw_path.is_dir() {
        return Err(HostError::InvalidTarget(format!(
            "Target directory does not exist: {}",
            raw_path.display()
        )));
    }

    let canonical = raw_path
        .canonicalize()
        .map_err(|e| HostError::InvalidTarget(e.to_string()))?;

    // Require real git rev-parse --show-toplevel success. No fake .git fallback!
    let output = Command::new("git")
        .args(["-C", &canonical.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .output()
        .map_err(|e| HostError::InvalidTarget(format!("Failed to execute git: {}", e)))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(HostError::InvalidTarget(format!(
            "Target '{}' is not a valid git repository or worktree: {}",
            raw_path.display(),
            err.trim()
        )));
    }

    let toplevel_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if toplevel_str.is_empty() {
        return Err(HostError::InvalidTarget(format!(
            "git rev-parse returned empty toplevel for '{}'",
            raw_path.display()
        )));
    }

    let toplevel_path = PathBuf::from(toplevel_str);
    toplevel_path
        .canonicalize()
        .map_err(|e| HostError::InvalidTarget(e.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupOptions {
    pub browser: String,
    pub profile_id: String,
    pub target_path: String,
    pub target_id: Option<String>,
    pub policy_revision: String,
    pub tool_policy: String,
    pub approval_policy: String,
    pub extension_id: String,
    pub state_dir: Option<PathBuf>,
    pub skip_registry: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetupResult {
    pub pairing_id: String,
    pub bootstrap_token: String,
    pub browser: String,
    pub profile_id: String,
    pub target_id: String,
    pub canonical_target_path: String,
    pub policy_revision: String,
    pub manifest_path: PathBuf,
    pub trust_notice: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalInitOptions {
    pub browser: String,
    pub extension_id: String,
    pub state_dir: Option<PathBuf>,
    pub skip_registry: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalInitResult {
    pub browser: String,
    pub extension_id: String,
    pub pairing_id: String,
    pub manifest_path: PathBuf,
}

pub fn execute_local_init(opts: &LocalInitOptions) -> Result<LocalInitResult, HostError> {
    if !opts.skip_registry {
        #[cfg(not(windows))]
        {
            return Err(HostError::Registry(
                "Native messaging host automatic registration is only supported on Windows. Use --skip-registry for manual host manifest setup on non-Windows platforms.".to_string(),
            ));
        }
        #[cfg(windows)]
        if opts.state_dir.is_some() {
            return Err(HostError::Storage(
                "--state-dir is only supported with --skip-registry".to_string(),
            ));
        }
    }

    let browser = opts.browser.trim().to_lowercase();
    if browser != "chrome" && browser != "edge" {
        return Err(HostError::Storage(format!(
            "Invalid browser '{}': only 'chrome' and 'edge' are supported",
            opts.browser
        )));
    }
    let extension_id = opts.extension_id.trim();
    if extension_id.is_empty() {
        return Err(HostError::Storage("Missing required --extension-id".to_string()));
    }

    let state_dir = if opts.skip_registry {
        resolve_state_dir(opts.state_dir.as_deref())?
    } else {
        resolve_default_state_dir()?
    };
    std::fs::create_dir_all(&state_dir)?;
    let state_dir = state_dir.canonicalize()?;
    let journal = Journal::open(&state_dir.join("journal.sqlite"))
        .map_err(|e| HostError::Storage(e.to_string()))?;
    journal
        .ensure_local_pairing()
        .map_err(|e| HostError::Storage(e.to_string()))?;

    let manifest_path = state_dir.join(format!("{}.json", DEFAULT_HOST_NAME));
    let previous_manifest = match std::fs::read(&manifest_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(HostError::Io(e)),
    };

    let current_exe = std::env::current_exe()?;
    // Snapshot the pre-existing registry state BEFORE any manifest side effect, so a
    // snapshot failure (reg.exe spawn/read) aborts before the manifest is replaced
    // instead of leaving the manifest and journal inconsistent.
    #[cfg(windows)]
    let prior_reg_snapshot = if !opts.skip_registry {
        Some(snapshot_manifest_registry(&browser)?)
    } else {
        None
    };

    let manifest_json = json!({
        "name": DEFAULT_HOST_NAME,
        "description": "Hands Return Bridge Native Companion Host",
        "path": current_exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{}/", extension_id)]
    });
    write_manifest_atomic(&manifest_path, &serde_json::to_vec_pretty(&manifest_json)?)?;

    if !opts.skip_registry {
        #[cfg(windows)]
        {
            if let Err(e) = register_manifest_registry(&browser, &manifest_path) {
                let restore_result = match previous_manifest.as_deref() {
                    Some(bytes) => write_manifest_atomic(&manifest_path, bytes),
                    None => match std::fs::remove_file(&manifest_path) {
                        Ok(()) => Ok(()),
                        Err(remove_err) if remove_err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(remove_err) => Err(HostError::Io(remove_err)),
                    },
                };
                let reg_restore = if let Some(snapshot) = &prior_reg_snapshot {
                    restore_manifest_registry_snapshot(snapshot).map_err(|re| re.to_string())
                } else {
                    Ok(())
                };
                match (restore_result, reg_restore) {
                    (Err(restore_err), Ok(())) => {
                        return Err(HostError::Registry(format!(
                            "{}; additionally failed to restore previous manifest: {}",
                            e, restore_err
                        )));
                    }
                    (Ok(()), Err(reg_err)) => {
                        return Err(HostError::Registry(format!(
                            "{}; additionally failed to rollback registry registration: {}",
                            e, reg_err
                        )));
                    }
                    (Err(restore_err), Err(reg_err)) => {
                        return Err(HostError::Registry(format!(
                            "{}; additionally failed to restore previous manifest: {}; additionally failed to rollback registry registration: {}",
                            e, restore_err, reg_err
                        )));
                    }
                    (Ok(()), Ok(())) => return Err(e),
                }
            }
        }
    }

    // Deterministic test-only seam: fail after manifest/registry write to exercise rollback.
    // Gated behind cfg(test) so it is compiled out of production builds, and scoped to the
    // injected state_dir so parallel tests using other temp dirs are unaffected.
    #[cfg(test)]
    let fault_armed = std::env::var("HANDS_RETURN_BRIDGE_FAULT_LOCAL_INIT_JOURNAL")
        .map(|v| v == "1" || v == state_dir.to_string_lossy())
        .unwrap_or(false);
    #[cfg(not(test))]
    let fault_armed = false;
    let journal_update_result = if fault_armed {
        Err(HostError::Storage("injected journal update failure".to_string()))
    } else {
        journal.set_local_extension_id(extension_id).map_err(|e| HostError::Storage(e.to_string()))
    };
    if let Err(e) = journal_update_result {
        let restore_result = match previous_manifest.as_deref() {
            Some(bytes) => write_manifest_atomic(&manifest_path, bytes),
            None => match std::fs::remove_file(&manifest_path) {
                Ok(()) => Ok(()),
                Err(remove_err) if remove_err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(remove_err) => Err(HostError::Io(remove_err)),
            },
        };
        #[cfg(windows)]
        let registry_rollback: Result<(), String> = if let Some(snapshot) = &prior_reg_snapshot {
            restore_manifest_registry_snapshot(snapshot).map_err(|re| re.to_string())
        } else {
            Ok(())
        };
        #[cfg(not(windows))]
        let registry_rollback: Result<(), String> = Ok(());
        match (restore_result, registry_rollback) {
            (Ok(()), Ok(())) => return Err(e),
            (Err(restore_err), Ok(())) => {
                return Err(HostError::Storage(format!(
                    "{}; additionally failed to restore previous manifest: {}",
                    e, restore_err
                )))
            }
            (Ok(()), Err(reg_err)) => {
                return Err(HostError::Storage(format!(
                    "{}; additionally failed to rollback registry registration: {}",
                    e, reg_err
                )))
            }
            (Err(restore_err), Err(reg_err)) => {
                return Err(HostError::Storage(format!(
                    "{}; additionally failed to restore previous manifest: {}; additionally failed to rollback registry registration: {}",
                    e, restore_err, reg_err
                )))
            }
        }
    }
    Ok(LocalInitResult {
        browser,
        extension_id: extension_id.to_string(),
        pairing_id: LOCAL_PAIRING_ID.to_string(),
        manifest_path,
    })
}

pub fn execute_setup(opts: &SetupOptions) -> Result<SetupResult, HostError> {
    // Fail-closed explicitly on unsupported platforms before any durable setup side effects
    if !opts.skip_registry {
        #[cfg(not(windows))]
        {
            return Err(HostError::Registry(
                "Native messaging host automatic registration is only supported on Windows in this issue. Use --skip-registry for manual host manifest setup on non-Windows platforms.".to_string(),
            ));
        }
        #[cfg(windows)]
        if opts.state_dir.is_some() {
            return Err(HostError::Storage(
                "--state-dir is only supported with --skip-registry; registered browser launches use the fixed per-user state directory".to_string(),
            ));
        }
    }

    // Validate browser: reject invalid browsers rather than defaulting
    let browser_norm = opts.browser.to_lowercase();
    if browser_norm != "chrome" && browser_norm != "edge" {
        return Err(HostError::Storage(format!(
            "Invalid browser '{}': only 'chrome' and 'edge' are supported",
            opts.browser
        )));
    }
    if opts.profile_id.trim().is_empty() {
        return Err(HostError::Storage("Missing required --profile identifier".to_string()));
    }
    if opts.extension_id.trim().is_empty() {
        return Err(HostError::Storage("Missing required --extension-id".to_string()));
    }
    if opts.policy_revision.trim().is_empty() {
        return Err(HostError::Storage("Missing required --policy-revision".to_string()));
    }
    if opts.tool_policy.trim().is_empty() || !is_supported_tool_policy(&opts.tool_policy) {
        return Err(HostError::Storage(format!(
            "Unsupported --tool-policy '{}'; must be one of: standard, all, read_only, none, no_tools",
            opts.tool_policy
        )));
    }
    if opts.approval_policy.trim().is_empty() || !is_supported_approval_policy(&opts.approval_policy) {
        return Err(HostError::Storage(format!(
            "Unsupported --approval-policy '{}'; must be one of: prompt, ask, write, auto, yolo",
            opts.approval_policy
        )));
    }

    let state_dir = if opts.skip_registry {
        resolve_state_dir(opts.state_dir.as_deref())?
    } else {
        resolve_default_state_dir()?
    };
    std::fs::create_dir_all(&state_dir)?;
    let state_dir = state_dir.canonicalize()?;

    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;

    // Canonicalize & verify git target (real git rev-parse required)
    let raw_target = Path::new(&opts.target_path);
    let canonical = verify_and_canonicalize_git_target(raw_target)?;
    let canonical_path_str = canonical.to_string_lossy().to_string();

    let target_id = match opts.target_id.as_deref() {
        Some(id) if id.trim().is_empty() => {
            return Err(HostError::Storage("--target-id must not be empty or whitespace".to_string()));
        }
        Some(id) => id.trim().to_string(),
        None => canonical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("target_workspace")
            .to_string(),
    };

    let pairing_id = generate_random_id("pair", 8)?;
    let bootstrap_token = generate_random_id("rb_boot", 12)?;

    let targets = vec![TargetRecord {
        target_id: target_id.clone(),
        canonical_path: canonical_path_str.clone(),
        name: target_id.clone(),
    }];

    let policy = PolicyRecord {
        policy_revision: opts.policy_revision.clone(),
        tool_policy: opts.tool_policy.clone(),
        approval_policy: opts.approval_policy.clone(),
    };

    // Store bootstrap token hash (zero plaintext credential at rest)
    journal
        .create_bootstrap(
            &pairing_id,
            &bootstrap_token,
            &browser_norm,
            &opts.profile_id,
            &targets,
            &policy,
        )
        .map_err(|e| HostError::Storage(e.to_string()))?;

    // Persist expected extension ID in journal configuration.
    // An existing expected extension ID may only be reused if identical; a different ID must be rejected.
    // Expected extension ID is sticky once established and is not cleared on later failures.
    if let Err(e) = journal.set_expected_extension_id(&opts.extension_id) {
        let _ = journal.delete_pairing(&pairing_id);
        return Err(HostError::Storage(e.to_string()));
    }

    // Generate manifest
    let current_exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            let _ = journal.delete_pairing(&pairing_id);
            return Err(HostError::Io(e));
        }
    };
    let manifest_path = state_dir.join(format!("{}.json", DEFAULT_HOST_NAME));

    let allowed_origins = vec![format!("chrome-extension://{}/", opts.extension_id)];

    let manifest_json = json!({
        "name": DEFAULT_HOST_NAME,
        "description": "Hands Return Bridge Native Companion Host",
        "path": current_exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": allowed_origins
    });

    let manifest_bytes = serde_json::to_vec_pretty(&manifest_json)?;
    let previous_manifest = match std::fs::read(&manifest_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            let _ = journal.delete_pairing(&pairing_id);
            return Err(HostError::Io(e));
        }
    };
    if let Err(e) = write_manifest_atomic(&manifest_path, &manifest_bytes) {
        let _ = journal.delete_pairing(&pairing_id);
        return Err(e);
    }

    // Register in Windows Registry if requested
    if !opts.skip_registry {
        #[cfg(windows)]
        {
            if let Err(e) = register_manifest_registry(&browser_norm, &manifest_path) {
                // Restore the exact prior manifest instead of deleting a pre-existing valid setup.
                let restore_result = match previous_manifest.as_deref() {
                    Some(bytes) => write_manifest_atomic(&manifest_path, bytes),
                    None => match std::fs::remove_file(&manifest_path) {
                        Ok(()) => Ok(()),
                        Err(remove_err) if remove_err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(remove_err) => Err(HostError::Io(remove_err)),
                    },
                };
                let _ = journal.delete_pairing(&pairing_id);
                if let Err(restore_err) = restore_result {
                    return Err(HostError::Registry(format!(
                        "{}; additionally failed to restore the previous manifest: {}",
                        e, restore_err
                    )));
                }
                return Err(e);
            }
        }
    }

    Ok(SetupResult {
        pairing_id,
        bootstrap_token,
        browser: browser_norm,
        profile_id: opts.profile_id.clone(),
        target_id,
        canonical_target_path: canonical_path_str,
        policy_revision: opts.policy_revision.clone(),
        manifest_path,
        trust_notice: TRUST_NOTICE.to_string(),
    })
}

#[cfg(windows)]
pub fn manifest_registry_keys(browser: &str) -> Vec<String> {
    let host_name = std::env::var("HANDS_RETURN_BRIDGE_TEST_HOST_NAME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_HOST_NAME.to_string());

    match browser.to_lowercase().as_str() {
        "edge" => vec![format!(
            r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\{}",
            host_name
        )],
        "chrome" => vec![format!(
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{}",
            host_name
        )],
        "all" | "both" => vec![
            format!(
                r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{}",
                host_name
            ),
            format!(
                r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\{}",
                host_name
            ),
        ],
        _ => vec![format!(
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{}",
            host_name
        )],
    }
}

/// Snapshot of the pre-registration registry state for rollback.
///
/// Per key: `None` = key did not exist; `Some(None)` = key existed but had no
/// (usable) default value; `Some(Some(value))` = key existed with that default
/// value. Key existence is probed separately from the default value because
/// `reg.exe query <key> /ve` renders an unset default inconsistently (empty field
/// or "(value not set)") across key shapes; treating that as "key absent" would
/// delete a pre-existing key and its named values during rollback.
#[cfg(windows)]
pub fn snapshot_manifest_registry(browser: &str) -> Result<Vec<(String, Option<Option<String>>)>, HostError> {
    let keys = manifest_registry_keys(browser);
    let mut snapshot = Vec::new();
    for key in keys {
        let key_query = Command::new("reg.exe").args(["query", &key]).output()?;
        if !key_query.status.success() {
            // Key does not exist prior to registration
            snapshot.push((key, None));
            continue;
        }
        let default_query = Command::new("reg.exe").args(["query", &key, "/ve"]).output()?;
        if !default_query.status.success() {
            // Key exists but has no default value at all
            snapshot.push((key, Some(None)));
            continue;
        }
        let stdout = String::from_utf8_lossy(&default_query.stdout);
        let mut prior_default: Option<String> = None;
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.contains("REG_SZ") {
                if let Some(pos) = trimmed.find("REG_SZ") {
                    let raw_val = trimmed[pos + "REG_SZ".len()..].trim();
                    // `reg query /ve` renders an unset default both as an empty field and as
                    // "(value not set)"; neither is a usable prior manifest path, so both
                    // collapse to the same state (key exists, no default value).
                    if !raw_val.is_empty() && !raw_val.eq_ignore_ascii_case("(value not set)") {
                        prior_default = Some(raw_val.to_string());
                    }
                }
                break;
            }
        }
        snapshot.push((key, Some(prior_default)));
    }
    Ok(snapshot)
}

#[cfg(windows)]
pub fn restore_manifest_registry_snapshot(
    snapshot: &[(String, Option<Option<String>>)],
) -> Result<(), HostError> {
    for (key, prior_state) in snapshot {
        match prior_state {
            Some(Some(prev_val)) => {
                // Key existed before: restore its prior default value
                let output = Command::new("reg.exe")
                    .args(["add", key, "/ve", "/t", "REG_SZ", "/d", prev_val, "/f"])
                    .output()?;
                if !output.status.success() {
                    let err_msg = String::from_utf8_lossy(&output.stderr);
                    return Err(HostError::Registry(format!(
                        "reg.exe add restore failed for {}: {}",
                        key, err_msg
                    )));
                }
            }
            Some(None) => {
                // Key existed before without a default value: delete only the value
                // registration added, preserving the key and its named values.
                let output = Command::new("reg.exe")
                    .args(["delete", key, "/ve", "/f"])
                    .output()?;
                if !output.status.success() {
                    let err_msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    if !(err_msg.contains("unable to find") || err_msg.contains("was not found")) {
                        return Err(HostError::Registry(format!(
                            "reg.exe delete restore failed for {}: {}",
                            key, err_msg
                        )));
                    }
                }
            }
            None => {
                // Key did not exist before: delete the newly created key
                let output = Command::new("reg.exe").args(["delete", key, "/f"]).output()?;
                if !output.status.success() {
                    let err_msg = String::from_utf8_lossy(&output.stderr).trim().to_string();
                    if err_msg.contains("unable to find") || err_msg.contains("was not found") {
                        continue;
                    }
                    return Err(HostError::Registry(format!(
                        "reg.exe delete restore failed for {}: {}",
                        key, err_msg
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
pub fn register_manifest_registry(browser: &str, manifest_path: &Path) -> Result<(), HostError> {
    let reg_keys = manifest_registry_keys(browser);

    for key in reg_keys {
        let manifest_str = manifest_path.to_string_lossy();
        let output = Command::new("reg.exe")
            .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", &manifest_str, "/f"])
            .output()?;

        if !output.status.success() {
            let err_msg = String::from_utf8_lossy(&output.stderr);
            return Err(HostError::Registry(format!(
                "reg.exe add failed for {}: {}",
                key, err_msg
            )));
        }
    }

    Ok(())
}

fn push_endpoint_path(state_dir: &Path) -> PathBuf {
    state_dir.join(PUSH_ENDPOINT_FILE)
}

fn write_native_message_locked(
    stdout_lock: &Arc<Mutex<()>>,
    value: &serde_json::Value,
) -> Result<(), HostError> {
    let _guard = stdout_lock.lock();
    let mut stdout = std::io::stdout();
    write_native_message(&mut stdout, value)?;
    Ok(())
}

fn start_push_subscription(
    state_dir: &Path,
    stdout_lock: Arc<Mutex<()>>,
) -> Result<PushSubscription, HostError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    let token = generate_random_id("push", 16)?;
    let endpoint_path = push_endpoint_path(state_dir);
    let endpoint = PushEndpoint {
        port,
        token: token.clone(),
        pid: std::process::id(),
    };
    std::fs::write(&endpoint_path, serde_json::to_vec(&endpoint)?)?;

    let expected_token = token.clone();
    std::thread::spawn(move || {
        for incoming in listener.incoming() {
            let mut stream = match incoming {
                Ok(stream) => stream,
                Err(_) => break,
            };
            let _ = stream.set_read_timeout(Some(PUSH_CONNECT_TIMEOUT));
            let mut body = String::new();
            if std::io::Read::by_ref(&mut stream)
                .take(MAX_PUSH_SIGNAL_BYTES)
                .read_to_string(&mut body)
                .is_err()
            {
                continue;
            }
            let signal: PushWakeSignal = match serde_json::from_str(&body) {
                Ok(signal) => signal,
                Err(_) => continue,
            };
            if signal.token != expected_token {
                continue;
            }
            let event = json!({
                "event": "receipt_ready",
                "receiptId": signal.receipt_id,
                "executionId": signal.execution_id,
                "taskId": signal.task_id,
                "state": signal.state,
            });
            if write_native_message_locked(&stdout_lock, &event).is_err() {
                break;
            }
        }
    });

    Ok(PushSubscription {
        endpoint_path,
        token,
    })
}

fn cleanup_push_subscription(subscription: &PushSubscription) {
    let current = std::fs::read(&subscription.endpoint_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<PushEndpoint>(&bytes).ok());
    if current.as_ref().is_some_and(|endpoint| endpoint.token == subscription.token) {
        let _ = std::fs::remove_file(&subscription.endpoint_path);
    }
}

fn signal_push_receipt(state_dir: &Path, result: &ExplicitNotificationResult) -> bool {
    let endpoint: PushEndpoint = match std::fs::read(push_endpoint_path(state_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    {
        Some(endpoint) => endpoint,
        None => return false,
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], endpoint.port));
    let mut stream = match TcpStream::connect_timeout(&addr, PUSH_CONNECT_TIMEOUT) {
        Ok(stream) => stream,
        Err(_) => return false,
    };
    let _ = stream.set_write_timeout(Some(PUSH_CONNECT_TIMEOUT));
    let signal = PushWakeSignal {
        token: endpoint.token,
        receipt_id: result.receipt_id.clone(),
        execution_id: result.execution_id.clone(),
        task_id: result.task_id.clone(),
        state: result.state.clone(),
    };
    serde_json::to_writer(&mut stream, &signal).is_ok() && stream.flush().is_ok()
}


pub fn run_native_host(state_dir_opt: Option<&Path>, origin: Option<&str>) -> Result<(), HostError> {
    let state_dir = resolve_state_dir(state_dir_opt)?;
    std::fs::create_dir_all(&state_dir)?;
    let state_dir = state_dir.canonicalize()?;
    std::env::set_var("HANDS_RETURN_BRIDGE_STATE_DIR", &state_dir);
    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;
    // Exact native origin authority: caller origin is mandatory
    let origin_str = match origin {
        Some(o) if !o.trim().is_empty() => o.trim(),
        _ => {
            return Err(HostError::Protocol(ProtocolError::Io(
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Missing native messaging caller origin",
                ),
            )));
        }
    };

    // Determine expected extension ID strictly from SQLite journal host_config (single origin authority)
    let expected_id = match journal.get_expected_extension_id() {
        Ok(Some(id)) if !id.trim().is_empty() => id.trim().to_string(),
        _ => {
            return Err(HostError::Protocol(ProtocolError::Io(
                std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "No valid expected extension ID configured in journal authority",
                ),
            )));
        }
    };

    let expected_origin = format!("chrome-extension://{}/", expected_id);
    if origin_str != expected_origin {
        return Err(HostError::Protocol(ProtocolError::Io(
            std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "Unauthorized extension origin: expected exact '{}', got '{}'",
                    expected_origin, origin_str
                ),
            ),
        )));
    }

    let mut stdin = std::io::stdin();
    let stdout_lock = Arc::new(Mutex::new(()));
    let mut push_subscription: Option<PushSubscription> = None;

    loop {
        match read_native_message(&mut stdin) {
            Ok(Some(msg)) => {
                let resp = if msg.get("op").and_then(|v| v.as_str()) == Some("subscribe_events") {
                    if push_subscription.is_none() {
                        push_subscription = Some(start_push_subscription(
                            &state_dir,
                            Arc::clone(&stdout_lock),
                        )?);
                    }
                    json!({ "status": "ok", "subscribed": true })
                } else {
                    handle_native_message(&msg, &journal)
                };
                write_native_message_locked(&stdout_lock, &resp)?;
            }
            Ok(None) => {
                // EOF: Chrome closed the native host pipe
                break;
            }
            Err(e) => {
                // Return error to Chrome if possible
                let err_resp = json!({
                    "status": "error",
                    "code": "protocol_error",
                    "message": e.to_string()
                });
                let _ = write_native_message_locked(&stdout_lock, &err_resp);
                break;
            }
        }
    }

    if let Some(subscription) = &push_subscription {
        cleanup_push_subscription(subscription);
    }

    Ok(())
}

#[derive(Debug, Clone)]
pub struct TargetAddOptions {
    pub pairing_id: Option<String>,
    pub target_path: String,
    pub target_id: Option<String>,
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct TargetRemoveOptions {
    pub pairing_id: Option<String>,
    pub target_id: String,
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct TargetListOptions {
    pub pairing_id: Option<String>,
    pub state_dir: Option<PathBuf>,
}

fn resolve_pairing_id_for_cli_cmd(
    journal: &Journal,
    explicit_pairing_id: Option<&str>,
) -> Result<String, HostError> {
    if let Some(pid) = explicit_pairing_id {
        let trimmed = pid.trim();
        if trimmed.is_empty() {
            return Err(HostError::Storage(
                "--pairing-id must not be empty or whitespace".to_string(),
            ));
        }
        return Ok(trimmed.to_string());
    }
    let active_ids = journal
        .get_active_pairing_ids()
        .map_err(|e| HostError::Storage(e.to_string()))?;
    match active_ids.len() {
        0 => Err(HostError::Storage(
            "No active pairing found. Please run setup first or provide --pairing-id.".to_string(),
        )),
        1 => Ok(active_ids[0].clone()),
        _ => Err(HostError::Storage(format!(
            "Multiple active pairings found ({:?}). Please specify --pairing-id <id> explicitly.",
            active_ids
        ))),
    }
}

pub fn execute_target_add(opts: &TargetAddOptions) -> Result<TargetRecord, HostError> {
    let state_dir = resolve_state_dir(opts.state_dir.as_deref())?;
    std::fs::create_dir_all(&state_dir)?;
    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;

    let pairing_id = resolve_pairing_id_for_cli_cmd(&journal, opts.pairing_id.as_deref())?;

    let raw_path = Path::new(&opts.target_path);
    let canonical_path = verify_and_canonicalize_git_target(raw_path)?;
    let canonical_path_str = canonical_path.to_string_lossy().to_string();

    let derived_name = canonical_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("workspace")
        .to_string();

    let target_id = match opts.target_id.as_deref() {
        Some(id) if id.trim().is_empty() => {
            return Err(HostError::Storage(
                "--target-id must not be empty or whitespace".to_string(),
            ));
        }
        Some(id) => id.trim().to_string(),
        None => derived_name.clone(),
    };

    let target = TargetRecord {
        target_id,
        canonical_path: canonical_path_str,
        name: derived_name,
    };

    journal
        .add_target_admin(&pairing_id, &target)
        .map_err(|e| HostError::Storage(e.to_string()))?;

    Ok(target)
}

pub fn execute_target_remove(opts: &TargetRemoveOptions) -> Result<(), HostError> {
    let state_dir = resolve_state_dir(opts.state_dir.as_deref())?;
    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;

    let pairing_id = resolve_pairing_id_for_cli_cmd(&journal, opts.pairing_id.as_deref())?;

    let target_id = opts.target_id.trim();
    if target_id.is_empty() {
        return Err(HostError::Storage(
            "--target-id must not be empty or whitespace".to_string(),
        ));
    }

    journal
        .remove_target_admin(&pairing_id, target_id)
        .map_err(|e| HostError::Storage(e.to_string()))?;

    Ok(())
}

pub fn execute_target_list(opts: &TargetListOptions) -> Result<Vec<TargetRecord>, HostError> {
    let state_dir = resolve_state_dir(opts.state_dir.as_deref())?;
    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;

    let pairing_id = resolve_pairing_id_for_cli_cmd(&journal, opts.pairing_id.as_deref())?;

    let status = journal.get_pairing_status(&pairing_id).map_err(|e| HostError::Storage(e.to_string()))?;
    if status != crate::journal::PairingStatus::Active {
        return Err(HostError::Storage(format!("Pairing '{}' is not active ({:?})", pairing_id, status)));
    }

    let targets = journal
        .get_targets(&pairing_id)
        .map_err(|e| HostError::Storage(e.to_string()))?;
    Ok(targets)
}

#[derive(Debug, Clone)]
pub struct NotifyOptions {
    pub task_id: Option<String>,
    pub execution_id: Option<String>,
    pub status: ExplicitNotificationStatus,
    pub state_dir: Option<PathBuf>,
}

pub fn execute_notify(options: NotifyOptions) -> Result<ExplicitNotificationResult, HostError> {
    let state_dir = resolve_state_dir(options.state_dir.as_deref())?;
    let db_path = state_dir.join("journal.sqlite");
    if !db_path.exists() {
        return Err(HostError::Storage(format!(
            "Return Bridge journal database not found at {}",
            db_path.display()
        )));
    }
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;
    let task_id = options.task_id.or_else(|| {
        std::env::var("HANDS_TASK_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    let execution_id = options.execution_id.or_else(|| {
        std::env::var("HANDS_RETURN_BRIDGE_EXECUTION_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });

    let params = ExplicitNotificationParams {
        task_id,
        execution_id,
        status: options.status,
    };
    let result = journal.record_explicit_notification(&params).map_err(|e| match e {
        crate::journal::PairingError::StorageError(s) => HostError::Storage(s),
        other => HostError::Notification(other.to_string()),
    })?;

    // Durability comes first. The push signal is only a wake-up hint for the connected
    // extension; if it is absent/stale, the committed receipt is recovered on reconnect.
    let _ = signal_push_receipt(&state_dir, &result);
    Ok(result)
}

#[derive(Debug, Clone)]
pub struct PrepareOptions {
    pub pairing_id: Option<String>,
    pub conversation_id: String,
    pub state_dir: Option<PathBuf>,
}

/// Worker identity + environment minted for one registered conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedWorker {
    pub task_id: String,
    pub execution_id: String,
    pub origin_conversation_id: String,
    pub policy_revision: String,
    pub state: String,
    pub state_dir: String,
    pub env: std::collections::BTreeMap<String, String>,
}

/// Claims durable task/execution identity for a normal Orca OMP worker that the user launches
/// outside the extension. Routing comes from the registered conversation; the worker only needs
/// the returned environment to report terminal state with `notify`.
pub fn execute_prepare(options: PrepareOptions) -> Result<PreparedWorker, HostError> {
    let conversation_id = options.conversation_id.trim().to_string();
    if conversation_id.is_empty() {
        return Err(HostError::Prepare(
            "prepare requires an explicit --conversation <conversation_id>".to_string(),
        ));
    }

    let state_dir = resolve_state_dir(options.state_dir.as_deref())?;
    std::fs::create_dir_all(&state_dir)?;
    let db_path = state_dir.join("journal.sqlite");
    if !db_path.exists() {
        return Err(HostError::Storage(format!(
            "Return Bridge journal database not found at {}",
            db_path.display()
        )));
    }

    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;
    let pairing_id = resolve_pairing_id_for_cli_cmd(&journal, options.pairing_id.as_deref())?;
    let claim = journal
        .prepare_external_worker_claim(&pairing_id, &conversation_id)
        .map_err(|e| match e {
            crate::journal::PairingError::StorageError(s) => HostError::Storage(s),
            other => HostError::Prepare(other.to_string()),
        })?;

    let state_dir_str = state_dir.to_string_lossy().to_string();
    let mut env = std::collections::BTreeMap::new();
    env.insert("HANDS_TASK_ID".to_string(), claim.task_id.clone());
    env.insert(
        "HANDS_RETURN_BRIDGE_EXECUTION_ID".to_string(),
        claim.execution_id.clone(),
    );
    env.insert(
        "HANDS_RETURN_BRIDGE_STATE_DIR".to_string(),
        state_dir_str.clone(),
    );
    // Audit-only: identity of the conversation this task will report back to.
    env.insert(
        "HANDS_RETURN_BRIDGE_CONVERSATION_ID".to_string(),
        claim.origin_conversation_id.clone(),
    );

    Ok(PreparedWorker {
        task_id: claim.task_id,
        execution_id: claim.execution_id,
        origin_conversation_id: claim.origin_conversation_id,
        policy_revision: claim.policy_revision,
        state: claim.state,
        state_dir: state_dir_str,
        env,
    })
}

#[cfg(test)]
mod push_tests {
    use super::*;
    use std::sync::mpsc;
    use tempfile::tempdir;

    #[test]
    fn test_signal_push_receipt_wakes_registered_local_endpoint() {
        let dir = tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let token = "push_test_token_123".to_string();
        let endpoint = PushEndpoint {
            port,
            token: token.clone(),
            pid: std::process::id(),
        };
        std::fs::write(
            push_endpoint_path(dir.path()),
            serde_json::to_vec(&endpoint).unwrap(),
        )
        .unwrap();

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut body = String::new();
            stream.read_to_string(&mut body).unwrap();
            tx.send(serde_json::from_str::<PushWakeSignal>(&body).unwrap())
                .unwrap();
        });

        let result = ExplicitNotificationResult {
            receipt_id: "rcpt_push_test".to_string(),
            execution_id: "exec_push_test".to_string(),
            task_id: "task_push_test".to_string(),
            state: "completed".to_string(),
            is_idempotent: false,
        };
        assert!(signal_push_receipt(dir.path(), &result));

        let signal = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(signal.token, token);
        assert_eq!(signal.receipt_id, result.receipt_id);
        assert_eq!(signal.execution_id, result.execution_id);
        assert_eq!(signal.task_id, result.task_id);
        assert_eq!(signal.state, result.state);
    }

    #[test]
    fn test_signal_push_receipt_without_subscriber_is_best_effort_false() {
        let dir = tempdir().unwrap();
        let result = ExplicitNotificationResult {
            receipt_id: "rcpt_no_push".to_string(),
            execution_id: "exec_no_push".to_string(),
            task_id: "task_no_push".to_string(),
            state: "completed".to_string(),
            is_idempotent: false,
        };
        assert!(!signal_push_receipt(dir.path(), &result));
    }
}

#[cfg(test)]
mod local_init_rollback_tests {
    use super::*;
    use std::process::Command;
    use std::sync::Mutex;
    use tempfile::tempdir;

    /// Serializes tests that mutate process-global environment variables used by the
    /// local-init rollback seam (`LOCALAPPDATA`, `HANDS_RETURN_BRIDGE_TEST_HOST_NAME`).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn arm_fault(state_dir: &Path) {
        std::env::set_var(
            "HANDS_RETURN_BRIDGE_FAULT_LOCAL_INIT_JOURNAL",
            state_dir.to_string_lossy().to_string(),
        );
    }

    fn clear_fault() {
        std::env::remove_var("HANDS_RETURN_BRIDGE_FAULT_LOCAL_INIT_JOURNAL");
    }

    /// `reg query /ve` cannot distinguish "no default value" from a default value set to
    /// the literal string "(value not set)"; `reg export` can, because it emits an `@=`
    /// assignment only when a default value actually exists.
    #[cfg(windows)]
    fn export_key_text(key: &str, dir: &Path) -> String {
        let export_path = dir.join("registry_default_probe.reg");
        let output = Command::new("reg.exe")
            .args(["export", key, &export_path.to_string_lossy(), "/y"])
            .output()
            .expect("reg.exe export must run");
        assert!(
            output.status.success(),
            "reg.exe export failed for {}: {}",
            key,
            String::from_utf8_lossy(&output.stderr)
        );
        let bytes = std::fs::read(&export_path).unwrap();
        // reg.exe export writes UTF-16LE; fall back to UTF-8 if the file is not.
        let text = if bytes.len() % 2 == 0 && bytes.len() >= 2 {
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(&bytes).to_string()
        };
        text
    }

    #[test]
    fn test_local_init_preserves_previous_manifest_if_journal_update_fails() {
        let _guard = lock_env();
        let dir = tempdir().unwrap();
        let state_dir = dir.path().to_path_buf();
        let ext_initial = "initial_extension_id_abcdef";

        // 1. First init succeeds
        let res1 = execute_local_init(&LocalInitOptions {
            browser: "chrome".to_string(),
            extension_id: ext_initial.to_string(),
            state_dir: Some(state_dir.clone()),
            skip_registry: true,
        })
        .expect("first local init failed");
        assert!(std::fs::read_to_string(&res1.manifest_path)
            .unwrap()
            .contains(ext_initial));

        // 2. Deterministic post-manifest failure: arm the seam with the canonical state
        // dir the host itself resolves, so the injected journal failure actually triggers
        // rollback instead of leaving the call a vacuous success.
        let canonical_state = state_dir.canonicalize().unwrap();
        arm_fault(&canonical_state);
        let res2 = execute_local_init(&LocalInitOptions {
            browser: "chrome".to_string(),
            extension_id: "second_extension_id_xyz123".to_string(),
            state_dir: Some(state_dir.clone()),
            skip_registry: true,
        });
        clear_fault();
        assert!(res2.is_err(), "Local init must fail when journal cannot be updated");
        let err_msg = res2.unwrap_err().to_string();
        assert!(
            !err_msg.contains("additionally failed to restore previous manifest"),
            "Rollback restore must succeed on this path, got: {}",
            err_msg
        );

        // 3. Manifest must be restored to the previous valid manifest
        let manifest_after = std::fs::read_to_string(&res1.manifest_path).unwrap();
        assert!(
            manifest_after.contains(ext_initial),
            "Manifest must be restored to previous valid manifest on failure"
        );
        assert!(
            !manifest_after.contains("second_extension_id_xyz123"),
            "Failed init must not leave partial manifest on disk"
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_local_init_rollback_restores_prior_registry_snapshot() {
        let _guard = lock_env();

        // Isolated test host name so live com.hands.return_bridge is never touched.
        let test_host_name = "com.hands.return_bridge.test_reg_rollback";
        std::env::set_var("HANDS_RETURN_BRIDGE_TEST_HOST_NAME", test_host_name);

        // Registered local-init resolves its state dir from LOCALAPPDATA (a caller-supplied
        // --state-dir is rejected unless --skip-registry). Point LOCALAPPDATA at a temp dir
        // so the fault can be armed against the exact path the host resolves; otherwise the
        // armed value never matches and the test would pass without exercising rollback.
        let local_app_data = tempdir().unwrap();
        let previous_local_app_data = std::env::var("LOCALAPPDATA").ok();
        std::env::set_var("LOCALAPPDATA", local_app_data.path());
        let resolved_state_dir = local_app_data.path().join("Hands").join("return-bridge");
        std::fs::create_dir_all(&resolved_state_dir).unwrap();
        let canonical_state = std::fs::canonicalize(&resolved_state_dir).unwrap();

        let test_key = manifest_registry_keys("chrome")[0].clone();
        let _ = Command::new("reg.exe").args(["delete", &test_key, "/f"]).output();

        let run_registered_init = |extension_id: &str| {
            arm_fault(&canonical_state);
            let result = execute_local_init(&LocalInitOptions {
                browser: "chrome".to_string(),
                extension_id: extension_id.to_string(),
                state_dir: None,
                skip_registry: false,
            });
            clear_fault();
            result
        };

        // Scenario A: key absent prior to init -> rollback must remove the created key.
        let res_a = run_registered_init("ext_test_reg_rollback_a");
        assert!(res_a.is_err(), "Local init must fail when fault is armed");
        assert!(
            !Command::new("reg.exe")
                .args(["query", &test_key])
                .output()
                .unwrap()
                .status
                .success(),
            "Rollback must remove registry key when it did not exist prior to init"
        );

        // Scenario B: key existed with a default value -> rollback restores that exact value.
        let prior_dummy_path = r"C:\prior\nonexistent\host.json";
        let seed_default = Command::new("reg.exe")
            .args(["add", &test_key, "/ve", "/t", "REG_SZ", "/d", prior_dummy_path, "/f"])
            .output()
            .unwrap();
        assert!(seed_default.status.success(), "Failed to seed prior registry default value");

        let res_b = run_registered_init("ext_test_reg_rollback_b");
        assert!(res_b.is_err(), "Local init must fail when fault is armed");
        let query_b = Command::new("reg.exe").args(["query", &test_key, "/ve"]).output().unwrap();
        assert!(query_b.status.success(), "Prior registry key must still exist after rollback");
        let stdout_b = String::from_utf8_lossy(&query_b.stdout);
        assert!(
            stdout_b.contains(prior_dummy_path),
            "Rollback must restore prior default value {}, got: {}",
            prior_dummy_path,
            stdout_b
        );

        // Scenario C: key existed WITHOUT a default value (but with a named value).
        // `reg query <key> /ve` still exits 0 for that key shape, so the snapshot must
        // not record the "(value not set)" rendering as a real prior value: rollback
        // would then write that literal back and leave a fake default value behind.
        let _ = Command::new("reg.exe").args(["delete", &test_key, "/f"]).output();
        let seed_named = Command::new("reg.exe")
            .args(["add", &test_key, "/v", "NamedKeep", "/t", "REG_SZ", "/d", "keepme", "/f"])
            .output()
            .unwrap();
        assert!(seed_named.status.success(), "Failed to seed named-only registry value");
        let export_dir = tempdir().unwrap();
        let exported_before_init = export_key_text(&test_key, export_dir.path());
        assert!(
            !exported_before_init.contains("@="),
            "Precondition: seeded key must have no default value; export: {}",
            exported_before_init
        );

        let res_c = run_registered_init("ext_test_reg_rollback_c");
        assert!(res_c.is_err(), "Local init must fail when fault is armed");
        let named_query = Command::new("reg.exe")
            .args(["query", &test_key, "/v", "NamedKeep"])
            .output()
            .unwrap();
        assert!(
            named_query.status.success() && String::from_utf8_lossy(&named_query.stdout).contains("keepme"),
            "Rollback must preserve a pre-existing key and its named values when the key had no default value"
        );
        let exported_after_rollback = export_key_text(&test_key, export_dir.path());
        assert!(
            !exported_after_rollback.contains("@="),
            "Rollback must not leave a default value (real or the '(value not set)' rendering) on a key that had none; export: {}",
            exported_after_rollback
        );

        // Cleanup isolated registry key and environment overrides
        let _ = Command::new("reg.exe").args(["delete", &test_key, "/f"]).output();
        std::env::remove_var("HANDS_RETURN_BRIDGE_TEST_HOST_NAME");
        match previous_local_app_data {
            Some(value) => std::env::set_var("LOCALAPPDATA", value),
            None => std::env::remove_var("LOCALAPPDATA"),
        }
    }
}
