//! Windows Task Scheduler supervisor backend.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::host;

pub use super::unix::write_wrapper;

pub const TASK_NAME: &str = "dev.hands.tunnel";
pub const WATCH_TASK_NAME: &str = "dev.hands.watch";

pub fn supervisor_name() -> &'static str {
    "Task Scheduler"
}

pub fn installed() -> bool {
    let script = format!(
        "if (Get-ScheduledTask -TaskName '{}' -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}",
        ps_single_quote(TASK_NAME)
    );
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
    out.map(|o| o.status.success()).unwrap_or(false)
}

pub fn ps_single_quote(s: &str) -> String {
    s.replace('\'', "''")
}

pub fn scheduled_task_registration_script(
    task_name: &str,
    exec_path: &Path,
    args: &str,
    description: &str,
) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$user = [System.Security.Principal.WindowsIdentity]::GetCurrent().Name
$action = New-ScheduledTaskAction -Execute '{}' -Argument '{}'
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
$principal = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew -Priority 7 -RestartCount 999 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName '{}' -Action $action -Trigger $trigger -Principal $principal -Settings $settings -Description '{}' -Force | Out-Null
"#,
        ps_single_quote(&exec_path.display().to_string()),
        ps_single_quote(args),
        ps_single_quote(task_name),
        ps_single_quote(description),
    )
}

pub fn run_windows_powershell(script: &str, label: &str) -> Result<(), String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("{label}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let detail = if !stderr.is_empty() { stderr } else { stdout };
        Err(format!("{label} failed: {detail}"))
    }
}

pub fn register_windows_task(
    task_name: &str,
    exec_path: &Path,
    args: &str,
    description: &str,
) -> Result<(), String> {
    let script = scheduled_task_registration_script(task_name, exec_path, args, description);
    run_windows_powershell(&script, "Register-ScheduledTask")
}

pub fn start_windows_task(task_name: &str) -> Result<(), String> {
    let script = format!(
        "Start-ScheduledTask -TaskName '{}' -ErrorAction Stop",
        ps_single_quote(task_name)
    );
    run_windows_powershell(&script, "Start-ScheduledTask")
}

pub fn stop_windows_task(task_name: &str) {
    let script = format!(
        "Stop-ScheduledTask -TaskName '{}' -ErrorAction SilentlyContinue",
        ps_single_quote(task_name)
    );
    let _ = run_windows_powershell(&script, "Stop-ScheduledTask");
}

pub fn unregister_windows_task(task_name: &str) {
    let script = format!(
        "Unregister-ScheduledTask -TaskName '{}' -Confirm:$false -ErrorAction SilentlyContinue",
        ps_single_quote(task_name)
    );
    let _ = run_windows_powershell(&script, "Unregister-ScheduledTask");
}

pub fn remove_stale_task_xml(stem: &str) {
    let path = host::config_dir().join(format!("{stem}.xml"));
    if path.is_file() {
        let _ = fs::remove_file(path);
    }
}

pub fn install_supervisor() -> Result<(), String> {
    let hands = super::super::harness_bin()?;

    // A previous scheduled `hands run-tunnel` may be inside its own retry
    // loop. End the owning Task Scheduler instance before replacing the task
    // definition, then clean up only its recorded tunnel-client tree.
    stop_supervisor()?;
    register_windows_task(
        TASK_NAME,
        &hands,
        "run-tunnel",
        "Hands ChatGPT tunnel supervisor",
    )?;
    remove_stale_task_xml("tunnel-task");

    start_supervisor()?;
    Ok(())
}

pub fn start_supervisor() -> Result<(), String> {
    start_windows_task(TASK_NAME)
}

pub fn stop_supervisor() -> Result<(), String> {
    stop_windows_task(TASK_NAME);
    stop_unmanaged();
    Ok(())
}

pub fn uninstall_supervisor() -> Result<(), String> {
    stop_supervisor()?;
    let _ = uninstall_watch();
    unregister_windows_task(TASK_NAME);
    remove_stale_task_xml("tunnel-task");
    Ok(())
}

