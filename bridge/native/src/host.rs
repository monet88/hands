use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::journal::{
    is_supported_approval_policy, is_supported_tool_policy, Journal, PolicyRecord, TargetRecord,
    LOCAL_PAIRING_ID,
};
use crate::protocol::{
    ProtocolError, TRUST_NOTICE, handle_native_message, read_native_message, write_native_message,
};

pub const DEFAULT_HOST_NAME: &str = "com.hands.return_bridge";
pub const DEFAULT_EXTENSION_ID: &str = "mkkajdpmlmliildflmnnmfndboldnnfa";

#[derive(Debug)]
pub enum HostError {
    Io(std::io::Error),
    Storage(String),
    Crypto(String),
    Protocol(ProtocolError),
    InvalidTarget(String),
    Registry(String),
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
                if let Err(restore_err) = restore_result {
                    return Err(HostError::Registry(format!(
                        "{}; additionally failed to restore previous manifest: {}",
                        e, restore_err
                    )));
                }
                return Err(e);
            }
        }
    }

    if let Err(e) = journal.set_local_extension_id(extension_id) {
        let _ = match previous_manifest.as_deref() {
            Some(bytes) => write_manifest_atomic(&manifest_path, bytes),
            None => match std::fs::remove_file(&manifest_path) {
                Ok(()) => Ok(()),
                Err(remove_err) if remove_err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(remove_err) => Err(HostError::Io(remove_err)),
            },
        };
        return Err(HostError::Storage(e.to_string()));
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
pub fn register_manifest_registry(browser: &str, manifest_path: &Path) -> Result<(), HostError> {
    let reg_keys = match browser.to_lowercase().as_str() {
        "edge" => vec![format!(
            r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\{}",
            DEFAULT_HOST_NAME
        )],
        "chrome" => vec![format!(
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{}",
            DEFAULT_HOST_NAME
        )],
        "all" | "both" => vec![
            format!(
                r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{}",
                DEFAULT_HOST_NAME
            ),
            format!(
                r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\{}",
                DEFAULT_HOST_NAME
            ),
        ],
        _ => vec![format!(
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{}",
            DEFAULT_HOST_NAME
        )],
    };

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
    let mut stdout = std::io::stdout();

    loop {
        match read_native_message(&mut stdin) {
            Ok(Some(msg)) => {
                let resp = handle_native_message(&msg, &journal);
                write_native_message(&mut stdout, &resp)?;
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
                let _ = write_native_message(&mut stdout, &err_resp);
                break;
            }
        }
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

fn resolve_pairing_id_for_target_cmd(
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

    let pairing_id = resolve_pairing_id_for_target_cmd(&journal, opts.pairing_id.as_deref())?;

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

    let pairing_id = resolve_pairing_id_for_target_cmd(&journal, opts.pairing_id.as_deref())?;

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

    let pairing_id = resolve_pairing_id_for_target_cmd(&journal, opts.pairing_id.as_deref())?;

    let status = journal.get_pairing_status(&pairing_id).map_err(|e| HostError::Storage(e.to_string()))?;
    if status != crate::journal::PairingStatus::Active {
        return Err(HostError::Storage(format!("Pairing '{}' is not active ({:?})", pairing_id, status)));
    }

    let targets = journal
        .get_targets(&pairing_id)
        .map_err(|e| HostError::Storage(e.to_string()))?;
    Ok(targets)
}
