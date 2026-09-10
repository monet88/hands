use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

use hands_return_bridge::journal::{
    compute_payload_digest, AttemptEvidence, LaunchRequestParams,
    Journal, PairingError, PairingStatus, PolicyRecord, TargetRecord,
};

fn init_git_repo(path: &std::path::Path) {
    let output = std::process::Command::new("git")
        .args(["init", &path.to_string_lossy()])
        .output()
        .expect("git init must succeed");
    assert!(output.status.success(), "git init failed");
}

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
        canonical_path: "test_target_canonical".to_string(),
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


#[test]
fn test_launch_reserve_idempotent_claim_and_conflict() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_launch_test";
    let bootstrap_token = "boot_launch_token";
    let profile_id = "profile_launch";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let targets = vec![TargetRecord {
        target_id: "target_1".to_string(),
        canonical_path: canonical_path.clone(),
        name: "target_1".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_100".to_string(),
        origin_conversation_id: "conv_abc".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_abc".to_string(),
        transcript_evidence_hash: "hash_transcript_1".to_string(),
        account_evidence_hash: "hash_account_1".to_string(),
        target_id: "target_1".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Inspect codebase and fix bug".to_string(),
    };

    // 1. Initial claim allocates opaque IDs
    let claim1 = journal.reserve_or_claim_launch(&params).expect("Claim failed");
    assert!(!claim1.is_replayed);
    assert_eq!(claim1.canonical_target_path, canonical_path);
    assert_eq!(claim1.state, "claimed");
    assert!(claim1.execution_id.starts_with("exec_"));
    assert!(claim1.return_token.starts_with("ret_"));

    // 2. Idempotent retry returns identical execution without side effect
    let claim2 = journal.reserve_or_claim_launch(&params).expect("Retry claim failed");
    assert!(claim2.is_replayed);
    assert_eq!(claim2.execution_id, claim1.execution_id);
    assert_eq!(claim2.return_token, claim1.return_token);
    assert_eq!(claim2.prompt_text, claim1.prompt_text);

    // 3. Changed payload under the same request key yields explicit conflict
    let mut conflict_params = params.clone();
    conflict_params.prompt_text = "Different prompt text!".to_string();
    let err_conflict = journal.reserve_or_claim_launch(&conflict_params);
    assert_eq!(err_conflict.unwrap_err(), PairingError::PayloadConflict);

    // 4. Mark launch attempt: first call succeeds, second fails closed
    let attempt1 = journal.mark_launch_attempt(&claim1.execution_id, pairing_id).unwrap();
    assert!(attempt1, "First launch attempt mark must succeed");

    let attempt2 = journal.mark_launch_attempt(&claim1.execution_id, pairing_id).unwrap();
    assert!(!attempt2, "Second attempt mark must return false");

    // 5. Record start evidence
    let evidence = AttemptEvidence {
        orca_terminal_handle: Some("term_12345".to_string()),
        orca_tab_id: Some("tab_abc".to_string()),
        orca_pane_key: Some("pane_xyz".to_string()),
        orca_pty_id: Some("pty_999".to_string()),
    };
    journal.record_launch_start_evidence(&claim1.execution_id, &evidence).unwrap();
    journal.record_launch_started(&claim1.execution_id, &evidence).unwrap();

    // 6. Inspect launch summaries
    let summaries = journal.get_launch_summaries(pairing_id).unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].launch_request_id, "req_100");
    assert_eq!(summaries[0].execution_id, claim1.execution_id);
    assert_eq!(summaries[0].state, "started");
    assert_eq!(summaries[0].orca_terminal_handle.as_deref(), Some("term_12345"));

    // 7. Pruning detailed launch_request row activates tombstone guard:
    // Same payload on tombstoned key returns ReplayTombstoned; changed payload returns PayloadConflict
    {
        // Open raw connection to simulate retention pruning of launch_requests
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("DELETE FROM launch_requests WHERE launch_request_id = 'req_100'", []).unwrap();
    }
    let err_tombstoned = journal.reserve_or_claim_launch(&params);
    assert_eq!(err_tombstoned.unwrap_err(), PairingError::ReplayTombstoned);

    let mut conflict_after_prune = params.clone();
    conflict_after_prune.prompt_text = "Completely different after prune".to_string();
    let err_conflict_pruned = journal.reserve_or_claim_launch(&conflict_after_prune);
    assert_eq!(err_conflict_pruned.unwrap_err(), PairingError::PayloadConflict);
}
#[test]
fn test_concurrent_launch_claims_converge_on_same_execution() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Arc::new(Journal::open(&db_path).expect("Failed to open journal"));

    let pairing_id = "pair_launch_race";
    let bootstrap_token = "boot_launch_race";
    let profile_id = "profile_race";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let targets = vec![TargetRecord {
        target_id: "target_race".to_string(),
        canonical_path: canonical_path.clone(),
        name: "target_race".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_race_1".to_string(),
        origin_conversation_id: "conv_race".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_race".to_string(),
        transcript_evidence_hash: "hash_race".to_string(),
        account_evidence_hash: "hash_acc".to_string(),
        target_id: "target_race".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Concurrent launch test".to_string(),
    };

    let num_threads = 8;
    let mut handles = Vec::new();

    for _ in 0..num_threads {
        let j = Arc::clone(&journal);
        let p = params.clone();
        handles.push(thread::spawn(move || {
            j.reserve_or_claim_launch(&p)
        }));
    }

    let mut execution_ids = Vec::new();
    for handle in handles {
        let claim = handle.join().unwrap().expect("Claim should succeed");
        execution_ids.push(claim.execution_id);
    }

    // All threads must converge on the exact same executionId!
    let first_id = &execution_ids[0];
    for id in &execution_ids {
        assert_eq!(id, first_id, "All concurrent threads must converge on identical executionId");
    }
}

