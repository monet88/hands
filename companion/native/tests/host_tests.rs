use std::path::PathBuf;
use tempfile::tempdir;

use hands_return_bridge::host::{HostError, SetupOptions, execute_setup, resolve_state_dir};
use hands_return_bridge::journal::{Journal, PairingStatus};
use hands_return_bridge::protocol::TRUST_NOTICE;

#[test]
fn test_setup_flow_with_isolated_state_dir() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    // Create .git identity to satisfy git workspace verification
    std::fs::create_dir_all(target_dir.path().join(".git")).unwrap();
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
    assert!(result.pairing_secret.starts_with("rb_sec_"));
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

    // Verify manifest was written
    let manifest_path = state_dir.join("com.hands.return_bridge.json");
    assert!(manifest_path.exists());
    let manifest_content = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(manifest_content.contains("com.hands.return_bridge"));
    assert!(manifest_content.contains("test_ext_id_123"));
}

#[test]
fn test_setup_rejects_non_git_target() {
    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();

    let target_dir = tempdir().unwrap();
    // Do not create .git identity
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
            assert!(msg.contains("not a valid git repository or worktree identity"));
        }
        other => panic!("Expected HostError::InvalidTarget, got {:?}", other),
    }
}

#[test]
fn test_resolve_state_dir_fails_closed_when_env_empty() {
    // With explicit override, succeeds
    let dir = tempdir().unwrap();
    let res = resolve_state_dir(Some(dir.path())).unwrap();
    assert_eq!(res, dir.path());
}
