//! Workspace pin + ToolBridge. Unofficial; runtime from xai-org/grok-build.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use xai_grok_tools::bridge::ToolBridge;
use xai_grok_tools::computer::local::{LocalFs, LocalTerminalBackend};
use xai_grok_tools::implementations::codex::ApplyPatchTool;
use xai_grok_tools::implementations::{
    BashTool, GrepTool, KillTaskTool, ListDirTool, OpenCodeGlobTool, OpenCodeWriteTool,
    ReadFileTool, SearchReplaceTool, TaskOutputTool, TodoWriteTool,
};
use xai_grok_tools::notification::ToolNotificationHandle;
use xai_grok_tools::registry::types::{SessionContext, ToolConfig, ToolServerConfig};
use xai_grok_tools::reminders::DEFAULT_REMINDER_TAG;

pub const APP: &str = "hands";
pub const DISPLAY: &str = "Hands";

/// Short platform name for user-facing messages.
#[cfg(target_os = "macos")]
pub const PLATFORM_SHORT: &str = "Mac";
#[cfg(windows)]
pub const PLATFORM_SHORT: &str = "PC";
#[cfg(not(any(target_os = "macos", windows)))]
pub const PLATFORM_SHORT: &str = "machine";

/// Human-readable name of the OS credential store.
#[cfg(target_os = "macos")]
pub const CREDENTIAL_STORE: &str = "Keychain";
#[cfg(windows)]
pub const CREDENTIAL_STORE: &str = "Windows Credential Manager";
#[cfg(not(any(target_os = "macos", windows)))]
pub const CREDENTIAL_STORE: &str = "credential store";

/// Install hint shown when tunnel-client is not found.
#[cfg(windows)]
pub const TUNNEL_CLIENT_HINT: &str =
    "missing \u{2014} run install.ps1 or place tunnel-client.exe in PATH";
#[cfg(target_os = "macos")]
pub const TUNNEL_CLIENT_HINT: &str = "missing \u{2014} brew install openai/tools/tunnel-client";
#[cfg(not(any(target_os = "macos", windows)))]
pub const TUNNEL_CLIENT_HINT: &str = "missing \u{2014} install tunnel-client on PATH";

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// XDG on Unix (`~/.config/hands`). `%APPDATA%\hands` on Windows.
/// Tests set `HANDS_CONFIG_DIR` to an isolated temp root so they never touch
/// the user's real Hands config (workspace pin, key file).
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HANDS_CONFIG_DIR")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    {
        dirs::config_dir()
            .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
            .join(APP)
    }
    #[cfg(not(windows))]
    {
        home_dir().join(".config").join(APP)
    }
}

pub fn tunnel_client_dir() -> PathBuf {
    #[cfg(windows)]
    {
        // The official Windows tunnel-client follows the same ~/.config
        // profile location as its cross-platform runtime. Using APPDATA here
        // makes `tunnel-client run --profile hands` look in a different
        // directory than Hands writes, so the supervised child immediately
        // exits with "config file ... not found".
        home_dir().join(".config/tunnel-client")
    }
    #[cfg(not(windows))]
    {
        home_dir().join(".config/tunnel-client")
    }
}

pub fn workspace_file() -> PathBuf {
    config_dir().join("workspace")
}

pub fn workspace_generation_file() -> PathBuf {
    config_dir().join("workspace_generation")
}

pub fn current_workspace_generation() -> String {
    migrate_from_legacy();
    if let Ok(gen_str) = std::fs::read_to_string(workspace_generation_file()) {
        let trimmed = gen_str.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "gen_0".to_string()
}

/// Resolve the unambiguous native Windows command processor (`cmd.exe`).
/// Avoids PATH shadowing from third-party scripts or executables named `cmd`.
pub fn native_cmd_exe() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        resolve_native_cmd(
            std::env::var_os("ComSpec"),
            std::env::var_os("SystemRoot"),
            |path| path.is_file(),
        )
    }
    #[cfg(not(windows))]
    {
        Ok(PathBuf::from("sh"))
    }
}

#[cfg(windows)]
fn resolve_native_cmd<F>(
    comspec: Option<std::ffi::OsString>,
    system_root: Option<std::ffi::OsString>,
    exists: F,
) -> Result<PathBuf, String>
where
    F: Fn(&Path) -> bool,
{
    let mut candidates = Vec::with_capacity(3);
    if let Some(comspec) = comspec.filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(comspec));
    }
    if let Some(system_root) = system_root.filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(system_root).join("System32").join("cmd.exe"));
    }
    candidates.push(PathBuf::from(r"C:\Windows\System32\cmd.exe"));

    candidates
        .iter()
        .find(|candidate| exists(candidate))
        .cloned()
        .ok_or_else(|| {
            let tried = candidates
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("native Windows cmd.exe not found; tried: {tried}")
        })
}