#[test]
fn test_workspace_path_with_spaces_and_unicode() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_unicode_space";
    let bootstrap_token = "boot_unicode_space";
    let profile_id = "profile_unicode";

    // Create workspace directory with spaces and non-ASCII characters
    let target_parent = dir.path().join("space and unicode test đặng 123");
    std::fs::create_dir_all(&target_parent).unwrap();
    init_git_repo(&target_parent);
    let canonical_path = target_parent.canonicalize().unwrap().to_string_lossy().to_string();

    let targets = vec![TargetRecord {
        target_id: "target_unicode".to_string(),
        canonical_path: canonical_path.clone(),
        name: "target_unicode".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_unicode_1".to_string(),
        origin_conversation_id: "conv_unicode".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_unicode".to_string(),
        transcript_evidence_hash: "hash_t_u".to_string(),
        account_evidence_hash: "hash_a_u".to_string(),
        target_id: "target_unicode".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Test prompt for workspace with spaces & unicode".to_string(),
    };

    let claim = journal.reserve_or_claim_launch(&params).expect("Claim with spaces and unicode target must succeed");
    assert_eq!(claim.canonical_target_path, canonical_path);
    assert_eq!(claim.state, "claimed");
}

#[test]
fn test_verify_git_target_identity_rejects_subdirectory() {
    let dir = tempdir().unwrap();
    init_git_repo(dir.path());
    let subdir = dir.path().join("src").join("nested");
    std::fs::create_dir_all(&subdir).unwrap();

    // OpenCode claim: subdirectories should be accepted.
    // Requirement: Keep exact canonical worktree-root identity fail-closed; reject subdirectories.
    let res = Journal::verify_git_target_identity(&subdir.to_string_lossy());
    assert_eq!(res.unwrap_err(), PairingError::TargetRevokedOrMismatched);
}

#[test]
fn test_crash_immediately_after_attempt_mark_and_replay_semantics() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_crash_test";
    let bootstrap_token = "boot_crash_token";
    let profile_id = "profile_crash";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let targets = vec![TargetRecord {
        target_id: "target_crash".to_string(),
        canonical_path,
        name: "target_crash".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_crash_1".to_string(),
        origin_conversation_id: "conv_crash".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_crash".to_string(),
        transcript_evidence_hash: "hash_crash_t".to_string(),
        account_evidence_hash: "hash_crash_a".to_string(),
        target_id: "target_crash".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Crash recovery probe".to_string(),
    };

    let claim = journal.reserve_or_claim_launch(&params).unwrap();
    assert_eq!(claim.state, "claimed");

    // Finding 3: mark_launch_attempt MUST persist an unresolved/unknown state at that boundary,
    // NOT 'attempting'. If process dies right after attempt mark, replay/recovery must see 'unknown'.
    let marked = journal.mark_launch_attempt(&claim.execution_id, pairing_id).unwrap();
    assert!(marked, "First attempt mark must succeed");

    // Simulate process death & reboot by reopening SQLite DB from disk
    drop(journal);
    let journal_reboot = Journal::open(&db_path).expect("Reopened journal after simulated crash");

    // Summary after crash must report 'unknown', not 'attempting'
    let summary = journal_reboot.get_launch_request_by_id(pairing_id, "req_crash_1").unwrap().expect("Summary must exist");
    assert_eq!(summary.state, "unknown", "State after crash around attempt mark must be 'unknown', not 'attempting'");

    // Replay of same request must NOT invoke a second attempt and must return 'unknown'
    let replay_claim = journal_reboot.reserve_or_claim_launch(&params).unwrap();
    assert!(replay_claim.is_replayed);
    assert_eq!(replay_claim.state, "unknown");
    assert_eq!(replay_claim.execution_id, claim.execution_id);

    // Automatic re-invocation is forbidden: attempt mark returns false
    let second_attempt = journal_reboot.mark_launch_attempt(&claim.execution_id, pairing_id).unwrap();
    assert!(!second_attempt, "Replay must never allow a second launch attempt");
}

#[test]
fn test_zero_side_effects_on_rejected_journal_launches() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_zero_side_effect";
    let bootstrap_token = "boot_zero";
    let profile_id = "profile_zero";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let targets = vec![TargetRecord {
        target_id: "target_zero".to_string(),
        canonical_path,
        name: "target_zero".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    // Case A: Unknown target
    let mut bad_target_params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_bad_t".to_string(),
        origin_conversation_id: "c_1".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/c_1".to_string(),
        transcript_evidence_hash: "hash_t".to_string(),
        account_evidence_hash: "hash_a".to_string(),
        target_id: "nonexistent_target".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "test".to_string(),
    };
    let res_t = journal.reserve_or_claim_launch(&bad_target_params);
    assert_eq!(res_t.unwrap_err(), PairingError::TargetNotFound);

    // Case B: Policy mismatch
    bad_target_params.target_id = "target_zero".to_string();
    bad_target_params.policy_revision = "v_mismatch".to_string();
    let res_p = journal.reserve_or_claim_launch(&bad_target_params);
    assert_eq!(res_p.unwrap_err(), PairingError::PolicyMismatch);

    // Verify ZERO side effects in journal tables
    let summaries = journal.get_launch_summaries(pairing_id).unwrap();
    assert_eq!(summaries.len(), 0, "Zero launch requests must exist after rejected launch claims");
}

