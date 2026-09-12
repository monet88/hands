use std::process::Command;
use tempfile::tempdir;

use hands_return_bridge::host::{
    HostError, LocalInitOptions, SetupOptions, execute_local_init, execute_setup, resolve_state_dir,
    run_native_host,
};
use hands_return_bridge::journal::{Journal, PairingStatus, LOCAL_PAIRING_ID};
use hands_return_bridge::protocol::TRUST_NOTICE;

fn init_git_repo(path: &std::path::Path) {
    let output = Command::new("git")
        .args(["init", &path.to_string_lossy()])
        .output()
        .expect("git init must succeed");
    assert!(output.status.success(), "git init failed");
}

#[test]
fn test_setup_flow_with_isolated_state_dir() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_1".to_string(),
        target_path: target_path.clone(),
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_id_123".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    let result = execute_setup(&opts).expect("Setup execution failed");

    assert!(result.pairing_id.starts_with("pair_"));
    assert!(result.bootstrap_token.starts_with("rb_boot_"));
    assert_eq!(result.trust_notice, TRUST_NOTICE);

    // Verify database was created and contains the setup
    let db_path = state_dir.join("journal.sqlite");
    assert!(db_path.exists());

    let journal = Journal::open(&db_path).expect("Failed to open journal");
    let status = journal.get_pairing_status(&result.pairing_id).unwrap();
    assert_eq!(status, PairingStatus::Pending);

    // Verify canonical target
    let targets = journal.get_targets(&result.pairing_id).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].target_id, "test_target");

    // Verify expected extension ID was persisted in journal
    let exp_id = journal.get_expected_extension_id().unwrap();
    assert_eq!(exp_id.as_deref(), Some("test_ext_id_123"));

    // Verify manifest was written
    let manifest_path = state_dir.join("com.hands.return_bridge.json");
    assert!(manifest_path.exists());
    let manifest_content = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(manifest_content.contains("com.hands.return_bridge"));
    assert!(manifest_content.contains("test_ext_id_123"));
}

#[test]
fn test_local_init_creates_active_local_host_without_bootstrap() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();
    let extension_id = "abcdefghijklmnopabcdefghijklmnop";

    let result = execute_local_init(&LocalInitOptions {
        browser: "chrome".to_string(),
        extension_id: extension_id.to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    })
    .expect("local init failed");

    assert_eq!(result.browser, "chrome");
    assert_eq!(result.extension_id, extension_id);
    assert_eq!(result.pairing_id, LOCAL_PAIRING_ID);

    let journal = Journal::open(&state_dir.join("journal.sqlite")).unwrap();
    assert_eq!(
        journal.get_pairing_status(LOCAL_PAIRING_ID).unwrap(),
        PairingStatus::Active
    );
    assert_eq!(
        journal.get_expected_extension_id().unwrap().as_deref(),
        Some(extension_id)
    );

    let manifest = std::fs::read_to_string(&result.manifest_path).unwrap();
    assert!(manifest.contains("com.hands.return_bridge"));
    assert!(manifest.contains(extension_id));
}

#[test]
fn test_setup_rejects_non_git_target() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    // Do not run git init
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_1".to_string(),
        target_path,
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_id_123".to_string(),
        state_dir: Some(state_dir),
        skip_registry: true,
    };

    let result = execute_setup(&opts);
    assert!(result.is_err());
    match result.unwrap_err() {
        HostError::InvalidTarget(msg) => {
            assert!(msg.contains("not a valid git repository or worktree"));
        }
        other => panic!("Expected HostError::InvalidTarget, got {:?}", other),
    }
}

#[test]
fn test_setup_rejects_fake_git_directory() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    // Create an empty fake .git directory without git initialization
    std::fs::create_dir_all(target_dir.path().join(".git")).unwrap();
    // Corrupt it so git rev-parse fails
    std::fs::write(target_dir.path().join(".git").join("HEAD"), "corrupt").unwrap();

    let target_path = target_dir.path().to_str().unwrap().to_string();

    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_1".to_string(),
        target_path,
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_id_123".to_string(),
        state_dir: Some(state_dir),
        skip_registry: true,
    };

    let result = execute_setup(&opts);
    assert!(result.is_err(), "Fake .git directory must be rejected by git rev-parse check");
}

