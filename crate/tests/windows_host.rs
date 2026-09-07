mod common;
use common::TestHarness;
use hands::host;
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;
/// Locate the hands repo root (works in both standalone hands and injected grok-build).
fn hands_repo_root() -> std::path::PathBuf {
    if let Ok(repo) = std::env::var("HANDS_REPO") {
        let p = std::path::PathBuf::from(repo);
        if p.join("scripts").join("package_windows_bundle.py").is_file() {
            return p;
        }
    }
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut cur = dunce::canonicalize(&manifest).unwrap_or(manifest);
    loop {
        if cur
            .join("scripts")
            .join("package_windows_bundle.py")
            .is_file()
        {
            return cur;
        }
        if let Some(parent) = cur.parent() {
            let sibling = parent.join("hands");
            if sibling
                .join("scripts")
                .join("package_windows_bundle.py")
                .is_file()
            {
                return sibling;
            }
            cur = parent.to_path_buf();
        } else {
            break;
        }
    }
    panic!("failed to locate hands repo root with scripts/package_windows_bundle.py");
}

fn resolve_python_cmd() -> &'static str {
    if std::process::Command::new("python3").arg("--version").output().is_ok() {
        "python3"
    } else if std::process::Command::new("python").arg("--version").output().is_ok() {
        "python"
    } else {
        "py"
    }
}

fn write_dummy_pe(path: &std::path::Path, dlls: &[&str]) {
    let py_cmd = resolve_python_cmd();
    let dlls_repr = format!("{:?}", dlls);
    let code = format!(
        "import sys, pathlib; sys.path.insert(0, '.'); from scripts.package_windows_bundle import make_dummy_pe; pathlib.Path(sys.argv[1]).write_bytes(make_dummy_pe({dlls_repr}))"
    );
    let status = std::process::Command::new(py_cmd)
        .arg("-B")
        .arg("-c")
        .arg(code)
        .arg(path)
        .current_dir(hands_repo_root())
        .status()
        .expect("write dummy pe");
    assert!(status.success(), "write_dummy_pe failed");
}

/// Locate the outer or sibling grok-build reference repository for offline patch regression tests.
fn grok_build_reference_source() -> Option<std::path::PathBuf> {
    // 1. If running inside injected grok-build, walk up from CARGO_MANIFEST_DIR
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let cur = dunce::canonicalize(&manifest).unwrap_or(manifest);
    let mut p = cur.as_path();
    while let Some(parent) = p.parent() {
        if parent
            .join("crates")
            .join("codegen")
            .join("xai-grok-tools")
            .is_dir()
            && parent.join(".git").exists()
        {
            return Some(parent.to_path_buf());
        }
        p = parent;
    }

    // 2. If running in standalone hands repo, check .grok-build or sibling checkouts
    let hands_root = hands_repo_root();
    let embedded = hands_root.join(".grok-build");
    if embedded
        .join("crates")
        .join("codegen")
        .join("xai-grok-tools")
        .is_dir()
        && embedded.join(".git").exists()
    {
        return Some(embedded);
    }

    if let Some(parent) = hands_root.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut candidates = Vec::new();
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("grok-build")
                        && path
                            .join("crates")
                            .join("codegen")
                            .join("xai-grok-tools")
                            .is_dir()
                        && path.join(".git").exists()
                    {
                        candidates.push(path);
                    }
                }
            }
            candidates.sort();
            if let Some(first) = candidates.into_iter().next() {
                return Some(first);
            }
        }
    }

    if let Ok(dir) = std::env::var("GROK_BUILD_SOURCE_DIR") {
        let p = std::path::PathBuf::from(dir);
        if p.join(".git").exists() {
            return Some(p);
        }
    }

    None
}

/// Panic-safe RG_BIN_PATH override: restores the prior value on drop.
struct RgBinGuard {
    prev: Option<std::ffi::OsString>,
}