#[test]
fn test_get_launch_summaries_bounded_to_32() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let pairing_id = "pair_bounded_test";
    let bootstrap_token = "boot_bounded_test";
    let profile_id = "profile_bounded";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let targets = vec![TargetRecord {
        target_id: "hands".to_string(),
        canonical_path: canonical_path.clone(),
        name: "hands".to_string(),
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
            "chrome",
            profile_id,
            &targets,
            &policy,
        )
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    // Insert 40 launch requests
    for i in 0..40 {
        let params = LaunchRequestParams {
            pairing_id: pairing_id.to_string(),
            launch_request_id: format!("req_bounded_{}", i),
            origin_conversation_id: "c_bound".to_string(),
            origin_conversation_url: "https://chatgpt.com/c/c_bound".to_string(),
            transcript_evidence_hash: format!("hash_t_{}", i),
            account_evidence_hash: "hash_a".to_string(),
            target_id: "hands".to_string(),
            policy_revision: "v1".to_string(),
            prompt_text: format!("prompt {}", i),
        };
        let claim = journal.reserve_or_claim_launch(&params).unwrap();
        assert!(!claim.is_replayed);
    }

    // Verify bounded to exactly 32 summaries
    let summaries = journal.get_launch_summaries(pairing_id).unwrap();
    assert_eq!(summaries.len(), 32, "Summaries must be bounded to 32 recent entries");
}

#[test]
fn test_replay_preserves_execution_even_if_registered_policy_changes_later() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let pairing_id = "pair_replay_policy_test";
    let bootstrap_token = "boot_replay_policy_test";
    let profile_id = "profile_replay";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let targets = vec![TargetRecord {
        target_id: "hands".to_string(),
        canonical_path: canonical_path.clone(),
        name: "hands".to_string(),
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
            "chrome",
            profile_id,
            &targets,
            &policy,
        )
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_replay_1".to_string(),
        origin_conversation_id: "c_replay".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/c_replay".to_string(),
        transcript_evidence_hash: "hash_t".to_string(),
        account_evidence_hash: "hash_a".to_string(),
        target_id: "hands".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "do task".to_string(),
    };
    let claim1 = journal.reserve_or_claim_launch(&params).unwrap();
    assert!(!claim1.is_replayed);
    let original_exec_id = claim1.execution_id;

    // Modify target's registered policy in SQLite to v2
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "UPDATE policies SET policy_revision = 'v2' WHERE pairing_id = ?1",
            rusqlite::params![pairing_id],
        ).unwrap();
    }

    // Replaying original request (same launch_request_id + payload) MUST return the existing execution
    // (recovery/idempotency across process restarts or policy changes)
    let claim_replay = journal.reserve_or_claim_launch(&params).unwrap();
    assert!(claim_replay.is_replayed);
    assert_eq!(claim_replay.execution_id, original_exec_id);

    // A NEW request with v1 policy now fails policy revalidation because registered target is now v2
    let mut new_params = params.clone();
    new_params.launch_request_id = "req_new_different".to_string();
    new_params.prompt_text = "new task".to_string();
    let new_res = journal.reserve_or_claim_launch(&new_params);
    assert_eq!(new_res.unwrap_err(), PairingError::PolicyMismatch);
}

#[test]
fn test_unsupported_registered_policy_fails_closed_without_allocating_claim_or_tombstone() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");
    let pairing_id = "pair_unsupported_pol";
    let bootstrap_token = "bt_unsupp";
    let profile_id = "prof_unsupp";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_canonical = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let targets = vec![TargetRecord {
        target_id: "hands".to_string(),
        canonical_path: target_canonical,
        name: "hands".to_string(),
    }];

    // Seed a registered policy with unsupported tool_policy = "unrestricted"
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "unrestricted".to_string(),
        approval_policy: "prompt".to_string(),
    };
    journal
        .create_bootstrap(
            pairing_id,
            bootstrap_token,
            "chrome",
            profile_id,
            &targets,
            &policy,
        )
        .unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_unsupported_1".to_string(),
        origin_conversation_id: "c_unsupported".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/c_unsupported".to_string(),
        transcript_evidence_hash: "hash_t".to_string(),
        account_evidence_hash: "hash_a".to_string(),
        target_id: "hands".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "do unrestricted task".to_string(),
    };

    // 1. A NEW request with unsupported policy MUST fail before claim or tombstone allocation
    let claim_res = journal.reserve_or_claim_launch(&params);
    assert_eq!(claim_res.unwrap_err(), PairingError::PolicyUnsupported);

    // 2. Prove ZERO launch_request, ZERO tombstone, and ZERO attempt in SQLite
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let lr_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM launch_requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(lr_count, 0, "Must be zero launch_requests in database");

        let rt_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM replay_tombstones", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rt_count, 0, "Must be zero replay_tombstones in database");
    }

    // 3. Preserve replay semantics: an already-accepted identical key replays its original execution
    // Simulate an existing accepted launch request in DB
    let original_exec_id = "exec_pre_accepted_99";
    let original_ret_token = "ret_token_99";
    let payload_digest = hands_return_bridge::journal::compute_payload_digest(
        &params.origin_conversation_id,
        &params.origin_conversation_url,
        &params.transcript_evidence_hash,
        &params.account_evidence_hash,
        &params.target_id,
        &params.policy_revision,
        &params.prompt_text,
    );
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO launch_requests (
                pairing_id, launch_request_id, execution_id, return_token,
                origin_conversation_id, origin_conversation_url,
                transcript_evidence_hash, account_evidence_hash,
                target_id, canonical_target_path,
                policy_revision, effective_tool_policy, effective_approval_policy,
                prompt_text, payload_digest, state, created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, 1000, 1000
            )
            "#,
            rusqlite::params![
                pairing_id,
                params.launch_request_id,
                original_exec_id,
                original_ret_token,
                params.origin_conversation_id,
                params.origin_conversation_url,
                params.transcript_evidence_hash,
                params.account_evidence_hash,
                params.target_id,
                targets[0].canonical_path,
                params.policy_revision,
                "standard",
                "prompt",
                params.prompt_text,
                payload_digest,
                "claimed",
            ],
        ).unwrap();
    }

    let replay_claim = journal.reserve_or_claim_launch(&params).unwrap();
    assert!(replay_claim.is_replayed, "Accepted request must replay");
    assert_eq!(replay_claim.execution_id, original_exec_id);
    assert_eq!(replay_claim.return_token, original_ret_token);
}

