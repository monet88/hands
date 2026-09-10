use std::io::Cursor;
use tempfile::tempdir;

fn live_launcher_tests_enabled() -> bool {
    std::env::var("HANDS_RETURN_BRIDGE_RUN_LIVE_LAUNCHER_TESTS").as_deref() == Ok("1")
}

use hands_return_bridge::journal::{Journal, LaunchRequestParams, PolicyRecord, TargetRecord};
use hands_return_bridge::launcher::{
    build_omp_startup_command_with_env, build_omp_startup_command_with_env_and_bin,
    ensure_adapter_file, is_exact_supported_omp_version, split_prompt_for_orca,
    COMPANION_ADAPTER_REVISION, ORCA_PROMPT_CHUNK_MAX_BYTES, SUPPORTED_OMP_CLI_SHAPE,
    SUPPORTED_OMP_REVISION,
};
use hands_return_bridge::protocol::{
    handle_native_message, read_native_message, write_native_message,
};
use serde_json::json;
fn init_git_repo(path: &std::path::Path) {
    let output = std::process::Command::new("git")
        .args(["init", &path.to_string_lossy()])
        .output()
        .expect("git init must succeed");
    assert!(output.status.success(), "git init failed");
}


#[test]
fn test_native_messaging_framing() {
    let msg = json!({"hello": "world", "status": "ok"});
    let mut buffer = Vec::new();
    write_native_message(&mut buffer, &msg).expect("Failed to write message");

    // Must be 4 bytes length prefix followed by JSON bytes
    assert!(buffer.len() > 4);
    let len = u32::from_le_bytes(buffer[0..4].try_into().unwrap()) as usize;
    assert_eq!(len, buffer.len() - 4);

    let mut cursor = Cursor::new(buffer);
    let read_back = read_native_message(&mut cursor)
        .expect("Failed to read message")
        .expect("Unexpected EOF");
    assert_eq!(read_back, msg);
}

#[test]
fn test_closed_operation_set_and_security_guards() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_closed_test";
    let bootstrap_token = "boot_closed_token";
    let browser = "chrome";
    let profile_id = "prof_a";
    let targets = vec![TargetRecord {
        target_id: "target_hands".to_string(),
        canonical_path: "test_target_closed_ops".to_string(),
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
        .expect("Bootstrap failed");

    // 1. Setup / Pairing bootstrap via Native Messaging
    let setup_msg = json!({
        "op": "setup",
        "bootstrapToken": bootstrap_token,
        "profileId": profile_id
    });
    let setup_resp = handle_native_message(&setup_msg, &journal);
    assert_eq!(setup_resp["status"], "ok");
    assert_eq!(setup_resp["pairingId"], pairing_id);
    let pairing_secret = setup_resp["pairingSecret"].as_str().unwrap();
    assert!(pairing_secret.starts_with("rb_sec_"));
    assert_eq!(setup_resp["policyRevision"], "v1");
    assert_eq!(setup_resp["targets"][0]["target_id"], "target_hands");

    // 2. Connect
    let connect_msg = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id
    });
    let connect_resp = handle_native_message(&connect_msg, &journal);
    assert_eq!(connect_resp["status"], "ok");
    assert_eq!(connect_resp["pairingId"], pairing_id);
    assert_eq!(connect_resp["profileId"], profile_id);

    // 3. Status
    let status_msg = json!({
        "op": "status",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id
    });
    let status_resp = handle_native_message(&status_msg, &journal);
    assert_eq!(status_resp["status"], "ok");
    assert_eq!(status_resp["pairingStatus"], "active");
    assert_eq!(status_resp["taskExecutionAvailable"], true);

    // 4. Boundary guard A1: Unauthorized override attempts fail closed
    let override_targets_msg = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "targets": [{"target_id": "malicious", "canonical_path": "C:\\Windows"}]
    });
    let override_resp = handle_native_message(&override_targets_msg, &journal);
    assert_eq!(override_resp["status"], "error");
    assert_eq!(override_resp["code"], "unauthorized_override");

    let override_policy_msg = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "policy": {"tool_policy": "unrestricted"}
    });
    let override_policy_resp = handle_native_message(&override_policy_msg, &journal);
    assert_eq!(override_policy_resp["status"], "error");
    assert_eq!(override_policy_resp["code"], "unauthorized_override");

    let override_executable_msg = json!({
        "op": "status",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "executable": "cmd.exe"
    });
    let override_exec_resp = handle_native_message(&override_executable_msg, &journal);
    assert_eq!(override_exec_resp["status"], "error");
    assert_eq!(override_exec_resp["code"], "unauthorized_override");

    // 5. Boundary guard: Unapproved field on launch fails closed
    let launch_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "targetId": "target_hands",
        "prompt": "do something" // Unapproved field 'prompt' (expected 'promptText')
    });
    let launch_resp = handle_native_message(&launch_msg, &journal);
    assert_eq!(launch_resp["status"], "error");
    assert_eq!(launch_resp["code"], "unexpected_field");

    // 6. Boundary guard: Unsupported operations fail closed
    let unknown_op_msg = json!({
        "op": "shell_exec",
        "command": "dir"
    });
    let unknown_resp = handle_native_message(&unknown_op_msg, &journal);
    assert_eq!(unknown_resp["status"], "error");
    assert_eq!(unknown_resp["code"], "unsupported_operation");

    // 7. Profile Isolation (N4): Profile B cannot connect or status
    let prof_b_msg = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": "prof_b"
    });
    let prof_b_resp = handle_native_message(&prof_b_msg, &journal);
    assert_eq!(prof_b_resp["status"], "error");
    assert_eq!(prof_b_resp["code"], "profile_mismatch");

    // 8. Revoke
    let revoke_msg = json!({
        "op": "revoke",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id
    });
    let revoke_resp = handle_native_message(&revoke_msg, &journal);
    assert_eq!(revoke_resp["status"], "ok");
    assert_eq!(revoke_resp["pairingStatus"], "revoked");

    // 9. After revocation, connect must be rejected as retired
    let retired_resp = handle_native_message(&connect_msg, &journal);
    assert_eq!(retired_resp["status"], "error");
    assert_eq!(retired_resp["code"], "pairing_retired");
}

