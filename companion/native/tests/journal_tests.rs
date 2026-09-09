use std::sync::Arc;
use std::thread;
use tempfile::tempdir;

use hands_return_bridge::journal::{
    AttemptEvidence, LaunchRequestParams,
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