#[test]
fn test_setup_rejects_invalid_browser() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let opts = SetupOptions {
        browser: "safari".to_string(), // Invalid browser
        profile_id: "test_profile_1".to_string(),
        target_path,
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_id_123".to_string(),
        state_dir: Some(state_dir),
        skip_registry: true,
    };

    let result = execute_setup(&opts);
    assert!(result.is_err(), "Invalid browser must be rejected");
}

#[test]
fn test_run_native_host_exact_origin_authority() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_1".to_string(),
        target_path,
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "pinned_ext_id_abc".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    execute_setup(&opts).expect("Setup must succeed");

    // Case 1: Missing origin must be rejected
    let err_missing = run_native_host(Some(&state_dir), None);
    assert!(err_missing.is_err(), "Missing origin must be rejected");

    // Case 2: Wrong extension ID origin must be rejected
    let err_wrong = run_native_host(Some(&state_dir), Some("chrome-extension://wrong_ext_id_xyz/"));
    assert!(err_wrong.is_err(), "Wrong extension origin must be rejected");

    // Case 3: Scheme prefix only without matching ID must be rejected
    let err_scheme = run_native_host(Some(&state_dir), Some("chrome-extension://other/"));
    assert!(err_scheme.is_err(), "Non-matching scheme prefix origin must be rejected");

    // Case 4: Forged/tampered manifest with attacker origin cannot authorize when journal authority has different ID
    let manifest_path = state_dir.join("com.hands.return_bridge.json");
    let forged_manifest = serde_json::json!({
        "name": "com.hands.return_bridge",
        "description": "Forged manifest",
        "path": "hands-return-bridge.exe",
        "type": "stdio",
        "allowed_origins": ["chrome-extension://forged_attacker_ext_id/"]
    });
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&forged_manifest).unwrap()).unwrap();

    let err_forged = run_native_host(Some(&state_dir), Some("chrome-extension://forged_attacker_ext_id/"));
    assert!(err_forged.is_err(), "Forged manifest must not authorize origin when journal authority does not match");

    // Case 5: Forged manifest when journal has no expected_extension_id configured must fail closed (no fallback)
    {
        let db_path = state_dir.join("journal.sqlite");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DELETE FROM host_config WHERE key = 'expected_extension_id'", []).unwrap();
    }
    let err_no_journal = run_native_host(Some(&state_dir), Some("chrome-extension://forged_attacker_ext_id/"));
    assert!(err_no_journal.is_err(), "Manifest must not be used as fallback when journal authority is absent");
}

#[test]
fn test_resolve_state_dir_explicit_override_is_absolute() {
    let dir = tempdir().unwrap();
    let res = resolve_state_dir(Some(dir.path())).unwrap();
    assert!(res.is_absolute());
    assert_eq!(res, dir.path());

    let relative = std::path::Path::new("bridge/native/relative-state-dir-probe");
    let relative_res = resolve_state_dir(Some(relative)).unwrap();
    assert!(relative_res.is_absolute());
    assert_eq!(relative_res, std::env::current_dir().unwrap().join(relative));
}
#[test]
fn test_setup_rejects_missing_explicit_options() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let valid_opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_1".to_string(),
        target_path: target_path.clone(),
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_id_123".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    // 1. Missing profile_id
    let mut opts = valid_opts.clone();
    opts.profile_id = "".to_string();
    assert!(execute_setup(&opts).is_err(), "Empty profile_id must be rejected");

    // 2. Missing extension_id
    let mut opts = valid_opts.clone();
    opts.extension_id = "".to_string();
    assert!(execute_setup(&opts).is_err(), "Empty extension_id must be rejected");

    // 3. Missing policy_revision
    let mut opts = valid_opts.clone();
    opts.policy_revision = "".to_string();
    assert!(execute_setup(&opts).is_err(), "Empty policy_revision must be rejected");

    // 4. Missing tool_policy
    let mut opts = valid_opts.clone();
    opts.tool_policy = "".to_string();
    assert!(execute_setup(&opts).is_err(), "Empty tool_policy must be rejected");

    // 5. Missing approval_policy
    let mut opts = valid_opts.clone();
    opts.approval_policy = "".to_string();
    assert!(execute_setup(&opts).is_err(), "Empty approval_policy must be rejected");

    // 6. Explicit target ID must not be empty or whitespace.
    let mut opts = valid_opts.clone();
    opts.target_id = Some("   \t".to_string());
    let err = execute_setup(&opts).expect_err("Whitespace target_id must be rejected");
    assert!(err.to_string().contains("--target-id"));
}

