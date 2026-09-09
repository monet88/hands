use std::io::{Read, Write};

use serde_json::{Value, json};

use crate::host::resolve_state_dir;
use crate::journal::{
    Journal, LaunchRequestParams, PairingError,
};
use crate::launcher::{
    build_omp_startup_command, ensure_adapter_file, launch_orca_terminal,
    send_orca_terminal_prompt, verify_launch_preflight, wait_orca_terminal_idle, LauncherError,
};
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1 MB
pub const TRUST_NOTICE: &str = "Notice: A paired extension may submit coding-agent tasks. Target, argv, and policy validation does not sandbox model-directed tool execution or contain a compromised paired extension.";

/// Parses a canonical ChatGPT conversation URL and returns the canonical conversation ID.
/// Rejects other domains, paths (home, chat, new_chat, provisional), queries, and fragments.
pub fn parse_canonical_conversation_id(url: &str) -> Option<String> {
    if !url.starts_with("https://chatgpt.com/") {
        return None;
    }
    // Reject fragments and queries
    if url.contains('#') || url.contains('?') {
        return None;
    }
    let path = &url["https://chatgpt.com/".len()..];
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let id = match segments.as_slice() {
        ["c", id] => *id,
        ["g", gizmo, "c", id] if !gizmo.is_empty() => *id,
        _ => return None,
    };
    if id.is_empty()
        || id == "new"
        || id == "chat"
        || id.contains("new_chat")
        || id.contains("provisional")
    {
        return None;
    }
    Some(id.to_string())
}

#[derive(Debug)]
pub enum ProtocolError {
    Io(std::io::Error),
    Json(serde_json::Error),
    MessageTooLarge(usize),
}

impl From<std::io::Error> for ProtocolError {
    fn from(e: std::io::Error) -> Self {
        ProtocolError::Io(e)
    }
}

impl From<serde_json::Error> for ProtocolError {
    fn from(e: serde_json::Error) -> Self {
        ProtocolError::Json(e)
    }
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtocolError::Io(e) => write!(f, "IO error: {}", e),
            ProtocolError::Json(e) => write!(f, "JSON error: {}", e),
            ProtocolError::MessageTooLarge(size) => {
                write!(f, "Message size {} exceeds limit of 1MB", size)
            }
        }
    }
}

impl std::error::Error for ProtocolError {}

pub fn read_native_message<R: Read>(reader: &mut R) -> Result<Option<Value>, ProtocolError> {
    let mut len_bytes = [0u8; 4];
    match reader.read_exact(&mut len_bytes) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(ProtocolError::Io(e)),
    }

    let len = u32::from_le_bytes(len_bytes) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge(len));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf)?;

    let value: Value = serde_json::from_slice(&buf)?;
    Ok(Some(value))
}

pub fn write_native_message<W: Write>(writer: &mut W, value: &Value) -> Result<(), ProtocolError> {
    let json_bytes = serde_json::to_vec(value)?;
    let len = json_bytes.len();
    if len > MAX_MESSAGE_SIZE {
        return Err(ProtocolError::MessageTooLarge(len));
    }

    let len_bytes = (len as u32).to_le_bytes();
    writer.write_all(&len_bytes)?;
    writer.write_all(&json_bytes)?;
    writer.flush()?;
    Ok(())
}