/// Locate Git for Windows Bash runtime deterministically.
pub fn find_git_bash() -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        let mut candidates = Vec::new();
        if let Some(prog_files) = std::env::var_os("ProgramFiles") {
            candidates.push(PathBuf::from(prog_files.clone()).join(r"Git\bin\bash.exe"));
            candidates.push(PathBuf::from(prog_files).join(r"Git\usr\bin\bash.exe"));
        }
        if let Some(prog_files_x86) = std::env::var_os("ProgramFiles(x86)") {
            candidates.push(PathBuf::from(prog_files_x86.clone()).join(r"Git\bin\bash.exe"));
            candidates.push(PathBuf::from(prog_files_x86).join(r"Git\usr\bin\bash.exe"));
        }
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            candidates.push(PathBuf::from(local_app_data.clone()).join(r"Programs\Git\bin\bash.exe"));
            candidates.push(PathBuf::from(local_app_data).join(r"Programs\Git\usr\bin\bash.exe"));
        }
        candidates.push(PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"));
        candidates.push(PathBuf::from(r"C:\Program Files\Git\usr\bin\bash.exe"));
        candidates.push(PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"));

        if let Some(git_exe) = crate::service::which("git") {
            if let Some(git_dir) = git_exe.parent() {
                candidates.push(git_dir.join("bash.exe"));
                if let Some(parent) = git_dir.parent() {
                    candidates.push(parent.join(r"bin\bash.exe"));
                    candidates.push(parent.join(r"usr\bin\bash.exe"));
                }
            }
        }

        for candidate in &candidates {
            if candidate.is_file() {
                return Ok(candidate.clone());
            }
        }

        let tried = candidates
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        Err(format!("Git for Windows Bash not found; tried: {tried}"))
    }
    #[cfg(not(windows))]
    {
        crate::service::which("bash")
            .or_else(|| crate::service::which("sh"))
            .ok_or_else(|| "bash not found in PATH".to_string())
    }
}

/// Read User Environment PATH from Windows registry `HKCU\Environment\Path`.
#[cfg(windows)]
fn read_registry_user_path() -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;

    type HKEY = *mut std::ffi::c_void;
    type LSTATUS = i32;
    const HKEY_CURRENT_USER: HKEY = 0x80000001u32 as usize as HKEY;
    const KEY_READ: u32 = 0x20019;
    const ERROR_SUCCESS: LSTATUS = 0;

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegOpenKeyExW(
            hKey: HKEY,
            lpSubKey: *const u16,
            ulOptions: u32,
            samDesired: u32,
            phkResult: *mut HKEY,
        ) -> LSTATUS;
        fn RegQueryValueExW(
            hKey: HKEY,
            lpValueName: *const u16,
            lpReserved: *mut u32,
            lpType: *mut u32,
            lpData: *mut u8,
            lpcbData: *mut u32,
        ) -> LSTATUS;
        fn RegCloseKey(hKey: HKEY) -> LSTATUS;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn ExpandEnvironmentStringsW(lpSrc: *const u16, lpDst: *mut u16, nSize: u32) -> u32;
    }

    let subkey: Vec<u16> = "Environment\0".encode_utf16().collect();
    let val_name: Vec<u16> = "Path\0".encode_utf16().collect();
    let mut hkey: HKEY = std::ptr::null_mut();

    let res = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, subkey.as_ptr(), 0, KEY_READ, &mut hkey) };
    if res != ERROR_SUCCESS {
        return None;
    }

    let mut data_type: u32 = 0;
    let mut data_size: u32 = 0;
    let res = unsafe {
        RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            std::ptr::null_mut(),
            &mut data_type,
            std::ptr::null_mut(),
            &mut data_size,
        )
    };
    if res != ERROR_SUCCESS || data_size == 0 {
        unsafe { RegCloseKey(hkey) };
        return None;
    }

    let mut buf: Vec<u8> = vec![0u8; data_size as usize];
    let res = unsafe {
        RegQueryValueExW(
            hkey,
            val_name.as_ptr(),
            std::ptr::null_mut(),
            &mut data_type,
            buf.as_mut_ptr(),
            &mut data_size,
        )
    };
    unsafe { RegCloseKey(hkey) };

    if res != ERROR_SUCCESS {
        return None;
    }

    buf.truncate(data_size as usize);
    crate::host_env::parse_registry_path_payload(data_type, &buf, |raw| {
        let src: Vec<u16> = raw.encode_utf16().chain(std::iter::once(0)).collect();
        let required = unsafe { ExpandEnvironmentStringsW(src.as_ptr(), std::ptr::null_mut(), 0) };
        if required == 0 {
            return None;
        }
        let mut expanded = vec![0u16; required as usize];
        let written = unsafe {
            ExpandEnvironmentStringsW(src.as_ptr(), expanded.as_mut_ptr(), expanded.len() as u32)
        };
        if written == 0 || written > expanded.len() as u32 {
            return None;
        }
        let len = written.saturating_sub(1) as usize;
        OsString::from_wide(&expanded[..len]).into_string().ok()
    })
    .ok()
}

/// Compose host tool PATH so that user-installed tools (Orca, cargo, pnpm, etc.)
/// are resolvable even when Hands is launched by the Windows supervisor or background daemon.
pub fn compose_host_path() {
    #[cfg(windows)]
    {
        if let Some(user_path) = read_registry_user_path() {
            let current_path = std::env::var_os("PATH").unwrap_or_default();
            if let Ok(joined) =
                crate::host_env::merge_path(&current_path, std::ffi::OsStr::new(&user_path))
            {
                if joined != current_path {
                    unsafe {
                        std::env::set_var("PATH", joined);
                    }
                }
            }
        }
    }
}

