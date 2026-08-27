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
                notify(
                    "Hands",
                    "Tunnel is down. ChatGPT cannot reach this Mac until it is back.",
                );
                notified_at = Some(Instant::now());
            }
        }
        thread::sleep(Duration::from_secs(15));
    }
}

fn notify(title: &str, body: &str) {
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
    #[cfg(not(target_os = "macos"))]
    {
        let _ = std::process::Command::new("notify-send")
            .args([title, body])
            .status();
    }
}

#[cfg(target_os = "macos")]
fn escape_as(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}