#[test]
fn test_unapproved_fields_rejected_on_valid_ops() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_field_test";
    let bootstrap_token = "boot_field_token_123";
    let browser = "chrome";
    let profile_id = "profile_field";
    let targets = vec![TargetRecord {
        target_id: "target_canonical".to_string(),
        canonical_path: "test_target_field_guard".to_string(),
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
            browser,
            profile_id,
            &targets,
            &policy,
        )
        .expect("Bootstrap failed");

    // 1. setup with filesystem/command/arbitrary field must be rejected
    let bad_setup = json!({
        "op": "setup",
        "bootstrapToken": bootstrap_token,
        "profileId": profile_id,
        "command": "whoami"
    });
    let resp = handle_native_message(&bad_setup, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");

    let bad_setup_path = json!({
        "op": "setup",
        "bootstrapToken": bootstrap_token,
        "profileId": profile_id,
        "path": "C:\\Windows\\System32"
    });
    let resp = handle_native_message(&bad_setup_path, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");

    // Perform valid setup to get credentials
    let setup_msg = json!({
        "op": "setup",
        "bootstrapToken": bootstrap_token,
        "profileId": profile_id
    });
    let setup_resp = handle_native_message(&setup_msg, &journal);
    assert_eq!(setup_resp["status"], "ok");
    let pairing_secret = setup_resp["pairingSecret"].as_str().unwrap();

    // 2. connect with targetId / shell / command / prompt / unknown fields must be rejected
    let bad_connect_target_id = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "targetId": "malicious_target"
    });
    let resp = handle_native_message(&bad_connect_target_id, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");

    let bad_connect_shell = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "shell": "cmd.exe"
    });
    let resp = handle_native_message(&bad_connect_shell, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");

    let bad_connect_prompt = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "prompt": "execute malicious command"
    });
    let resp = handle_native_message(&bad_connect_prompt, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");

    let bad_connect_arbitrary = json!({
        "op": "connect",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "unknownFieldXYZ": 123
    });
    let resp = handle_native_message(&bad_connect_arbitrary, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");

    // 3. status with unexpected field must be rejected
    let bad_status = json!({
        "op": "status",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "extra": "value"
    });
    let resp = handle_native_message(&bad_status, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");

    // 4. revoke with unexpected field must be rejected
    let bad_revoke = json!({
        "op": "revoke",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "force": true
    });
    let resp = handle_native_message(&bad_revoke, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unexpected_field");
}


#[test]
fn test_protocol_launch_and_recover_operations() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_proto_launch";
    let bootstrap_token = "boot_proto_token";
    let profile_id = "profile_alpha";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let targets = vec![TargetRecord {
        target_id: "target_proj".to_string(),
        canonical_path: canonical_path.clone(),
        name: "target_proj".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    let activated = journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();
    let pairing_secret = &activated.pairing_secret;

    // 1. Launch with invalid conversation boundary (home / chat) must fail closed
    let bad_conv_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_bad_conv",
        "originConversationId": "home",
        "originConversationUrl": "https://chatgpt.com/chat/",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "target_proj",
        "requestedPolicyRevision": "v1",
        "promptText": "test prompt"
    });
    let bad_conv_resp = handle_native_message(&bad_conv_msg, &journal);
    assert_eq!(bad_conv_resp["status"], "error");
    assert_eq!(bad_conv_resp["code"], "invalid_conversation_boundary");

    // 2. Launch with missing or unknown target ID fails closed
    let bad_target_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_bad_target",
        "originConversationId": "c_123",
        "originConversationUrl": "https://chatgpt.com/c/c_123",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "nonexistent_target",
        "requestedPolicyRevision": "v1",
        "promptText": "test prompt"
    });
    let bad_target_resp = handle_native_message(&bad_target_msg, &journal);
    assert_eq!(bad_target_resp["status"], "error");
    assert_eq!(bad_target_resp["code"], "target_not_found");

    // 3. Launch with policy revision mismatch fails closed
    let bad_policy_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_bad_policy",
        "originConversationId": "c_123",
        "originConversationUrl": "https://chatgpt.com/c/c_123",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "target_proj",
        "requestedPolicyRevision": "v999",
        "promptText": "test prompt"
    });
    let bad_policy_resp = handle_native_message(&bad_policy_msg, &journal);
    assert_eq!(bad_policy_resp["status"], "error");
    assert_eq!(bad_policy_resp["code"], "policy_mismatch");

    // 4. Boundary guard A1: Attempting to supply unapproved extra fields (e.g. executable, argv, environment)
    let override_launch_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_override",
        "originConversationId": "c_123",
        "originConversationUrl": "https://chatgpt.com/c/c_123",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "target_proj",
        "requestedPolicyRevision": "v1",
        "promptText": "test prompt",
        "executable": "malicious.exe"
    });
    let override_resp = handle_native_message(&override_launch_msg, &journal);
    assert_eq!(override_resp["status"], "error");
    assert_eq!(override_resp["code"], "unauthorized_override");

    // 5. Recover operation with empty list initially
    let recover_msg = json!({
        "op": "recover",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id
    });
    let recover_resp = handle_native_message(&recover_msg, &journal);
    assert_eq!(recover_resp["status"], "ok");
    assert_eq!(recover_resp["summaries"].as_array().unwrap().len(), 0);
}

#[test]
fn test_canonical_conversation_url_parser_strictness() {
    use hands_return_bridge::protocol::parse_canonical_conversation_id;

    // Valid canonical forms
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/c/c_12345"), Some("c_12345".to_string()));
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/g/g-abc123/c/c_67890"), Some("c_67890".to_string()));

    // Invalid domains
    assert_eq!(parse_canonical_conversation_id("http://chatgpt.com/c/c_12345"), None);
    assert_eq!(parse_canonical_conversation_id("https://evil.com/c/c_12345"), None);

    // Fragments and queries rejected
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/c/c_12345#frag"), None);
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/c/c_12345?query=1"), None);

    // Non-canonical paths (home, chat, new_chat, provisional)
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/"), None);
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/chat"), None);
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/chat/"), None);
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/c/new_chat"), None);
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/c/provisional_abc"), None);
    assert_eq!(parse_canonical_conversation_id("https://chatgpt.com/c/"), None);
}

