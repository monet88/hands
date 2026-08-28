//! Local diagnostics for Hands host configuration, environment, and runtime.

use std::path::Path;
use serde_json::{json, Value};

use crate::host;
use crate::secrets;
use crate::service;

#[derive(Debug, Clone)]
pub struct CheckItem {
    pub name: String,
    pub status: String, // "ok", "warn", "fail", "info"
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct WorkspaceReport {
    pub path: String,
    pub pinned: bool,
    pub pin: Option<String>,
    pub exists: bool,
    pub is_dir: bool,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct TunnelClientReport {
    pub found: bool,
    pub path: Option<String>,
    pub hint: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct ConfigurationReport {
    pub config_dir: String,
    pub has_key: bool,
    pub key_source: Option<String>,
    pub has_tunnel_id: bool,
    pub tunnel_id: Option<String>,
    pub profile_exists: bool,
    pub profile_path: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeReport {
    pub service_installed: bool,
    pub service_status: String,
    pub supervisor_name: String,
    pub probe_ready: bool,
    pub health_url: String,
    pub admin_url: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub name: String,
    pub version: String,
    pub source_git_sha: String,
    pub platform: String,
    pub platform_short: String,
    pub ok: bool,
    pub summary: String,
    pub workspace: WorkspaceReport,
    pub tunnel_client: TunnelClientReport,
    pub configuration: ConfigurationReport,
    pub runtime: RuntimeReport,
    pub checks: Vec<CheckItem>,
}

impl DoctorReport {
    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "version": self.version,
            "source_git_sha": self.source_git_sha,
            "platform": self.platform,
            "platform_short": self.platform_short,
            "ok": self.ok,
            "summary": self.summary,
            "workspace": {
                "path": self.workspace.path,
                "pinned": self.workspace.pinned,
                "pin": self.workspace.pin,
                "exists": self.workspace.exists,
                "is_dir": self.workspace.is_dir,
                "status": self.workspace.status,
            },
            "tunnel_client": {
                "found": self.tunnel_client.found,
                "path": self.tunnel_client.path,
                "hint": self.tunnel_client.hint,
                "status": self.tunnel_client.status,
            },
            "configuration": {
                "config_dir": self.configuration.config_dir,
                "has_key": self.configuration.has_key,
                "key_source": self.configuration.key_source,
                "has_tunnel_id": self.configuration.has_tunnel_id,
                "tunnel_id": self.configuration.tunnel_id,
                "profile_exists": self.configuration.profile_exists,
                "profile_path": self.configuration.profile_path,
                "status": self.configuration.status,
            },
            "runtime": {
                "service_installed": self.runtime.service_installed,
                "service_status": self.runtime.service_status,
                "supervisor_name": self.runtime.supervisor_name,
                "probe_ready": self.runtime.probe_ready,
                "health_url": self.runtime.health_url,
                "admin_url": self.runtime.admin_url,
                "status": self.runtime.status,
            },
            "checks": self.checks.iter().map(|c| {
                json!({
                    "name": c.name,
                    "status": c.status,
                    "message": c.message,
                })
            }).collect::<Vec<_>>(),
        })
    }

    pub fn render_human(&self) -> String {
        let mut out = String::new();
        out.push_str("Hands Doctor — Local Diagnostics\n\n");

        out.push_str("Hands:\n");
        out.push_str(&format!("  version       {} (sha: {})\n", self.version, self.source_git_sha));
        out.push_str(&format!("  platform      {} ({})\n\n", self.platform_short, self.platform));

        out.push_str("Workspace:\n");
        let ws_mark = if self.workspace.exists && self.workspace.is_dir { "ok" } else { "FAIL" };
        out.push_str(&format!("  [{}] path     {}\n", ws_mark, self.workspace.path));
        out.push_str(&format!(
            "  [{}] pin      {}\n",
            if self.workspace.pinned { "ok" } else { "·" },
            self.workspace.pin.as_deref().unwrap_or("(none — using cwd/env)")
        ));
        let ws_state = if !self.workspace.exists {
            "path does not exist"
        } else if !self.workspace.is_dir {
            "path is not a directory"
        } else {
            "directory exists"
        };
        out.push_str(&format!("  [{}] state    {}\n\n", ws_mark, ws_state));

        out.push_str("Tunnel Client:\n");
        let tc_mark = if self.tunnel_client.found { "ok" } else { "WARN" };
        if self.tunnel_client.found {
            out.push_str(&format!(
                "  [{}] binary   {}\n\n",
                tc_mark,
                self.tunnel_client.path.as_deref().unwrap_or("found")
            ));
        } else {
            out.push_str(&format!(
                "  [{}] binary   {}\n\n",
                tc_mark,
                self.tunnel_client.hint.as_deref().unwrap_or("missing")
            ));
        }

        out.push_str("Configuration:\n");
        out.push_str(&format!("  [ok] config   {}\n", self.configuration.config_dir));
        let key_mark = if self.configuration.has_key { "ok" } else { "WARN" };
        let key_desc = if self.configuration.has_key {
            format!("present ({})", self.configuration.key_source.as_deref().unwrap_or("saved"))
        } else {
            "missing (run 'hands setup' or set CONTROL_PLANE_API_KEY)".into()
        };
        out.push_str(&format!("  [{}] key      {}\n", key_mark, key_desc));

        let id_mark = if self.configuration.has_tunnel_id { "ok" } else { "WARN" };
        let id_desc = if let Some(id) = &self.configuration.tunnel_id {
            id.clone()
        } else {
            "missing (run 'hands setup' or set CONTROL_PLANE_TUNNEL_ID)".into()
        };
        out.push_str(&format!("  [{}] tunnel   {}\n", id_mark, id_desc));

        let prof_mark = if self.configuration.profile_exists { "ok" } else { "·" };
        let prof_desc = if self.configuration.profile_exists {
            self.configuration.profile_path.clone()
        } else {
            format!("not generated yet ({})", self.configuration.profile_path)
        };
        out.push_str(&format!("  [{}] profile  {}\n\n", prof_mark, prof_desc));

        out.push_str("Supervisor & Runtime:\n");
        let svc_mark = if self.runtime.service_installed { "ok" } else { "·" };
        out.push_str(&format!(
            "  [{}] service  {} ({})\n",
            svc_mark, self.runtime.service_status, self.runtime.supervisor_name
        ));

        let probe_mark = if self.runtime.probe_ready { "ok" } else { "·" };
        let probe_desc = if self.runtime.probe_ready {
            format!("ready ({})", self.runtime.health_url)
        } else {
            format!("down ({})", self.runtime.health_url)
        };
        out.push_str(&format!("  [{}] probe    {}\n", probe_mark, probe_desc));
        out.push_str(&format!("  [{}] admin    {}\n\n", probe_mark, self.runtime.admin_url));

        out.push_str(&format!("Summary: {}\n", self.summary));
        out
    }
}

pub fn diagnose(workspace: &Path) -> DoctorReport {
    host::migrate_from_legacy();

    // 1. Hands metadata
    let name = host::DISPLAY.to_string();
    let version = env!("CARGO_PKG_VERSION").to_string();
    let source_git_sha = crate::build_provenance::SOURCE_GIT_SHA.to_string();
    let platform = std::env::consts::OS.to_string();
    let platform_short = host::PLATFORM_SHORT.to_string();

    // 2. Workspace
    let pin = host::read_pinned_workspace().map(|p| p.display().to_string());
    let pinned = pin.is_some();
    let ws_exists = workspace.exists();
    let ws_is_dir = workspace.is_dir();
    let ws_status = if !ws_exists {
        "missing".to_string()
    } else if !ws_is_dir {
        "not_a_directory".to_string()
    } else {
        "ok".to_string()
    };
    let workspace_report = WorkspaceReport {
        path: workspace.display().to_string(),
        pinned,
        pin,
        exists: ws_exists,
        is_dir: ws_is_dir,
        status: ws_status,
    };

    // 3. Tunnel client
    let tc_res = service::tunnel_client_bin();
    let tc_found = tc_res.is_ok();
    let tc_path = tc_res.as_ref().ok().map(|p| p.display().to_string());
    let tc_hint = if tc_found {
        None
    } else {
        Some(host::TUNNEL_CLIENT_HINT.to_string())
    };
    let tc_status = if tc_found { "ok".to_string() } else { "missing".to_string() };
    let tunnel_client_report = TunnelClientReport {
        found: tc_found,
        path: tc_path,
        hint: tc_hint,
        status: tc_status,
    };

    // 4. Configuration
    let config_dir = host::config_dir().display().to_string();
    let has_key = service::has_key();
    let key_source = secrets::source().map(|s| s.to_string());
    let tunnel_id = service::tunnel_id_opt();
    let has_tunnel_id = tunnel_id.is_some();
    let profile_file = service::profile_file();
    let profile_exists = profile_file.is_file();
    let profile_path = profile_file.display().to_string();
    let cfg_status = if has_key && has_tunnel_id && profile_exists {
        "ok".to_string()
    } else if has_key && has_tunnel_id {
        "configured".to_string()
    } else {
        "incomplete".to_string()
    };
    let configuration_report = ConfigurationReport {
        config_dir,
        has_key,
        key_source,
        has_tunnel_id,
        tunnel_id,
        profile_exists,
        profile_path,
        status: cfg_status,
    };

    // 5. Runtime / Supervisor
    let service_installed = service::installed();
    let service_status = if service_installed { "enabled".to_string() } else { "off".to_string() };
    let supervisor_name = service::supervisor_name().to_string();

    let probe_ready = service::ready();
    let health_url = format!("{}/readyz", service::HEALTH_BASE);
    let admin_url = format!("{}/ui", service::HEALTH_BASE);
    let runtime_status = if probe_ready {
        "ready".to_string()
    } else if service_installed {
        "stopped".to_string()
    } else {
        "off".to_string()
    };
    let runtime_report = RuntimeReport {
        service_installed,
        service_status,
        supervisor_name,
        probe_ready,
        health_url: health_url.clone(),
        admin_url,
        status: runtime_status,
    };

    // 6. Detailed checks
    let mut checks = Vec::new();

    // Check workspace
    if ws_exists && ws_is_dir {
        checks.push(CheckItem {
            name: "workspace".into(),
            status: "ok".into(),
            message: format!("workspace directory exists ({})", workspace.display()),
        });
    } else if !ws_exists {
        checks.push(CheckItem {
            name: "workspace".into(),
            status: "fail".into(),
            message: format!("workspace path does not exist: {}", workspace.display()),
        });
    } else {
        checks.push(CheckItem {
            name: "workspace".into(),
            status: "fail".into(),
            message: format!("workspace path is not a directory: {}", workspace.display()),
        });
    }

    // Check tunnel-client
    if tc_found {
        checks.push(CheckItem {
            name: "tunnel_client".into(),
            status: "ok".into(),
            message: format!("tunnel-client binary found at {}", tunnel_client_report.path.as_deref().unwrap_or("")),
        });
    } else {
        checks.push(CheckItem {
            name: "tunnel_client".into(),
            status: "warn".into(),
            message: host::TUNNEL_CLIENT_HINT.to_string(),
        });
    }

    // Check runtime key
    if has_key {
        checks.push(CheckItem {
            name: "runtime_key".into(),
            status: "ok".into(),
            message: format!("Runtime API Key present in {}", configuration_report.key_source.as_deref().unwrap_or("storage")),
        });
    } else {
        checks.push(CheckItem {
            name: "runtime_key".into(),
            status: "warn".into(),
            message: "Runtime API Key missing (run 'hands setup' or export CONTROL_PLANE_API_KEY)".into(),
        });
    }

    // Check tunnel ID
    if let Some(id) = &configuration_report.tunnel_id {
        checks.push(CheckItem {
            name: "tunnel_id".into(),
            status: "ok".into(),
            message: format!("Tunnel ID configured ({id})"),
        });
    } else {
        checks.push(CheckItem {
            name: "tunnel_id".into(),
            status: "warn".into(),
            message: "Tunnel ID missing (run 'hands setup' or export CONTROL_PLANE_TUNNEL_ID)".into(),
        });
    }

    // Check supervisor
    if service_installed {
        checks.push(CheckItem {
            name: "service".into(),
            status: "ok".into(),
            message: format!("{} auto-start enabled", runtime_report.supervisor_name),
        });
    } else {
        checks.push(CheckItem {
            name: "service".into(),
            status: "info".into(),
            message: "Supervisor service not registered (run 'hands enable')".into(),
        });
    }

    // Check local health probe
    if probe_ready {
        checks.push(CheckItem {
            name: "local_probe".into(),
            status: "ok".into(),
            message: format!("Local health probe ready at {health_url}"),
        });
    } else {
        checks.push(CheckItem {
            name: "local_probe".into(),
            status: "info".into(),
            message: format!("Local health probe inactive at {health_url}"),
        });
    }

    // Overall OK condition
    let ok = ws_exists && ws_is_dir && tc_found && has_key && has_tunnel_id;
    let summary = if ok && probe_ready {
        "all checks passed, tunnel active".to_string()
    } else if ok {
        "configuration ready, tunnel service not running".to_string()
    } else {
        "action required: configure missing components".to_string()
    };

    DoctorReport {
        name,
        version,
        source_git_sha,
        platform,
        platform_short,
        ok,
        summary,
        workspace: workspace_report,
        tunnel_client: tunnel_client_report,
        configuration: configuration_report,
        runtime: runtime_report,
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn test_doctor_report_json_schema_and_secrecy() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!("hands_doc_test_1_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let ws_dir = temp_dir.join("workspace");
        fs::create_dir_all(&ws_dir).expect("create ws");

        let cfg_dir = temp_dir.join("config");
        fs::create_dir_all(&cfg_dir).expect("create cfg");

        let secret_key = "sk-test-secret-key-123456789012345678901234567890";
        unsafe {
            std::env::set_var("HANDS_CONFIG_DIR", cfg_dir.to_str().unwrap());
            std::env::set_var("HANDS_TEST_CRED_NAMESPACE", "1");
            std::env::set_var("CONTROL_PLANE_API_KEY", secret_key);
            std::env::set_var("CONTROL_PLANE_TUNNEL_ID", "tunnel_test123");
        }

        let report = diagnose(&ws_dir);
        let json = report.to_json();
        let human = report.render_human();

        assert_eq!(json["name"], host::DISPLAY);
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["workspace"]["exists"], true);
        assert_eq!(json["workspace"]["is_dir"], true);
        assert_eq!(json["configuration"]["has_key"], true);
        assert_eq!(json["configuration"]["has_tunnel_id"], true);
        assert_eq!(json["configuration"]["tunnel_id"], "tunnel_test123");

        // CRITICAL SECRECY ASSERTIONS: The secret key must NEVER appear in JSON or human text.
        let json_str = json.to_string();
        assert!(
            !json_str.contains(secret_key),
            "Secret key leaked in JSON: {json_str}"
        );
        assert!(
            !human.contains(secret_key),
            "Secret key leaked in human report: {human}"
        );

        unsafe {
            std::env::remove_var("HANDS_CONFIG_DIR");
            std::env::remove_var("HANDS_TEST_CRED_NAMESPACE");
            std::env::remove_var("CONTROL_PLANE_API_KEY");
            std::env::remove_var("CONTROL_PLANE_TUNNEL_ID");
        }
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_doctor_missing_workspace() {
        let _guard = TEST_LOCK.lock().unwrap();
        let nonexistent = Path::new("Z:/path/that/does/not/exist/for/sure/hands_test");
        let report = diagnose(nonexistent);
        let json = report.to_json();

        assert_eq!(report.workspace.exists, false);
        assert_eq!(report.workspace.status, "missing");
        assert_eq!(report.ok, false);
        assert_eq!(json["workspace"]["exists"], false);
        assert_eq!(json["ok"], false);

        // Even with missing workspace, all other check items are populated.
        assert!(!report.checks.is_empty());
        let human = report.render_human();
        assert!(human.contains("Workspace:"));
        assert!(human.contains("path does not exist"));
    }

    #[test]
    fn test_doctor_missing_components() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!("hands_doc_test_2_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let ws_dir = temp_dir.join("workspace");
        fs::create_dir_all(&ws_dir).expect("create ws");

        // Set HANDS_CONFIG_DIR to empty temp dir so no keys or configs are found
        let cfg_dir = temp_dir.join("hands_cfg");
        fs::create_dir_all(&cfg_dir).expect("create cfg");
        unsafe {
            std::env::set_var("HANDS_CONFIG_DIR", cfg_dir.to_str().unwrap());
            std::env::set_var("HANDS_TEST_CRED_NAMESPACE", "1");
            std::env::remove_var("CONTROL_PLANE_API_KEY");
            std::env::remove_var("CONTROL_PLANE_TUNNEL_ID");
        }

        let report = diagnose(&ws_dir);
        let json = report.to_json();

        assert_eq!(report.configuration.has_key, false);
        assert_eq!(json["configuration"]["has_key"], false);

        let human = report.render_human();
        assert!(human.contains("Hands Doctor"));
        assert!(human.contains("Configuration:"));

        unsafe {
            std::env::remove_var("HANDS_CONFIG_DIR");
            std::env::remove_var("HANDS_TEST_CRED_NAMESPACE");
        }
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_doctor_pinned_workspace() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!("hands_doc_test_3_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let ws_dir = temp_dir.join("workspace");
        fs::create_dir_all(&ws_dir).expect("create ws");

        let cfg_dir = temp_dir.join("hands_cfg");
        fs::create_dir_all(&cfg_dir).expect("create cfg");
        unsafe {
            std::env::set_var("HANDS_CONFIG_DIR", cfg_dir.to_str().unwrap());
            std::env::set_var("HANDS_TEST_CRED_NAMESPACE", "1");
        }

        // Pin workspace
        let pinned_path = host::pin_workspace(&ws_dir).expect("pin ws");
        let report = diagnose(&pinned_path);
        let json = report.to_json();

        assert_eq!(report.workspace.pinned, true);
        assert!(report.workspace.pin.is_some());
        assert_eq!(json["workspace"]["pinned"], true);

        unsafe {
            std::env::remove_var("HANDS_CONFIG_DIR");
            std::env::remove_var("HANDS_TEST_CRED_NAMESPACE");
        }
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_doctor_not_a_directory_workspace() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!("hands_doc_test_4_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("create temp");
        let file_path = temp_dir.join("file.txt");
        fs::write(&file_path, "hello").expect("write file");

        let report = diagnose(&file_path);
        let json = report.to_json();

        assert_eq!(report.workspace.exists, true);
        assert_eq!(report.workspace.is_dir, false);
        assert_eq!(report.workspace.status, "not_a_directory");
        assert_eq!(report.ok, false);
        assert_eq!(json["workspace"]["is_dir"], false);
        assert_eq!(json["workspace"]["status"], "not_a_directory");

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_doctor_partial_failure_independence() {
        let _guard = TEST_LOCK.lock().unwrap();
        let temp_dir = std::env::temp_dir().join(format!("hands_doc_test_5_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let ws_dir = temp_dir.join("workspace");
        fs::create_dir_all(&ws_dir).expect("create ws");

        let cfg_dir = temp_dir.join("hands_cfg");
        fs::create_dir_all(&cfg_dir).expect("create cfg");
        // Write tunnel_id file but no key
        fs::write(cfg_dir.join("tunnel_id"), "tunnel_partial_test123\n").expect("write id");

        unsafe {
            std::env::set_var("HANDS_CONFIG_DIR", cfg_dir.to_str().unwrap());
            std::env::set_var("HANDS_TEST_CRED_NAMESPACE", "1");
            std::env::remove_var("CONTROL_PLANE_API_KEY");
            std::env::remove_var("CONTROL_PLANE_TUNNEL_ID");
        }

        let report = diagnose(&ws_dir);
        let json = report.to_json();

        // Workspace passes
        assert_eq!(report.workspace.exists, true);
        assert_eq!(report.workspace.status, "ok");

        // Key is missing, but Tunnel ID is found
        assert_eq!(report.configuration.has_key, false);
        assert_eq!(report.configuration.has_tunnel_id, true);
        assert_eq!(report.configuration.tunnel_id, Some("tunnel_partial_test123".to_string()));

        // All checks are populated
        assert!(report.checks.iter().any(|c| c.name == "workspace" && c.status == "ok"));
        assert!(report.checks.iter().any(|c| c.name == "runtime_key" && c.status == "warn"));
        assert!(report.checks.iter().any(|c| c.name == "tunnel_id" && c.status == "ok"));

        assert_eq!(json["configuration"]["has_key"], false);
        assert_eq!(json["configuration"]["has_tunnel_id"], true);

        unsafe {
            std::env::remove_var("HANDS_CONFIG_DIR");
            std::env::remove_var("HANDS_TEST_CRED_NAMESPACE");
        }
        let _ = fs::remove_dir_all(&temp_dir);
    }
}
