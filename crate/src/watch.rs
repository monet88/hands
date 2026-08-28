//! Notify once when the tunnel drops after having been ready.

use std::thread;
use std::time::{Duration, Instant};

use crate::service;

pub fn run() -> Result<(), String> {
    let mut seen_ready = false;
    let mut notified_at: Option<Instant> = None;
    // Let enable/kickstart finish before the first sample.
    thread::sleep(Duration::from_secs(20));
    loop {
        let ready = service::ready();
        if ready {
            seen_ready = true;
            notified_at = None;
        } else if seen_ready {
            let due = match notified_at {
                None => true,
                Some(t) => t.elapsed() > Duration::from_secs(30 * 60),
            };
            if due {
                let machine_desc = crate::host::PLATFORM_SHORT;
                notify(
                    "Hands",
                    &format!(
                        "Tunnel is down. ChatGPT cannot reach this {machine_desc} until it is back."
                    ),
                );
                notified_at = Some(Instant::now());
            }
        }
        thread::sleep(Duration::from_secs(15));
    }
}

pub fn notify(title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display notification \"{}\" with title \"{}\" sound name \"Basso\"",
            escape_as(body),
            escape_as(title)
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    }
    #[cfg(windows)]
    {
        let script = windows_toast_script(title, body);
        // Notification failure is non-fatal: the tunnel must keep running.
        let _ = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .status();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("notify-send")
            .args([title, body])
            .status();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (title, body);
    }
}

/// Build the Windows PowerShell script used for notifications.
/// Extracted for hermetic testing and to allow an unpackaged fallback.
/// Attempts WinRT toast (requires AUMID registration via Start Menu shortcut
/// `Hands`); on failure falls back to System.Windows.Forms balloon tip which
/// works without registration.
#[cfg(windows)]
fn windows_toast_script(title: &str, body: &str) -> String {
    format!(
        r#"try {{ [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; $template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); $textNodes = $template.GetElementsByTagName('text'); $textNodes.Item(0).AppendChild($template.CreateTextNode('{}')) > $null; $textNodes.Item(1).AppendChild($template.CreateTextNode('{}')) > $null; $toast = [Windows.UI.Notifications.ToastNotification]::new($template); [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Hands').Show($toast); }} catch {{ try {{ Add-Type -AssemblyName System.Windows.Forms -ErrorAction SilentlyContinue > $null; Add-Type -AssemblyName System.Drawing -ErrorAction SilentlyContinue > $null; $ni = New-Object System.Windows.Forms.NotifyIcon; $ni.Icon = [System.Drawing.SystemIcons]::Information; $ni.Visible = $true; $ni.ShowBalloonTip(10000, '{}', '{}', [System.Windows.Forms.ToolTipIcon]::Info); Start-Sleep -Seconds 3; $ni.Dispose(); }} catch {{ }} }}"#,
        escape_ps(title),
        escape_ps(body),
        escape_ps(title),
        escape_ps(body)
    )
}

#[cfg(target_os = "macos")]
fn escape_as(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(windows)]
fn escape_ps(s: &str) -> String {
    s.replace('\'', "''")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(windows)]
    fn test_escape_ps() {
        assert_eq!(escape_ps("Hands's Alert"), "Hands''s Alert");
        assert_eq!(escape_ps("Normal text"), "Normal text");
    }

    #[test]
    #[cfg(windows)]
    fn test_windows_toast_script_escapes_and_has_fallback() {
        let script = windows_toast_script("Hands's Alert", "Tunnel 'down' & needs attention");
        // Single quotes must be doubled for PowerShell CreateTextNode.
        assert!(
            script.contains("Hands''s Alert"),
            "title must be escaped: {script}"
        );
        assert!(
            script.contains("Tunnel ''down''"),
            "body must be escaped: {script}"
        );
        // Must attempt WinRT toast first.
        assert!(
            script.contains("ToastNotificationManager"),
            "should attempt toast: {script}"
        );
        assert!(
            script.contains("CreateToastNotifier('Hands')"),
            "should target Hands AUMID: {script}"
        );
        // Must have unpackaged fallback path.
        assert!(
            script.contains("System.Windows.Forms.NotifyIcon"),
            "should fallback to NotifyIcon: {script}"
        );
        assert!(
            script.contains("ShowBalloonTip"),
            "fallback must show balloon: {script}"
        );
        // Non-fatal: outer try/catch.
        assert!(script.starts_with("try {"), "script must be fault-tolerant");
    }

    #[test]
    fn test_notify_does_not_panic() {
        // Hermetic: on Windows validate script generation/escaping; on all
        // platforms ensure the dispatcher wrapper never panics. Never spawns
        // a real PowerShell/toast during normal test suites.
        #[cfg(windows)]
        {
            let _script = windows_toast_script("Hands Test", "Testing notification dispatch");
        }
        #[cfg(not(windows))]
        {
            notify("Hands Test", "Testing notification dispatch");
        }
    }
}