fn insert_test_receipt(
    db_path: &std::path::Path,
    receipt_id: &str,
    execution_id: &str,
    pairing_id: &str,
    return_token: &str,
    text: &str,
) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        r#"
        INSERT INTO completion_receipts (
            receipt_id, execution_id, pairing_id, return_token,
            origin_conversation_id, turn_index, stop_reason,
            assistant_message_id, assistant_text, content_digest,
            tool_call_count, state, committed_at
        ) VALUES (?1, ?2, ?3, ?4, 'conv_rcpt_1', 0, 'stop', 'msg_final_1', ?5, 'digest_123', 3, 'completed', 1000)
        "#,
        rusqlite::params![receipt_id, execution_id, pairing_id, return_token, text],
    ).unwrap();
    conn.execute(
        "UPDATE launch_requests SET state = 'completed', updated_at = 1000 WHERE execution_id = ?1",
        rusqlite::params![execution_id],
    ).unwrap();
}

#[test]
fn test_completion_receipt_readability_across_reopen() {
    let dir = tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);

    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let pairing_id = "pair_rcpt_1";
    let boot_token = "boot_rcpt_1";
    let targets = vec![TargetRecord {
        target_id: "t_main".to_string(),
        canonical_path: repo_dir.to_string_lossy().to_string(),
        name: "test".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal.create_bootstrap(pairing_id, boot_token, "chrome", "profile_1", &targets, &policy).unwrap();
    journal.activate_bootstrap(boot_token, "profile_1").unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_rcpt_1".to_string(),
        origin_conversation_id: "conv_rcpt_1".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_rcpt_1".to_string(),
        transcript_evidence_hash: "hash_trans_1".to_string(),
        account_evidence_hash: "hash_acct_1".to_string(),
        target_id: "t_main".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Test prompt for receipt".to_string(),
    };

    let claim = journal.reserve_or_claim_launch(&params).unwrap();
    assert_eq!(claim.state, "claimed");

    journal.mark_launch_attempt(&claim.execution_id, pairing_id).unwrap();
    let ev = AttemptEvidence {
        orca_terminal_handle: Some("term_rcpt_1".to_string()),
        orca_tab_id: Some("tab_rcpt_1".to_string()),
        orca_pane_key: Some("pane_rcpt_1".to_string()),
        orca_pty_id: Some("pty_rcpt_1".to_string()),
    };
    journal.record_launch_started(&claim.execution_id, &ev).unwrap();

    // Commit receipt via SQLite
    let receipt_id = "rcpt_test_read_1";
    insert_test_receipt(
        &db_path,
        receipt_id,
        &claim.execution_id,
        pairing_id,
        &claim.return_token,
        "Final completed turn result from agent",
    );

    // Verify state transitioned to completed and receipt is returned
    let summary_before = journal.get_launch_request_by_id(pairing_id, &params.launch_request_id).unwrap().unwrap();
    assert_eq!(summary_before.state, "completed");
    let rcpt = summary_before.completion_receipt.unwrap();
    assert_eq!(rcpt.receipt_id, receipt_id);
    assert_eq!(rcpt.execution_id, claim.execution_id);
    assert_eq!(rcpt.pairing_id, pairing_id);
    assert_eq!(rcpt.assistant_text, "Final completed turn result from agent");
    assert_eq!(rcpt.tool_call_count, 3);

    // Drop original journal to simulate producer / native-host exit
    drop(journal);

    // Reopen journal fresh and verify receipt survives
    let journal2 = Journal::open(&db_path).unwrap();
    let loaded = journal2.get_completion_receipt(&claim.execution_id).unwrap().unwrap();
    assert_eq!(loaded, rcpt);

    let summary_after = journal2.get_launch_request_by_id(pairing_id, &params.launch_request_id).unwrap().unwrap();
    assert_eq!(summary_after.state, "completed");
    assert_eq!(summary_after.completion_receipt, Some(rcpt));
}

