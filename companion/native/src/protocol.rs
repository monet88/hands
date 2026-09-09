use std::io::{Read, Write};

use serde_json::{Value, json};

use crate::journal::{Journal, PairingError};

pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024; // 1 MB
pub const TRUST_NOTICE: &str = "Notice: A paired extension may submit coding-agent tasks. Target, argv, and policy validation does not sandbox model-directed tool execution or contain a compromised paired extension.";

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
        "launch" => {
            // Task execution remains unavailable under #66
            return json!({
                "status": "error",
                "code": "task_execution_unavailable",
                "message": "Task execution is unavailable until owned-launch slice lands (#67)"
            });
        }
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
                    "taskExecutionAvailable": false,
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
        PairingError::StorageError(e) => json!({
            "status": "error",
            "code": "storage_error",
            "message": format!("Database error: {}", e)
        }),
    }
}
