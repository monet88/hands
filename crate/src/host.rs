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

fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// XDG on Unix (`~/.config/hands`). `%APPDATA%\hands` on Windows.
pub fn config_dir() -> PathBuf {
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
        dirs::config_dir()
            .unwrap_or_else(|| home_dir().join("AppData/Roaming"))
            .join("tunnel-client")
    }
    #[cfg(not(windows))]
    {
        home_dir().join(".config/tunnel-client")
    }
}

pub fn workspace_file() -> PathBuf {
    config_dir().join("workspace")
}

/// Resolve the unambiguous native Windows command processor (`cmd.exe`).
/// Avoids PATH shadowing from third-party scripts or executables named `cmd`.
pub fn native_cmd_exe() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(comspec) = std::env::var("ComSpec") {
            let p = PathBuf::from(&comspec);
            if p.is_file() {
                return p;
            }
        }
        if let Ok(sysroot) = std::env::var("SystemRoot") {
            let p = PathBuf::from(sysroot).join("System32\\cmd.exe");
            if p.is_file() {
                return p;
            }
        }
        let default_cmd = PathBuf::from("C:\\Windows\\System32\\cmd.exe");
        if default_cmd.is_file() {
            return default_cmd;
        }
        PathBuf::from("cmd.exe")
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("sh")
    }
}

/// Read User Environment PATH from Windows registry `HKCU\Environment\Path` and expand variables.
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
        fn ExpandEnvironmentStringsW(
            lpSrc: *const u16,
            lpDst: *mut u16,
            nSize: u32,
        ) -> u32;
    }

    let subkey: Vec<u16> = "Environment\0".encode_utf16().collect();
    let val_name: Vec<u16> = "Path\0".encode_utf16().collect();
    let mut hkey: HKEY = std::ptr::null_mut();

    let res = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            0,
            KEY_READ,
            &mut hkey,
        )
    };
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

    let u16_slice: &[u16] = unsafe {
        std::slice::from_raw_parts(buf.as_ptr() as *const u16, (data_size / 2) as usize)
    };
    let len = u16_slice.iter().position(|&c| c == 0).unwrap_or(u16_slice.len());
    let raw_u16 = &u16_slice[..len];

    let mut expanded_buf = vec![0u16; 32768];
    let mut src_with_null = raw_u16.to_vec();
    src_with_null.push(0);
    let expanded_len = unsafe {
        ExpandEnvironmentStringsW(
            src_with_null.as_ptr(),
            expanded_buf.as_mut_ptr(),
            expanded_buf.len() as u32,
        )
    };
    if expanded_len > 0 && (expanded_len as usize) < expanded_buf.len() {
        let trimmed_len = if expanded_buf[(expanded_len - 1) as usize] == 0 {
            (expanded_len - 1) as usize
        } else {
            expanded_len as usize
        };
        let s = OsString::from_wide(&expanded_buf[..trimmed_len]);
        return s.into_string().ok();
    }

    let s = OsString::from_wide(raw_u16);
    s.into_string().ok()
}

/// Compose host tool PATH so that user-installed tools (Orca, cargo, pnpm, etc.)
/// are resolvable even when Hands is launched by the Windows supervisor or background daemon.
pub fn compose_host_path() {
    #[cfg(windows)]
    {
        if let Some(user_path) = read_registry_user_path() {
            let current_path = std::env::var("PATH").unwrap_or_default();
            let mut current_entries: Vec<PathBuf> = std::env::split_paths(&current_path).collect();
            let user_entries = std::env::split_paths(&user_path);
            let mut modified = false;
            for entry in user_entries {
                if !entry.as_os_str().is_empty() && !current_entries.iter().any(|e| e == &entry) {
                    current_entries.push(entry);
                    modified = true;
                }
            }
            if modified {
                if let Ok(joined) = std::env::join_paths(current_entries) {
                    unsafe {
                        std::env::set_var("PATH", joined);
                    }
                }
            }
        }
    }
}

/// Copy `~/.config/grok-harness` once if the new dir is empty.
pub fn migrate_from_legacy() {
    compose_host_path();
    let dest = config_dir();
    if dest.join("workspace").is_file() || dest.join("control-plane.key").is_file() {
        return;
    }
    let src = home_dir().join(".config/grok-harness");
    if !src.is_dir() {
        return;
    }
    let _ = std::fs::create_dir_all(&dest);
    for name in ["workspace", "control-plane.key"] {
        let from = src.join(name);
        let to = dest.join(name);
        if from.is_file() && !to.exists() {
            let _ = std::fs::copy(&from, &to);
        }
    }
}