pub fn handle_native_message(msg: &Value, journal: &Journal) -> Value {
    let obj = match msg.as_object() {
        Some(o) => o,
        None => {
            return json!({
                "status": "error",
                "code": "invalid_payload",
                "message": "Message must be a JSON object"
            });
        }
    };

    // Check for unauthorized browser overrides:
    // Browser messages cannot register or replace targets, policy, executable, adapter, argv, environment, or extra providers.
    const FORBIDDEN_FIELDS: &[&str] = &[
        "targets",
        "target",
        "canonicalPath",
        "policy",
        "policyRevision",
        "toolPolicy",
        "approvalPolicy",
        "executable",
        "cwd",
        "adapter",
        "argv",
        "environment",
        "providers",
        "extraProviders",
        "launchPolicy",
    ];

    for field in FORBIDDEN_FIELDS {
        if obj.contains_key(*field) {
            return json!({
                "status": "error",
                "code": "unauthorized_override",
                "message": format!("Browser input cannot register or replace '{}'", field)
            });
        }
    }

    let op = match obj.get("op").and_then(|v| v.as_str()) {
        Some(o) => o,
        None => {
            return json!({
                "status": "error",
                "code": "missing_operation",
                "message": "Missing 'op' field"
            });
        }
    };

    // Strict per-op field allowlists: reject unknown, unapproved, or filesystem/command fields
    let allowed_fields: &[&str] = match op {
        "setup" => &["op", "bootstrapToken", "profileId"],
        "connect" | "status" | "revoke" => &["op", "pairingId", "pairingSecret", "profileId"],
        "launch" => &[
            "op",
            "pairingId",
            "pairingSecret",
            "profileId",
            "launchRequestId",
            "originConversationId",
            "originConversationUrl",
            "transcriptEvidenceHash",
            "accountEvidenceHash",
            "targetId",
            "requestedPolicyRevision",
            "promptText",
        ],
        "recover" => &[
            "op",
            "pairingId",
            "pairingSecret",
            "profileId",
            "launchRequestId",
        ],
        _ => {
            return json!({
                "status": "error",
                "code": "unsupported_operation",
                "message": format!("Unsupported operation: {}", op)
            });
        }
    };

    for key in obj.keys() {
        if !allowed_fields.contains(&key.as_str()) {
            return json!({
                "status": "error",
                "code": "unexpected_field",
                "message": format!("Unexpected or unapproved field '{}' for op '{}'", key, op)
            });
        }
    }

    match op {
        "setup" => {
            let bootstrap_token = match obj.get("bootstrapToken").and_then(|v| v.as_str()) {
                Some(t) if !t.trim().is_empty() => t.trim(),
                _ => {
                    return json!({
                        "status": "error",
                        "code": "missing_bootstrap_token",
                        "message": "Missing or empty 'bootstrapToken'"
                    });
                }
            };

            let profile_id = match obj.get("profileId").and_then(|v| v.as_str()) {
                Some(p) if !p.trim().is_empty() => p.trim(),
                _ => {
                    return json!({
                        "status": "error",
                        "code": "missing_profile_id",
                        "message": "Missing or empty 'profileId'"
                    });
                }
            };

            match journal.activate_bootstrap(bootstrap_token, profile_id) {
                Ok(activated) => json!({
                    "status": "ok",
                    "pairingId": activated.pairing_id,
                    "pairingSecret": activated.pairing_secret,
                    "profileId": activated.profile_id,
                    "targets": activated.targets,
                    "policyRevision": activated.policy.policy_revision,
                    "trustNotice": TRUST_NOTICE
                }),
                Err(e) => map_pairing_error(e),
            }
        }
        "connect" => {
            let (pairing_id, pairing_secret, profile_id) = match extract_credentials(obj) {
                Ok(creds) => creds,
                Err(resp) => return resp,
            };

            match journal.authenticate_pairing(pairing_id, pairing_secret, profile_id) {
                Ok(ctx) => json!({
                    "status": "ok",
                    "pairingId": ctx.pairing_id,
                    "profileId": ctx.profile_id,
                    "pairingStatus": ctx.status.as_str(),
                    "targets": ctx.targets,
                    "policyRevision": ctx.policy_revision,
                    "policy": ctx.policy,
                    "trustNotice": TRUST_NOTICE
                }),
                Err(e) => map_pairing_error(e),
            }
        }
        "status" => {
            let (pairing_id, pairing_secret, profile_id) = match extract_credentials(obj) {
                Ok(creds) => creds,
                Err(resp) => return resp,
            };

            match journal.authenticate_pairing(pairing_id, pairing_secret, profile_id) {
                Ok(ctx) => json!({
                    "status": "ok",
                    "pairingId": ctx.pairing_id,
                    "profileId": ctx.profile_id,
                    "pairingStatus": ctx.status.as_str(),
                    "taskExecutionAvailable": true,
                    "targetsCount": ctx.targets.len(),
                    "policyRevision": ctx.policy_revision,
                    "trustNotice": TRUST_NOTICE
                }),
                Err(e) => map_pairing_error(e),
            }
        }
        "revoke" => {
            let (pairing_id, pairing_secret, profile_id) = match extract_credentials(obj) {
                Ok(creds) => creds,
                Err(resp) => return resp,
            };

            match journal.revoke_pairing(pairing_id, pairing_secret, profile_id) {
                Ok(()) => json!({
                    "status": "ok",
                    "pairingId": pairing_id,
                    "pairingStatus": "revoked"
                }),
                Err(e) => map_pairing_error(e),
            }
        }
        "launch" => {
            let (pairing_id, pairing_secret, profile_id) = match extract_credentials(obj) {
                Ok(creds) => creds,
                Err(resp) => return resp,
            };

            // 1. Authenticate pairing before claim/spawn
            let _ctx = match journal.authenticate_pairing(pairing_id, pairing_secret, profile_id) {
                Ok(c) => c,
                Err(e) => return map_pairing_error(e),
            };

            let launch_request_id = match obj.get("launchRequestId").and_then(|v| v.as_str()) {
                Some(id) if !id.trim().is_empty() => id.trim(),
                _ => return json!({ "status": "error", "code": "missing_launch_request_id", "message": "Missing or empty launchRequestId" }),
            };

            let origin_conv_id = match obj.get("originConversationId").and_then(|v| v.as_str()) {
                Some(id) if !id.trim().is_empty() => id.trim(),
                _ => return json!({ "status": "error", "code": "missing_origin_conversation_id", "message": "Missing or empty originConversationId" }),
            };

            let origin_conv_url = match obj.get("originConversationUrl").and_then(|v| v.as_str()) {
                Some(u) if !u.trim().is_empty() => u.trim(),
                _ => return json!({ "status": "error", "code": "missing_origin_conversation_url", "message": "Missing or empty originConversationUrl" }),
            };

            // Canonical conversation boundary & URL shape validation
            let canonical_id = match parse_canonical_conversation_id(origin_conv_url) {
                Some(id) => id,
                None => {
                    return json!({
                        "status": "error",
                        "code": "invalid_conversation_boundary",
                        "message": "Origin conversation URL must be a canonical existing ChatGPT conversation (e.g. https://chatgpt.com/c/<id>) without queries or fragments"
                    });
                }
            };

            if canonical_id != origin_conv_id {
                return json!({
                    "status": "error",
                    "code": "invalid_conversation_boundary",
                    "message": "Origin conversation ID does not match the canonical ID in the conversation URL"
                });
            }
            let transcript_hash = match obj.get("transcriptEvidenceHash").and_then(|v| v.as_str()) {
                Some(h) if !h.trim().is_empty() => h.trim(),
                _ => return json!({ "status": "error", "code": "missing_transcript_evidence_hash", "message": "Missing or empty transcriptEvidenceHash" }),
            };

            let account_hash = match obj.get("accountEvidenceHash").and_then(|v| v.as_str()) {
                Some(h) if !h.trim().is_empty() => h.trim(),
                _ => return json!({ "status": "error", "code": "missing_account_evidence_hash", "message": "Missing or empty accountEvidenceHash" }),
            };

            let target_id = match obj.get("targetId").and_then(|v| v.as_str()) {
                Some(t) if !t.trim().is_empty() => t.trim(),
                _ => return json!({ "status": "error", "code": "missing_target_id", "message": "Missing or empty targetId" }),
            };

            let policy_rev = match obj.get("requestedPolicyRevision").and_then(|v| v.as_str()) {
                Some(p) if !p.trim().is_empty() => p.trim(),
                _ => return json!({ "status": "error", "code": "missing_policy_revision", "message": "Missing or empty requestedPolicyRevision" }),
            };

            let prompt_text = match obj.get("promptText").and_then(|v| v.as_str()) {
                Some(p) if !p.trim().is_empty() => p,
                _ => return json!({ "status": "error", "code": "missing_prompt_text", "message": "Missing or empty promptText" }),
            };

            // Bounded prompt bytes check
            if prompt_text.len() > 128 * 1024 {
                return json!({ "status": "error", "code": "prompt_too_large", "message": "Prompt exceeds bounded size limit (128 KB)" });
            }

            let params = LaunchRequestParams {
                pairing_id: pairing_id.to_string(),
                launch_request_id: launch_request_id.to_string(),
                origin_conversation_id: origin_conv_id.to_string(),
                origin_conversation_url: origin_conv_url.to_string(),
                transcript_evidence_hash: transcript_hash.to_string(),
                account_evidence_hash: account_hash.to_string(),
                target_id: target_id.to_string(),
                policy_revision: policy_rev.to_string(),
                prompt_text: prompt_text.to_string(),
            };

            // 2. Reserve or claim launch atomically in SQLite transaction
            let claim = match journal.reserve_or_claim_launch(&params) {
                Ok(c) => c,
                Err(e) => return map_pairing_error(e),
            };

            if claim.is_replayed {
                // Return existing execution details without re-running launch attempt!
                let summary_opt = journal.get_launch_request_by_id(pairing_id, launch_request_id).unwrap_or(None);
                return json!({
                    "status": "ok",
                    "executionId": claim.execution_id,
                    "returnToken": claim.return_token,
                    "state": claim.state,
                    "isReplayed": true,
                    "summary": summary_opt,
                    "trustNotice": TRUST_NOTICE
                });
            }

            // 3. Resolve state dir & ensure adapter (pre-spawn)
            let state_dir = match resolve_state_dir(None) {
                Ok(sd) => sd,
                Err(e) => {
                    return json!({ "status": "error", "code": "storage_error", "message": e.to_string() });
                }
            };

            let adapter_path = match ensure_adapter_file(&state_dir) {
                Ok(p) => p,
                Err(e) => {
                    return json!({ "status": "error", "code": "adapter_error", "message": e.to_string() });
                }
            };

            // 4. Build native-owned OMP startup command (NO user prompt in command line)
            let startup_cmd = match build_omp_startup_command(
                &adapter_path,
                &claim.effective_tool_policy,
                &claim.effective_approval_policy,
            ) {
                Ok(cmd) => cmd,
                Err(e) => {
                    return json!({ "status": "error", "code": "launcher_error", "message": e.to_string() });
                }
            };
            // 4b. Explicit compatibility preflight check BEFORE marking attempt
            if let Err(e) = verify_launch_preflight(None) {
                return json!({
                    "status": "error",
                    "code": "preflight_failed",
                    "message": e.to_string()
                });
            }

            // 5. Mark single launch attempt atomically BEFORE invoking Orca CLI
            let can_attempt = match journal.mark_launch_attempt(&claim.execution_id, pairing_id) {
                Ok(can) => can,
                Err(e) => return map_pairing_error(e),
            };
            if !can_attempt {
                return json!({
                    "status": "error",
                    "code": "already_attempted",
                    "message": "Launch attempt has already been made for this execution"
                });
            }

            // 6. Create terminal with native-owned startup command
            let evidence = match launch_orca_terminal(&claim.canonical_target_path, &startup_cmd, &claim.execution_id) {
                Ok(ev) => {
                    let _ = journal.record_launch_start_evidence(&claim.execution_id, &ev);
                    ev
                }
                Err(LauncherError::OrcaSpawnFailed(e)) => {
                    // Provably no process started (binary missing or spawn failed)
                    let _ = journal.record_launch_pre_spawn_failure(&claim.execution_id, &e);
                    return json!({
                        "status": "error",
                        "code": "orca_spawn_failed",
                        "executionId": claim.execution_id,
                        "message": e
                    });
                }
                Err(LauncherError::OrcaExecutionUncertain(e)) => {
                    // Orca process was spawned; outcome uncertain: side effect may have occurred
                    let _ = journal.record_launch_uncertain(&claim.execution_id, None, &e);
                    return json!({
                        "status": "error",
                        "code": "launch_uncertain",
                        "executionId": claim.execution_id,
                        "state": "unknown",
                        "message": format!("Orca invocation returned uncertain outcome; terminal state unknown: {}", e),
                        "trustNotice": TRUST_NOTICE
                    });
                }
                Err(e) => {
                    let _ = journal.record_launch_uncertain(&claim.execution_id, None, &e.to_string());
                    return json!({
                        "status": "error",
                        "code": "launch_uncertain",
                        "executionId": claim.execution_id,
                        "state": "unknown",
                        "message": e.to_string(),
                        "trustNotice": TRUST_NOTICE
                    });
                }
            };

            // 7. Terminal handle exists: wait for terminal to become tui-idle, then send literal prompt
            let terminal_handle = evidence.orca_terminal_handle.as_ref().unwrap();
            if let Err(e) = wait_orca_terminal_idle(terminal_handle, 10000) {
                let err_str = format!("Orca terminal wait tui-idle failed: {}", e);
                let _ = journal.record_launch_uncertain(&claim.execution_id, Some(&evidence), &err_str);
                return json!({
                    "status": "error",
                    "code": "launch_uncertain",
                    "executionId": claim.execution_id,
                    "state": "unknown",
                    "message": err_str,
                    "terminalEvidence": {
                        "orcaTerminalHandle": evidence.orca_terminal_handle,
                        "orcaTabId": evidence.orca_tab_id,
                        "orcaPaneKey": evidence.orca_pane_key,
                        "orcaPtyId": evidence.orca_pty_id,
                    },
                    "trustNotice": TRUST_NOTICE
                });
            }

            if let Err(e) = send_orca_terminal_prompt(terminal_handle, &claim.prompt_text) {
                let err_str = format!("Orca terminal send prompt failed: {}", e);
                let _ = journal.record_launch_uncertain(&claim.execution_id, Some(&evidence), &err_str);
                return json!({
                    "status": "error",
                    "code": "launch_uncertain",
                    "executionId": claim.execution_id,
                    "state": "unknown",
                    "message": err_str,
                    "terminalEvidence": {
                        "orcaTerminalHandle": evidence.orca_terminal_handle,
                        "orcaTabId": evidence.orca_tab_id,
                        "orcaPaneKey": evidence.orca_pane_key,
                        "orcaPtyId": evidence.orca_pty_id,
                    },
                    "trustNotice": TRUST_NOTICE
                });
            }

            // 8. Success! Record started state
            let _ = journal.record_launch_started(&claim.execution_id, &evidence);

            json!({
                "status": "ok",
                "executionId": claim.execution_id,
                "returnToken": claim.return_token,
                "canonicalTargetPath": claim.canonical_target_path,
                "state": "started",
                "isReplayed": false,
                "terminalEvidence": {
                    "orcaTerminalHandle": evidence.orca_terminal_handle,
                    "orcaTabId": evidence.orca_tab_id,
                    "orcaPaneKey": evidence.orca_pane_key,
                    "orcaPtyId": evidence.orca_pty_id,
                },
                "trustNotice": TRUST_NOTICE
            })
        }
        "recover" => {
            let (pairing_id, pairing_secret, profile_id) = match extract_credentials(obj) {
                Ok(creds) => creds,
                Err(resp) => return resp,
            };

            // Authenticate pairing
            let _ctx = match journal.authenticate_pairing(pairing_id, pairing_secret, profile_id) {
                Ok(c) => c,
                Err(e) => return map_pairing_error(e),
            };

            let req_id_opt = obj.get("launchRequestId").and_then(|v| v.as_str());
            if let Some(req_id) = req_id_opt {
                match journal.get_launch_request_by_id(pairing_id, req_id) {
                    Ok(Some(summary)) => json!({
                        "status": "ok",
                        "summary": summary,
                        "trustNotice": TRUST_NOTICE
                    }),
                    Ok(None) => json!({
                        "status": "error",
                        "code": "request_not_found",
                        "message": format!("Launch request '{}' not found", req_id)
                    }),
                    Err(e) => map_pairing_error(e),
                }
            } else {
                // Return bounded launch-intent summaries
                match journal.get_launch_summaries(pairing_id) {
                    Ok(summaries) => json!({
                        "status": "ok",
                        "summaries": summaries,
                        "trustNotice": TRUST_NOTICE
                    }),
                    Err(e) => map_pairing_error(e),
                }
            }
        }
        other => json!({
            "status": "error",
            "code": "unsupported_operation",
            "message": format!("Operation '{}' is not supported", other)
        }),
    }
}

