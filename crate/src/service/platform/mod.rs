//! Platform-specific supervisor backends (compile-time dispatch).

#[cfg_attr(
    any(windows, target_os = "macos", target_os = "linux"),
    allow(dead_code, unused_imports)
)]
pub mod fallback;
#[cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
pub mod linux;
#[cfg_attr(not(target_os = "macos"), allow(dead_code, unused_imports))]
pub mod macos;
#[cfg_attr(windows, allow(dead_code))]
mod unix;
#[cfg_attr(not(windows), allow(dead_code, unused_imports))]
pub mod windows;

#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
pub use fallback::*;

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn test_cross_platform_supervisor_names() {
        assert_eq!(windows::supervisor_name(), "Task Scheduler");
        assert_eq!(macos::supervisor_name(), "LaunchAgent");
        assert_eq!(linux::supervisor_name(), "systemd user unit");
        assert_eq!(fallback::supervisor_name(), "supervisor");
    }

    #[test]
    fn test_cross_platform_wrapper_generation() {
        let fake_client = Path::new("/custom/bin/tunnel-client");
        let macos_script = macos::render_wrapper_script(fake_client);
        assert!(macos_script.contains(r#"CLIENT="/custom/bin/tunnel-client""#));
        assert!(macos_script.contains("--profile hands"));
        assert!(macos_script.contains("caffeinate"));

        let linux_script = linux::render_wrapper_script(fake_client);
        assert!(linux_script.contains(r#"CLIENT="/custom/bin/tunnel-client""#));
        assert!(linux_script.contains("--profile hands"));
        assert!(linux_script.contains("systemd-inhibit"));
    }

    #[test]
    fn test_cross_platform_linux_unit_rendering() {
        let fake_wrapper = Path::new("/home/user/.config/hands/run-tunnel.sh");
        let unit = linux::render_service_unit(fake_wrapper);
        assert!(unit.contains("ExecStart=/home/user/.config/hands/run-tunnel.sh"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("RestartSec=2"));

        let fake_hands = Path::new("/home/user/.local/bin/hands");
        let watch_unit = linux::render_watch_unit(fake_hands);
        assert!(watch_unit.contains("ExecStart=/home/user/.local/bin/hands watch"));
        assert!(watch_unit.contains("Restart=always"));
        assert!(watch_unit.contains("RestartSec=10"));
    }

    #[test]
    fn test_cross_platform_macos_plist_rendering() {
        let fake_wrapper = Path::new("/Users/user/.config/hands/run-tunnel.sh");
        let fake_out = Path::new("/Users/user/.config/hands/logs/tunnel.out");
        let fake_err = Path::new("/Users/user/.config/hands/logs/tunnel.err");
        let plist = macos::render_service_plist(fake_wrapper, fake_out, fake_err);
        assert!(plist.contains("<string>dev.hands.tunnel</string>"));
        assert!(plist.contains("<string>/Users/user/.config/hands/run-tunnel.sh</string>"));
        assert!(plist.contains("<key>KeepAlive</key><true/>"));

        let fake_hands = Path::new("/Users/user/.local/bin/hands");
        let fake_watch_out = Path::new("/Users/user/.config/hands/logs/watch.out");
        let fake_watch_err = Path::new("/Users/user/.config/hands/logs/watch.err");
        let watch_plist = macos::render_watch_plist(fake_hands, fake_watch_out, fake_watch_err);
        assert!(watch_plist.contains("<string>dev.hands.watch</string>"));
        assert!(watch_plist.contains("<string>/Users/user/.local/bin/hands</string>"));
        assert!(watch_plist.contains("<string>watch</string>"));
    }
}