#[test]
fn test_execution_adapter_claims_one_time_uniqueness() {
    let dir = tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);

    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let pairing_id = "pair_claim_test";
    let boot_token = "boot_claim_test";
    let targets = vec![TargetRecord {
        target_id: "t_main".to_string(),
        canonical_path: repo_dir.to_string_lossy().to_string(),
        name: "test".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal.create_bootstrap(pairing_id, boot_token, "chrome", "profile_claim", &targets, &policy).unwrap();
    journal.activate_bootstrap(boot_token, "profile_claim").unwrap();

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_claim_1".to_string(),
        origin_conversation_id: "conv_claim_1".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_claim_1".to_string(),
        transcript_evidence_hash: "thash".to_string(),
        account_evidence_hash: "ahash".to_string(),
        target_id: "t_main".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Claim test prompt".to_string(),
    };

    let claim = journal.reserve_or_claim_launch(&params).unwrap();

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // First adapter claims ownership
    let first_insert = conn.execute(
        "INSERT INTO execution_adapter_claims (execution_id, adapter_instance_id, session_id, claimed_at) VALUES (?1, 'adp_owner_1', 'sess_1', 1000)",
        rusqlite::params![&claim.execution_id],
    );
    assert!(first_insert.is_ok(), "First adapter claim must succeed");

    // Second adapter instance attempts to claim same execution_id -> MUST fail closed on PRIMARY KEY
    let second_insert = conn.execute(
        "INSERT INTO execution_adapter_claims (execution_id, adapter_instance_id, session_id, claimed_at) VALUES (?1, 'adp_intruder_2', 'sess_2', 1001)",
        rusqlite::params![&claim.execution_id],
    );
    assert!(second_insert.is_err(), "Second adapter claim must fail due to unique constraint");

    // Verify the owner remains adp_owner_1
    let owner: String = conn.query_row(
        "SELECT adapter_instance_id FROM execution_adapter_claims WHERE execution_id = ?1",
        rusqlite::params![&claim.execution_id],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(owner, "adp_owner_1");
}

#[test]
fn test_payload_digest_preserves_field_boundaries() {
    let digest_a = compute_payload_digest(
        "a:b", "c", "hash_t", "hash_a", "target", "v1", "prompt",
    );
    let digest_b = compute_payload_digest(
        "a", "b:c", "hash_t", "hash_a", "target", "v1", "prompt",
    );
    assert_ne!(digest_a, digest_b, "Field boundaries must be unambiguous in the payload digest");
}

#[test]
fn test_revoked_pairing_cannot_claim_or_replay_launch() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let pairing_id = "pair_revoke_claim";
    let bootstrap_token = "boot_revoke_claim";
    let profile_id = "profile_revoke_claim";
    journal.create_bootstrap(
        pairing_id,
        bootstrap_token,
        "chrome",
        profile_id,
        &[TargetRecord { target_id: "target_revoke".into(), canonical_path, name: "target_revoke".into() }],
        &PolicyRecord { policy_revision: "v1".into(), tool_policy: "standard".into(), approval_policy: "prompt".into() },
    ).unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();
    let params = LaunchRequestParams {
        pairing_id: pairing_id.into(),
        launch_request_id: "req_revoke_claim".into(),
        origin_conversation_id: "conv_revoke".into(),
        origin_conversation_url: "https://chatgpt.com/c/conv_revoke".into(),
        transcript_evidence_hash: "hash_t_revoke".into(),
        account_evidence_hash: "hash_a_revoke".into(),
        target_id: "target_revoke".into(),
        policy_revision: "v1".into(),
        prompt_text: "revoke race".into(),
    };

    let first = journal.reserve_or_claim_launch(&params).unwrap();
    assert!(!first.is_replayed);
    journal.revoke_pairing_admin(pairing_id).unwrap();

    let replay_err = journal.reserve_or_claim_launch(&params).unwrap_err();
    assert_eq!(replay_err, PairingError::Retired);

    let mut fresh = params.clone();
    fresh.launch_request_id = "req_after_revoke".into();
    let fresh_err = journal.reserve_or_claim_launch(&fresh).unwrap_err();
    assert_eq!(fresh_err, PairingError::Retired);
}