pub fn install_watch() -> Result<(), String> {
    let hands = super::super::harness_bin()?;
    stop_windows_task(WATCH_TASK_NAME);
    register_windows_task(
        WATCH_TASK_NAME,
        &hands,
        "watch",
        "Hands tunnel down notifier",
    )?;
    remove_stale_task_xml("watch-task");
    start_windows_task(WATCH_TASK_NAME)
}

pub fn uninstall_watch() -> Result<(), String> {
    stop_windows_task(WATCH_TASK_NAME);
    unregister_windows_task(WATCH_TASK_NAME);
    remove_stale_task_xml("watch-task");
    Ok(())
}

/// PID/state file for the Hands-owned tunnel-client tree. Written by the
/// supervisor path (`run_tunnel_daemon`) before spawn; `stop_unmanaged` only
/// ever stops PIDs proven by this file (live check + name + command line +
/// creation time to defeat PID recycling).
pub fn tunnel_pid_file() -> PathBuf {
    host::config_dir().join("tunnel-pid.json")
}

pub fn query_process_creation(pid: u32) -> Option<String> {
    #[cfg(not(windows))]
    {
        let _ = pid;
        return None;
    }

    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(
                dwDesiredAccess: u32,
                bInheritHandle: i32,
                dwProcessId: u32,
            ) -> *mut std::ffi::c_void;
            fn GetProcessTimes(
                hProcess: *mut std::ffi::c_void,
                lpCreationTime: *mut u64,
                lpExitTime: *mut u64,
                lpKernelTime: *mut u64,
                lpUserTime: *mut u64,
            ) -> i32;
            fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
        }
        const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
        // FILETIME epoch is 1601-01-01; .NET DateTime.Ticks epoch is 0001-01-01.
        // The PowerShell comparator in `stop_unmanaged` uses
        // `$p.CreationDate.ToUniversalTime().Ticks` (.NET ticks), so this writer
        // must produce the same value.
        const FILETIME_TO_DOTNET_TICKS: u64 = 504_911_232_000_000_000;

        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let mut creation = 0u64;
        let mut exit = 0u64;
        let mut kernel = 0u64;
        let mut user = 0u64;
        let ok =
            unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        unsafe { CloseHandle(handle) };
        if ok == 0 {
            return None;
        }
        Some((creation + FILETIME_TO_DOTNET_TICKS).to_string())
    }
}

pub fn write_tunnel_pid(pid: u32) -> Result<(), String> {
    let path = tunnel_pid_file();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let creation = query_process_creation(pid)
        .ok_or_else(|| format!("cannot prove process creation token for PID {pid}"))?;
    let json = format!(
        r#"{{"pid":{pid},"creation":"{creation}","profile":"{}"}}"#,
        super::super::PROFILE
    );
    fs::write(&path, json).map_err(|e| format!("write {}: {e}", path.display()))
}