#[test]
#[cfg(windows)]
fn test_registered_setup_rejects_custom_state_dir_before_side_effects() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("must_not_be_created");
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());

    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_registered_state".to_string(),
        target_path: target_dir.path().to_string_lossy().to_string(),
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_registered_state".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: false,
    };

    let err = execute_setup(&opts).expect_err("Registered setup must reject custom state dir");
    assert!(err.to_string().contains("--state-dir is only supported with --skip-registry"));
    assert!(!state_dir.exists(), "Rejected custom state dir must have zero durable side effects");
}

#[test]
fn test_manifest_replacement_leaves_no_partial_temp_files() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());

    let base = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_manifest_a".to_string(),
        target_path: target_dir.path().to_string_lossy().to_string(),
        target_id: Some("target_manifest_a".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "same_manifest_extension_id".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    execute_setup(&base).expect("First setup must write manifest");
    let mut second = base.clone();
    second.profile_id = "profile_manifest_b".to_string();
    second.target_id = Some("target_manifest_b".to_string());
    execute_setup(&second).expect("Second compatible setup must atomically replace manifest");

    let manifest_path = state_dir.join("com.hands.return_bridge.json");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    assert_eq!(manifest["allowed_origins"][0], "chrome-extension://same_manifest_extension_id/");

    let leftovers: Vec<_> = std::fs::read_dir(&state_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "Atomic manifest writes must not leave temp files: {leftovers:?}");
}

#[test]
fn test_revoke_cli_rejects_unknown_and_missing_option_values() {
    let bin = env!("CARGO_BIN_EXE_hands-return-bridge");

    let unknown = Command::new(bin)
        .args(["revoke", "--pairing-id", "pair_x", "--bogus"])
        .output()
        .expect("revoke CLI must run");
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("Unknown option for revoke: --bogus"));

    let missing = Command::new(bin)
        .args(["revoke", "--state-dir"])
        .output()
        .expect("revoke CLI must run");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("--state-dir requires a value"));

    let whitespace = Command::new(bin)
        .args(["revoke", "--pairing-id", "   "])
        .output()
        .expect("revoke CLI must run");
    assert!(!whitespace.status.success());
    assert!(String::from_utf8_lossy(&whitespace.stderr).contains("must not be empty or whitespace"));
}
#[test]
fn test_setup_skip_registry_semantics() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_1".to_string(),
        target_path,
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_id_123".to_string(),
        state_dir: Some(state_dir),
        skip_registry: true,
    };

    let result = execute_setup(&opts);
    assert!(result.is_ok(), "--skip-registry must allow isolated setup without platform registry side effects");
}

#[test]
#[cfg(not(windows))]
fn test_non_windows_setup_fails_closed_without_skip_registry() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().join("should_not_exist");

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "test_profile_1".to_string(),
        target_path,
        target_id: Some("test_target".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_id_123".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: false,
    };

    let result = execute_setup(&opts);
    assert!(result.is_err(), "execute_setup without --skip-registry must fail on non-Windows");
    match result.unwrap_err() {
        HostError::Registry(msg) => {
            assert!(msg.contains("automatic registration is only supported on Windows"));
        }
        other => panic!("Expected HostError::Registry, got {:?}", other),
    }

    // Assert zero durable side effects before error return
    assert!(!state_dir.exists(), "state_dir must not be created when registration check fails");
}