#[test]
fn test_journal_drain_and_ack_completion_receipts() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let pairing_id_1 = "pair_drain_1";
    let pairing_id_2 = "pair_drain_2";
    let profile_1 = "profile_drain_1";
    let profile_2 = "profile_drain_2";

    journal.create_bootstrap(
        pairing_id_1,
        "boot_1",
        "chrome",
        profile_1,
        &[TargetRecord { target_id: "target_1".into(), canonical_path: canonical_path.clone(), name: "target_1".into() }],
        &PolicyRecord { policy_revision: "v1".into(), tool_policy: "standard".into(), approval_policy: "prompt".into() },
    ).unwrap();
    journal.activate_bootstrap("boot_1", profile_1).unwrap();

    journal.create_bootstrap(
        pairing_id_2,
        "boot_2",
        "chrome",
        profile_2,
        &[TargetRecord { target_id: "target_2".into(), canonical_path, name: "target_2".into() }],
        &PolicyRecord { policy_revision: "v1".into(), tool_policy: "standard".into(), approval_policy: "prompt".into() },
    ).unwrap();
    journal.activate_bootstrap("boot_2", profile_2).unwrap();

    let params_1 = LaunchRequestParams {
        pairing_id: pairing_id_1.into(),
        launch_request_id: "req_drain_1".into(),
        origin_conversation_id: "conv_drain_1".into(),
        origin_conversation_url: "https://chatgpt.com/c/conv_drain_1".into(),
        transcript_evidence_hash: "hash_t1".into(),
        account_evidence_hash: "hash_a1".into(),
        target_id: "target_1".into(),
        policy_revision: "v1".into(),
        prompt_text: "task 1".into(),
    };
    let claim_1 = journal.reserve_or_claim_launch(&params_1).unwrap();

    let params_2 = LaunchRequestParams {
        pairing_id: pairing_id_2.into(),
        launch_request_id: "req_drain_2".into(),
        origin_conversation_id: "conv_drain_2".into(),
        origin_conversation_url: "https://chatgpt.com/c/conv_drain_2".into(),
        transcript_evidence_hash: "hash_t2".into(),
        account_evidence_hash: "hash_a2".into(),
        target_id: "target_2".into(),
        policy_revision: "v1".into(),
        prompt_text: "task 2".into(),
    };
    let claim_2 = journal.reserve_or_claim_launch(&params_2).unwrap();

    // Before receipt commit: drain for pairing 1 returns summaries but 0 receipts
    let (summaries_pre, receipts_pre) = journal.drain_records(pairing_id_1, None).unwrap();
    assert_eq!(summaries_pre.len(), 1);
    assert_eq!(summaries_pre[0].launch_request_id, "req_drain_1");
    assert_eq!(receipts_pre.len(), 0);

    // Commit completion receipt for claim 1
    let receipt_id_1 = "rcpt_drain_1";
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            r#"
            INSERT INTO completion_receipts (
                receipt_id, execution_id, pairing_id, return_token,
                origin_conversation_id, turn_index, stop_reason,
                assistant_message_id, assistant_text, content_digest,
                tool_call_count, state, committed_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'completed', ?12)
            "#,
            rusqlite::params![
                receipt_id_1,
                claim_1.execution_id,
                pairing_id_1,
                claim_1.return_token,
                "conv_drain_1",
                0,
                "stop",
                "msg_1",
                "Turn 1 done",
                "digest_1",
                1,
                1000
            ],
        ).unwrap();
    }

    // Drain pairing 1: should return the unacknowledged receipt
    let (summaries_1, receipts_1) = journal.drain_records(pairing_id_1, None).unwrap();
    assert_eq!(summaries_1.len(), 1);
    assert_eq!(receipts_1.len(), 1);
    assert_eq!(receipts_1[0].receipt_id, receipt_id_1);
    assert_eq!(receipts_1[0].execution_id, claim_1.execution_id);
    assert_eq!(receipts_1[0].pairing_id, pairing_id_1);

    // Isolation (N4): Profile/pairing 2 drain must NOT see pairing 1's receipt or summary
    let (summaries_2, receipts_2) = journal.drain_records(pairing_id_2, None).unwrap();
    assert_eq!(summaries_2.len(), 1);
    assert_eq!(summaries_2[0].launch_request_id, "req_drain_2");
    assert_eq!(receipts_2.len(), 0, "Pairing 2 must not see pairing 1's completion receipt");

    // ACK receipt from wrong pairing fails closed (N4 isolation)
    let ack_wrong = journal.acknowledge_receipt(pairing_id_2, receipt_id_1, &claim_1.execution_id, "received");
    assert!(ack_wrong.is_err());

    // ACK receipt with mismatched execution_id fails closed
    let ack_mismatch = journal.acknowledge_receipt(pairing_id_1, receipt_id_1, "wrong_exec", "received");
    assert!(ack_mismatch.is_err());

    // Valid ACK receipt for pairing 1
    let ack_ok = journal.acknowledge_receipt(pairing_id_1, receipt_id_1, &claim_1.execution_id, "received").unwrap();
    assert!(ack_ok, "First ACK must record true");

    // Replay ACK is idempotent and safe
    let ack_replay = journal.acknowledge_receipt(pairing_id_1, receipt_id_1, &claim_1.execution_id, "received").unwrap();
    assert!(ack_replay, "Replay ACK must be idempotent");

    // Next drain for pairing 1: receipt is already acknowledged so it is NOT returned in pending unacked receipts
    let (summaries_after, receipts_after) = journal.drain_records(pairing_id_1, None).unwrap();
    assert_eq!(summaries_after.len(), 1);
    assert_eq!(receipts_after.len(), 0, "Acknowledged receipt must not be returned in unacked drain");

    // However, receipt remains safely in completion_receipts journal for later delivery (Issue #70)
    let stored_rcpt = journal.get_completion_receipt(&claim_1.execution_id).unwrap();
    assert!(stored_rcpt.is_some(), "Durable receipt must remain in journal after transport ACK");
}