impl RgBinGuard {
    fn set(path: &std::path::Path) -> Self {
        let prev = std::env::var_os("RG_BIN_PATH");
        // SAFETY: tests in this file are #[serial]; no other thread touches
        // process env concurrently within this test binary's serial gate.
        unsafe {
            std::env::set_var("RG_BIN_PATH", path);
        }
        Self { prev }
    }
}

impl Drop for RgBinGuard {
    fn drop(&mut self) {
        // SAFETY: same serial-test confinement as set().
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("RG_BIN_PATH", v),
                None => std::env::remove_var("RG_BIN_PATH"),
            }
        }
    }
}

#[tokio::test]
#[serial]
async fn test_windows_workspace_path_with_spaces() {
    let base_temp = TempDir::new().expect("base tempdir");
    let space_dir = base_temp.path().join("workspace with spaces");
    std::fs::create_dir_all(&space_dir).expect("create space dir");

    let harness = TestHarness::new_with_dir(base_temp);

    // 1. Test set_workspace with space-containing path
    let set_ws = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": space_dir.to_str().unwrap()
                }
            }),
        )
        .await;
    assert_eq!(set_ws["result"]["isError"], false);
    let pinned_ws = set_ws["result"]["structuredContent"]["workspace"]
        .as_str()
        .expect("workspace string");
    assert!(
        pinned_ws.contains("workspace with spaces"),
        "workspace must contain space path: {pinned_ws}"
    );
    assert!(
        !pinned_ws.starts_with(r"\\?\"),
        "workspace must not have verbatim UNC \\\\?\\ prefix: {pinned_ws}"
    );

    // 2. Test workspace_info reflects the workspace
    let ws_info = harness
        .rpc(
            "tools/call",
            json!({
                "name": "workspace_info",
                "arguments": {}
            }),
        )
        .await;
    assert_eq!(ws_info["result"]["isError"], false);
    let info_ws = ws_info["result"]["structuredContent"]["workspace"]
        .as_str()
        .expect("workspace string");
    assert_eq!(info_ws, pinned_ws);

    // 3. Test file tools (write and read_file) inside workspace with spaces
    let file_path = space_dir.join("test file.txt");
    let file_path_str = file_path.to_str().unwrap();

    let write_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "write",
                "arguments": {
                    "file_path": file_path_str,
                    "content": "Hello Windows World"
                }
            }),
        )
        .await;
    assert_eq!(write_res["result"]["isError"], false, "write tool must succeed: {:?}", write_res);

    let read_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "read_file",
                "arguments": {
                    "target_file": file_path_str
                }
            }),
        )
        .await;
    assert_eq!(read_res["result"]["isError"], false, "read_file must succeed: {:?}", read_res);
    let text = read_res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Hello Windows World"), "read content mismatch: {text}");
}

#[tokio::test]
#[serial]
async fn test_windows_terminal_command_bounded_output() {
    let base_temp = TempDir::new().expect("base tempdir");
    let space_dir = base_temp.path().join("terminal space");
    std::fs::create_dir_all(&space_dir).expect("create space dir");

    let harness = TestHarness::new_with_dir(base_temp);

    // Pin workspace to space_dir
    let _ = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": space_dir.to_str().unwrap()
                }
            }),
        )
        .await;

    // Execute a harmless command via direct run_terminal_cmd
    let cmd_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": "echo WindowsHostExecutionOK",
                    "description": "harmless smoke test command"
                }
            }),
        )
        .await;

    assert_eq!(cmd_res["result"]["isError"], false, "command failed: {:?}", cmd_res);
    let text = cmd_res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("WindowsHostExecutionOK"),
        "command output missing expected text: {text}"
    );

    // Verify output is bounded and not empty
    assert!(!text.is_empty());
    assert!(text.len() < 100_000, "output should be bounded");
}

