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
                let machine_desc = if cfg!(target_os = "macos") {
                    "Mac"
                } else if cfg!(windows) {
                    "PC"
                } else {
                    "machine"
                };
                notify(
                    "Hands",
                    &format!("Tunnel is down. ChatGPT cannot reach this {machine_desc} until it is back."),
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
        let script = format!(
            r#"[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null; $template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); $textNodes = $template.GetElementsByTagName('text'); $textNodes.Item(0).AppendChild($template.CreateTextNode('{}')) > $null; $textNodes.Item(1).AppendChild($template.CreateTextNode('{}')) > $null; $toast = [Windows.UI.Notifications.ToastNotification]::new($template); [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Hands').Show($toast);"#,
            escape_ps(title),
            escape_ps(body)
        );
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
    fn test_notify_does_not_panic() {
        notify("Hands Test", "Testing notification dispatch");
    }
}