pub fn read_tunnel_pid() -> Option<(u32, String)> {
    let raw = fs::read_to_string(tunnel_pid_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v.get("profile")?.as_str()? != super::super::PROFILE {
        return None;
    }
    let pid = v.get("pid")?.as_u64()? as u32;
    let creation = v.get("creation")?.as_str()?.to_string();
    if creation.is_empty() || !creation.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((pid, creation))
}

/// Stop only the Hands-owned tunnel tree: the PID recorded by our supervisor
/// (validated against executable name + managed profile marker + creation
/// time), plus any child processes recursively. Unrelated `tunnel-client.exe`
/// processes are never touched. Cleans stale PID records.
pub fn stop_unmanaged() {
    let Some((pid, stored_creation)) = read_tunnel_pid() else {
        let _ = fs::remove_file(tunnel_pid_file());
        return;
    };
    // Validate ownership before killing; use creation time to defeat PID recycling.
    if let Some(live) = query_process_creation(pid) {
        if live.trim() != stored_creation.trim() {
            let _ = fs::remove_file(tunnel_pid_file());
            return;
        }
    } else {
        // Process gone: clean stale record.
        let _ = fs::remove_file(tunnel_pid_file());
        return;
    }
    let pid_file_esc = tunnel_pid_file().display().to_string().replace('\'', "''");
    let script = format!(
        r#"
$owned = {pid}
if (-not $owned) {{ exit 0 }}
$p = Get-CimInstance Win32_Process -Filter "ProcessId=$owned" -ErrorAction SilentlyContinue
if (-not $p) {{
    Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
    exit 0
}}
$isOurs = ($p.Name -ieq 'tunnel-client.exe') -and ($p.CommandLine -match '--profile hands')
if (-not $isOurs) {{
    # PID was recycled or state file is stale: do not kill an unknown owner.
    Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
    exit 0
}}
# Additional creation-time check from PowerShell when Rust-stored creation exists
$storedCreation = '{stored_creation_esc}'
if ($storedCreation) {{
    $liveCreation = $p.CreationDate.ToUniversalTime().Ticks
    if ($liveCreation -and "$liveCreation" -ne $storedCreation) {{
        Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
        exit 0
    }}
}}
# Terminate the complete owned descendant tree recursively (handles grandchildren)
try {{ taskkill.exe /PID $owned /T /F | Out-Null }} catch {{}}
try {{ Stop-Process -Id $owned -Force -ErrorAction SilentlyContinue }} catch {{}}
Remove-Item -Path '{pid_file_esc}' -Force -ErrorAction SilentlyContinue
"#,
        pid = pid,
        pid_file_esc = pid_file_esc,
        stored_creation_esc = stored_creation.replace('\'', "''")
    );
    let _ = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    std::thread::sleep(Duration::from_millis(300));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn test_process_creation_token_is_numeric_and_stable() {
        let pid = std::process::id();
        let first = query_process_creation(pid).expect("current process creation token");
        let second = query_process_creation(pid).expect("current process creation token again");
        assert!(first.chars().all(|c| c.is_ascii_digit()));
        assert_eq!(first, second);
    }

    #[test]
    fn test_task_registration_script_is_user_scoped_and_bounded() {
        let exe = PathBuf::from(r"C:\Program Files\Hands\hands.exe");
        let script =
            scheduled_task_registration_script(TASK_NAME, &exe, "run-tunnel", "Hands test task");
        assert!(script.contains("New-ScheduledTaskTrigger -AtLogOn -User $user"));
        assert!(script.contains("-LogonType Interactive -RunLevel Limited"));
        assert!(script.contains("-MultipleInstances IgnoreNew"));
        assert!(script.contains("-RestartCount 999"));
        assert!(script.contains("-RestartInterval (New-TimeSpan -Minutes 1)"));
        assert!(script.contains("-ExecutionTimeLimit ([TimeSpan]::Zero)"));
        assert!(script.contains("-Argument 'run-tunnel'"));
        assert!(script.contains(r"C:\Program Files\Hands\hands.exe"));
        assert!(!script.contains("sk-"));
    }

    #[test]
    fn test_write_wrapper_preserves_windows_artifacts() {
        let (_env_lock, env) = crate::testenv::isolate_env("service_windows_logs");
        write_wrapper(Path::new("tunnel-client.exe")).expect("write wrapper artifacts");
        assert!(env.root.join("logs").is_dir());
        let wrapper = env.root.join("run-tunnel.sh");
        assert!(wrapper.is_file());
        let body = fs::read_to_string(wrapper).expect("read wrapper");
        assert!(body.contains("--profile hands"));
    }

    #[test]
    #[cfg(windows)]
    fn test_stop_unmanaged_preserves_unrelated_process() {
        let (_env_lock, _env) = crate::testenv::isolate_env("service_windows_ownership");
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unrelated child");
        let pid = child.id();
        write_tunnel_pid(pid).expect("record child creation token");

        stop_unmanaged();

        assert!(
            child.try_wait().expect("query unrelated child").is_none(),
            "stop_unmanaged must not kill a non-tunnel-client process even when its PID record is current"
        );
        let _ = child.kill();
        let _ = child.wait();
    }
}
