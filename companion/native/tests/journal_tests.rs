use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

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
            browser,
            profile_id,
            &targets,
            &policy,
        )
        .expect("Create bootstrap failed");

    // Before activation, status is pending
    let status = journal.get_pairing_status(pairing_id).unwrap();
    assert_eq!(status, PairingStatus::Pending);

    // Finding 3: authenticate/connect/status must reject non-Active (Pending) pairings
    let err_pending_auth = journal.authenticate_pairing(pairing_id, "any_secret", profile_id);
    assert_eq!(err_pending_auth.unwrap_err(), PairingError::NotActive);

    // Finding 2: Activation with mismatched profile ID must fail closed
    let err_profile_act = journal.activate_bootstrap(bootstrap_token, "wrong_profile");
    assert_eq!(err_profile_act.unwrap_err(), PairingError::ProfileMismatch);

    // Activate using bootstrap token with correct profile ID
    let activated = journal
        .activate_bootstrap(bootstrap_token, profile_id)
        .expect("Activation failed");
    assert_eq!(activated.pairing_id, pairing_id);
    assert_eq!(activated.profile_id, profile_id);
    assert_eq!(activated.targets.len(), 1);
    assert_eq!(activated.targets[0].target_id, "target_canonical");
    assert_eq!(activated.policy.policy_revision, "v1");
    assert!(activated.pairing_secret.starts_with("rb_sec_"));

    let pairing_secret = activated.pairing_secret;

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
        .authenticate_pairing(pairing_id, &pairing_secret, profile_id)
        .expect("Authentication failed");
    assert_eq!(auth.pairing_id, pairing_id);
    assert_eq!(auth.profile_id, profile_id);

    // Rejection 1: Mismatched profile ID (N4 profile isolation)
    let err_profile = journal.authenticate_pairing(pairing_id, &pairing_secret, "profile_beta");
    assert_eq!(err_profile.unwrap_err(), PairingError::ProfileMismatch);

    // Rejection 2: Wrong secret
    let err_secret = journal.authenticate_pairing(pairing_id, "wrong_secret", profile_id);
    assert_eq!(err_secret.unwrap_err(), PairingError::InvalidSecret);

    // Rejection 3: Unknown pairing
    let err_unknown = journal.authenticate_pairing("unknown_id", &pairing_secret, profile_id);
    assert_eq!(err_unknown.unwrap_err(), PairingError::NotFound);

    // Reopen DB to prove committed setup survives host exit/restart
    drop(journal);
    let reopened = Journal::open(&db_path).expect("Failed to reopen journal");

    let auth_reopened = reopened
        .authenticate_pairing(pairing_id, &pairing_secret, profile_id)
        .expect("Authentication on reopened DB failed");
    assert_eq!(auth_reopened.pairing_id, pairing_id);

    // Revocation
    reopened
        .revoke_pairing(pairing_id, &pairing_secret, profile_id)
        .expect("Revoke failed");

    // After revocation, authentication must fail with Retired
    let err_revoked = reopened.authenticate_pairing(pairing_id, &pairing_secret, profile_id);
    assert_eq!(err_revoked.unwrap_err(), PairingError::Retired);
}