#[tokio::test]
#[serial]
async fn test_upstream_direct_dispatch_architecture_contract() {
    let base_temp = TempDir::new().expect("base tempdir");
    let harness = TestHarness::new_with_dir(base_temp);

    let resp = harness.rpc("tools/list", json!({})).await;
    let tools = resp["result"]["tools"].as_array().expect("tools array");

    let run_cmd = tools
        .iter()
        .find(|t| t["name"] == "run_terminal_cmd")
        .expect("run_terminal_cmd tool");

    let cmd_props = &run_cmd["inputSchema"]["properties"];

    // Prove NO execution_mode or yield_after_ms parameters
    assert!(
        cmd_props.get("execution_mode").is_none(),
        "Must NOT introduce execution_mode"
    );
    assert!(
        cmd_props.get("yield_after_ms").is_none(),
        "Must NOT introduce yield_after_ms"
    );

    // Ensure UPSTREAM_BASE_COMMIT tracks the merged upstream baseline.
    assert_eq!(host::UPSTREAM_BASE_COMMIT, "c059e0d");
}

#[tokio::test]
#[serial]
async fn test_windows_command_resolution_no_cwd_preemption() {
    let base_temp = TempDir::new().expect("base tempdir");
    let ws_dir = base_temp.path().join("workspace");
    std::fs::create_dir_all(&ws_dir).expect("create ws dir");

    // Place dummy script in workspace matching common shell/command names
    #[cfg(windows)]
    {
        let fake_cmd = ws_dir.join("powershell.bat");
        std::fs::write(&fake_cmd, "@echo off\necho CWD_SCRIPT_PREEMPTED\n").expect("write fake script");
    }
    #[cfg(not(windows))]
    {
        let fake_cmd = ws_dir.join("sh");
        std::fs::write(&fake_cmd, "#!/bin/sh\necho CWD_SCRIPT_PREEMPTED\n").expect("write fake script");
    }

    let harness = TestHarness::new_with_dir(base_temp);
    let set_ws = harness
        .rpc(
            "tools/call",
            json!({
                "name": "set_workspace",
                "arguments": {
                    "path": ws_dir.to_str().unwrap()
                }
            }),
        )
        .await;
    assert_eq!(set_ws["result"]["isError"], false);

    // Run safe command
    #[cfg(windows)]
    let test_cmd = "powershell -NoProfile -Command \"Write-Output 'SAFE_RESOLVED'\"";
    #[cfg(not(windows))]
    let test_cmd = "echo 'SAFE_RESOLVED'";

    let run_res = harness
        .rpc(
            "tools/call",
            json!({
                "name": "run_terminal_cmd",
                "arguments": {
                    "command": test_cmd,
                    "description": "Verify no cwd script preemption"
                }
            }),
        )
        .await;
    assert_eq!(run_res["result"]["isError"], false);
    let text = run_res["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("SAFE_RESOLVED"), "output must contain SAFE_RESOLVED: {text}");
    assert!(!text.contains("CWD_SCRIPT_PREEMPTED"), "cwd script must not preempt system command: {text}");
}

#[tokio::test]
#[serial]
async fn test_rg_bin_path_environment_resolution() {
    let base_temp = TempDir::new().expect("base tempdir");
    let fake_rg = base_temp.path().join(if cfg!(windows) { "fake_rg.exe" } else { "fake_rg" });
    std::fs::write(&fake_rg, b"mock").expect("write fake rg");

    let _guard = RgBinGuard::set(&fake_rg);

    let resolved = host::ensure_bundled_rg();
    assert_eq!(resolved, Some(fake_rg.clone()));
    assert_eq!(std::env::var("RG_BIN_PATH").ok(), Some(fake_rg.display().to_string()));
}

#[tokio::test]
#[serial]
async fn test_package_windows_bundle_staging_and_manifest_verification() {
    // Regression test for Issue #62:
    // Verify that the Hands-owned package_windows_bundle script creates
    // a self-contained runtime bundle with complete composition (hands.exe +
    // tunnel-client.exe + pinned rg.exe) beside each other, generates valid
    // manifest.json + SHA256SUMS.txt, and validates cleanly with --verify-only.
    let staging_dir = TempDir::new().expect("staging tempdir");
    let fake_hands = staging_dir.path().join("fake_hands.exe");
    write_dummy_pe(&fake_hands, &[]);

    let fake_tc = staging_dir.path().join("fake_tunnel_client.exe");
    write_dummy_pe(&fake_tc, &[]);

    let fake_rg = staging_dir.path().join("fake_rg.exe");
    let rg_bytes = b"real pinned rg mock bytes";
    std::fs::write(&fake_rg, rg_bytes).expect("write fake rg");

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(rg_bytes);
    let expected_hash = format!("{:x}", hasher.finalize());

    let bundle_out = staging_dir.path().join("bundle");

    let repo_root = hands_repo_root();

    // 1. Internal function invocation with test expected hash succeeds
    let python_script = r#"
import sys, pathlib
sys.path.insert(0, '.')
from scripts.package_windows_bundle import stage_bundle_for_testing, verify_bundle_for_testing
out_dir = pathlib.Path(sys.argv[1])
hands_bin = pathlib.Path(sys.argv[2])
tc_bin = pathlib.Path(sys.argv[3])
rg_bin = pathlib.Path(sys.argv[4])
expected_hash = sys.argv[5]
stage_bundle_for_testing(out_dir, hands_bin=hands_bin, tunnel_client_bin=tc_bin, rg_bin=rg_bin, version='0.1.0-test', expected_rg_hash=expected_hash)
verify_bundle_for_testing(out_dir, expected_rg_hash=expected_hash)
"#;
    let status = std::process::Command::new(resolve_python_cmd())
        .arg("-B")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(python_script)
        .arg(&bundle_out)
        .arg(&fake_hands)
        .arg(&fake_tc)
        .arg(&fake_rg)
        .arg(&expected_hash)
        .current_dir(&repo_root)
        .output()
        .expect("run internal packaging invocation");

    assert!(
        status.status.success(),
        "internal packaging invocation must succeed: {}",
        String::from_utf8_lossy(&status.stderr)
    );

    // Full composition check
    assert!(bundle_out.join("hands.exe").is_file());
    assert!(bundle_out.join("tunnel-client.exe").is_file());
    assert!(bundle_out.join("rg.exe").is_file());
    assert!(bundle_out.join("manifest.json").is_file());
    assert!(bundle_out.join("SHA256SUMS.txt").is_file());

    // Read manifest.json
    let manifest_str = std::fs::read_to_string(bundle_out.join("manifest.json")).expect("read manifest");
    let manifest: serde_json::Value = serde_json::from_str(&manifest_str).expect("parse manifest");
    assert_eq!(manifest["bundle_version"], "0.1.0-test");
    assert_eq!(manifest["target_os"], "windows");

    let files = manifest["files"].as_array().expect("files array");
    assert_eq!(files.len(), 3);
    assert!(files.iter().any(|f| f["name"] == "rg.exe" && f["license"] == "MIT OR Unlicense"));
    assert!(files.iter().any(|f| f["name"] == "hands.exe" && f["version"] == "0.1.0-test"));
    assert!(files.iter().any(|f| f["name"] == "tunnel-client.exe" && f["license"] == "Proprietary"));
    // NOTE: production-CLI staging against the real pinned rg.exe is NOT
    // asserted here (no third-party binary is checked in for unit tests).
    // That behavior belongs to the clean-Windows packaged soak (Seam 2).
}

#[tokio::test]
#[serial]
async fn test_package_windows_bundle_fails_closed_on_wrong_rg_hash() {
    // Negative regression test for Issue #62:
    // Packaging MUST fail closed when rg.exe does not match the pinned hash.
    let staging_dir = TempDir::new().expect("staging tempdir");
    let fake_hands = staging_dir.path().join("fake_hands.exe");
    write_dummy_pe(&fake_hands, &[]);

    let fake_tc = staging_dir.path().join("fake_tunnel_client.exe");
    write_dummy_pe(&fake_tc, &[]);

    let fake_rg = staging_dir.path().join("fake_rg.exe");
    std::fs::write(&fake_rg, b"tampered or mismatched rg binary bytes").expect("write fake rg");

    let bundle_out = staging_dir.path().join("bundle_bad");

    let repo_root = hands_repo_root();
    let status = std::process::Command::new(resolve_python_cmd())
        .arg("-B")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("scripts/package_windows_bundle.py")
        .arg("--out-dir")
        .arg(&bundle_out)
        .arg("--hands-bin")
        .arg(&fake_hands)
        .arg("--tunnel-client-bin")
        .arg(&fake_tc)
        .arg("--rg-bin")
        .arg(&fake_rg)
        .current_dir(&repo_root)
        .output()
        .expect("run package_windows_bundle.py");

    assert!(
        !status.status.success(),
        "packaging script MUST fail closed when rg.exe does not match the pinned hash"
    );
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("failed pinning validation") || stderr.contains("mismatch"),
        "stderr must report pinning rejection: {stderr}"
    );
}