pub fn read_pinned_workspace() -> Option<PathBuf> {
    migrate_from_legacy();
    let raw = std::fs::read_to_string(workspace_file()).ok()?;
    let path = PathBuf::from(raw.trim());
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
    std::fs::write(workspace_file(), format!("{}\n", cwd.display()))
        .map_err(|e| format!("write workspace pin: {e}"))?;
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

    #[test]
    #[cfg(windows)]
    fn test_native_cmd_exe_resolution() {
        let cmd = native_cmd_exe();
        assert!(cmd.is_file(), "native_cmd_exe should resolve to a valid file on Windows: {}", cmd.display());
        let name = cmd.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name.eq_ignore_ascii_case("cmd.exe"),
            "resolved name should be cmd.exe, got: {name}"
        );
    }

    #[test]
    #[cfg(windows)]
    fn test_cmd_path_shadowing_regression() {
        // Create a temporary directory containing a fake shadowing cmd.ps1 and cmd.cmd
        let temp_dir = std::env::temp_dir().join(format!("hands_cmd_shadow_test_{}", std::process::id()));
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
        let cmd_exe = native_cmd_exe();
        let output = std::process::Command::new(&cmd_exe)
            .args(["/c", "echo", "HANDS_WINDOWS_OK"])
            .output()
            .expect("must execute native cmd");

        // Restore original PATH
        unsafe {
            std::env::set_var("PATH", &orig_path);
        }
        let _ = std::fs::remove_dir_all(&temp_dir);

        assert!(output.status.success(), "native cmd should succeed");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("HANDS_WINDOWS_OK"), "output should be HANDS_WINDOWS_OK, got: {stdout}");
        assert!(!stdout.contains("SHADOWED_BY_NPM"), "output must not come from shadowing script");
    }

    #[test]
    #[cfg(windows)]
    fn test_compose_host_path() {
        compose_host_path();
        let path = std::env::var("PATH").unwrap_or_default();
        assert!(!path.is_empty(), "PATH should not be empty");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_orca_resolution_through_hands() {
        compose_host_path();
        let cwd = std::env::current_dir().unwrap();
        let bridge = build_bridge(cwd).await.expect("bridge should build");

        let args = serde_json::json!({
            "command": "orca --version",
            "description": "check orca version"
        });
        let result = bridge.call("run_terminal_cmd", args, "test-orca-version").await;
        assert!(result.is_ok(), "orca --version should succeed via Hands bridge: {:?}", result);
        let res = result.unwrap();
        assert!(!res.prompt_text.contains("is not recognized"), "orca should be recognized: {}", res.prompt_text);

        let args_status = serde_json::json!({
            "command": "orca status --json",
            "description": "check orca status"
        });
        let result_status = bridge.call("run_terminal_cmd", args_status, "test-orca-status").await;
        assert!(result_status.is_ok(), "orca status --json should succeed via Hands bridge: {:?}", result_status);

        let args_repo = serde_json::json!({
            "command": "orca repo list --json",
            "description": "check orca repo list"
        });
        let result_repo = bridge.call("run_terminal_cmd", args_repo, "test-orca-repo").await;
        assert!(result_repo.is_ok(), "orca repo list --json should succeed via Hands bridge: {:?}", result_repo);
    }

    #[cfg(windows)]
    fn is_process_alive_by_pid(pid: u32) -> bool {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut std::ffi::c_void;
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
            .args(["-NoProfile", "-NonInteractive", "-Command", "Start-Sleep -Seconds 60"])
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
        let bg_res = bridge.call("run_terminal_cmd", bg_args, "test-bg-task").await.expect("bg call should succeed");
        // Extract task_id from result: look for <task-id> XML tag or text pattern
        let task_id = if let Some(start) = bg_res.prompt_text.find("<task-id>") {
            let rest = &bg_res.prompt_text[start + 9..];
            if let Some(end) = rest.find("</task-id>") {
                rest[..end].trim().to_string()
            } else {
                String::new()
            }
        } else {
            bg_res.prompt_text
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

        assert!(!task_id.is_empty(), "should have received a background task ID: {}", bg_res.prompt_text);

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
        let descendant_pid = descendant_pid_opt.expect("descendant PID must be captured deterministically");
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
        let out_res = bridge.call("get_task_output", out_args, "test-get-task-1").await.expect("get_task_output should succeed");
        assert!(out_res.prompt_text.contains("running") || out_res.prompt_text.contains("output") || out_res.prompt_text.contains("Task"), "task should be active");

        // Call real Hands kill_task path
        let kill_args = serde_json::json!({ "task_id": &task_id });
        let kill_res = bridge.call("kill_task", kill_args, "test-kill-task").await.expect("kill_task should succeed");
        assert!(kill_res.prompt_text.contains("killed") || kill_res.prompt_text.contains("cancelled") || kill_res.prompt_text.contains("stopped") || !kill_res.prompt_text.is_empty());

        // Confirm get_task_output reports cancelled/terminal state
        let out_after = bridge.call("get_task_output", serde_json::json!({ "task_id": &task_id }), "test-get-task-2").await.expect("get_task_output after kill should succeed");
        assert!(
            out_after.prompt_text.contains("cancelled") || out_after.prompt_text.contains("completed") || out_after.prompt_text.contains("exit_code") || out_after.prompt_text.contains("killed"),
            "killed task should be in terminal state: {}", out_after.prompt_text
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
}
