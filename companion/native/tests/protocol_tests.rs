use std::io::Cursor;
use tempfile::tempdir;

use hands_return_bridge::journal::{Journal, PolicyRecord, TargetRecord};
use hands_return_bridge::protocol::{
    handle_native_message, read_native_message, write_native_message,
};
use serde_json::json;

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
    let pairing_secret = "secret_closed_xyz";
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
            pairing_secret,
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
    assert_eq!(setup_resp["pairingSecret"], pairing_secret);
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
    assert_eq!(status_resp["taskExecutionAvailable"], false);

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

    // 5. Boundary guard: Task launch unavailable (issue #67 boundary)
    let launch_msg = json!({
        "op": "launch",
        "pairingId": pairing_id,
        "pairingSecret": pairing_secret,
        "profileId": profile_id,
        "targetId": "target_hands",
        "prompt": "do something"
    });
    let launch_resp = handle_native_message(&launch_msg, &journal);
    assert_eq!(launch_resp["status"], "error");
    assert_eq!(launch_resp["code"], "task_execution_unavailable");

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