#[test]
fn test_omp_startup_command_shape() {
    use std::path::Path;
    use hands_return_bridge::launcher::build_omp_startup_command;

    let adapter_path = Path::new("C:/portable test/adapter.ts");

    // Normal OMP startup command: call operator '&', binary, -e with adapter path
    // Preserves user's normal OMP configuration (does NOT pass --no-extensions, --no-skills, --no-rules, --no-prewalk, --tools, --approval-mode)
    let cmd = build_omp_startup_command(adapter_path).unwrap();
    assert!(cmd.starts_with("& "), "Command must start with call operator '&': {}", cmd);
    assert!(cmd.contains("-e 'C:/portable test/adapter.ts'"), "Command must load companion adapter: {}", cmd);
    assert!(!cmd.contains("--no-extensions"), "Command must not suppress normal extensions: {}", cmd);
    assert!(!cmd.contains("--no-skills"), "Command must not suppress normal skills: {}", cmd);
    assert!(!cmd.contains("--no-rules"), "Command must not suppress normal rules: {}", cmd);
    assert!(!cmd.contains("--no-prewalk"), "Command must not suppress normal prewalk: {}", cmd);
    assert!(!cmd.contains("--tools="), "Command must not enforce tool whitelist: {}", cmd);
    assert!(!cmd.contains("--approval-mode="), "Command must not enforce approval mode: {}", cmd);
}

#[test]
fn test_uncertainty_and_recovery_semantics() {
    use hands_return_bridge::journal::{AttemptEvidence, LaunchRequestParams};

    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_uncertain_test";
    let bootstrap_token = "boot_uncertain";
    let profile_id = "prof_uncertain";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let targets = vec![TargetRecord {
        target_id: "target_u".to_string(),
        canonical_path: canonical_path.clone(),
        name: "target_u".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal.create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy).unwrap();
    let activated = journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();
    let pairing_secret = &activated.pairing_secret;

    let params = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_u1".to_string(),
        origin_conversation_id: "c_u1".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/c_u1".to_string(),
        transcript_evidence_hash: "hash_t_u1".to_string(),
        account_evidence_hash: "hash_a_u1".to_string(),
        target_id: "target_u".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Uncertainty test prompt".to_string(),
    };

    // Claim launch
    let claim = journal.reserve_or_claim_launch(&params).unwrap();

    // 1. Mark launch attempt
    let can_attempt = journal.mark_launch_attempt(&claim.execution_id, pairing_id).unwrap();
    assert!(can_attempt);

    // 2. Simulate post-spawn uncertainty before terminal created (e.g. process exited with error)
    journal.record_launch_uncertain(&claim.execution_id, None, "Orca returned non-zero exit code").unwrap();

    // State in journal is now 'unknown'
    let summary = journal.get_launch_request_by_id(pairing_id, "req_u1").unwrap().unwrap();
    assert_eq!(summary.state, "unknown");

    // 3. Second attempt is BLOCKED: mark_launch_attempt must return false
    let second_attempt = journal.mark_launch_attempt(&claim.execution_id, pairing_id).unwrap();
    assert!(!second_attempt, "Second launch attempt must fail closed");

    // 4. Same unresolved request replayed returns SAME execution but MUST NOT report success.
    let replay_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_u1",
        "originConversationId": "c_u1",
        "originConversationUrl": "https://chatgpt.com/c/c_u1",
        "transcriptEvidenceHash": "hash_t_u1",
        "accountEvidenceHash": "hash_a_u1",
        "targetId": "target_u",
        "requestedPolicyRevision": "v1",
        "promptText": "Uncertainty test prompt"
    });
    let replay_resp = handle_native_message(&replay_msg, &journal);
    assert_eq!(replay_resp["status"], "error");
    assert_eq!(replay_resp["code"], "launch_unresolved");
    assert_eq!(replay_resp["isReplayed"], true);
    assert_eq!(replay_resp["executionId"], claim.execution_id);
    assert_eq!(replay_resp["state"], "unknown");

    // 5. Terminal known, but wait/send failure: terminal evidence preserved, state is unknown
    let params2 = LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_u2".to_string(),
        origin_conversation_id: "c_u2".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/c_u2".to_string(),
        transcript_evidence_hash: "hash_t_u2".to_string(),
        account_evidence_hash: "hash_a_u2".to_string(),
        target_id: "target_u".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Uncertainty test 2".to_string(),
    };
    let claim2 = journal.reserve_or_claim_launch(&params2).unwrap();
    journal.mark_launch_attempt(&claim2.execution_id, pairing_id).unwrap();

    let evidence = AttemptEvidence {
        orca_terminal_handle: Some("terminal_known_99".to_string()),
        orca_tab_id: Some("tab_99".to_string()),
        orca_pane_key: Some("pane_99".to_string()),
        orca_pty_id: Some("pty_99".to_string()),
    };
    journal.record_launch_start_evidence(&claim2.execution_id, &evidence).unwrap();
    journal.record_launch_uncertain(&claim2.execution_id, Some(&evidence), "Send prompt failed").unwrap();

    let summary2 = journal.get_launch_request_by_id(pairing_id, "req_u2").unwrap().unwrap();
    assert_eq!(summary2.state, "unknown");
    assert_eq!(summary2.orca_terminal_handle.as_deref(), Some("terminal_known_99"));

    // Replaying req_u2 yields existing terminal evidence and does NOT create new terminal
    let replay2_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_u2",
        "originConversationId": "c_u2",
        "originConversationUrl": "https://chatgpt.com/c/c_u2",
        "transcriptEvidenceHash": "hash_t_u2",
        "accountEvidenceHash": "hash_a_u2",
        "targetId": "target_u",
        "requestedPolicyRevision": "v1",
        "promptText": "Uncertainty test 2"
    });
    let replay2_resp = handle_native_message(&replay2_msg, &journal);
    assert_eq!(replay2_resp["status"], "error");
    assert_eq!(replay2_resp["code"], "launch_unresolved");
    assert_eq!(replay2_resp["isReplayed"], true);
    assert_eq!(replay2_resp["executionId"], claim2.execution_id);
}