/// Copy `~/.config/grok-harness` once if the new dir is empty.
/// On Windows, a legacy plaintext `control-plane.key` is never copied as a
/// file: if the user opts in (HANDS_MIGRATE_LEGACY_KEY=1), it is migrated
/// into the Credential Manager and the plaintext source is deleted. Ordinary
/// config (workspace pin) copies as-is.
/// Workspace migration and optional Runtime Key migration are independent
/// so an existing workspace pin does not block an explicit key migration.
pub fn migrate_from_legacy() {
    let dest = config_dir();
    let src = home_dir().join(".config/grok-harness");
    if !src.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(&dest);
    // Workspace pin: ordinary config, safe to copy if destination missing.
    {
        let from = src.join("workspace");
        let to = dest.join("workspace");
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
    // Runtime key: never a plaintext copy on Windows. Migrate into the
    // Credential Manager only with explicit opt-in; remove the source after
    // a successful import. This runs independently of workspace state so
    // `HANDS_MIGRATE_LEGACY_KEY=1` is honored even when the new workspace pin
    // already exists. Unix keeps the existing plaintext file copy.
    #[cfg(windows)]
    {
        let key_path = src.join("control-plane.key");
        if std::env::var("HANDS_MIGRATE_LEGACY_KEY").is_ok_and(|v| v == "1")
            && let Ok(key) = std::fs::read_to_string(&key_path)
        {
            let key = key.trim().to_string();
            if crate::secrets::valid_runtime_key(&key) && crate::secrets::win_cred_set(&key).is_ok()
            {
                let _ = std::fs::remove_file(&key_path);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let from = src.join("control-plane.key");
        let to = dest.join("control-plane.key");
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

pub fn read_workspace_pin_raw() -> Option<PathBuf> {
    migrate_from_legacy();
    let raw = std::fs::read_to_string(workspace_file()).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

pub fn read_pinned_workspace() -> Option<PathBuf> {
    let path = read_workspace_pin_raw()?;
    if path.is_dir() {
        dunce::canonicalize(&path).ok()
    } else {
        None
    }
}

pub fn pin_workspace(dir: &Path) -> Result<PathBuf, String> {
    migrate_from_legacy();
    if !dir.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    let cwd = dunce::canonicalize(dir).map_err(|e| format!("canonicalize: {e}"))?;
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;

    let prev_gen = std::fs::read(workspace_generation_file()).ok();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let new_gen = format!("gen_{now}_{}", std::process::id());

    // 1. Write generation file first: if this fails, workspace_file is completely untouched
    if let Err(e) = std::fs::write(workspace_generation_file(), format!("{new_gen}
")) {
        return Err(format!("write workspace generation: {e}"));
    }

    // 2. Write workspace pin file: if this fails, rollback generation file and report any secondary error
    if let Err(e) = std::fs::write(workspace_file(), format!("{}
", cwd.display())) {
        let mut rollback_err = None;
        match prev_gen {
            Some(prev) => {
                if let Err(re) = std::fs::write(workspace_generation_file(), prev) {
                    rollback_err = Some(format!("failed to restore generation: {re}"));
                }
            }
            None => {
                if let Err(re) = std::fs::remove_file(workspace_generation_file()) {
                    rollback_err = Some(format!("failed to remove generation: {re}"));
                }
            }
        }
        if let Some(r_err) = rollback_err {
            return Err(format!("write workspace pin: {e} (rollback failed: {r_err})"));
        }
        return Err(format!("write workspace pin: {e}"));
    }

    Ok(cwd)
}

/// Active workspace: env → pin file → `--cwd`/process cwd.
pub fn resolve_workspace(fallback: &Path) -> PathBuf {
    migrate_from_legacy();
    for var in ["HANDS_WORKSPACE", "GROK_HARNESS_WORKSPACE"] {
        if let Ok(env_path) = std::env::var(var) {
            let p = PathBuf::from(env_path);
            if let Ok(c) = dunce::canonicalize(&p) {
                if c.is_dir() {
                    return c;
                }
            }
        }
    }
    if let Some(pinned) = read_pinned_workspace() {
        return pinned;
    }
    dunce::canonicalize(fallback).unwrap_or_else(|_| fallback.to_path_buf())
}

fn allowlist() -> ToolServerConfig {
    ToolServerConfig {
        tools: vec![
            ToolConfig::from(&ReadFileTool),
            ToolConfig::from(&GrepTool),
            ToolConfig::from(&ListDirTool),
            ToolConfig::from(&OpenCodeGlobTool),
            ToolConfig::from(&SearchReplaceTool),
            ToolConfig::from(&OpenCodeWriteTool),
            ToolConfig::from(&ApplyPatchTool),
            ToolConfig::from(&TodoWriteTool),
            ToolConfig::from(&BashTool),
            ToolConfig::from(&TaskOutputTool),
            ToolConfig::from(&KillTaskTool),
        ],
        behavior_preset: None,
    }
}

fn session_context(cwd: PathBuf) -> SessionContext {
    let host_dir = std::env::temp_dir().join(APP);
    let _ = std::fs::create_dir_all(&host_dir);
    SessionContext {
        backend: Arc::new(LocalTerminalBackend::new()),
        fs: Arc::new(LocalFs),
        cwd,
        session_folder: host_dir.join("session"),
        session_env: Arc::new(HashMap::new()),
        notification_handle: ToolNotificationHandle::noop(),
        owner_session_id: None,
        subagent: None,
        parent_scheduler_handle: None,
        skills: vec![],
        state_path: host_dir.join("state.json"),
        memory_backend: None,
        web_search_config: Default::default(),
        web_fetch_config: Default::default(),
        lsp: None,
        image_gen_config: Default::default(),
        video_gen_config: Default::default(),
        app_builder_deployer_config: Default::default(),
        api_key_provider: None,
        auth_provider: None,
        attribution_callback: None,
        system_reminder_tag: DEFAULT_REMINDER_TAG,
    }
}

pub async fn build_bridge(cwd: PathBuf) -> Result<ToolBridge, String> {
    let mut builder = ToolBridge::get_builder();
    builder.set_system_reminders_enabled(false);
    ToolBridge::finalize_builder(builder, allowlist(), session_context(cwd))
        .await
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::isolate_env;

    #[test]
    #[cfg(windows)]
    fn test_windows_tunnel_client_profile_dir_matches_runtime_contract() {
        let expected = home_dir().join(".config/tunnel-client");
        assert_eq!(tunnel_client_dir(), expected);
    }

    #[test]
    #[cfg(windows)]
    fn test_native_cmd_exe_resolution() {
        let cmd = native_cmd_exe().expect("native cmd should resolve on Windows");
        assert!(
            cmd.is_file(),
            "native_cmd_exe should resolve to a valid file on Windows: {}",
            cmd.display()
        );
        let name = cmd.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name.eq_ignore_ascii_case("cmd.exe"),
            "resolved name should be cmd.exe, got: {name}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_cmd_path_shadowing_regression() {
        let (_env_lock, _env) = isolate_env("host_cmd_path_shadow");
        // Create a temporary directory containing a fake shadowing cmd.ps1 and cmd.cmd
        let temp_dir =
            std::env::temp_dir().join(format!("hands_cmd_shadow_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let fake_ps1 = temp_dir.join("cmd.ps1");
        let fake_cmd = temp_dir.join("cmd.cmd");
        let _ = std::fs::write(&fake_ps1, "Write-Error 'SHADOWED_BY_NPM'");
        let _ = std::fs::write(&fake_cmd, "@echo off\necho SHADOWED_BY_NPM\nexit /b 1\n");

        // Prepend fake directory to PATH
        let orig_path = std::env::var("PATH").unwrap_or_default();
        let mut entries = vec![temp_dir.clone()];
        entries.extend(std::env::split_paths(&orig_path));
        let shadowed_path = std::env::join_paths(entries).unwrap();
        unsafe {
            std::env::set_var("PATH", &shadowed_path);
        }

        // Verify native_cmd_exe still invokes the real Windows command processor
        let cmd_exe = native_cmd_exe().expect("native cmd should resolve despite PATH shadowing");
        let output = std::process::Command::new(&cmd_exe)
            .args(["/c", "echo", "HANDS_WINDOWS_OK"])
            .output()
            .expect("must execute native cmd");

        let _ = std::fs::remove_dir_all(&temp_dir);

        assert!(output.status.success(), "native cmd should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("HANDS_WINDOWS_OK"),
            "output should be HANDS_WINDOWS_OK, got: {stdout}"
        );
        assert!(
            !stdout.contains("SHADOWED_BY_NPM"),
            "output must not come from shadowing script"
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_native_cmd_resolution_order_and_explicit_failure() {
        let comspec = std::ffi::OsString::from(r"D:\custom\cmd.exe");
        let system_root = std::ffi::OsString::from(r"E:\Windows");

        let from_comspec =
            resolve_native_cmd(Some(comspec.clone()), Some(system_root.clone()), |path| {
                path == Path::new(r"D:\custom\cmd.exe")
            })
            .expect("ComSpec should have first precedence");
        assert_eq!(from_comspec, PathBuf::from(r"D:\custom\cmd.exe"));

        let from_system_root = resolve_native_cmd(Some(comspec), Some(system_root), |path| {
            path == Path::new(r"E:\Windows\System32\cmd.exe")
        })
        .expect("SystemRoot should be used when ComSpec is invalid");
        assert_eq!(
            from_system_root,
            PathBuf::from(r"E:\Windows\System32\cmd.exe")
        );

        let from_default = resolve_native_cmd(None, None, |path| {
            path == Path::new(r"C:\Windows\System32\cmd.exe")
        })
        .expect("hardcoded native path should be the final deterministic fallback");
        assert_eq!(from_default, PathBuf::from(r"C:\Windows\System32\cmd.exe"));

        let error = resolve_native_cmd(None, None, |_| false)
            .expect_err("missing native cmd must fail instead of falling back to PATH");
        assert!(
            error.contains("native Windows cmd.exe not found"),
            "unexpected diagnostic: {error}"
        );
        assert!(
            error.contains(r"C:\Windows\System32\cmd.exe"),
            "diagnostic should list attempted native path: {error}"
        );
        assert!(
            !error.contains("tried: cmd.exe"),
            "diagnostic must not include a bare PATH-resolved cmd.exe candidate: {error}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_compose_host_path() {
        let (_env_lock, _env) = isolate_env("host_compose_path");
        compose_host_path();
        let path = std::env::var("PATH").unwrap_or_default();
        assert!(!path.is_empty(), "PATH should not be empty");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_orca_resolution_through_hands() {
        let (_env_lock, _env) = isolate_env("host_orca_resolution");
        compose_host_path();
        let cwd = std::env::current_dir().unwrap();
        let bridge = build_bridge(cwd).await.expect("bridge should build");

        fn assert_exit_zero(label: &str, prompt: &str) {
            assert!(
                prompt.lines().any(|line| {
                    let line = line.trim_start();
                    line == "exit: 0" || line.starts_with("exit: 0 [")
                }),
                "{label} should exit successfully via Hands bridge: {prompt}"
            );
        }

        fn parse_json_output(label: &str, prompt: &str) -> serde_json::Value {
            let start = prompt
                .find('{')
                .unwrap_or_else(|| panic!("{label} did not return JSON: {prompt}"));
            let end = prompt
                .rfind('}')
                .unwrap_or_else(|| panic!("{label} returned truncated JSON: {prompt}"));
            serde_json::from_str(&prompt[start..=end])
                .unwrap_or_else(|error| panic!("{label} returned invalid JSON ({error}): {prompt}"))
        }

        let resolve_args = serde_json::json!({
            "command": "powershell.exe -NoProfile -NonInteractive -Command \"(Get-Command orca -ErrorAction Stop).Source\"",
            "description": "resolve orca executable"
        });
        let resolve = bridge
            .call("run_terminal_cmd", resolve_args, "test-orca-resolve")
            .await
            .expect("Get-Command orca bridge call should succeed");
        assert_exit_zero("Get-Command orca", &resolve.prompt_text);
        let executable = resolve
            .prompt_text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with("exit: 0"))
            .expect("Get-Command orca should return a non-empty executable path");
        assert!(
            Path::new(executable).is_file(),
            "resolved Orca path should be an executable file: {executable}"
        );

        let args = serde_json::json!({
            "command": "orca --version",
            "description": "check orca version"
        });
        let version = bridge
            .call("run_terminal_cmd", args, "test-orca-version")
            .await
            .expect("orca --version bridge call should succeed");
        assert_exit_zero("orca --version", &version.prompt_text);

        let args_status = serde_json::json!({
            "command": "orca status --json",
            "description": "check orca status"
        });
        let status = bridge
            .call("run_terminal_cmd", args_status, "test-orca-status")
            .await
            .expect("orca status --json bridge call should succeed");
        assert_exit_zero("orca status --json", &status.prompt_text);
        let status_json = parse_json_output("orca status --json", &status.prompt_text);
        assert_eq!(
            status_json
                .pointer("/result/runtime/state")
                .and_then(serde_json::Value::as_str),
            Some("ready")
        );
        assert_eq!(
            status_json
                .pointer("/result/runtime/reachable")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );

        let args_repo = serde_json::json!({
            "command": "orca repo list --json",
            "description": "check orca repo list"
        });
        let repo = bridge
            .call("run_terminal_cmd", args_repo, "test-orca-repo")
            .await
            .expect("orca repo list --json bridge call should succeed");
        assert_exit_zero("orca repo list --json", &repo.prompt_text);
        let repo_json = parse_json_output("orca repo list --json", &repo.prompt_text);
        assert!(
            repo_json.get("result").is_some(),
            "orca repo list --json should contain a result object: {}",
            repo.prompt_text
        );
    }

    #[cfg(windows)]
    fn is_process_alive_by_pid(pid: u32) -> bool {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(
                dwDesiredAccess: u32,
                bInheritHandle: i32,
                dwProcessId: u32,
            ) -> *mut std::ffi::c_void;
            fn WaitForSingleObject(hHandle: *mut std::ffi::c_void, dwMilliseconds: u32) -> u32;
            fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        const SYNCHRONIZE: u32 = 0x00100000;
        const WAIT_TIMEOUT: u32 = 0x00000102;

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
            if handle.is_null() {
                return false;
            }
            let wait_res = WaitForSingleObject(handle, 0);
            CloseHandle(handle);
            wait_res == WAIT_TIMEOUT
        }
    }

    #[cfg(windows)]
    struct ProcessCleanupGuard {
        control_child: Option<std::process::Child>,
        descendant_pid: Option<u32>,
        pid_file: Option<PathBuf>,
    }

    #[cfg(windows)]
    impl Drop for ProcessCleanupGuard {
        fn drop(&mut self) {
            if let Some(mut child) = self.control_child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Some(pid) = self.descendant_pid {
                if is_process_alive_by_pid(pid) {
                    let _ = std::process::Command::new("taskkill")
                        .args(["/F", "/PID", &pid.to_string()])
                        .output();
                }
            }
            if let Some(path) = &self.pid_file {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_process_tree_isolation_on_kill_task() {
        // Start an unrelated control process that is NOT managed by Hands
        let control_child = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 60",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("should spawn control process");

        let control_pid = control_child.id();
        let pid_file = std::env::temp_dir().join(format!(
            "hands_descendant_test_{}_{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        let _ = std::fs::remove_file(&pid_file);

        let mut cleanup = ProcessCleanupGuard {
            control_child: Some(control_child),
            descendant_pid: None,
            pid_file: Some(pid_file.clone()),
        };

        let cwd = std::env::current_dir().unwrap();
        let bridge = build_bridge(cwd).await.expect("bridge should build");

        let pid_file_path = pid_file.to_string_lossy().replace('\\', "/");
        let bg_cmd = format!(
            "$p = Start-Process powershell.exe -ArgumentList '-NoProfile','-NonInteractive','-Command','Start-Sleep -Seconds 60' -PassThru; [System.IO.File]::WriteAllText('{}', $p.Id.ToString()); Start-Sleep -Seconds 60",
            pid_file_path
        );

        // Start a Hands-owned background task that spawns a child/descendant process
        let bg_args = serde_json::json!({
            "command": bg_cmd,
            "description": "start background sleep with descendant",
            "is_background": true
        });
        let bg_res = bridge
            .call("run_terminal_cmd", bg_args, "test-bg-task")
            .await
            .expect("bg call should succeed");
        // Extract task_id from result: look for <task-id> XML tag or text pattern
        let task_id = if let Some(start) = bg_res.prompt_text.find("<task-id>") {
            let rest = &bg_res.prompt_text[start + 9..];
            if let Some(end) = rest.find("</task-id>") {
                rest[..end].trim().to_string()
            } else {
                String::new()
            }
        } else {
            bg_res
                .prompt_text
                .lines()
                .find_map(|line| {
                    if line.contains("Task ID:") || line.contains("task_id:") {
                        line.split(':').nth(1).map(|s| s.trim().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        };

        assert!(
            !task_id.is_empty(),
            "should have received a background task ID: {}",
            bg_res.prompt_text
        );

        // Poll for descendant PID file to be written by the background task
        let mut descendant_pid_opt: Option<u32> = None;
        for _ in 0..100 {
            if pid_file.is_file() {
                if let Ok(content) = std::fs::read_to_string(&pid_file) {
                    let trimmed = content.trim();
                    if let Ok(pid) = trimmed.parse::<u32>() {
                        if pid > 0 {
                            descendant_pid_opt = Some(pid);
                            break;
                        }
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let descendant_pid =
            descendant_pid_opt.expect("descendant PID must be captured deterministically");
        cleanup.descendant_pid = Some(descendant_pid);
        // Confirm descendant process is alive before kill_task
        assert!(
            is_process_alive_by_pid(descendant_pid),
            "descendant process (PID {}) MUST be alive before kill_task",
            descendant_pid
        );

        // Confirm unrelated control process is alive before kill_task
        assert!(
            is_process_alive_by_pid(control_pid),
            "unrelated control process (PID {}) MUST be alive before kill_task",
            control_pid
        );

        // Verify task output reports active / running state
        let out_args = serde_json::json!({ "task_id": &task_id });
        let out_res = bridge
            .call("get_task_output", out_args, "test-get-task-1")
            .await
            .expect("get_task_output should succeed");
        assert!(
            out_res.prompt_text.contains("running")
                || out_res.prompt_text.contains("output")
                || out_res.prompt_text.contains("Task"),
            "task should be active"
        );

        // Call real Hands kill_task path
        let kill_args = serde_json::json!({ "task_id": &task_id });
        let kill_res = bridge
            .call("kill_task", kill_args, "test-kill-task")
            .await
            .expect("kill_task should succeed");
        assert!(
            !kill_res.prompt_text.trim().is_empty(),
            "kill_task should return a result"
        );

        // Confirm get_task_output reports cancelled/terminal state
        let out_after = bridge
            .call(
                "get_task_output",
                serde_json::json!({ "task_id": &task_id }),
                "test-get-task-2",
            )
            .await
            .expect("get_task_output after kill should succeed");
        assert!(
            out_after
                .prompt_text
                .to_ascii_lowercase()
                .contains("status: cancelled"),
            "killed task should be specifically cancelled: {}",
            out_after.prompt_text
        );

        // Confirm descendant process belonging to the Hands task is NO LONGER alive after kill_task
        let mut descendant_alive = true;
        for _ in 0..50 {
            if !is_process_alive_by_pid(descendant_pid) {
                descendant_alive = false;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            !descendant_alive,
            "descendant process (PID {}) belonging to Hands task MUST be terminated after kill_task",
            descendant_pid
        );

        // Confirm unrelated control process remains alive
        if let Some(child) = &mut cleanup.control_child {
            let is_control_alive = match child.try_wait() {
                Ok(None) => true,
                _ => false,
            };
            assert!(
                is_control_alive && is_process_alive_by_pid(control_pid),
                "unrelated control process (PID {}) MUST remain alive after Hands task is killed",
                control_pid
            );
        }
    }

    #[test]
    fn test_workspace_pin_and_resolve_with_unicode_and_spaces() {
        let (_env_lock, env) = isolate_env("host_workspace_pin_unicode");
        let ws_dir = env.root.join("Sub Folder With Spaces 測試");
        std::fs::create_dir_all(&ws_dir).expect("should create test workspace directory");

        let pinned = pin_workspace(&ws_dir)
            .expect("pin_workspace should succeed for unicode path with spaces");
        assert_eq!(pinned, dunce::canonicalize(&ws_dir).unwrap());

        let read =
            read_pinned_workspace().expect("read_pinned_workspace should return pinned path");
        assert_eq!(read, pinned);

        let resolved = resolve_workspace(&std::env::temp_dir());
        assert_eq!(resolved, pinned);
    }
    #[test]
    fn test_which_executable_discovery() {
        let (_env_lock, _env) = isolate_env("host_which_discovery");
        let temp_dir = std::env::temp_dir().join(format!(
            "hands_which_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        let _ = std::fs::create_dir_all(&temp_dir);

        #[cfg(windows)]
        let fake_exe = temp_dir.join("fake-tool.exe");
        #[cfg(not(windows))]
        let fake_exe = temp_dir.join("fake-tool");

        std::fs::write(&fake_exe, "mock binary").expect("write fake binary");

        let orig_path = std::env::var("PATH").unwrap_or_default();
        let mut entries = vec![temp_dir.clone()];
        entries.extend(std::env::split_paths(&orig_path));
        let new_path = std::env::join_paths(entries).unwrap();
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        let found = crate::service::which("fake-tool");
        // Preserve comparison before destroying the fixture directory.
        let found_canon = found.as_ref().and_then(|p| p.canonicalize().ok());
        let expected_canon = fake_exe.canonicalize().ok();

        assert!(
            found.is_some(),
            "which('fake-tool') should locate fake-tool binary on PATH"
        );
        assert_eq!(found_canon, expected_canon);
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_windows_terminal_foreground_and_bounded_output() {
        let (_env_lock, _env) = isolate_env("host_terminal_foreground");
        compose_host_path();
        let temp_ws = std::env::temp_dir().join(format!(
            "hands_term_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        let _ = std::fs::create_dir_all(&temp_ws);
        let canonical_ws = dunce::canonicalize(&temp_ws).unwrap();

        let bridge = build_bridge(canonical_ws.clone())
            .await
            .expect("bridge should build with canonical workspace");

        // 1. Foreground command execution — must run in the intended
        // workspace CWD, not the process cwd.
        let fg_args = serde_json::json!({
            "command": "powershell.exe -NoProfile -NonInteractive -Command \"(Get-Location).Path; Write-Output 'TERMINAL_FOREGROUND_OK'\"",
            "description": "check foreground execution"
        });
        let fg_res = bridge
            .call("run_terminal_cmd", fg_args, "test-term-fg")
            .await
            .expect("foreground terminal call should succeed");
        assert!(
            fg_res.prompt_text.contains("TERMINAL_FOREGROUND_OK"),
            "foreground terminal output should contain expected marker: {}",
            fg_res.prompt_text
        );
        let fg_has_cwd = fg_res
            .prompt_text
            .contains(canonical_ws.as_os_str().to_string_lossy().as_ref());
        assert!(
            fg_has_cwd,
            "foreground command must report the intended workspace CWD ({}), got: {}",
            canonical_ws.display(),
            fg_res.prompt_text
        );
        // 2. Background command returning task ID and retrieving ordered output.
        // Assert relative positions: LINE_1 < LINE_2 < LINE_3.
        let bg_args = serde_json::json!({
            "command": "powershell.exe -NoProfile -NonInteractive -Command \"Write-Output LINE_1; Write-Output LINE_2; Write-Output LINE_3; Start-Sleep -Milliseconds 200\"",
            "description": "ordered output test",
            "is_background": true
        });
        let bg_res = bridge
            .call("run_terminal_cmd", bg_args, "test-term-bg")
            .await
            .expect("background terminal call should succeed");

        let task_id = if let Some(start) = bg_res.prompt_text.find("<task-id>") {
            let rest = &bg_res.prompt_text[start + 9..];
            if let Some(end) = rest.find("</task-id>") {
                rest[..end].trim().to_string()
            } else {
                String::new()
            }
        } else {
            bg_res
                .prompt_text
                .lines()
                .find_map(|line| {
                    if line.contains("Task ID:") || line.contains("task_id:") {
                        line.split(':').nth(1).map(|s| s.trim().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default()
        };
        assert!(!task_id.is_empty(), "background task should return task ID");

        // Wait for background job to emit output
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let out_args = serde_json::json!({ "task_id": &task_id });
        let out_res = bridge
            .call("get_task_output", out_args, "test-term-out")
            .await
            .expect("get_task_output should succeed");

        let pos_1 = out_res
            .prompt_text
            .find("LINE_1")
            .expect("output should contain LINE_1");
        let pos_2 = out_res
            .prompt_text
            .find("LINE_2")
            .expect("output should contain LINE_2");
        let pos_3 = out_res
            .prompt_text
            .find("LINE_3")
            .expect("output should contain LINE_3");
        assert!(
            pos_1 < pos_2 && pos_2 < pos_3,
            "output must be ordered LINE_1 < LINE_2 < LINE_3: {}",
            out_res.prompt_text
        );

        // 3. Bounded/truncation behavior: deterministic output far beyond the
        // 40 KB tool-output cap. Response must stay bounded, carry the
        // truncation marker, and not hang.
        let big_args = serde_json::json!({
            "command": "powershell.exe -NoProfile -NonInteractive -Command \"[Console]::Out.Write(('BULK_A:' + [string][char]0x41) * 60000); Write-Output 'TRUNC_TAIL_SENTINEL'\"",
            "description": "truncation bound test",
            "is_background": true
        });
        let big_res = bridge
            .call("run_terminal_cmd", big_args, "test-term-big")
            .await
            .expect("large output terminal call should succeed");
        let big_task_id = if let Some(start) = big_res.prompt_text.find("<task-id>") {
            let rest = &big_res.prompt_text[start + 9..];
            if let Some(end) = rest.find("</task-id>") {
                rest[..end].trim().to_string()
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        assert!(
            !big_task_id.is_empty(),
            "large output task should return task ID"
        );

        // Block (bounded) on completion with `timeout_ms`, then read the
        // truncated output. `run_terminal_cmd` enforces a 20K-char ring
        // (front+back halves joined by a truncation marker), so the ~420KB
        // raw output is always clipped without hanging. The tool response
        // MUST carry the truncation marker and the truncation hint footer.
        let big_out = bridge
            .call(
                "get_task_output",
                serde_json::json!({ "task_id": &big_task_id, "timeout_ms": 30000 }),
                "test-term-big-out",
            )
            .await
            .expect("get_task_output for large output should succeed");
        assert!(
            big_out.prompt_text.contains("... (output truncated) ..."),
            "oversized output must carry the front/back truncation marker: {}",
            big_out.prompt_text
        );
        assert!(
            big_out.prompt_text.contains("[truncated - use read_file"),
            "oversized output must carry the truncation hint footer: {}",
            big_out.prompt_text
        );
        assert!(
            big_out.prompt_text.len() <= 60_000,
            "rendered output must stay bounded (20K-char ring + hint), got {} bytes",
            big_out.prompt_text.len()
        );

        // 4. kill_task on the bulk producer: owned task tree is stopped.
        let kill_res = bridge
            .call(
                "kill_task",
                serde_json::json!({ "task_id": &big_task_id }),
                "test-term-big-kill",
            )
            .await
            .expect("kill_task should succeed");
        let _ = kill_res;

        let _ = std::fs::remove_dir_all(&temp_ws);
    }
    #[test]
    fn test_pin_workspace_generation_failure_rolls_back_and_fails_closed() {
        let (_env_lock, env_guard) = isolate_env("pin_gen_fail");
        let ws1 = env_guard.root.join("ws1");
        let ws2 = env_guard.root.join("ws2");
        std::fs::create_dir_all(&ws1).unwrap();
        std::fs::create_dir_all(&ws2).unwrap();

        // 1. Initially pin ws1 successfully
        let pinned1 = pin_workspace(&ws1).expect("initial pin of ws1 should succeed");
        assert_eq!(pinned1, dunce::canonicalize(&ws1).unwrap());
        let initial_gen = current_workspace_generation();

        // 2. Make workspace_generation path a directory so writing to it will fail deterministically
        let gen_file = workspace_generation_file();
        let _ = std::fs::remove_file(&gen_file);
        std::fs::create_dir_all(&gen_file).expect("create dir at generation file path");

        // 3. Attempting to pin ws2 must fail closed
        let pin_err = pin_workspace(&ws2);
        assert!(pin_err.is_err(), "pin_workspace must fail when generation write fails");
        let err_msg = pin_err.unwrap_err();
        assert!(err_msg.contains("write workspace generation"), "error should describe generation write failure: {err_msg}");

        // 4. Workspace pin file was never touched, so read_pinned_workspace() is still ws1 immediately without manual repair
        let current_pin = read_pinned_workspace();
        assert_eq!(current_pin, Some(dunce::canonicalize(&ws1).unwrap()), "workspace pin must remain ws1 after generation write failure");

        // 5. Clean up the blocking directory and verify normal pinning (including A->B->A) works
        let _ = std::fs::remove_dir_all(&gen_file);
        let pinned2 = pin_workspace(&ws2).expect("pin ws2 after restoring gen path");
        assert_eq!(pinned2, dunce::canonicalize(&ws2).unwrap());
        let gen_ws2 = current_workspace_generation();
        assert_ne!(gen_ws2, initial_gen);

        let pinned1_again = pin_workspace(&ws1).expect("re-pin ws1 (A->B->A)");
        assert_eq!(pinned1_again, dunce::canonicalize(&ws1).unwrap());
        let gen_ws1_again = current_workspace_generation();
        assert_ne!(gen_ws1_again, gen_ws2);
        assert_ne!(gen_ws1_again, initial_gen, "A->B->A must generate a new unique generation");
    }
}