#[test]
fn test_dispatch_fence_acquisition_contention_and_cas_settlement() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let pairing_id = "pair_fence_test";
    let bootstrap_token = "boot_fence_test";
    let profile_id = "profile_fence_test";
    journal.create_bootstrap(
        pairing_id,
        bootstrap_token,
        "chrome",
        profile_id,
        &[TargetRecord { target_id: "target_1".into(), canonical_path, name: "target_1".into() }],
        &PolicyRecord { policy_revision: "v1".into(), tool_policy: "standard".into(), approval_policy: "prompt".into() },
    ).unwrap();
    journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    // Seed two launch requests and completion receipts in the SAME conversation
    let conv_id = "conv_shared_fence";
    let conv_url = format!("https://chatgpt.com/c/{}", conv_id);

    let claim1 = journal.reserve_or_claim_launch(&LaunchRequestParams {
        pairing_id: pairing_id.into(),
        launch_request_id: "req_fence_1".into(),
        origin_conversation_id: conv_id.into(),
        origin_conversation_url: conv_url.clone(),
        transcript_evidence_hash: "hash_t1".into(),
        account_evidence_hash: "hash_a1".into(),
        target_id: "target_1".into(),
        policy_revision: "v1".into(),
        prompt_text: "task 1".into(),
    }).unwrap();

    let claim2 = journal.reserve_or_claim_launch(&LaunchRequestParams {
        pairing_id: pairing_id.into(),
        launch_request_id: "req_fence_2".into(),
        origin_conversation_id: conv_id.into(),
        origin_conversation_url: conv_url.clone(),
        transcript_evidence_hash: "hash_t2".into(),
        account_evidence_hash: "hash_a2".into(),
        target_id: "target_1".into(),
        policy_revision: "v1".into(),
        prompt_text: "task 2".into(),
    }).unwrap();

    let rcpt1_id = "rcpt_fence_1";
    let rcpt2_id = "rcpt_fence_2";
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO completion_receipts VALUES (?1, ?2, ?3, ?4, ?5, 0, 'stop', 'msg_1', 'assistant finished', 'digest_1', 1, 'completed', 1000)",
            rusqlite::params![rcpt1_id, &claim1.execution_id, pairing_id, &claim1.return_token, conv_id],
        ).unwrap();
        conn.execute(
            "INSERT INTO completion_receipts VALUES (?1, ?2, ?3, ?4, ?5, 0, 'stop', 'msg_2', 'assistant finished 2', 'digest_2', 1, 'completed', 1001)",
            rusqlite::params![rcpt2_id, &claim2.execution_id, pairing_id, &claim2.return_token, conv_id],
        ).unwrap();
    }

    // 1. Initial Grant Acquisition for receipt 1
    let claim_params_1 = hands_return_bridge::journal::DispatchClaimParams {
        pairing_id: pairing_id.into(),
        receipt_id: rcpt1_id.into(),
        execution_id: claim1.execution_id.clone(),
        attempt_id: "attempt_1_alpha".into(),
        expected_delivery_revision: 1,
        payload_digest: "digest_payload_stable".into(),
        receipt_marker: "marker_stable_1".into(),
        origin_conversation_id: conv_id.into(),
        origin_conversation_url: conv_url.clone(),
        account_evidence_hash: "hash_a1".into(),
        transcript_evidence_hash: "hash_t1".into(),
        tab_id: Some("tab_1".into()),
        document_id: "doc_1_alpha".into(),
    };

    let grant1 = journal.acquire_dispatch_fence(&claim_params_1).unwrap();
    assert!(grant1.granted, "First dispatch fence grant must succeed");
    assert_eq!(grant1.state, "dispatching/uncertain");
    assert_eq!(grant1.attempt_id, "attempt_1_alpha");
    assert_eq!(grant1.owner_document_id, "doc_1_alpha");

    // 2. Exact same owner replay is idempotent and returns granted=true
    let replay1 = journal.acquire_dispatch_fence(&claim_params_1).unwrap();
    assert!(replay1.granted, "Replay of exact attempt and document must return granted=true");
    assert_eq!(replay1.attempt_id, "attempt_1_alpha");

    // 3. Competing tab/document attempting receipt 1 receives status, NEVER permission (granted=false)
    let mut competing_params_1 = claim_params_1.clone();
    competing_params_1.attempt_id = "attempt_1_beta".into();
    competing_params_1.document_id = "doc_1_beta".into();
    competing_params_1.tab_id = Some("tab_2".into());

    let competing_grant = journal.acquire_dispatch_fence(&competing_params_1).unwrap();
    assert!(!competing_grant.granted, "Competing attempt on same receipt MUST be denied grant");
    assert_eq!(competing_grant.state, "dispatching/uncertain");
    assert_eq!(competing_grant.owner_document_id, "doc_1_alpha", "Owner document remains doc_1_alpha");

    // 4. Conversation Slot Contention: Receipt 2 in SAME conversation is BLOCKED by active conversation slot!
    let claim_params_2 = hands_return_bridge::journal::DispatchClaimParams {
        pairing_id: pairing_id.into(),
        receipt_id: rcpt2_id.into(),
        execution_id: claim2.execution_id.clone(),
        attempt_id: "attempt_2_alpha".into(),
        expected_delivery_revision: 1,
        payload_digest: "digest_payload_2".into(),
        receipt_marker: "marker_stable_2".into(),
        origin_conversation_id: conv_id.into(),
        origin_conversation_url: conv_url.clone(),
        account_evidence_hash: "hash_a2".into(),
        transcript_evidence_hash: "hash_t2".into(),
        tab_id: Some("tab_1".into()),
        document_id: "doc_1_alpha".into(),
    };
    let grant2 = journal.acquire_dispatch_fence(&claim_params_2).unwrap();
    assert!(!grant2.granted, "Receipt 2 must be denied grant because conversation slot is held by receipt 1");

    // 5. Inconclusive settlement ("uncertain") leaves attempt dispatching/uncertain and RETAINS slot
    let uncertain_settle = journal.settle_dispatch_fence(&hands_return_bridge::journal::DispatchSettlementParams {
        pairing_id: pairing_id.into(),
        receipt_id: rcpt1_id.into(),
        execution_id: claim1.execution_id.clone(),
        attempt_id: "attempt_1_alpha".into(),
        expected_delivery_revision: 1,
        outcome: "uncertain".into(),
        observed_message_id: None,
        transcript_evidence_hash: None,
        details: Some("Lost contact before click confirmation".into()),
    }).unwrap();
    assert!(uncertain_settle.settled);
    assert!(!uncertain_settle.slot_released, "Uncertain settlement must NOT release conversation slot");

    // Slot is still blocked for receipt 2
    let grant2_after_uncertain = journal.acquire_dispatch_fence(&claim_params_2).unwrap();
    assert!(!grant2_after_uncertain.granted, "Slot must remain held after uncertain settlement");

    // 6. Stale or mismatched CAS settlement attempt fails closed
    let stale_settle = journal.settle_dispatch_fence(&hands_return_bridge::journal::DispatchSettlementParams {
        pairing_id: pairing_id.into(),
        receipt_id: rcpt1_id.into(),
        execution_id: claim1.execution_id.clone(),
        attempt_id: "attempt_stale_wrong".into(), // Wrong attempt
        expected_delivery_revision: 1,
        outcome: "not-sent".into(),
        observed_message_id: None,
        transcript_evidence_hash: None,
        details: None,
    });
    assert!(stale_settle.is_err(), "Stale CAS attempt mismatch must fail closed");

    // 7. Conclusive settlement ("submitted-observed") requires message ID, settles fence, and releases slot
    let missing_msg_id = journal.settle_dispatch_fence(&hands_return_bridge::journal::DispatchSettlementParams {
        pairing_id: pairing_id.into(),
        receipt_id: rcpt1_id.into(),
        execution_id: claim1.execution_id.clone(),
        attempt_id: "attempt_1_alpha".into(),
        expected_delivery_revision: 1,
        outcome: "submitted-observed".into(),
        observed_message_id: None, // Missing!
        transcript_evidence_hash: None,
        details: None,
    });
    assert!(missing_msg_id.is_err(), "submitted-observed requires non-empty observed_message_id");

    let valid_submitted = journal.settle_dispatch_fence(&hands_return_bridge::journal::DispatchSettlementParams {
        pairing_id: pairing_id.into(),
        receipt_id: rcpt1_id.into(),
        execution_id: claim1.execution_id.clone(),
        attempt_id: "attempt_1_alpha".into(),
        expected_delivery_revision: 1,
        outcome: "submitted-observed".into(),
        observed_message_id: Some("msg_observed_777".into()),
        transcript_evidence_hash: Some("hash_transcript_final".into()),
        details: None,
    }).unwrap();
    assert!(valid_submitted.settled);
    assert!(valid_submitted.slot_released, "Conclusive settlement must release conversation slot");

    // 8. After conclusive settlement of receipt 1, receipt 2 can now acquire the conversation slot!
    let grant2_now = journal.acquire_dispatch_fence(&claim_params_2).unwrap();
    assert!(grant2_now.granted, "Receipt 2 must now successfully acquire the conversation slot");
    assert_eq!(grant2_now.attempt_id, "attempt_2_alpha");

    // 9. Replay on settled receipt 1 NEVER grants permission again
    let replay_settled_1 = journal.acquire_dispatch_fence(&claim_params_1).unwrap();
    assert!(!replay_settled_1.granted, "Settled receipt 1 can NEVER grant permission again");
    assert_eq!(replay_settled_1.state, "submitted-observed");
}

