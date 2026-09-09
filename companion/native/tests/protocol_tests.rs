use std::io::Cursor;
use tempfile::tempdir;

use hands_return_bridge::journal::{Journal, PolicyRecord, TargetRecord};
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
        canonical_path: "F:\\CodeBase\\hands".to_string(),
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
fn test_policy_enforcement_and_omp_startup_command() {
    use std::path::Path;
    use hands_return_bridge::launcher::build_omp_startup_command;

    let adapter_path = Path::new("F:/CodeBase/test/adapter.ts");

    // Standard policy: explicit tool set, no-extensions, -e, no-prewalk, approval-mode
    let cmd_standard = build_omp_startup_command(adapter_path, "standard", "prompt").unwrap();
    assert!(cmd_standard.contains("--tools=read,edit,write,bash,grep,glob,lsp,todo"));
    assert!(cmd_standard.contains("--no-extensions"));
    assert!(cmd_standard.contains("-e \"F:/CodeBase/test/adapter.ts\""));
    assert!(cmd_standard.contains("--no-prewalk"));
    assert!(cmd_standard.contains("--approval-mode=always-ask"));

    // Read-only policy
    let cmd_ro = build_omp_startup_command(adapter_path, "read_only", "write").unwrap();
    assert!(cmd_ro.contains("--tools=read,grep,glob,lsp"));
    assert!(cmd_ro.contains("--approval-mode=write"));

    // None / no_tools policy
    let cmd_none = build_omp_startup_command(adapter_path, "none", "auto").unwrap();
    assert!(cmd_none.contains("--no-tools"));
    assert!(cmd_none.contains("--approval-mode=yolo"));

    // Unsupported tool policy fails closed
    assert!(build_omp_startup_command(adapter_path, "unrestricted", "prompt").is_err());
    // Unsupported approval policy fails closed
    assert!(build_omp_startup_command(adapter_path, "standard", "invalid_approval").is_err());
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

    // 4. Same request replayed returns SAME execution, isReplayed: true, state: unknown
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
    assert_eq!(replay_resp["status"], "ok");
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
    assert_eq!(replay2_resp["status"], "ok");
    assert_eq!(replay2_resp["isReplayed"], true);
    assert_eq!(replay2_resp["executionId"], claim2.execution_id);
}
