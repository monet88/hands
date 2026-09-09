use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::journal::{Journal, PolicyRecord, TargetRecord};
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

pub fn resolve_state_dir(override_opt: Option<&Path>) -> Result<PathBuf, HostError> {
    if let Some(p) = override_opt {
        return Ok(p.to_path_buf());
    }
    if let Ok(val) = std::env::var("HANDS_RETURN_BRIDGE_STATE_DIR") {
        if !val.trim().is_empty() {
            return Ok(PathBuf::from(val.trim()));
        }
    }
    if let Ok(val) = std::env::var("RETURN_BRIDGE_STATE_DIR") {
        if !val.trim().is_empty() {
            return Ok(PathBuf::from(val.trim()));
        }
    }

    #[cfg(windows)]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            if !local_app_data.trim().is_empty() {
                return Ok(PathBuf::from(local_app_data)
                    .join("Hands")
                    .join("return-bridge"));
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
                return Ok(PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("hands")
                    .join("return-bridge"));
            }
        }
        Err(HostError::Storage(
            "Cannot resolve per-user state directory: HOME environment variable is missing or empty".to_string(),
        ))
    }
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

pub fn execute_setup(opts: &SetupOptions) -> Result<SetupResult, HostError> {
    // Validate browser: reject invalid browsers rather than defaulting
    let browser_norm = opts.browser.to_lowercase();
    if browser_norm != "chrome" && browser_norm != "edge" {
        return Err(HostError::Storage(format!(
            "Invalid browser '{}': only 'chrome' and 'edge' are supported",
            opts.browser
        )));
    }

    let state_dir = resolve_state_dir(opts.state_dir.as_deref())?;
    std::fs::create_dir_all(&state_dir)?;

    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;

    // Canonicalize & verify git target (real git rev-parse required)
    let raw_target = Path::new(&opts.target_path);
    let canonical = verify_and_canonicalize_git_target(raw_target)?;
    let canonical_path_str = canonical.to_string_lossy().to_string();

    let target_id = opts.target_id.clone().unwrap_or_else(|| {
        canonical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("target_workspace")
            .to_string()
    });

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

    // Persist expected extension ID in journal configuration
    journal
        .set_expected_extension_id(&opts.extension_id)
        .map_err(|e| HostError::Storage(e.to_string()))?;

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

    if let Err(e) = std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest_json)?) {
        let _ = journal.delete_pairing(&pairing_id);
        return Err(HostError::Io(e));
    }

    // Register in Windows Registry if requested
    if !opts.skip_registry {
        #[cfg(windows)]
        {
            if let Err(e) = register_manifest_registry(&browser_norm, &manifest_path) {
                // Cleanup orphan manifest and database row on registration failure
                let _ = std::fs::remove_file(&manifest_path);
                let _ = journal.delete_pairing(&pairing_id);
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

    // Determine expected extension ID from journal host_config or state manifest
    let expected_id = if let Ok(Some(id)) = journal.get_expected_extension_id() {
        id
    } else {
        // Fallback: check manifest file in state_dir
        let manifest_path = state_dir.join(format!("{}.json", DEFAULT_HOST_NAME));
        if let Ok(manifest_content) = std::fs::read_to_string(&manifest_path) {
            if let Ok(val) = serde_json::from_str::<serde_json::Value>(&manifest_content) {
                if let Some(origins) = val.get("allowed_origins").and_then(|v| v.as_array()) {
                    if let Some(first_orig) = origins.first().and_then(|v| v.as_str()) {
                        first_orig
                            .trim_start_matches("chrome-extension://")
                            .trim_end_matches('/')
                            .to_string()
                    } else {
                        DEFAULT_EXTENSION_ID.to_string()
                    }
                } else {
                    DEFAULT_EXTENSION_ID.to_string()
                }
            } else {
                DEFAULT_EXTENSION_ID.to_string()
            }
        } else {
            DEFAULT_EXTENSION_ID.to_string()
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