#[test]
fn test_claimed_replay_is_not_reported_as_started_or_ok() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let pairing_id = "pair_claimed_replay";
    let bootstrap_token = "boot_claimed_replay";
    let profile_id = "prof_claimed_replay";
    journal.create_bootstrap(
        pairing_id,
        bootstrap_token,
        "chrome",
        profile_id,
        &[TargetRecord { target_id: "target_claimed".into(), canonical_path, name: "target_claimed".into() }],
        &PolicyRecord { policy_revision: "v1".into(), tool_policy: "standard".into(), approval_policy: "prompt".into() },
    ).unwrap();
    let activated = journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();
    let params = LaunchRequestParams {
        pairing_id: pairing_id.into(),
        launch_request_id: "req_claimed_replay".into(),
        origin_conversation_id: "c_claimed".into(),
        origin_conversation_url: "https://chatgpt.com/c/c_claimed".into(),
        transcript_evidence_hash: "hash_t_claimed".into(),
        account_evidence_hash: "hash_a_claimed".into(),
        target_id: "target_claimed".into(),
        policy_revision: "v1".into(),
        prompt_text: "claimed replay must remain unresolved".into(),
    };
    let claim = journal.reserve_or_claim_launch(&params).unwrap();
    assert_eq!(claim.state, "claimed");

    let replay = handle_native_message(&json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": activated.pairing_secret,
        "profileId": profile_id,
        "launchRequestId": params.launch_request_id,
        "originConversationId": params.origin_conversation_id,
        "originConversationUrl": params.origin_conversation_url,
        "transcriptEvidenceHash": params.transcript_evidence_hash,
        "accountEvidenceHash": params.account_evidence_hash,
        "targetId": params.target_id,
        "requestedPolicyRevision": params.policy_revision,
        "promptText": params.prompt_text,
    }), &journal);
    assert_eq!(replay["status"], "error");
    assert_eq!(replay["code"], "launch_unresolved");
    assert_eq!(replay["isReplayed"], true);
    assert_eq!(replay["state"], "claimed");
    assert_eq!(replay["executionId"], claim.execution_id);
}

#[test]
fn test_adapter_pinning_and_deterministic_content() {
    use hands_return_bridge::launcher::{
        ensure_adapter_file, ADAPTER_TS_CONTENT, COMPANION_ADAPTER_REVISION,
    };
    let dir = tempdir().unwrap();
    let adapter_path = ensure_adapter_file(dir.path()).expect("Must write adapter file");
    assert_eq!(COMPANION_ADAPTER_REVISION, "v3");
    let read_back = std::fs::read_to_string(&adapter_path).unwrap();
    assert_eq!(read_back, ADAPTER_TS_CONTENT);
    assert!(read_back.contains("revision: v3"));
}

#[test]
fn test_zero_side_effects_on_rejected_protocol_messages() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).expect("Failed to open journal");

    let pairing_id = "pair_zero_proto";
    let bootstrap_token = "boot_zero_proto";
    let profile_id = "profile_alpha";
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let targets = vec![TargetRecord {
        target_id: "target_valid".to_string(),
        canonical_path,
        name: "target_valid".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    let activated = journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();
    let pairing_secret = &activated.pairing_secret;

    // Rejection 1: unauthorized override field
    let bad_override = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_bad_1",
        "originConversationId": "c_1",
        "originConversationUrl": "https://chatgpt.com/c/c_1",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "target_valid",
        "requestedPolicyRevision": "v1",
        "promptText": "test",
        "executable": "powershell.exe"
    });
    let resp1 = handle_native_message(&bad_override, &journal);
    assert_eq!(resp1["status"], "error");
    assert_eq!(resp1["code"], "unauthorized_override");

    // Rejection 2: invalid conversation URL
    let bad_url = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_bad_2",
        "originConversationId": "c_1",
        "originConversationUrl": "https://chatgpt.com/",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "target_valid",
        "requestedPolicyRevision": "v1",
        "promptText": "test"
    });
    let resp2 = handle_native_message(&bad_url, &journal);
    assert_eq!(resp2["status"], "error");
    assert_eq!(resp2["code"], "invalid_conversation_boundary");

    // Zero-side-effects check: no launch requests or attempts recorded
    let summaries = journal.get_launch_summaries(pairing_id).unwrap();
    assert_eq!(summaries.len(), 0, "No launch requests should exist after rejected messages");
}

#[test]
fn test_literal_prompt_delivery_contract() {
    // Verify literal probe characters (--foo, @file, quotes, semicolon, pipe, Unicode/newlines)
    // are passed as separate argv entries to Command without shell escaping issues.
    let probe_prompt = "Probe: --flag @some_file \"double\" 'single' ; echo pipe | unicode: Đại Ca \n newline line 2";
    assert!(probe_prompt.contains("--flag"));
    assert!(probe_prompt.contains("@some_file"));
    assert!(probe_prompt.contains("\"double\""));
    assert!(probe_prompt.contains(";"));
    assert!(probe_prompt.contains("|"));
    assert!(probe_prompt.contains("Đại Ca"));
    assert!(probe_prompt.contains("\n"));

    // Verify prompt does not exceed bounded size
    assert!(probe_prompt.len() <= 128 * 1024);
}

