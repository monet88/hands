use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

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
            HostError::Protocol(p) => write!(f, "Protocol error: {}", p),
            HostError::InvalidTarget(t) => write!(f, "Invalid target path: {}", t),
            HostError::Registry(r) => write!(f, "Registry error: {}", r),
        }
    }
}

impl std::error::Error for HostError {}

pub struct HostConfig {
    pub state_dir: PathBuf,
    pub db_path: PathBuf,
}

pub fn resolve_state_dir(override_opt: Option<&Path>) -> PathBuf {
    if let Some(p) = override_opt {
        return p.to_path_buf();
    }
    if let Ok(val) = std::env::var("HANDS_RETURN_BRIDGE_STATE_DIR") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim());
        }
    }
    if let Ok(val) = std::env::var("RETURN_BRIDGE_STATE_DIR") {
        if !val.trim().is_empty() {
            return PathBuf::from(val.trim());
        }
    }

    #[cfg(windows)]
    {
        if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
            return PathBuf::from(local_app_data)
                .join("Hands")
                .join("return-bridge");
        }
        PathBuf::from(r"C:\ProgramData\Hands\return-bridge")
    }
    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("hands")
                .join("return-bridge");
        }
        PathBuf::from("/tmp/hands-return-bridge")
    }
}

fn generate_random_id(prefix: &str, num_bytes: usize) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let mut hasher = Sha256::new();
    hasher.update(now.to_le_bytes());
    hasher.update(pid.to_le_bytes());
    hasher.update(prefix.as_bytes());
    // Mix in environment or memory addresses for extra entropy
    let env_str = format!("{:?}", std::env::vars().count());
    hasher.update(env_str.as_bytes());
    let digest = hasher.finalize();
    let hex_part: String = digest[..num_bytes]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    format!("{}_{}", prefix, hex_part)
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
    pub pairing_secret: String,
    pub browser: String,
    pub profile_id: String,
    pub target_id: String,
    pub canonical_target_path: String,
    pub policy_revision: String,
    pub manifest_path: PathBuf,
    pub trust_notice: String,
}

pub fn execute_setup(opts: &SetupOptions) -> Result<SetupResult, HostError> {
    let state_dir = resolve_state_dir(opts.state_dir.as_deref());
    std::fs::create_dir_all(&state_dir)?;

    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;

    // Canonicalize target path
    let raw_target = Path::new(&opts.target_path);
    if !raw_target.exists() || !raw_target.is_dir() {
        return Err(HostError::InvalidTarget(format!(
            "Directory does not exist: {}",
            opts.target_path
        )));
    }
    let canonical = raw_target
        .canonicalize()
        .map_err(|e| HostError::InvalidTarget(e.to_string()))?;
    let canonical_path_str = canonical.to_string_lossy().to_string();

    let target_id = opts.target_id.clone().unwrap_or_else(|| {
        raw_target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("target_workspace")
            .to_string()
    });

    let pairing_id = generate_random_id("pair", 8);
    let bootstrap_token = generate_random_id("rb_boot", 12);
    let pairing_secret = generate_random_id("rb_sec", 16);

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

    journal
        .create_bootstrap(
            &pairing_id,
            &bootstrap_token,
            &pairing_secret,
            &opts.browser,
            &opts.profile_id,
            &targets,
            &policy,
        )
        .map_err(|e| HostError::Storage(e.to_string()))?;

    // Generate manifest
    let current_exe = std::env::current_exe()?;
    let manifest_path = state_dir.join(format!("{}.json", DEFAULT_HOST_NAME));

    let allowed_origins = vec![format!("chrome-extension://{}/", opts.extension_id)];

    let manifest_json = json!({
        "name": DEFAULT_HOST_NAME,
        "description": "Hands Return Bridge Native Companion Host",
        "path": current_exe.to_string_lossy(),
        "type": "stdio",
        "allowed_origins": allowed_origins
    });

    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest_json)?)?;

    // Register in Windows Registry if requested
    if !opts.skip_registry {
        #[cfg(windows)]
        {
            register_manifest_registry(&opts.browser, &manifest_path)?;
        }
    }

    Ok(SetupResult {
        pairing_id,
        bootstrap_token,
        pairing_secret,
        browser: opts.browser.clone(),
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

pub fn run_native_host(state_dir_opt: Option<&Path>) -> Result<(), HostError> {
    let state_dir = resolve_state_dir(state_dir_opt);
    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).map_err(|e| HostError::Storage(e.to_string()))?;

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
