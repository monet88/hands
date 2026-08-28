use std::fs;
#[cfg(not(windows))]
use std::io::IsTerminal;
use std::path::PathBuf;
#[cfg(not(windows))]
use std::process::{Command, Stdio};

use crate::host;

const SERVICE: &str = "dev.hands.runtime-key";
const ACCOUNT: &str = "hands";

#[cfg(windows)]
#[allow(dead_code)]
const TEST_SERVICE: &str = "dev.hands.runtime-key.test";
/// Credential Manager target name: production or test namespace. Automated
/// tests only ever touch the `…test` target, never production.
/// Production (non-test) builds NEVER consult `HANDS_TEST_CRED_NAMESPACE`;
/// that override is compiled out so an inherited env var cannot redirect
/// real `hands setup` credentials.
#[cfg(windows)]
fn service_target() -> &'static str {
    #[cfg(test)]
    {
        if std::env::var("HANDS_TEST_CRED_NAMESPACE").as_deref() == Ok("1") {
            return TEST_SERVICE;
        }
    }
    SERVICE
}

#[cfg(windows)]
fn target_utf16() -> Vec<u16> {
    service_target()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

pub fn valid_runtime_key(key: &str) -> bool {
    key.starts_with("sk-") && key.len() >= 32 && !key.contains(char::is_whitespace)
}

pub fn key_file() -> PathBuf {
    host::config_dir().join("control-plane.key")
}

pub fn get() -> Option<String> {
    if let Ok(k) = std::env::var("CONTROL_PLANE_API_KEY") {
        let k = k.trim().to_string();
        if valid_runtime_key(&k) {
            return Some(k);
        }
    }
    #[cfg(windows)]
    {
        if let Some(k) = win_cred_get() {
            if valid_runtime_key(&k) {
                return Some(k);
            }
        }
    }
    #[cfg(not(windows))]
    {
        if let Ok(file) = fs::read_to_string(key_file()) {
            let k = file.trim().to_string();
            if valid_runtime_key(&k) {
                return Some(k);
            }
        }
        // Keychain can prompt; only from a TTY (hands setup), never LaunchAgent.
        if std::io::stdin().is_terminal() {
            if let Some(k) = keychain_get() {
                if valid_runtime_key(&k) {
                    let _ = ensure_file(&k);
                    return Some(k);
                }
            }
        }
    }
    // Windows: no plaintext file fallback. The Runtime Key lives only in the
    // Credential Manager (or the env var); a leftover control-plane.key is
    // never read implicitly.
    None
}

pub fn set(key: &str) -> Result<PathBuf, String> {
    let key = key.trim();
    if !valid_runtime_key(key) {
        return Err("runtime key looks invalid (need sk-… from platform.openai.com)".into());
    }
    #[cfg(windows)]
    {
        win_cred_set(key)?;
        // Remove legacy/accidental plaintext key file if present on Windows
        let file = key_file();
        if file.is_file() {
            let _ = fs::remove_file(file);
        }
        Ok(key_file())
    }
    #[cfg(not(windows))]
    {
        let path = ensure_file(key)?;
        if std::io::stdin().is_terminal() {
            match keychain_set(key) {
                Ok(()) => {}
                Err(e) => eprintln!("keychain: {e} (key kept in {})", path.display()),
            }
        }
        Ok(path)
    }
}

#[allow(dead_code)]
pub fn delete() -> Result<(), String> {
    #[cfg(windows)]
    {
        let _ = win_cred_delete();
    }
    let file = key_file();
    if file.is_file() {
        let _ = fs::remove_file(file);
    }
    Ok(())
}

#[allow(dead_code)]
pub fn ensure_file(key: &str) -> Result<PathBuf, String> {
    let path = key_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    fs::write(&path, format!("{key}\n")).map_err(|e| format!("write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

#[allow(dead_code)]
fn keychain_get() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let out = Command::new("security")
            .args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let k = String::from_utf8(out.stdout).ok()?.trim().to_string();
        return if k.is_empty() { None } else { Some(k) };
    }
    #[cfg(target_os = "linux")]
    {
        secret_tool_get()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

#[allow(dead_code)]
fn keychain_set(key: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        // -U replace; -A allow this machine's agents (LaunchAgent) without a prompt each start.
        let status = Command::new("security")
            .args([
                "add-generic-password",
                "-U",
                "-A",
                "-s",
                SERVICE,
                "-a",
                ACCOUNT,
                "-w",
                key,
                "-l",
                "Hands ChatGPT runtime key",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .status()
            .map_err(|e| format!("security: {e}"))?;
        if status.success() {
            return Ok(());
        }
        return Err("macOS Keychain write failed".into());
    }
    #[cfg(target_os = "linux")]
    {
        secret_tool_set(key)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = key;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn secret_tool_get() -> Option<String> {
    let out = Command::new("secret-tool")
        .args(["lookup", "service", SERVICE, "account", ACCOUNT])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let k = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if k.is_empty() { None } else { Some(k) }
}

#[cfg(target_os = "linux")]
fn secret_tool_set(key: &str) -> Result<(), String> {
    let mut child = match Command::new("secret-tool")
        .args([
            "store",
            "--label",
            "Hands ChatGPT runtime key",
            "service",
            SERVICE,
            "account",
            ACCOUNT,
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Ok(()), // no libsecret; file is enough
    };
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        let _ = stdin.write_all(key.as_bytes());
    }
    let _ = child.wait();
    Ok(())
}

#[cfg(windows)]
#[allow(non_snake_case, dead_code)]
mod win_cred {
    use std::ffi::c_void;

    pub const CRED_TYPE_GENERIC: u32 = 1;
    pub const CRED_PERSIST_LOCAL_MACHINE: u32 = 2;

    #[repr(C)]
    pub struct CREDENTIALW {
        pub Flags: u32,
        pub Type: u32,
        pub TargetName: *mut u16,
        pub Comment: *mut u16,
        pub LastWritten: u64,
        pub CredentialBlobSize: u32,
        pub CredentialBlob: *mut u8,
        pub Persist: u32,
        pub AttributeCount: u32,
        pub Attributes: *mut c_void,
        pub TargetAlias: *mut u16,
        pub UserName: *mut u16,
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        pub fn CredWriteW(Credential: *const CREDENTIALW, Flags: u32) -> i32;
        pub fn CredReadW(
            TargetName: *const u16,
            Type: u32,
            Flags: u32,
            Credential: *mut *mut CREDENTIALW,
        ) -> i32;
        pub fn CredDeleteW(TargetName: *const u16, Type: u32, Flags: u32) -> i32;
        pub fn CredFree(Buffer: *mut c_void);
    }
}

#[cfg(windows)]
pub fn win_cred_get() -> Option<String> {
    use std::ptr;
    let target = target_utf16();
    let mut pcred: *mut win_cred::CREDENTIALW = ptr::null_mut();
    let res =
        unsafe { win_cred::CredReadW(target.as_ptr(), win_cred::CRED_TYPE_GENERIC, 0, &mut pcred) };
    if res == 0 || pcred.is_null() {
        return None;
    }
    let cred = unsafe { &*pcred };
    let bytes = if cred.CredentialBlobSize > 0 && !cred.CredentialBlob.is_null() {
        unsafe { std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize) }
    } else {
        &[]
    };
    let result = String::from_utf8(bytes.to_vec())
        .ok()
        .map(|s| s.trim().to_string());
    unsafe {
        win_cred::CredFree(pcred as *mut _);
    }
    result.filter(|s| !s.is_empty())
}

#[cfg(windows)]
pub fn win_cred_set(key: &str) -> Result<(), String> {
    use std::ptr;
    let mut target = target_utf16();
    let mut account: Vec<u16> = ACCOUNT.encode_utf16().chain(std::iter::once(0)).collect();
    let mut comment: Vec<u16> = "Hands ChatGPT runtime key\0".encode_utf16().collect();
    let mut blob = key.as_bytes().to_vec();

    let cred = win_cred::CREDENTIALW {
        Flags: 0,
        Type: win_cred::CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        Comment: comment.as_mut_ptr(),
        LastWritten: 0,
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: win_cred::CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: ptr::null_mut(),
        TargetAlias: ptr::null_mut(),
        UserName: account.as_mut_ptr(),
    };

    let res = unsafe { win_cred::CredWriteW(&cred, 0) };
    if res != 0 {
        Ok(())
    } else {
        Err(format!(
            "Windows CredWriteW failed (os error {})",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(windows)]
pub fn win_cred_delete() -> Result<(), String> {
    let target = target_utf16();
    let res = unsafe { win_cred::CredDeleteW(target.as_ptr(), win_cred::CRED_TYPE_GENERIC, 0) };
    if res != 0 {
        Ok(())
    } else {
        Err(format!(
            "Windows CredDeleteW failed (os error {})",
            std::io::Error::last_os_error()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_runtime_key() {
        assert!(valid_runtime_key("sk-abcdef123456789012345678901234567890"));
        assert!(!valid_runtime_key("invalid-key"));
        assert!(!valid_runtime_key("sk-short"));
        assert!(!valid_runtime_key("sk-with space 12345678901234567890"));
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_credential_manager_roundtrip() {
        // Isolated test-namespace target: the production credential
        // `dev.hands.runtime-key` is never read, written, or deleted here.
        // The guard restores (or clears) the test target on every exit path,
        // including panics.
        unsafe {
            std::env::set_var("HANDS_TEST_CRED_NAMESPACE", "1");
        }
        struct CleanupNamespace;
        impl Drop for CleanupNamespace {
            fn drop(&mut self) {
                let _ = win_cred_delete();
                unsafe {
                    std::env::remove_var("HANDS_TEST_CRED_NAMESPACE");
                }
            }
        }
        let _cleanup = CleanupNamespace;

        let test_key = "sk-test-key-123456789012345678901234567890";
        let set_res = win_cred_set(test_key);
        assert!(
            set_res.is_ok(),
            "win_cred_set should succeed: {:?}",
            set_res
        );

        let retrieved = win_cred_get();
        assert_eq!(retrieved.as_deref(), Some(test_key));

        let del_res = win_cred_delete();
        assert!(
            del_res.is_ok(),
            "win_cred_delete should succeed: {:?}",
            del_res
        );

        let after_del = win_cred_get();
        assert!(
            after_del.is_none(),
            "credential should be gone after delete"
        );
    }
}