#[tokio::test]
#[serial]
async fn test_package_windows_bundle_rejects_dynamic_msvc_crt_hands() {
    // Seam 2 regression: a checksum-valid bundle is still unusable on a clean
    // Windows machine if hands.exe imports VCRUNTIME/MSVCP from the host.
    // The package seam must reject that false-positive artifact before writing
    // a manifest that claims portability.
    let staging_dir = TempDir::new().expect("staging tempdir");
    let fake_hands = staging_dir.path().join("fake_dynamic_hands.exe");
    write_dummy_pe(&fake_hands, &["VCRUNTIME140.dll"]);

    let fake_tc = staging_dir.path().join("fake_tunnel_client.exe");
    write_dummy_pe(&fake_tc, &[]);
    let fake_rg = staging_dir.path().join("fake_rg.exe");
    let rg_bytes = b"test pinned rg bytes for crt rejection";
    std::fs::write(&fake_rg, rg_bytes).expect("write fake rg");
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(rg_bytes);
    let expected_hash = format!("{:x}", hasher.finalize());

    let bundle_out = staging_dir.path().join("bundle_dynamic_crt");
    let repo_root = hands_repo_root();
    let python_script = r#"
import sys, pathlib
sys.path.insert(0, '.')
from scripts.package_windows_bundle import stage_bundle_for_testing
out_dir = pathlib.Path(sys.argv[1])
hands_bin = pathlib.Path(sys.argv[2])
tc_bin = pathlib.Path(sys.argv[3])
rg_bin = pathlib.Path(sys.argv[4])
expected_hash = sys.argv[5]
stage_bundle_for_testing(out_dir, hands_bin=hands_bin, tunnel_client_bin=tc_bin, rg_bin=rg_bin, version='0.1.0-test', expected_rg_hash=expected_hash)
"#;
    let status = std::process::Command::new(resolve_python_cmd())
        .arg("-B")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(python_script)
        .arg(&bundle_out)
        .arg(&fake_hands)
        .arg(&fake_tc)
        .arg(&fake_rg)
        .arg(&expected_hash)
        .current_dir(&repo_root)
        .output()
        .expect("run dynamic CRT package rejection");

    assert!(
        !status.status.success(),
        "packaging MUST reject hands.exe that imports dynamic MSVC CRT"
    );
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("dynamic MSVC CRT") && stderr.contains("crt-static"),
        "rejection must explain the clean-Windows static CRT requirement: {stderr}"
    );
}
#[tokio::test]
#[serial]
async fn test_package_windows_bundle_rejects_malformed_truncated_pe() {
    let staging_dir = TempDir::new().expect("staging tempdir");
    let fake_hands = staging_dir.path().join("fake_truncated_hands.exe");
    let mut data = vec![0u8; 96];
    data[0] = b'M';
    data[1] = b'Z';
    data[0x3C] = 64;
    data[64] = b'P';
    data[65] = b'E';
    data[66] = 0;
    data[67] = 0;
    // machine x86_64 = 0x8664 at offset 68
    data[68] = 0x64;
    data[69] = 0x86;
    // size of optional header = 0 at offset 84
    std::fs::write(&fake_hands, data).expect("write 96-byte truncated PE");

    let repo_root = hands_repo_root();
    let python_script = r#"
import sys, pathlib
sys.path.insert(0, '.')
from scripts.package_windows_bundle import verify_pe_x86_64
hands_bin = pathlib.Path(sys.argv[1])
try:
    verify_pe_x86_64(hands_bin, "hands.exe")
    print("ACCEPTED", file=sys.stderr)
    sys.exit(1)
except RuntimeError as e:
    print(f"REJECTED: {e}", file=sys.stderr)
    sys.exit(0)
"#;
    let status = std::process::Command::new(resolve_python_cmd())
        .arg("-B")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(python_script)
        .arg(&fake_hands)
        .current_dir(&repo_root)
        .output()
        .expect("run malformed PE test");

    assert!(
        status.status.success(),
        "verify_pe_x86_64 must reject 96-byte malformed PE: stderr={}",
        String::from_utf8_lossy(&status.stderr)
    );
    let stderr = String::from_utf8_lossy(&status.stderr);
    assert!(
        stderr.contains("REJECTED") && (stderr.contains("optional header") || stderr.contains("invalid")),
        "rejection must report invalid/truncated PE header: {stderr}"
    );
}