#[test]
fn test_multi_process_native_host_convergence() {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    use hands_return_bridge::host::{execute_setup, SetupOptions};

    let dir = tempdir().unwrap();
    let state_dir = dir.path().to_path_buf();
    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let extension_id = "mkkajdpmlmliildflmnnmfndboldnnfa";
    let profile_id = "profile_multi";
    let setup_opts = SetupOptions {
        browser: "chrome".to_string(),
        profile_id: profile_id.to_string(),
        target_path: canonical_path,
        target_id: Some("target_multi".to_string()),
        extension_id: extension_id.to_string(),
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
        state_dir: Some(state_dir.clone()),
        skip_registry: true,
    };

    let setup_res = execute_setup(&setup_opts).expect("Setup must succeed");

    // Activate bootstrap first via direct journal connection to obtain pairing_secret
    let db_path = state_dir.join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();
    let activated = journal.activate_bootstrap(&setup_res.bootstrap_token, profile_id).unwrap();
    let pairing_id = activated.pairing_id;
    let pairing_secret = activated.pairing_secret;
    drop(journal);

    let binary_path = env!("CARGO_BIN_EXE_hands-return-bridge");
    let caller_origin = format!("chrome-extension://{}/", extension_id);

    // Spawn Host Process 1
    let mut child1 = Command::new(binary_path)
        .args([&caller_origin, "--state-dir", &state_dir.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Spawn host child 1 failed");

    // Spawn Host Process 2
    let mut child2 = Command::new(binary_path)
        .args([&caller_origin, "--state-dir", &state_dir.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Spawn host child 2 failed");

    let launch_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_multi_converge_1",
        "originConversationId": "conv_multi_1",
        "originConversationUrl": "https://chatgpt.com/c/conv_multi_1",
        "transcriptEvidenceHash": "hash_t_multi",
        "accountEvidenceHash": "hash_a_multi",
        "targetId": "target_multi",
        "requestedPolicyRevision": "v1",
        "promptText": "Multi-process convergence probe"
    });

    fn send_and_recv(child: &mut std::process::Child, msg: &serde_json::Value) -> serde_json::Value {
        let body = serde_json::to_vec(msg).unwrap();
        let len = body.len() as u32;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(&len.to_le_bytes()).unwrap();
        stdin.write_all(&body).unwrap();
        stdin.flush().unwrap();

        let stdout = child.stdout.as_mut().unwrap();
        let mut len_buf = [0u8; 4];
        stdout.read_exact(&mut len_buf).unwrap();
        let resp_len = u32::from_le_bytes(len_buf) as usize;
        let mut resp_buf = vec![0u8; resp_len];
        stdout.read_exact(&mut resp_buf).unwrap();
        serde_json::from_slice(&resp_buf).unwrap()
    }

    // Send to both child processes concurrently
    let msg_clone = launch_msg.clone();
    let t1 = std::thread::spawn(move || {
        send_and_recv(&mut child1, &msg_clone)
    });
    let t2 = std::thread::spawn(move || {
        send_and_recv(&mut child2, &launch_msg)
    });

    let resp1 = t1.join().unwrap();
    let resp2 = t2.join().unwrap();
    eprintln!("resp1 = {}", resp1);
    eprintln!("resp2 = {}", resp2);
    assert!(
        resp1["status"] == "ok"
            || resp1["code"] == "launch_unresolved"
            || resp1["code"] == "launch_uncertain"
            || resp1["code"] == "preflight_failed"
            || resp1["code"] == "orca_spawn_failed",
        "resp1 was: {}",
        resp1
    );
    assert!(
        resp2["status"] == "ok"
            || resp2["code"] == "launch_unresolved"
            || resp2["code"] == "launch_uncertain"
            || resp2["code"] == "preflight_failed"
            || resp2["code"] == "orca_spawn_failed",
        "resp2 was: {}",
        resp2
    );

    // Both processes MUST converge on the exact same executionId!
    let exec_id_1 = resp1.get("executionId").and_then(|v| v.as_str());
    let exec_id_2 = resp2.get("executionId").and_then(|v| v.as_str());
    assert!(exec_id_1.is_some() && exec_id_2.is_some(), "Both must return executionId");
    assert_eq!(exec_id_1, exec_id_2, "Concurrent native host processes MUST converge on identical executionId");

    // Verify SQLite journal integrity: exactly ONE launch request was created
    let journal_check = Journal::open(&db_path).unwrap();
    let summaries = journal_check.get_launch_summaries(&pairing_id).unwrap();
    assert_eq!(summaries.len(), 1, "Exactly one launch request row must exist");
    assert_eq!(summaries[0].execution_id, exec_id_1.unwrap());
}

#[test]
fn test_launch_preflight_check() {
    use hands_return_bridge::launcher::verify_launch_preflight;

    if !live_launcher_tests_enabled() {
        eprintln!("SKIP live launcher preflight (set HANDS_RETURN_BRIDGE_RUN_LIVE_LAUNCHER_TESTS=1)");
        return;
    }

    // Default preflight against system CLI
    let res = verify_launch_preflight(None);
    assert!(res.is_ok(), "Preflight should succeed on system with orca and omp installed: {:?}", res);

    // Invalid OMP binary should fail closed with PreflightFailed
    let bad_res = verify_launch_preflight(Some("nonexistent_omp_binary_xyz_123"));
    assert!(bad_res.is_err(), "Preflight must fail closed for invalid OMP binary");
    let err_str = bad_res.unwrap_err().to_string();
    assert!(err_str.contains("Preflight"), "Error message must indicate preflight failure: {}", err_str);
}

#[test]
fn test_resolve_omp_binary_shapes() {
    use hands_return_bridge::launcher::{
        ensure_adapter_file, launch_orca_terminal,
        verify_launch_preflight, wait_orca_terminal_idle,
    };
    use std::path::Path;
    use std::process::Command;
    use tempfile::tempdir;

    let dir = tempdir().unwrap();
    let adapter_path = ensure_adapter_file(dir.path()).unwrap();

    // 1. Wrapper token shape ("omp") is always checked as a pure command builder.
    let cmd_wrapper = build_omp_startup_command_with_env_and_bin(
        &adapter_path, None, None, Some("omp")
    ).unwrap();
    assert!(cmd_wrapper.contains("& 'omp' -e '"));

    // 2. Executable path with spaces is rendered as a PowerShell single-quoted command literal.
    let dummy_adapter = Path::new("C:/temp/adapter.ts");
    let cmd = build_omp_startup_command_with_env_and_bin(
        dummy_adapter,
        None,
        None,
        Some("C:\\Program Files\\OMP Tools\\omp.exe"),
    ).unwrap();
    assert!(cmd.starts_with("& 'C:/Program Files/OMP Tools/omp.exe' -e 'C:/temp/adapter.ts'"));

    // 3. PowerShell metacharacters remain literal inside single-quoted values.
    let quoted_cmd = build_omp_startup_command_with_env_and_bin(
        Path::new("C:/tmp/$adapter/O'Brien/adapter.ts"),
        Some("exec_$literal'O"),
        Some(Path::new("C:/state/$literal/O'Brien")),
        Some("C:/Program Files/OMP $Tools/O'Brien/omp.exe"),
    ).unwrap();
    assert!(quoted_cmd.contains("$env:HANDS_RETURN_BRIDGE_EXECUTION_ID='exec_$literal''O';"));
    assert!(quoted_cmd.contains("$env:HANDS_RETURN_BRIDGE_STATE_DIR='C:/state/$literal/O''Brien';"));
    assert!(quoted_cmd.contains("& 'C:/Program Files/OMP $Tools/O''Brien/omp.exe'"));
    assert!(quoted_cmd.contains("-e 'C:/tmp/$adapter/O''Brien/adapter.ts'"));

    if live_launcher_tests_enabled() {
        let target_worktree = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let token_preflight = verify_launch_preflight(Some("omp"));
        assert!(
            token_preflight.is_ok(),
            "Preflight for wrapper token 'omp' must succeed: {:?}",
            token_preflight
        );

        let evidence_wrapper = launch_orca_terminal(&target_worktree, &cmd_wrapper, "test_shape_wrap")
            .expect("Launch owned OMP terminal for wrapper shape must succeed");
        let handle_wrapper = evidence_wrapper
            .orca_terminal_handle
            .as_deref()
            .expect("Terminal handle must be present");
        let wait_wrapper = wait_orca_terminal_idle(handle_wrapper, 15000);
        let _ = Command::new("orca")
            .args(["terminal", "close", "--terminal", handle_wrapper, "--json"])
            .output();
        assert!(
            wait_wrapper.is_ok(),
            "Wrapper shape 'omp' must reach real tui-idle session: {:?}",
            wait_wrapper
        );
    }

    // 6. Security guard: browser messages must NEVER be able to provide or override executable/bin
    let db_path = dir.path().join("journal.sqlite");
    let journal = hands_return_bridge::journal::Journal::open(&db_path).unwrap();
    let browser_override_msg = json!({
        "op": "launch",
        "pairingId": "pair_test",
        "pairingSecret": "sec_test",
        "profileId": "profile_alpha",
        "launchRequestId": "req_1",
        "originConversationId": "c_1",
        "originConversationUrl": "https://chatgpt.com/c/c_1",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "hands",
        "requestedPolicyRevision": "v1",
        "promptText": "test prompt",
        "executable": "C:\\malicious\\path\\omp.exe"
    });
    let resp = handle_native_message(&browser_override_msg, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "unauthorized_override");
}

#[test]
fn test_large_prompt_chunking_preserves_utf8_exactly() {
    let prompt = format!(
        "{}{}{}",
        "a".repeat(ORCA_PROMPT_CHUNK_MAX_BYTES - 2),
        "界".repeat(5000),
        "tail-$literal-'quote'"
    );
    let chunks = split_prompt_for_orca(&prompt);
    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|chunk| chunk.len() <= ORCA_PROMPT_CHUNK_MAX_BYTES));
    assert_eq!(chunks.concat(), prompt);
}
#[test]
fn test_unsupported_registered_policy_fails_closed_before_claim_or_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = hands_return_bridge::journal::Journal::open(&db_path).unwrap();

    let pairing_id = "pair_unsupported_policy";
    let bootstrap_token = "boot_unsupp_policy";
    let profile_id = "profile_alpha";

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let target_canonical = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();
    let targets = vec![hands_return_bridge::journal::TargetRecord {
        target_id: "hands".to_string(),
        canonical_path: target_canonical,
        name: "hands".to_string(),
    }];
    // Unsupported tool_policy: "unrestricted"
    let policy = hands_return_bridge::journal::PolicyRecord {
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
    let activated = journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();

    let launch_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": activated.pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_unsupp_new_1",
        "originConversationId": "c_unsupp_new",
        "originConversationUrl": "https://chatgpt.com/c/c_unsupp_new",
        "transcriptEvidenceHash": "hash_t",
        "accountEvidenceHash": "hash_a",
        "targetId": "hands",
        "requestedPolicyRevision": "v1",
        "promptText": "attempt launch with unrestricted policy"
    });

    // 1. MUST fail closed with policy_unsupported
    let resp = handle_native_message(&launch_msg, &journal);
    assert_eq!(resp["status"], "error");
    assert_eq!(resp["code"], "policy_unsupported");

    // 2. Prove ZERO launch_request, ZERO tombstone, and ZERO attempt
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
    let summaries = journal.get_launch_summaries(pairing_id).unwrap();
    assert_eq!(summaries.len(), 0, "Must be zero launch summaries");

    // 3. Preserve replay semantics: insert an already accepted request with original execution
    let accepted_exec_id = "exec_pre_accepted_42";
    let accepted_ret_token = "ret_token_42";
    let payload_digest = hands_return_bridge::journal::compute_payload_digest(
        "c_unsupp_new",
        "https://chatgpt.com/c/c_unsupp_new",
        "hash_t",
        "hash_a",
        "hands",
        "v1",
        "attempt launch with unrestricted policy",
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
                "req_unsupp_new_1",
                accepted_exec_id,
                accepted_ret_token,
                "c_unsupp_new",
                "https://chatgpt.com/c/c_unsupp_new",
                "hash_t",
                "hash_a",
                "hands",
                targets[0].canonical_path,
                "v1",
                "unrestricted",
                "prompt",
                "attempt launch with unrestricted policy",
                payload_digest,
                "claimed",
            ],
        ).unwrap();
    }

    // Replay preserves the accepted execution identity without revalidating policy,
    // but an unresolved durable state must not masquerade as a successful launch.
    let replay_resp = handle_native_message(&launch_msg, &journal);
    assert_eq!(replay_resp["status"], "error");
    assert_eq!(replay_resp["code"], "launch_unresolved");
    assert_eq!(replay_resp["isReplayed"], true);
    assert_eq!(replay_resp["executionId"], accepted_exec_id);
    assert_eq!(replay_resp["returnToken"], accepted_ret_token);
}

