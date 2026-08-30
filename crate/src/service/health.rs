//! Tunnel readiness and health probing logic.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const HEALTH_LISTEN: &str = "127.0.0.1:18780";
pub const HEALTH_BASE: &str = "http://127.0.0.1:18780";

pub fn ready() -> bool {
    response_is_ready(ureq_get(&format!("{HEALTH_BASE}/readyz")))
}

fn response_is_ready(response: Result<String, ()>) -> bool {
    response.is_ok_and(|body| body == "ready")
}

pub fn wait_ready(timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    ready()
}

pub fn ureq_get(url: &str) -> Result<String, ()> {
    let mut child = Command::new("curl")
        .args(["-fsS", "--max-time", "1", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let mut buf = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut buf);
    }
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    if ok {
        Ok(buf.trim().to_string())
    } else {
        Err(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_health_constants() {
        assert_eq!(HEALTH_LISTEN, "127.0.0.1:18780");
        assert_eq!(HEALTH_BASE, "http://127.0.0.1:18780");
    }

    #[test]
    fn test_ureq_get_invalid_url_returns_err() {
        let res = ureq_get("http://127.0.0.1:9");
        assert!(res.is_err());
    }

    #[test]
    fn test_health_result_handling_requires_exact_ready_body() {
        assert!(response_is_ready(Ok("ready".into())));
        assert!(!response_is_ready(Ok("not ready".into())));
        assert!(!response_is_ready(Err(())));
    }
}