#[tokio::test]
#[serial]
async fn test_patch_grok_build_script_reproducibility_and_fail_closed() {
    let repo_root = hands_repo_root();
    let grok_source = match grok_build_reference_source() {
        Some(s) => s,
        None => {
            eprintln!("Skipping test_patch_grok_build_script_reproducibility_and_fail_closed: grok-build reference checkout not found");
            return;
        }
    };
    let patch_script = repo_root.join("scripts").join("patch_grok_build.py");
    let patches_dir = repo_root.join("patches").join("grok-build");

    assert!(patch_script.is_file(), "scripts/patch_grok_build.py must exist");
    assert!(patches_dir.is_dir(), "patches/grok-build/ must exist");

    let temp_base = tempfile::Builder::new()
        .prefix("hands_patch_regression_")
        .tempdir()
        .expect("tempdir");
    let temp_root = temp_base.path().to_path_buf();

    let python_test_code = r#"
import sys, pathlib, subprocess, os

repo_root = pathlib.Path(sys.argv[1])
grok_source = pathlib.Path(sys.argv[2])
temp_dir = pathlib.Path(sys.argv[3])
pinned_sha = sys.argv[4]

patches_dir = repo_root / "patches" / "grok-build"

sys.path.insert(0, str(repo_root))
from scripts.patch_grok_build import verify_and_patch, get_patch_target, normalize_diff, verify_target_diffs

# 1. Exact 3-file reproduction from clean pinned base
clone1 = temp_dir / "repro_clone"
subprocess.check_call(["git", "clone", "--no-local", "--shared", str(grok_source), str(clone1)], stderr=subprocess.DEVNULL)
subprocess.check_call(["git", "-C", str(clone1), "checkout", "--force", pinned_sha], stderr=subprocess.DEVNULL)
subprocess.check_call(["git", "-C", str(clone1), "clean", "-fdx"], stderr=subprocess.DEVNULL)

verify_and_patch(clone1, patches_dir, pinned_sha)
# Idempotent call must succeed
verify_and_patch(clone1, patches_dir, pinned_sha)

patch_files = sorted(patches_dir.glob("*.patch"))
targets = [get_patch_target(p.read_text(encoding="utf-8")) for p in patch_files]
assert len(targets) == 3, f"Expected exactly 3 patch targets, got {len(targets)}"

for target in targets:
    d_repro = subprocess.check_output(["git", "-C", str(clone1), "diff", "--", target]).replace(b"\r\n", b"\n")
    d_orig = subprocess.check_output(["git", "diff", "--", target], cwd=str(grok_source)).replace(b"\r\n", b"\n")
    assert normalize_diff(d_repro.decode("utf-8")) == normalize_diff(d_orig.decode("utf-8")), f"Bit-for-bit diff reproduction mismatch for target {target}"
    assert len(normalize_diff(d_repro.decode("utf-8"))) > 0, f"Diff for {target} must not be empty"

diff_targets = subprocess.check_output(["git", "-C", str(clone1), "diff", "--name-only"]).decode().splitlines()
assert sorted(pathlib.Path(t).as_posix() for t in diff_targets) == sorted(targets), (
    f"Reproduced diff must touch exactly the patch targets: {diff_targets}"
)
# 2. Wrong SHA fail-closed
clone2 = temp_dir / "wrong_sha_clone"
subprocess.check_call(["git", "clone", "--no-local", "--shared", str(grok_source), str(clone2)], stderr=subprocess.DEVNULL)
subprocess.check_call(["git", "-C", str(clone2), "checkout", "--force", "HEAD~1"], stderr=subprocess.DEVNULL)
try:
    verify_and_patch(clone2, patches_dir, pinned_sha)
    raise AssertionError("Wrong SHA did not fail closed")
except RuntimeError as e:
    assert "commit mismatch" in str(e), f"Unexpected error on wrong SHA: {e}"

# 3. Dirty/conflicting patch-target state fail-closed
clone3 = temp_dir / "dirty_clone"
subprocess.check_call(["git", "clone", "--no-local", "--shared", str(grok_source), str(clone3)], stderr=subprocess.DEVNULL)
subprocess.check_call(["git", "-C", str(clone3), "checkout", "--force", pinned_sha], stderr=subprocess.DEVNULL)
target_file = clone3 / "crates" / "codegen" / "xai-tty-utils" / "src" / "lib.rs"
target_file.write_text("// conflicting line\n" + target_file.read_text())
try:
    verify_and_patch(clone3, patches_dir, pinned_sha)
    raise AssertionError("Dirty patch-target state did not fail closed")
except RuntimeError as e:
    assert "unexpected" in str(e) or "dirty" in str(e) or "pre-check" in str(e), f"Unexpected error on dirty: {e}"

# 4. Post-apply divergence fail-closed
diverged_file = clone1 / "crates" / "codegen" / "xai-grok-tools" / "src" / "computer" / "local" / "terminal.rs"
diverged_file.write_text(diverged_file.read_text() + "\n// post-apply divergence line\n")
try:
    verify_and_patch(clone1, patches_dir, pinned_sha)
    raise AssertionError("Post-apply divergence did not fail closed")
except RuntimeError as e:
    assert "diverged" in str(e), f"Unexpected error on post-apply divergence: {e}"

# 5. Non-existent and non-git directories fail closed
try:
    verify_and_patch(temp_dir / "does_not_exist", patches_dir, pinned_sha)
    raise AssertionError("Non-existent directory did not fail closed")
except RuntimeError as e:
    assert "does not exist" in str(e)

non_git = temp_dir / "non_git_dir"
non_git.mkdir()
try:
    verify_and_patch(non_git, patches_dir, pinned_sha)
    raise AssertionError("Non-git directory did not fail closed")
except RuntimeError as e:
    assert "not a git repository" in str(e)

# 6. Malformed/duplicate Cargo.toml injection fail-closed
clone4 = temp_dir / "bad_cargo_toml_clone"
subprocess.check_call(["git", "clone", "--no-local", "--shared", str(grok_source), str(clone4)], stderr=subprocess.DEVNULL)
subprocess.check_call(["git", "-C", str(clone4), "checkout", "--force", pinned_sha], stderr=subprocess.DEVNULL)
verify_and_patch(clone4, patches_dir, pinned_sha)
bad_toml = clone4 / "Cargo.toml"
bad_toml.write_text(bad_toml.read_text() + '\n    "crates/codegen/hands",\n')
try:
    verify_and_patch(clone4, patches_dir, pinned_sha)
    raise AssertionError("Malformed/duplicate Cargo.toml did not fail closed")
except RuntimeError as e:
    assert "Cargo.toml does not match" in str(e) or "unexpected" in str(e)

# 7. Clean shallow checkout (CI-equivalent with depth 1) reproduction & fail-closed detection
clone5 = temp_dir / "shallow_clone"
subprocess.check_call(["git", "clone", "--depth", "1", "file:///" + str(grok_source).replace("\\", "/"), str(clone5)], stderr=subprocess.DEVNULL)
verify_and_patch(clone5, patches_dir, pinned_sha)
# Idempotent call on shallow clone must succeed
verify_and_patch(clone5, patches_dir, pinned_sha)

# Tampering shallow clone post-apply must fail closed
tampered_target = clone5 / "crates" / "codegen" / "xai-grok-tools" / "src" / "computer" / "local" / "terminal.rs"
tampered_target.write_text(tampered_target.read_text(encoding="utf-8") + "\n// shallow divergence line\n", encoding="utf-8")
try:
    verify_and_patch(clone5, patches_dir, pinned_sha)
    raise AssertionError("Tampered shallow clone did not fail closed")
except RuntimeError as e:
    assert "diverged" in str(e), f"Unexpected error on tampered shallow clone: {e}"
# 8. Content-only trailing-whitespace tamper must still be detected as divergence
clone6 = temp_dir / "trailing_ws_clone"
subprocess.check_call(["git", "clone", "--depth", "1", "file:///" + str(grok_source).replace("\\", "/"), str(clone6)], stderr=subprocess.DEVNULL)
verify_and_patch(clone6, patches_dir, pinned_sha)
ws_target = clone6 / "crates" / "codegen" / "xai-grok-tools" / "src" / "computer" / "local" / "terminal.rs"
ws_content = ws_target.read_text(encoding="utf-8")
needle = "pub(crate) const BACKGROUND_MAX_RUNTIME: Duration = Duration::from_secs(36_000);"
assert needle in ws_content, "Target needle must exist in patched file"
ws_tampered = ws_content.replace(needle, needle + " ")
ws_target.write_text(ws_tampered, encoding="utf-8")
try:
    verify_target_diffs(clone6, patch_files)
    raise AssertionError("Trailing-whitespace tamper did not fail closed")
except RuntimeError as e:
    assert "diverged" in str(e), f"Unexpected error on trailing whitespace tamper: {e}"

# 9. Already-applied tampered Cargo.lock fail-closed
clone7 = temp_dir / "tampered_lock_clone"
subprocess.check_call(["git", "clone", "--depth", "1", "file:///" + str(grok_source).replace("\\", "/"), str(clone7)], stderr=subprocess.DEVNULL)
verify_and_patch(clone7, patches_dir, pinned_sha)
(clone7 / "Cargo.lock").write_text("tampered-lock\n", encoding="utf-8")
try:
    verify_and_patch(clone7, patches_dir, pinned_sha)
    raise AssertionError("Already-applied tampered Cargo.lock did not fail closed")
except RuntimeError as e:
    assert "Cargo.lock does not match" in str(e) or "unexpected" in str(e), f"Unexpected error on tampered lock: {e}"

# 10. Clean up git readonly files so TempDir drops without Windows permission error
import stat, shutil
def remove_readonly(func, path, excinfo):
    try:
        os.chmod(path, stat.S_IWRITE)
        func(path)
    except Exception:
        pass
for p in temp_dir.glob("clone*"):
    if p.is_dir():
        shutil.rmtree(p, onerror=remove_readonly)
for p in temp_dir.glob("trailing_ws_clone*"):
    if p.is_dir():
        shutil.rmtree(p, onerror=remove_readonly)
"#;

    let status = std::process::Command::new(resolve_python_cmd())
        .arg("-B")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .arg("-c")
        .arg(python_test_code)
        .arg(&repo_root)
        .arg(&grok_source)
        .arg(&temp_root)
        .arg(host::GROK_BUILD_PINNED_SHA)
        .current_dir(&repo_root)
        .output()
        .expect("run patch reproducibility and fail-closed test");

    if !status.status.success() {
        let leaked = temp_base.into_path();
        panic!(
            "patch regression test failed (kept at {}):\nstdout: {}\nstderr: {}",
            leaked.display(),
            String::from_utf8_lossy(&status.stdout),
            String::from_utf8_lossy(&status.stderr)
        );
    }
}
