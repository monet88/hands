use std::process::Command;
use tempfile::tempdir;


use hands_bridge::host::{
    HostError, LocalInitOptions, SetupOptions, execute_local_init, execute_setup, resolve_state_dir,
    run_native_host,
};
use hands_bridge::journal::{Journal, PairingStatus, LOCAL_PAIRING_ID};
use hands_bridge::protocol::TRUST_NOTICE;

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
        "path": "hands-bridge.exe",
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
    let bin = env!("CARGO_BIN_EXE_hands-bridge");

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

    use hands_bridge::host::{
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

#[test]
fn test_notify_cli_done_and_failed_flow() {
    let bin = env!("CARGO_BIN_EXE_hands-bridge");
    let state_dir = tempdir().unwrap();
    let db_path = state_dir.path().join("journal.sqlite");
    let journal = hands_bridge::journal::Journal::open(&db_path).unwrap();

    let target_dir = tempdir().unwrap();
    let output = Command::new("git")
        .args(["init", &target_dir.path().to_string_lossy()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let canonical_target = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let setup_opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "prof_cli_notify".to_string(),
        target_path: canonical_target,
        target_id: Some("target_cli".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_notify".to_string(),
        state_dir: Some(state_dir.path().to_path_buf()),
        skip_registry: true,
    };
    let setup_res = execute_setup(&setup_opts).unwrap();
    journal.activate_bootstrap(&setup_res.bootstrap_token, "prof_cli_notify").unwrap();

    let task_id_1 = "task_cli_1";
    let launch_params_1 = hands_bridge::journal::LaunchRequestParams {
        pairing_id: setup_res.pairing_id.clone(),
        launch_request_id: task_id_1.to_string(),
        origin_conversation_id: "conv_cli_1".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_cli_1".to_string(),
        transcript_evidence_hash: "hash_t_cli".to_string(),
        account_evidence_hash: "hash_a_cli".to_string(),
        target_id: "target_cli".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "CLI notify test prompt".to_string(),
    };
    let claim_1 = journal.reserve_or_claim_launch(&launch_params_1).unwrap();
    journal.mark_launch_attempt(&claim_1.execution_id, &setup_res.pairing_id).unwrap();

    // 1. Run notify done with environment variables
    let out_done = Command::new(bin)
        .args(["notify", "done"])
        .env("HANDS_RETURN_BRIDGE_STATE_DIR", state_dir.path())
        .env("HANDS_RETURN_BRIDGE_EXECUTION_ID", &claim_1.execution_id)
        .env("HANDS_TASK_ID", task_id_1)
        .output()
        .unwrap();
    assert!(out_done.status.success(), "notify done failed: {}", String::from_utf8_lossy(&out_done.stderr));
    let stdout_done = String::from_utf8_lossy(&out_done.stdout);
    assert!(stdout_done.contains("Notification recorded"));
    assert!(stdout_done.contains("status: completed"));

    // 2. Repeated notify done is idempotent
    let out_done_repeat = Command::new(bin)
        .args(["notify", "done"])
        .env("HANDS_RETURN_BRIDGE_STATE_DIR", state_dir.path())
        .env("HANDS_RETURN_BRIDGE_EXECUTION_ID", &claim_1.execution_id)
        .env("HANDS_TASK_ID", task_id_1)
        .output()
        .unwrap();
    assert!(out_done_repeat.status.success());
    let stdout_repeat = String::from_utf8_lossy(&out_done_repeat.stdout);
    assert!(stdout_repeat.contains("Notification already recorded"));

    // 3. Notify failed on already completed execution rejects with conflict
    let out_conflict = Command::new(bin)
        .args(["notify", "failed", "--message", "Late failure"])
        .env("HANDS_RETURN_BRIDGE_STATE_DIR", state_dir.path())
        .env("HANDS_RETURN_BRIDGE_EXECUTION_ID", &claim_1.execution_id)
        .env("HANDS_TASK_ID", task_id_1)
        .output()
        .unwrap();
    assert!(!out_conflict.status.success());

    // 4. Fresh launch for notify failed with explicit --task and --execution-id flags
    let task_id_2 = "task_cli_2";
    let launch_params_2 = hands_bridge::journal::LaunchRequestParams {
        pairing_id: setup_res.pairing_id.clone(),
        launch_request_id: task_id_2.to_string(),
        origin_conversation_id: "conv_cli_2".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_cli_2".to_string(),
        transcript_evidence_hash: "hash_t_cli_2".to_string(),
        account_evidence_hash: "hash_a_cli_2".to_string(),
        target_id: "target_cli".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "CLI notify failed prompt".to_string(),
    };
    let claim_2 = journal.reserve_or_claim_launch(&launch_params_2).unwrap();
    journal.mark_launch_attempt(&claim_2.execution_id, &setup_res.pairing_id).unwrap();

    let out_failed = Command::new(bin)
        .args([
            "notify", "failed",
            "--message", "Build failed on cargo check",
            "--task", task_id_2,
            "--execution-id", &claim_2.execution_id,
            "--state-dir", &state_dir.path().to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(out_failed.status.success(), "notify failed failed: {}", String::from_utf8_lossy(&out_failed.stderr));
    let stdout_failed = String::from_utf8_lossy(&out_failed.stdout);
    assert!(stdout_failed.contains("Notification recorded"));
    assert!(stdout_failed.contains("status: failed"));

    let rcpt2 = journal.get_completion_receipt(&claim_2.execution_id).unwrap().unwrap();
    assert_eq!(rcpt2.state, "failed");
    assert_eq!(rcpt2.assistant_text, "Build failed on cargo check");

    // 5. Validation errors
    let out_missing_msg = Command::new(bin)
        .args(["notify", "failed"])
        .output()
        .unwrap();
    assert_eq!(out_missing_msg.status.code(), Some(2));

    let out_unknown_opt = Command::new(bin)
        .args(["notify", "done", "--bogus"])
        .output()
        .unwrap();
    assert_eq!(out_unknown_opt.status.code(), Some(2));
}

#[test]
fn test_prepare_cli_claims_registered_conversation_and_routes_notification_back() {
    let bin = env!("CARGO_BIN_EXE_hands-bridge");
    let state_dir = tempdir().unwrap();
    let db_path = state_dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    // A workspace target exists on the pairing, but the proactive worker path must never use it.
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_target = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let setup_opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: "prof_cli_prepare".to_string(),
        target_path: canonical_target,
        target_id: Some("target_cli_prepare".to_string()),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        extension_id: "test_ext_prepare".to_string(),
        state_dir: Some(state_dir.path().to_path_buf()),
        skip_registry: true,
    };
    let setup_res = execute_setup(&setup_opts).unwrap();
    let activated = journal
        .activate_bootstrap(&setup_res.bootstrap_token, "prof_cli_prepare")
        .unwrap();

    // 1. The paired extension registers the canonical conversation identity directly.
    let register_resp = hands_bridge::protocol::handle_native_message(
        &serde_json::json!({
            "op": "register_conversation",
            "pairingId": setup_res.pairing_id,
            "pairingSecret": activated.pairing_secret,
            "profileId": "prof_cli_prepare",
            "originConversationId": "conv_cli_prepare",
            "originConversationUrl": "https://chatgpt.com/c/conv_cli_prepare"
        }),
        &journal,
    );
    assert_eq!(register_resp["status"], "ok", "registration failed: {}", register_resp);
    assert_eq!(register_resp["isNew"], true);
    assert_eq!(register_resp["originConversationId"], "conv_cli_prepare");

    // 2. CLI prepare claims task/execution for that conversation and prints the worker env.
    let out = Command::new(bin)
        .args([
            "prepare",
            "--conversation",
            "conv_cli_prepare",
            "--state-dir",
            &state_dir.path().to_string_lossy(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "prepare failed: {}", String::from_utf8_lossy(&out.stderr));
    let prepared: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("prepare --json must emit JSON");
    assert_eq!(prepared["origin_conversation_id"], "conv_cli_prepare");
    assert_eq!(prepared["state"], "claimed");
    assert!(prepared.get("return_token").is_none(), "CLI output must not expose the return token");
    let task_id = prepared["task_id"].as_str().unwrap().to_string();
    let execution_id = prepared["execution_id"].as_str().unwrap().to_string();
    assert!(task_id.starts_with("task_"), "task id: {}", task_id);
    assert!(execution_id.starts_with("exec_"), "execution id: {}", execution_id);
    assert_eq!(
        prepared["env"]["HANDS_TASK_ID"].as_str().unwrap(),
        task_id
    );
    assert_eq!(
        prepared["env"]["HANDS_RETURN_BRIDGE_EXECUTION_ID"].as_str().unwrap(),
        execution_id
    );
    assert_eq!(
        prepared["env"]["HANDS_RETURN_BRIDGE_STATE_DIR"].as_str().unwrap(),
        state_dir.path().to_string_lossy()
    );
    assert_eq!(
        prepared["env"]["HANDS_RETURN_BRIDGE_CONVERSATION_ID"].as_str().unwrap(),
        "conv_cli_prepare"
    );

    // 3. The normal Orca OMP worker reports terminal state using only the injected environment.
    let out_done = Command::new(bin)
        .args(["notify", "done"])
        .env("HANDS_RETURN_BRIDGE_STATE_DIR", state_dir.path())
        .env("HANDS_TASK_ID", &task_id)
        .env("HANDS_RETURN_BRIDGE_EXECUTION_ID", &execution_id)
        .output()
        .unwrap();
    assert!(out_done.status.success(), "notify done failed: {}", String::from_utf8_lossy(&out_done.stderr));
    let stdout_done = String::from_utf8_lossy(&out_done.stdout);
    assert!(stdout_done.contains("status: completed"), "stdout: {}", stdout_done);
    assert!(stdout_done.contains(&task_id) && stdout_done.contains(&execution_id));

    // 4. Durable drain projects the exact conversation/task/execution.
    let (summaries, receipts) = journal.drain_records(&setup_res.pairing_id, None).unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].task_id.as_deref(), Some(task_id.as_str()));
    assert_eq!(receipts[0].execution_id, execution_id);
    assert_eq!(receipts[0].origin_conversation_id, "conv_cli_prepare");
    assert_eq!(
        receipts[0].origin_conversation_url.as_deref(),
        Some("https://chatgpt.com/c/conv_cli_prepare")
    );
    assert_eq!(receipts[0].state, "completed");
    let summary = summaries
        .iter()
        .find(|s| s.execution_id == execution_id)
        .expect("Prepared launch must be drained as a launch summary");
    assert_eq!(summary.launch_request_id, task_id);
    assert_eq!(summary.origin_conversation_id, "conv_cli_prepare");

    // 5. An unregistered conversation cannot be prepared, and the refusal mutates nothing.
    let out_unregistered = Command::new(bin)
        .args([
            "prepare",
            "--conversation",
            "conv_cli_absent",
            "--state-dir",
            &state_dir.path().to_string_lossy(),
        ])
        .output()
        .unwrap();
    assert!(!out_unregistered.status.success());
    let stderr_unregistered = String::from_utf8_lossy(&out_unregistered.stderr);
    assert!(
        stderr_unregistered.contains("conversation_not_registered"),
        "stderr: {}",
        stderr_unregistered
    );

    let out_missing_conversation = Command::new(bin).args(["prepare"]).output().unwrap();
    assert_eq!(out_missing_conversation.status.code(), Some(2));

    let out_unknown_opt = Command::new(bin)
        .args(["prepare", "--bogus"])
        .output()
        .unwrap();
    assert_eq!(out_unknown_opt.status.code(), Some(2));

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let launch_rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM launch_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(launch_rows, 1, "Only the successful prepare may allocate a launch row");
}

#[test]
fn test_worker_shorthand_done_and_failed_need_no_identity() {
    let bin = env!("CARGO_BIN_EXE_hands-bridge");
    let state_dir = tempdir().unwrap();
    let db_path = state_dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let pairing_id = "pairing_shorthand";
    let bootstrap_token = "tok_shorthand";
    let profile_id = "prof_shorthand";
    let policy = hands_bridge::journal::PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };
    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &[], &policy)
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();
    for conversation in ["conv_shorthand", "conv_other"] {
        journal
            .register_conversation(&hands_bridge::journal::ConversationRegistrationParams {
                pairing_id: pairing_id.to_string(),
                origin_conversation_id: conversation.to_string(),
                origin_conversation_url: format!("https://chatgpt.com/c/{}", conversation),
            })
            .unwrap();
    }

    let shorthand = |status: &str, conversation: Option<&str>, message: Option<&str>| {
        let mut command = Command::new(bin);
        command.arg(status).arg("--state-dir").arg(state_dir.path());
        if let Some(conversation) = conversation {
            command.args(["--conversation", conversation]);
        }
        if let Some(message) = message {
            command.args(["--message", message]);
        }
        for key in [
            "HANDS_TASK_ID",
            "HANDS_RETURN_BRIDGE_EXECUTION_ID",
            "HANDS_RETURN_BRIDGE_STATE_DIR",
            "HANDS_RETURN_BRIDGE_CONVERSATION_ID",
        ] {
            command.env_remove(key);
        }
        command.output().unwrap()
    };

    // 1. One command, no identity: the CLI claims the execution itself and records the receipt.
    let out_done = shorthand("done", Some("conv_shorthand"), None);
    assert!(out_done.status.success(), "done failed: {}", String::from_utf8_lossy(&out_done.stderr));
    let stdout_done = String::from_utf8_lossy(&out_done.stdout);
    assert!(stdout_done.contains("Notification recorded"), "stdout: {}", stdout_done);
    assert!(stdout_done.contains("conversation: conv_shorthand"), "stdout: {}", stdout_done);
    assert!(stdout_done.contains("status: completed"), "stdout: {}", stdout_done);

    // 2. The conversation, not a caller-supplied identity, is what the receipt routes by.
    let (summaries, receipts) = journal.drain_records(pairing_id, None).unwrap();
    assert_eq!(receipts.len(), 1, "one command must record exactly one receipt");
    assert_eq!(receipts[0].origin_conversation_id, "conv_shorthand");
    assert_eq!(receipts[0].state, "completed");
    assert!(receipts[0].task_id.as_deref().unwrap().starts_with("task_"));
    let summary = summaries
        .iter()
        .find(|s| s.origin_conversation_id == "conv_shorthand")
        .expect("the shorthand must leave a launch summary for the conversation");
    assert_eq!(summary.launch_request_id, receipts[0].task_id.as_deref().unwrap());

    // 3. The failure variant takes the message straight from the command line.
    let out_failed = shorthand("failed", Some("conv_shorthand"), Some("cargo test failed"));
    assert!(out_failed.status.success(), "failed: {}", String::from_utf8_lossy(&out_failed.stderr));
    let stdout_failed = String::from_utf8_lossy(&out_failed.stdout);
    assert!(stdout_failed.contains("status: failed"), "stdout: {}", stdout_failed);
    let (_, receipts) = journal.drain_records(pairing_id, None).unwrap();
    assert_eq!(receipts.len(), 2);
    let failed_receipt = receipts
        .iter()
        .find(|r| r.state == "failed")
        .expect("the failed run must be recorded as failed");
    assert_eq!(failed_receipt.assistant_text, "cargo test failed");

    // 4. Without a conversation and without worker environment, the shorthand fails closed.
    let out_no_conversation = shorthand("done", None, None);
    assert_eq!(out_no_conversation.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out_no_conversation.stderr).contains("usage:"),
        "stderr: {}",
        String::from_utf8_lossy(&out_no_conversation.stderr)
    );
    let (_, receipts) = journal.drain_records(pairing_id, None).unwrap();
    assert_eq!(receipts.len(), 2, "a refused shorthand must not record anything");

    // 5. A worker execution bound to one conversation refuses to report for another.
    let prepared = Command::new(bin)
        .args([
            "prepare",
            "--conversation",
            "conv_other",
            "--state-dir",
            &state_dir.path().to_string_lossy(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(prepared.status.success(), "prepare failed: {}", String::from_utf8_lossy(&prepared.stderr));
    let prepared: serde_json::Value = serde_json::from_slice(&prepared.stdout).unwrap();
    let out_mismatch = Command::new(bin)
        .args([
            "done",
            "--conversation",
            "conv_shorthand",
            "--state-dir",
            &state_dir.path().to_string_lossy(),
        ])
        .env("HANDS_TASK_ID", prepared["task_id"].as_str().unwrap())
        .env(
            "HANDS_RETURN_BRIDGE_EXECUTION_ID",
            prepared["execution_id"].as_str().unwrap(),
        )
        .env("HANDS_RETURN_BRIDGE_CONVERSATION_ID", "conv_other")
        .output()
        .unwrap();
    assert_eq!(out_mismatch.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out_mismatch.stderr).contains("does not match the conversation bound to this worker execution"),
        "stderr: {}",
        String::from_utf8_lossy(&out_mismatch.stderr)
    );
    let (_, receipts) = journal.drain_records(pairing_id, None).unwrap();
    assert_eq!(receipts.len(), 2, "a mismatched shorthand must not record anything");

    // 6. A worker launched by the extension reports with no arguments at all.
    let out_worker = Command::new(bin)
        .args(["done", "--state-dir", &state_dir.path().to_string_lossy()])
        .env("HANDS_TASK_ID", prepared["task_id"].as_str().unwrap())
        .env(
            "HANDS_RETURN_BRIDGE_EXECUTION_ID",
            prepared["execution_id"].as_str().unwrap(),
        )
        .env("HANDS_RETURN_BRIDGE_STATE_DIR", state_dir.path())
        .env("HANDS_RETURN_BRIDGE_CONVERSATION_ID", "conv_other")
        .output()
        .unwrap();
    assert!(out_worker.status.success(), "worker done failed: {}", String::from_utf8_lossy(&out_worker.stderr));
    let stdout_worker = String::from_utf8_lossy(&out_worker.stdout);
    assert!(stdout_worker.contains("conversation: conv_other"), "stdout: {}", stdout_worker);
    let (_, receipts) = journal.drain_records(pairing_id, None).unwrap();
    assert_eq!(receipts.len(), 3);
    let worker_receipt = receipts
        .iter()
        .find(|r| r.execution_id == prepared["execution_id"].as_str().unwrap())
        .expect("the launched worker execution must own the receipt");
    assert_eq!(worker_receipt.origin_conversation_id, "conv_other");
    assert_eq!(worker_receipt.state, "completed");
}

#[test]
fn test_help_is_discoverable_on_stdout() {
    let bin = env!("CARGO_BIN_EXE_hands-bridge");

    // A caller can pipe the usage: --help writes to stdout and exits 0.
    let top = Command::new(bin).arg("--help").output().unwrap();
    assert!(top.status.success());
    let stdout = String::from_utf8_lossy(&top.stdout);
    assert!(stdout.contains("Worker Commands"), "stdout: {}", stdout);
    assert!(stdout.contains("hands-bridge done --conversation"), "stdout: {}", stdout);
    assert!(String::from_utf8_lossy(&top.stderr).is_empty());

    // Per-command help works too instead of being rejected as an unknown option.
    for args in [["done", "--help"], ["failed", "--help"], ["notify", "--help"]] {
        let out = Command::new(bin).args(args).output().unwrap();
        assert!(out.status.success(), "{args:?} help failed");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("Worker Commands"),
            "{args:?} help must print usage on stdout"
        );
    }

    // Errors keep the error stream: usage goes to stderr, stdout stays empty.
    let bad = Command::new(bin).arg("bogus-command").output().unwrap();
    assert!(!bad.status.success());
    assert!(bad.stdout.is_empty());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("Unknown command"));
}