#[test]
fn test_concurrent_activation_exactly_one_winner() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Arc::new(Journal::open(&db_path).expect("Failed to open journal"));

    let pairing_id = "pair_race_test";
    let bootstrap_token = "boot_race_token_123";
    let browser = "chrome";
    let profile_id = "profile_race";
    let targets = vec![TargetRecord {
        target_id: "target_race".to_string(),
        canonical_path: "/workspace/race".to_string(),
        name: "race".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(
            pairing_id,
            bootstrap_token,
            browser,
            profile_id,
            &targets,
            &policy,
        )
        .expect("Create bootstrap failed");

    // Spawn 8 concurrent threads attempting activation with the same token
    let num_threads = 8;
    let mut handles = Vec::new();

    for _ in 0..num_threads {
        let j = Arc::clone(&journal);
        let token = bootstrap_token.to_string();
        let prof = profile_id.to_string();
        handles.push(thread::spawn(move || {
            j.activate_bootstrap(&token, &prof)
        }));
    }

    let mut successes = 0;
    let mut failures = 0;

    for handle in handles {
        match handle.join().expect("Thread panicked") {
            Ok(act) => {
                successes += 1;
                assert_eq!(act.pairing_id, pairing_id);
                assert!(act.pairing_secret.starts_with("rb_sec_"));
            }
            Err(_) => {
                failures += 1;
            }
        }
    }

    assert_eq!(successes, 1, "Exactly one concurrent thread must win activation");
    assert_eq!(failures, num_threads - 1, "All losing concurrent threads must fail closed");
}

#[test]
fn test_no_plaintext_credentials_at_rest() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_cred_test";
    let bootstrap_token = "boot_super_secret_token_never_plain_12345";
    let browser = "chrome";
    let profile_id = "profile_cred";
    let targets = vec![TargetRecord {
        target_id: "target_cred".to_string(),
        canonical_path: "/workspace/cred".to_string(),
        name: "cred".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(
            pairing_id,
            bootstrap_token,
            browser,
            profile_id,
            &targets,
            &policy,
        )
        .expect("Create bootstrap failed");

    let act = journal
        .activate_bootstrap(bootstrap_token, profile_id)
        .expect("Activation failed");
    let pairing_secret = act.pairing_secret;

    // Flush & close connection
    drop(journal);

    // Read raw SQLite database file contents from disk
    let raw_bytes = std::fs::read(&db_path).expect("Failed to read raw sqlite file");
    let raw_text = String::from_utf8_lossy(&raw_bytes);

    // Finding 4: No plaintext bootstrap token or pairing secret at rest in SQLite
    assert!(
        !raw_text.contains(bootstrap_token),
        "Plaintext bootstrap token must NOT exist anywhere in SQLite file at rest"
    );
    assert!(
        !raw_text.contains(&pairing_secret),
        "Plaintext pairing secret must NOT exist anywhere in SQLite file at rest"
    );
}

#[test]
fn test_bootstrap_persistence_atomic_rollback_on_failure() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");

    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_atomic_test";
    let bootstrap_token = "boot_atomic_abc";
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

#[test]
fn test_empty_stored_profile_id_fails_closed_on_auth_and_revoke() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");

    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_corrupt_profile";
    let bootstrap_token = "boot_corrupt_token_123";
    let browser = "chrome";
    let profile_id = "profile_alpha";
    let targets = vec![TargetRecord {
        target_id: "target_corrupt".to_string(),
        canonical_path: "/workspace/corrupt".to_string(),
        name: "corrupt".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(
            pairing_id,
            bootstrap_token,
            browser,
            profile_id,
            &targets,
            &policy,
        )
        .expect("Create bootstrap failed");

    let act = journal
        .activate_bootstrap(bootstrap_token, profile_id)
        .expect("Activation failed");
    let pairing_secret = act.pairing_secret;

    // Corrupt the stored pairing profile_id to empty string directly in SQLite
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE pairings SET profile_id = '' WHERE pairing_id = ?1",
            rusqlite::params![pairing_id],
        )
        .unwrap();
    }

    // Reopen journal or use existing handle
    let reopened = Journal::open(&db_path).expect("Failed to reopen journal");

    // Authenticate with empty string profile must fail closed
    let err_empty = reopened.authenticate_pairing(pairing_id, &pairing_secret, "");
    assert_eq!(
        err_empty.unwrap_err(),
        PairingError::ProfileMismatch,
        "Empty profile_id must not bypass authentication"
    );

    // Authenticate with original profile must fail closed
    let err_orig = reopened.authenticate_pairing(pairing_id, &pairing_secret, "profile_alpha");
    assert_eq!(
        err_orig.unwrap_err(),
        PairingError::ProfileMismatch,
        "Corrupted empty profile must fail closed against original profile"
    );

    // Revoke must also fail closed
    let err_revoke_empty = reopened.revoke_pairing(pairing_id, &pairing_secret, "");
    assert_eq!(
        err_revoke_empty.unwrap_err(),
        PairingError::ProfileMismatch,
        "Revoke with empty profile must fail closed"
    );

    let err_revoke_orig = reopened.revoke_pairing(pairing_id, &pairing_secret, "profile_alpha");
    assert_eq!(
        err_revoke_orig.unwrap_err(),
        PairingError::ProfileMismatch,
        "Revoke against corrupt empty profile must fail closed"
    );
}