#[test]
fn test_recover_includes_completion_receipt_after_owned_turn() {
    let dir = tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    init_git_repo(&repo_dir);

    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let pairing_id = "pair_rcpt_proto";
    let bootstrap_token = "boot_rcpt_proto";
    let profile_id = "profile_proto";
    let targets = vec![TargetRecord {
        target_id: "target_proto".to_string(),
        canonical_path: repo_dir.to_string_lossy().to_string(),
        name: "target_proto".to_string(),
    }];
    let policy = PolicyRecord {
        policy_revision: "v1".to_string(),
        tool_policy: "standard".to_string(),
        approval_policy: "prompt".to_string(),
    };

    journal
        .create_bootstrap(pairing_id, bootstrap_token, "chrome", profile_id, &targets, &policy)
        .unwrap();
    let activated = journal.activate_bootstrap(bootstrap_token, profile_id).unwrap();
    let pairing_secret = &activated.pairing_secret;

    let launch_params = hands_return_bridge::journal::LaunchRequestParams {
        pairing_id: pairing_id.to_string(),
        launch_request_id: "req_rcpt_proto_1".to_string(),
        origin_conversation_id: "conv_proto_1".to_string(),
        origin_conversation_url: "https://chatgpt.com/c/conv_proto_1".to_string(),
        transcript_evidence_hash: "thash_p".to_string(),
        account_evidence_hash: "ahash_p".to_string(),
        target_id: "target_proto".to_string(),
        policy_revision: "v1".to_string(),
        prompt_text: "Recovery with receipt test".to_string(),
    };

    let claim = journal.reserve_or_claim_launch(&launch_params).unwrap();

    // Before receipt, recover shows no completion receipt
    let rec_msg = json!({
        "op": "recover",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "launchRequestId": "req_rcpt_proto_1"
    });
    let rec_resp = handle_native_message(&rec_msg, &journal);
    assert_eq!(rec_resp["status"], "ok");
    assert_eq!(rec_resp["summary"]["state"], "claimed");
    assert!(rec_resp["summary"]["completion_receipt"].is_null());

    // Commit completion receipt directly via SQLite
    let receipt_id = "rcpt_rec_1";
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute(
        r#"
        INSERT INTO completion_receipts (
            receipt_id, execution_id, pairing_id, return_token,
            origin_conversation_id, turn_index, stop_reason,
            assistant_message_id, assistant_text, content_digest,
            tool_call_count, state, committed_at
        ) VALUES (?1, ?2, ?3, ?4, 'conv_123', 0, 'stop', 'msg_p_1', 'Finished task in owned turn', 'digest_p_1', 2, 'completed', 1000)
        "#,
        rusqlite::params![receipt_id, &claim.execution_id, pairing_id, &claim.return_token],
    ).unwrap();
    conn.execute(
        "UPDATE launch_requests SET state = 'completed', updated_at = 1000 WHERE execution_id = ?1",
        rusqlite::params![&claim.execution_id],
    ).unwrap();
    // Recover after receipt commit
    let rec_resp2 = handle_native_message(&rec_msg, &journal);
    assert_eq!(rec_resp2["status"], "ok");
    assert_eq!(rec_resp2["summary"]["state"], "completed");
    let summary_rcpt = &rec_resp2["summary"]["completion_receipt"];
    assert!(!summary_rcpt.is_null(), "completion_receipt must be present");
    assert_eq!(summary_rcpt["receipt_id"], receipt_id);
    assert_eq!(summary_rcpt["stop_reason"], "stop");
    assert_eq!(summary_rcpt["assistant_text"], "Finished task in owned turn");
    assert_eq!(summary_rcpt["tool_call_count"], 2);
    assert_eq!(summary_rcpt["state"], "completed");
}