#[test]
fn test_setup_a_remains_authoritative_after_competing_setup_b_fails() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let ext_a = "authoritative_extension_id_aaa".to_string();
    let ext_b = "competing_malicious_id_bbb".to_string();

    // 1. Setup A succeeds and configures authoritative extension ID
    let opts_a = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_a".to_string(),
        target_path: target_path.clone(),
        target_id: Some("target_a".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: ext_a.clone(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    let res_a = execute_setup(&opts_a);
    assert!(res_a.is_ok(), "Setup A must succeed");

    // Verify authoritative ID is recorded in journal
    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();
    assert_eq!(journal.get_expected_extension_id().unwrap().as_deref(), Some(ext_a.as_str()));

    // 2. Competing Setup B attempts to configure a different extension ID in the same state directory
    let opts_b = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_b".to_string(),
        target_path: target_path.clone(),
        target_id: Some("target_b".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: ext_b.clone(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    let res_b = execute_setup(&opts_b);
    assert!(res_b.is_err(), "Setup B with different extension ID must fail closed");
    match res_b.unwrap_err() {
        HostError::Storage(msg) => {
            assert!(msg.contains("cannot overwrite"));
        }
        other => panic!("Expected HostError::Storage, got {:?}", other),
    }

    // 3. Setup A remains authoritative
    assert_eq!(
        journal.get_expected_extension_id().unwrap().as_deref(),
        Some(ext_a.as_str()),
        "Authoritative extension ID from Setup A must not be clobbered"
    );

    // Origin validation continues to authorize ONLY extension A
    let origin_b = format!("chrome-extension://{}/", ext_b);

    // Origin B must be rejected
    let err_b = run_native_host(Some(&state_dir), Some(&origin_b));
    assert!(err_b.is_err(), "Origin B must be rejected");

    // 4. Identical reuse by a compatible setup succeeds
    let opts_a_reuse = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_a2".to_string(),
        target_path,
        target_id: Some("target_a2".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: ext_a.clone(),
        state_dir: Some(state_dir),
        skip_registry: true,
    };
    let res_reuse = execute_setup(&opts_a_reuse);
    assert!(res_reuse.is_ok(), "Setup reusing identical extension ID must succeed");
}

#[test]
fn test_failed_setup_does_not_clear_sticky_extension_authority() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_path = target_dir.path().to_str().unwrap().to_string();

    let ext_id = "sticky_extension_id_999".to_string();

    // 1. Initial setup establishes the sticky expected_extension_id
    let opts_init = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_init".to_string(),
        target_path: target_path.clone(),
        target_id: Some("target_init".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: ext_id.clone(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };
    let res_init = execute_setup(&opts_init);
    assert!(res_init.is_ok(), "Initial setup must succeed");

    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();
    assert_eq!(
        journal.get_expected_extension_id().unwrap().as_deref(),
        Some(ext_id.as_str())
    );

    // 2. Make the manifest path unwritable (replace file with a directory or lock)
    // so a second setup attempt with the same extension ID fails during manifest writing
    let manifest_path = state_dir.join("com.hands.return_bridge.json");
    let _ = std::fs::remove_file(&manifest_path);
    std::fs::create_dir_all(&manifest_path).unwrap(); // Directory at manifest path causes write error

    let opts_failing = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_failing".to_string(),
        target_path: target_path.clone(),
        target_id: Some("target_failing".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: ext_id.clone(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    let res_failing = execute_setup(&opts_failing);
    assert!(res_failing.is_err(), "Setup must fail when manifest cannot be written");

    // 3. PROOF: Even though the second attempt failed after reaching set_expected_extension_id,
    // the global expected_extension_id MUST REMAIN AUTHORITATIVE and intact!
    assert_eq!(
        journal.get_expected_extension_id().unwrap().as_deref(),
        Some(ext_id.as_str()),
        "A later failed setup attempt must NEVER clear or erase sticky global authority"
    );

    // Origin validation continues to succeed for the pinned extension ID
    let origin = format!("chrome-extension://{}/", ext_id);
    let auth_check = run_native_host(Some(&state_dir), Some(&origin));
    assert!(
        auth_check.is_ok(),
        "Native host origin authority must stay valid despite later failed setup attempt"
    );

    // Clean up unwritable dummy directory so retry succeeds
    let _ = std::fs::remove_dir_all(&manifest_path);
    let res_retry = execute_setup(&opts_failing);
    assert!(res_retry.is_ok(), "Retry with same extension ID must succeed");
}

#[test]
fn test_host_target_add_remove_list_flow() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target1_dir = tempdir().unwrap();
    init_git_repo(target1_dir.path());
    let target1_path = target1_dir.path().to_str().unwrap().to_string();

    // Setup active pairing
    let opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_host_t".to_string(),
        target_path: target1_path,
        target_id: Some("target_1".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_host_t".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };
    let setup_res = execute_setup(&opts).unwrap();

    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    // Target CLI on pending pairing fails with NotActive
    let target2_dir = tempdir().unwrap();
    init_git_repo(target2_dir.path());
    let target2_path = target2_dir.path().to_str().unwrap().to_string();

    use hands_return_bridge::host::{
        TargetAddOptions, TargetListOptions, TargetRemoveOptions,
        execute_target_add, execute_target_list, execute_target_remove,
    };

    let add_opts = TargetAddOptions {
        pairing_id: Some(setup_res.pairing_id.clone()),
        target_path: target2_path.clone(),
        target_id: Some("target_2".to_string()),
        state_dir: Some(state_dir.clone()),
    };
    let add_pending_err = execute_target_add(&add_opts);
    assert!(add_pending_err.is_err());

    // Activate pairing
    journal.activate_bootstrap(&setup_res.bootstrap_token, "profile_host_t").unwrap();

    let blank_target_id = TargetAddOptions {
        pairing_id: Some(setup_res.pairing_id.clone()),
        target_path: target2_path.clone(),
        target_id: Some("   \t".to_string()),
        state_dir: Some(state_dir.clone()),
    };
    let blank_target_err = execute_target_add(&blank_target_id)
        .expect_err("Explicit whitespace target ID must be rejected");
    assert!(blank_target_err.to_string().contains("--target-id"));

    let blank_pairing_id = TargetListOptions {
        pairing_id: Some("   \t".to_string()),
        state_dir: Some(state_dir.clone()),
    };
    let blank_pairing_err = execute_target_list(&blank_pairing_id)
        .expect_err("Explicit whitespace pairing ID must be rejected");
    assert!(blank_pairing_err.to_string().contains("--pairing-id"));

    // Now add target_2 succeeds
    let added = execute_target_add(&add_opts).expect("execute_target_add failed");
    assert_eq!(added.target_id, "target_2");

    // A target ID is a stable workspace identity. Reusing it for another repo must not
    // silently redirect existing browser conversation bindings to the new path.
    let target3_dir = tempdir().unwrap();
    init_git_repo(target3_dir.path());
    let retarget_opts = TargetAddOptions {
        pairing_id: Some(setup_res.pairing_id.clone()),
        target_path: target3_dir.path().to_str().unwrap().to_string(),
        target_id: Some("target_2".to_string()),
        state_dir: Some(state_dir.clone()),
    };
    let retarget_err = execute_target_add(&retarget_opts)
        .expect_err("Existing target_id must not be rebound to a different workspace");
    assert!(retarget_err.to_string().contains("target_id_conflict"));

    // List targets
    let list_opts = TargetListOptions {
        pairing_id: Some(setup_res.pairing_id.clone()),
        state_dir: Some(state_dir.clone()),
    };
    let list = execute_target_list(&list_opts).expect("execute_target_list failed");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].target_id, "target_1");
    assert_eq!(list[1].target_id, "target_2");
    assert_eq!(list[1].canonical_path, added.canonical_path);

    // Add target with invalid non-git path fails
    let non_git_dir = tempdir().unwrap();
    let add_invalid = TargetAddOptions {
        pairing_id: None, // Auto-resolves the single active pairing
        target_path: non_git_dir.path().to_str().unwrap().to_string(),
        target_id: Some("invalid_target".to_string()),
        state_dir: Some(state_dir.clone()),
    };
    let invalid_err = execute_target_add(&add_invalid);
    assert!(invalid_err.is_err());

    // Remove target_1
    let rm_opts = TargetRemoveOptions {
        pairing_id: None,
        target_id: "target_1".to_string(),
        state_dir: Some(state_dir.clone()),
    };
    execute_target_remove(&rm_opts).expect("execute_target_remove failed");

    let list_after_rm = execute_target_list(&list_opts).expect("execute_target_list failed");
    assert_eq!(list_after_rm.len(), 1);
    assert_eq!(list_after_rm[0].target_id, "target_2");

    // Once more than one pairing is active, local target administration must not guess.
    let second_setup = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "profile_host_t_2".to_string(),
        target_path: target3_dir.path().to_str().unwrap().to_string(),
        target_id: Some("target_other_pairing".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_host_t".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };
    let second_setup_res = execute_setup(&second_setup).expect("second setup failed");
    journal
        .activate_bootstrap(&second_setup_res.bootstrap_token, "profile_host_t_2")
        .unwrap();

    let ambiguous_list = TargetListOptions {
        pairing_id: None,
        state_dir: Some(state_dir.clone()),
    };
    let ambiguous_err = execute_target_list(&ambiguous_list)
        .expect_err("Multiple active pairings must require an explicit --pairing-id");
    assert!(ambiguous_err.to_string().contains("Multiple active pairings"));
}
