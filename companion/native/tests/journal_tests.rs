use std::path::PathBuf;
use tempfile::tempdir;

// We will import from hands_return_bridge::journal
use hands_return_bridge::journal::{
    Journal, PairingError, PairingStatus, PolicyRecord, TargetRecord,
};

#[test]
fn test_journal_bootstrap_activate_and_authenticate() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");

    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_test_123";
    let bootstrap_token = "boot_token_abc";
    let pairing_secret = "secret_xyz";
    let browser = "chrome";
    let profile_id = "profile_alpha";
    let targets = vec![TargetRecord {
        target_id: "target_canonical".to_string(),
        canonical_path: "F:\\CodeBase\\hands".to_string(),
        name: "hands".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "read_only".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(
            pairing_id,
            bootstrap_token,
            pairing_secret,
            browser,
            profile_id,
            &targets,
            &policy,
        )
        .expect("Create bootstrap failed");

    // Before activation, status is pending
    let status = journal.get_pairing_status(pairing_id).unwrap();
    assert_eq!(status, PairingStatus::Pending);

    // Activate using bootstrap token
    let activated = journal
        .activate_bootstrap(bootstrap_token, profile_id)
        .expect("Activation failed");
    assert_eq!(activated.pairing_id, pairing_id);
    assert_eq!(activated.profile_id, profile_id);
    assert_eq!(activated.targets.len(), 1);
    assert_eq!(activated.targets[0].target_id, "target_canonical");
    assert_eq!(activated.policy.policy_revision, "v1");

    // Token is one-time: second activation must fail
    let second_act = journal.activate_bootstrap(bootstrap_token, profile_id);
    assert!(second_act.is_err());

    // Status is now active
    assert_eq!(
        journal.get_pairing_status(pairing_id).unwrap(),
        PairingStatus::Active
    );

    // Authenticate with matching credentials
    let auth = journal
        .authenticate_pairing(pairing_id, pairing_secret, profile_id)
        .expect("Authentication failed");
    assert_eq!(auth.pairing_id, pairing_id);
    assert_eq!(auth.profile_id, profile_id);

    // Rejection 1: Mismatched profile ID (N4 profile isolation)
    let err_profile = journal.authenticate_pairing(pairing_id, pairing_secret, "profile_beta");
    assert_eq!(err_profile.unwrap_err(), PairingError::ProfileMismatch);

    // Rejection 2: Wrong secret
    let err_secret = journal.authenticate_pairing(pairing_id, "wrong_secret", profile_id);
    assert_eq!(err_secret.unwrap_err(), PairingError::InvalidSecret);

    // Rejection 3: Unknown pairing
    let err_unknown = journal.authenticate_pairing("unknown_id", pairing_secret, profile_id);
    assert_eq!(err_unknown.unwrap_err(), PairingError::NotFound);

    // Reopen DB to prove committed setup survives host exit/restart
    drop(journal);
    let reopened = Journal::open(&db_path).expect("Failed to reopen journal");

    let auth_reopened = reopened
        .authenticate_pairing(pairing_id, pairing_secret, profile_id)
        .expect("Authentication on reopened DB failed");
    assert_eq!(auth_reopened.pairing_id, pairing_id);

    // Revocation
    reopened
        .revoke_pairing(pairing_id, pairing_secret, profile_id)
        .expect("Revoke failed");

    // After revocation, authentication must fail with Retired
    let err_revoked = reopened.authenticate_pairing(pairing_id, pairing_secret, profile_id);
    assert_eq!(err_revoked.unwrap_err(), PairingError::Retired);
}

#[test]
fn test_bootstrap_persistence_atomic_rollback_on_failure() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");

    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_atomic_test";
    let bootstrap_token = "boot_atomic_abc";
    let pairing_secret = "secret_atomic_xyz";
    let browser = "chrome";
    let profile_id = "profile_alpha";

    // Intentionally create duplicate targets to trigger SQLite PRIMARY KEY (pairing_id, target_id) constraint violation
    let targets = vec![
        TargetRecord {
            target_id: "duplicate_id".to_string(),
            canonical_path: "/workspace/a".to_string(),
            name: "target_a".to_string(),
        },
        TargetRecord {
            target_id: "duplicate_id".to_string(), // Duplicate target_id!
            canonical_path: "/workspace/b".to_string(),
            name: "target_b".to_string(),
        },
    ];

    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    // create_bootstrap must fail due to constraint violation
    let result = journal.create_bootstrap(
        pairing_id,
        bootstrap_token,
        pairing_secret,
        browser,
        profile_id,
        &targets,
        &policy,
    );
    assert!(result.is_err(), "create_bootstrap must return error on target constraint violation");

    // Verify atomic rollback: pairing must NOT exist in the database!
    let status_res = journal.get_pairing_status(pairing_id);
    assert_eq!(status_res.unwrap_err(), PairingError::NotFound, "Rolled back pairing must not exist in pairings table");

    // Targets must also not exist
    let targets_res = journal.get_targets(pairing_id).unwrap();
    assert!(targets_res.is_empty(), "Rolled back targets must be empty");
}