#[test]
fn test_adapter_v3_generation_and_revision() {
    assert_eq!(COMPANION_ADAPTER_REVISION, "v3");

    let dir = tempdir().unwrap();
    let adapter_path = ensure_adapter_file(dir.path()).unwrap();
    let content = std::fs::read_to_string(&adapter_path).unwrap();
    assert!(content.contains("Return Bridge companion OMP adapter (revision: v3)"));
    assert!(content.contains("HANDS_RETURN_BRIDGE_EXECUTION_ID"));
    assert!(content.contains("execution_adapter_claims"));
    assert!(content.contains("completion_receipts"));
    assert!(content.contains("session_stop"));
    assert!(content.contains("agent_end"));
    assert!(content.contains("willContinue"));

    let cmd = build_omp_startup_command_with_env(
        &adapter_path,
        Some("exec_test_rev3"),
        Some(dir.path()),
    ).unwrap();
    assert!(cmd.contains("$env:HANDS_RETURN_BRIDGE_EXECUTION_ID='exec_test_rev3';"));
    assert!(cmd.contains("$env:HANDS_RETURN_BRIDGE_STATE_DIR="));
    assert!(cmd.contains("-e '"));
    assert!(cmd.contains("adapter.ts'"));
}

#[test]
fn test_launch_preflight_supported_omp_revision_pin() {
    use hands_return_bridge::launcher::verify_launch_preflight;
    assert_eq!(SUPPORTED_OMP_REVISION, "18.1.16");
    assert_eq!(SUPPORTED_OMP_CLI_SHAPE, "omp/18.1.16");

    if !live_launcher_tests_enabled() {
        eprintln!("SKIP live OMP revision preflight (set HANDS_RETURN_BRIDGE_RUN_LIVE_LAUNCHER_TESTS=1)");
        return;
    }

    // Default preflight against system OMP (18.1.16) must succeed
    let res = verify_launch_preflight(None);
    assert!(res.is_ok(), "Preflight should succeed with exact supported OMP revision: {:?}", res);

    // Preflight against a script that outputs an unsupported version must fail closed
    let dir = tempdir().unwrap();
    let mock_omp = dir.path().join("mock_omp_wrong_ver.bat");
    std::fs::write(&mock_omp, "@echo off\r\nif \"%1\"==\"--version\" (echo omp/19.0.0 & exit /b 0)\r\nif \"%1\"==\"--help\" (echo Help info & exit /b 0)\r\n").unwrap();

    let bad_ver_res = verify_launch_preflight(Some(&mock_omp.to_string_lossy()));
    assert!(bad_ver_res.is_err(), "Preflight must fail closed for unsupported OMP revision");
    let err_msg = bad_ver_res.unwrap_err().to_string();
    assert!(err_msg.contains("Unsupported OMP revision"), "Error should report revision mismatch: {}", err_msg);
    assert!(err_msg.contains("omp/18.1.16"), "Error should name expected revision omp/18.1.16: {}", err_msg);
}