#[test]
fn test_concurrent_dual_native_hosts_competing_on_same_receipt_and_conversation() {
    use std::sync::Arc;
    use std::thread;

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal_init = Journal::open(&db_path).unwrap();
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let pairing_id = "pair_concurrent_hosts";
    let bootstrap_token = "boot_concurrent_hosts";
    let profile_id = "profile_concurrent_hosts";
    journal_init.create_bootstrap(
        pairing_id,
        bootstrap_token,
        "chrome",
        profile_id,
        &[TargetRecord { target_id: "target_1".into(), canonical_path, name: "target_1".into() }],
        &PolicyRecord { policy_revision: "v1".into(), tool_policy: "standard".into(), approval_policy: "prompt".into() },
    ).unwrap();
    journal_init.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let conv_id = "conv_concurrent_shared";
    let conv_url = format!("https://chatgpt.com/c/{}", conv_id);
    let claim = journal_init.reserve_or_claim_launch(&LaunchRequestParams {
        pairing_id: pairing_id.into(),
        launch_request_id: "req_concurrent_1".into(),
        origin_conversation_id: conv_id.into(),
        origin_conversation_url: conv_url.clone(),
        transcript_evidence_hash: "hash_t_conc".into(),
        account_evidence_hash: "hash_a_conc".into(),
        target_id: "target_1".into(),
        policy_revision: "v1".into(),
        prompt_text: "concurrent task".into(),
    }).unwrap();

    let rcpt_id = "rcpt_concurrent_1";
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO completion_receipts VALUES (?1, ?2, ?3, ?4, ?5, 0, 'stop', 'msg_c', 'concurrent text', 'digest_c', 1, 'completed', 1000)",
            rusqlite::params![rcpt_id, &claim.execution_id, pairing_id, &claim.return_token, conv_id],
        ).unwrap();
    }
    drop(journal_init);

    // Simulate two real separate native host processes by opening two distinct Journal handles
    // to the same on-disk SQLite database file with WAL mode.
    let host_1 = Arc::new(Journal::open(&db_path).unwrap());
    let host_2 = Arc::new(Journal::open(&db_path).unwrap());

    let claim_params_1 = hands_return_bridge::journal::DispatchClaimParams {
        pairing_id: pairing_id.into(),
        receipt_id: rcpt_id.into(),
        execution_id: claim.execution_id.clone(),
        attempt_id: "attempt_host_1".into(),
        expected_delivery_revision: 1,
        payload_digest: "digest_conc_stable".into(),
        receipt_marker: "marker_conc_1".into(),
        origin_conversation_id: conv_id.into(),
        origin_conversation_url: conv_url.clone(),
        account_evidence_hash: "hash_a_conc".into(),
        transcript_evidence_hash: "hash_t_conc".into(),
        tab_id: Some("tab_host_1".into()),
        document_id: "doc_host_1".into(),
    };

    let mut claim_params_2 = claim_params_1.clone();
    claim_params_2.attempt_id = "attempt_host_2".into();
    claim_params_2.tab_id = Some("tab_host_2".into());
    claim_params_2.document_id = "doc_host_2".into();

    let h1 = {
        let host = Arc::clone(&host_1);
        let params = claim_params_1.clone();
        thread::spawn(move || host.acquire_dispatch_fence(&params).unwrap())
    };

    let h2 = {
        let host = Arc::clone(&host_2);
        let params = claim_params_2.clone();
        thread::spawn(move || host.acquire_dispatch_fence(&params).unwrap())
    };

    let res1 = h1.join().unwrap();
    let res2 = h2.join().unwrap();

    // Across separate native host processes, exactly ONE host can acquire the grant!
    let grants = [res1.granted, res2.granted];
    assert_eq!(grants.iter().filter(|&&g| g).count(), 1, "Exactly one host must be granted send permission: {:?}", grants);

    // The loser receives existing status, NEVER permission!
    if res1.granted {
        assert!(!res2.granted);
        assert_eq!(res2.owner_document_id, "doc_host_1");
    } else {
        assert!(res2.granted);
        assert_eq!(res1.owner_document_id, "doc_host_2");
    }
}