fn extract_credentials<'a>(
    obj: &'a serde_json::Map<String, Value>,
) -> Result<(&'a str, &'a str, &'a str), Value> {
    let pairing_id = match obj.get("pairingId").and_then(|v| v.as_str()) {
        Some(id) if !id.trim().is_empty() => id.trim(),
        _ => {
            return Err(json!({
                "status": "error",
                "code": "missing_pairing_id",
                "message": "Missing or empty 'pairingId'"
            }));
        }
    };

    let pairing_secret = match obj.get("pairingSecret").and_then(|v| v.as_str()) {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => {
            return Err(json!({
                "status": "error",
                "code": "missing_pairing_secret",
                "message": "Missing or empty 'pairingSecret'"
            }));
        }
    };

    let profile_id = match obj.get("profileId").and_then(|v| v.as_str()) {
        Some(p) if !p.trim().is_empty() => p.trim(),
        _ => {
            return Err(json!({
                "status": "error",
                "code": "missing_profile_id",
                "message": "Missing or empty 'profileId'"
            }));
        }
    };

    Ok((pairing_id, pairing_secret, profile_id))
}

fn map_pairing_error(err: PairingError) -> Value {
    match err {
        PairingError::NotFound => json!({
            "status": "error",
            "code": "pairing_not_found",
            "message": "Pairing ID or bootstrap token not found"
        }),
        PairingError::InvalidSecret => json!({
            "status": "error",
            "code": "invalid_pairing_secret",
            "message": "Invalid pairing secret"
        }),
        PairingError::ProfileMismatch => json!({
            "status": "error",
            "code": "profile_mismatch",
            "message": "Request profile ID does not match paired browser profile"
        }),
        PairingError::Retired => json!({
            "status": "error",
            "code": "pairing_retired",
            "message": "This pairing has been revoked or retired"
        }),
        PairingError::NotActive => json!({
            "status": "error",
            "code": "pairing_not_active",
            "message": "Pairing is not active"
        }),
        PairingError::TargetNotFound => json!({
            "status": "error",
            "code": "target_not_found",
            "message": "Target workspace not found or not registered for this pairing"
        }),
        PairingError::TargetRevokedOrMismatched => json!({
            "status": "error",
            "code": "target_revoked_or_mismatched",
            "message": "Target directory is invalid, revoked, or no longer exists"
        }),
        PairingError::PolicyMismatch => json!({
            "status": "error",
            "code": "policy_mismatch",
            "message": "Requested policy revision does not match registered native policy"
        }),
        PairingError::PolicyUnsupported => json!({
            "status": "error",
            "code": "policy_unsupported",
            "message": "Native policy specifies unsupported tool or approval policy"
        }),
        PairingError::PayloadConflict => json!({
            "status": "error",
            "code": "payload_conflict",
            "message": "Same launchRequestId reused with conflicting payload bytes or binding"
        }),
        PairingError::ReplayTombstoned => json!({
            "status": "error",
            "code": "replay_tombstoned",
            "message": "This launchRequestId has been tombstoned and cannot be launched again"
        }),
        PairingError::AlreadyAttempted => json!({
            "status": "error",
            "code": "already_attempted",
            "message": "This launch request has already been attempted"
        }),
        PairingError::StorageError(e) => json!({
            "status": "error",
            "code": "storage_error",
            "message": format!("Database error: {}", e)
        }),
    }
}