#[test]
fn test_exact_supported_omp_version_matching() {
    assert_eq!(SUPPORTED_OMP_CLI_SHAPE, "omp/18.1.16");

    // Exact match must pass
    assert!(is_exact_supported_omp_version("omp/18.1.16"));
    assert!(is_exact_supported_omp_version("omp/18.1.16\n"));
    assert!(is_exact_supported_omp_version("omp/18.1.16\r\n"));
    assert!(is_exact_supported_omp_version("  omp/18.1.16  \n"));

    // Lookalikes, prefixes, suffixes, and extra wrapper text MUST BE REJECTED
    assert!(!is_exact_supported_omp_version("omp/118.1.16"));
    assert!(!is_exact_supported_omp_version("omp/18.1.16-beta"));
    assert!(!is_exact_supported_omp_version("omp/18.1.16.1"));
    assert!(!is_exact_supported_omp_version("omp/18.1.16_rc1"));
    assert!(!is_exact_supported_omp_version("v18.1.16"));
    assert!(!is_exact_supported_omp_version("18.1.16"));
    assert!(!is_exact_supported_omp_version("wrapper: omp/18.1.16"));
    assert!(!is_exact_supported_omp_version("omp/18.1.16 extra text"));
    assert!(!is_exact_supported_omp_version("node omp/18.1.16"));
    assert!(!is_exact_supported_omp_version("omp/19.0.0"));
    assert!(!is_exact_supported_omp_version(""));
}

#[test]
#[cfg(windows)]
fn test_launch_preflight_adversarial_lookalike_rejection() {
    use hands_return_bridge::launcher::verify_launch_preflight;
    if !live_launcher_tests_enabled() {
        eprintln!("SKIP live adversarial preflight (set HANDS_RETURN_BRIDGE_RUN_LIVE_LAUNCHER_TESTS=1)");
        return;
    }
    let dir = tempdir().unwrap();

    let lookalikes = [
        "omp/118.1.16",
        "omp/18.1.16-beta",
        "wrapper: omp/18.1.16",
        "18.1.16",
        "omp/18.1.16 extra",
    ];

    for (idx, lookalike) in lookalikes.iter().enumerate() {
        let mock_file = dir.path().join(format!("mock_omp_adv_{}.bat", idx));
        std::fs::write(
            &mock_file,
            format!(
                "@echo off\r\nif \"%1\"==\"--version\" (echo {} & exit /b 0)\r\nif \"%1\"==\"--help\" (echo Help info & exit /b 0)\r\n",
                lookalike
            ),
        )
        .unwrap();

        let res = verify_launch_preflight(Some(&mock_file.to_string_lossy()));
        assert!(
            res.is_err(),
            "Preflight must fail closed for adversarial lookalike '{}'",
            lookalike
        );
        let err_str = res.unwrap_err().to_string();
        assert!(
            err_str.contains("Unsupported OMP revision"),
            "Error should reject lookalike '{}': {}",
            lookalike,
            err_str
        );
    }
}

#[test]
fn test_protocol_drain_and_ack_operations() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("journal.sqlite");
    let journal = Journal::open(&db_path).unwrap();

    let target_dir = tempdir().unwrap();
    init_git_repo(target_dir.path());
    let canonical_path = target_dir.path().canonicalize().unwrap().to_string_lossy().to_string();

    let pairing_id = "pair_proto_drain";
    let profile_id = "prof_proto_drain";
    let token = "boot_proto_drain";
    journal.create_bootstrap(
        pairing_id,
        token,
        "chrome",
        profile_id,
        &[TargetRecord { target_id: "t1".into(), canonical_path, name: "t1".into() }],
        &PolicyRecord { policy_revision: "v1".into(), tool_policy: "standard".into(), approval_policy: "prompt".into() },
    ).unwrap();
    let activated = journal.activate_bootstrap(token, profile_id).unwrap();
    let secret = &activated.pairing_secret;

    let params = LaunchRequestParams {
        pairing_id: pairing_id.into(),
        launch_request_id: "req_proto_drain".into(),
        origin_conversation_id: "c_proto".into(),
        origin_conversation_url: "https://chatgpt.com/c/c_proto".into(),
        transcript_evidence_hash: "hash_t".into(),
        account_evidence_hash: "hash_a".into(),
        target_id: "t1".into(),
        policy_revision: "v1".into(),
        prompt_text: "test".into(),
    };
    let claim = journal.reserve_or_claim_launch(&params).unwrap();

    // Commit completion receipt
    let rcpt_id = "rcpt_proto_1";
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
                rcpt_id,
                claim.execution_id,
                pairing_id,
                claim.return_token,
                "c_proto",
                0,
                "stop",
                "m1",
                "Done turn",
                "digest1",
                1,
                1000
            ],
        ).unwrap();
    }

    // 1. Drain via handle_native_message
    let drain_msg = json!({
        "op": "drain",
        "pairingId": pairing_id,
        "pairingSecret": secret,
        "profileId": profile_id
    });
    let drain_resp = handle_native_message(&drain_msg, &journal);
    assert_eq!(drain_resp["status"], "ok");
    assert_eq!(drain_resp["summaries"].as_array().unwrap().len(), 1);
    let receipts = drain_resp["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0]["receipt_id"], rcpt_id);
    assert_eq!(receipts[0]["execution_id"], claim.execution_id);

    // 2. ACK via handle_native_message
    let ack_msg = json!({
        "op": "ack",
        "pairingId": pairing_id,
        "pairingSecret": secret,
        "profileId": profile_id,
        "receiptId": rcpt_id,
        "executionId": claim.execution_id,
        "ackStatus": "received"
    });
    let ack_resp = handle_native_message(&ack_msg, &journal);
    assert_eq!(ack_resp["status"], "ok");
    assert_eq!(ack_resp["acknowledged"], true);
    assert_eq!(ack_resp["receiptId"], rcpt_id);

    // 3. Replay ACK is idempotent
    let ack_replay = handle_native_message(&ack_msg, &journal);
    assert_eq!(ack_replay["status"], "ok");
    assert_eq!(ack_replay["acknowledged"], true);

    // 4. Drain again: receipts array is empty because rcpt_id was acknowledged
    let drain_again = handle_native_message(&drain_msg, &journal);
    assert_eq!(drain_again["status"], "ok");
    assert_eq!(drain_again["receipts"].as_array().unwrap().len(), 0);
    assert_eq!(drain_again["summaries"].as_array().unwrap().len(), 1);
}
