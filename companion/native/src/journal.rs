use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use parking_lot::Mutex;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetRecord {
    pub target_id: String,
    pub canonical_path: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRecord {
    pub policy_revision: String,
    pub tool_policy: String,
    pub approval_policy: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingStatus {
    Pending,
    Active,
    Revoked,
}

impl PairingStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PairingStatus::Pending => "pending",
            PairingStatus::Active => "active",
            PairingStatus::Revoked => "revoked",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(PairingStatus::Pending),
            "active" => Some(PairingStatus::Active),
            "revoked" | "retired" => Some(PairingStatus::Revoked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivatedPairing {
    pub pairing_id: String,
    pub pairing_secret: String,
    pub profile_id: String,
    pub targets: Vec<TargetRecord>,
    pub policy: PolicyRecord,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingContext {
    pub pairing_id: String,
    pub profile_id: String,
    pub browser: String,
    pub status: PairingStatus,
    pub policy_revision: String,
    pub targets: Vec<TargetRecord>,
    pub policy: PolicyRecord,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairingError {
    NotFound,
    InvalidSecret,
    ProfileMismatch,
    Retired,
    NotActive,
    TargetNotFound,
    TargetRevokedOrMismatched,
    PolicyMismatch,
    PolicyUnsupported,
    PayloadConflict,
    ReplayTombstoned,
    AlreadyAttempted,
    StorageError(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::NotFound => write!(f, "pairing_not_found"),
            PairingError::InvalidSecret => write!(f, "invalid_pairing_secret"),
            PairingError::ProfileMismatch => write!(f, "profile_mismatch"),
            PairingError::Retired => write!(f, "pairing_retired"),
            PairingError::NotActive => write!(f, "pairing_not_active"),
            PairingError::TargetNotFound => write!(f, "target_not_found"),
            PairingError::TargetRevokedOrMismatched => write!(f, "target_revoked_or_mismatched"),
            PairingError::PolicyMismatch => write!(f, "policy_mismatch"),
            PairingError::PolicyUnsupported => write!(f, "policy_unsupported"),
            PairingError::PayloadConflict => write!(f, "payload_conflict"),
            PairingError::ReplayTombstoned => write!(f, "replay_tombstoned"),
            PairingError::AlreadyAttempted => write!(f, "already_attempted"),
            PairingError::StorageError(e) => write!(f, "storage_error: {}", e),
        }
    }
}

impl std::error::Error for PairingError {}

pub struct Journal {
    conn: Mutex<Connection>,
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"hands_rb_salt_v1:");
    hasher.update(secret.as_bytes());
    hex::encode(hasher.finalize())
}

fn generate_random_secret(prefix: &str, num_bytes: usize) -> Result<String, PairingError> {
    let mut bytes = vec![0u8; num_bytes];
    getrandom::fill(&mut bytes)
        .map_err(|e| PairingError::StorageError(format!("OS CSPRNG failure: {}", e)))?;
    let hex_part: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();
    Ok(format!("{}_{}", prefix, hex_part))
}

// Simple hex encoder to avoid another dependency
mod hex {
    pub fn encode(bytes: impl AsRef<[u8]>) -> String {
        bytes
            .as_ref()
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }
}

// Constant-time string comparison
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut result = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        result |= x ^ y;
    }
    result == 0
}


#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchRequestParams {
    pub pairing_id: String,
    pub launch_request_id: String,
    pub origin_conversation_id: String,
    pub origin_conversation_url: String,
    pub transcript_evidence_hash: String,
    pub account_evidence_hash: String,
    pub target_id: String,
    pub policy_revision: String,
    pub prompt_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchClaimResult {
    pub execution_id: String,
    pub return_token: String,
    pub canonical_target_path: String,
    pub effective_tool_policy: String,
    pub effective_approval_policy: String,
    pub prompt_text: String,
    pub state: String,
    pub is_replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchSummary {
    pub launch_request_id: String,
    pub execution_id: String,
    pub origin_conversation_id: String,
    pub origin_conversation_url: String,
    pub target_id: String,
    pub policy_revision: String,
    pub state: String,
    pub created_at: i64,
    pub attempt_marked_at: Option<i64>,
    pub orca_terminal_handle: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptEvidence {
    pub orca_terminal_handle: Option<String>,
    pub orca_tab_id: Option<String>,
    pub orca_pane_key: Option<String>,
    pub orca_pty_id: Option<String>,
}


pub fn compute_payload_digest(
    origin_conversation_id: &str,
    origin_conversation_url: &str,
    transcript_evidence_hash: &str,
    account_evidence_hash: &str,
    target_id: &str,
    policy_revision: &str,
    prompt_text: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"hands_rb_payload_v1:");
    hasher.update(origin_conversation_id.as_bytes());
    hasher.update(b":");
    hasher.update(origin_conversation_url.as_bytes());
    hasher.update(b":");
    hasher.update(transcript_evidence_hash.as_bytes());
    hasher.update(b":");
    hasher.update(account_evidence_hash.as_bytes());
    hasher.update(b":");
    hasher.update(target_id.as_bytes());
    hasher.update(b":");
    hasher.update(policy_revision.as_bytes());
    hasher.update(b":");
    hasher.update(prompt_text.as_bytes());
    hex::encode(hasher.finalize())
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS pairings (
                pairing_id TEXT PRIMARY KEY,
                bootstrap_token_hash TEXT UNIQUE,
                pairing_secret_hash TEXT,
                browser TEXT NOT NULL,
                profile_id TEXT NOT NULL,
                status TEXT NOT NULL,
                policy_revision TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS targets (
                pairing_id TEXT NOT NULL,
                target_id TEXT NOT NULL,
                canonical_path TEXT NOT NULL,
                name TEXT NOT NULL,
                PRIMARY KEY (pairing_id, target_id),
                FOREIGN KEY (pairing_id) REFERENCES pairings(pairing_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS policies (
                pairing_id TEXT NOT NULL,
                policy_revision TEXT NOT NULL,
                tool_policy TEXT NOT NULL,
                approval_policy TEXT NOT NULL,
                PRIMARY KEY (pairing_id, policy_revision),
                FOREIGN KEY (pairing_id) REFERENCES pairings(pairing_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS host_config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS launch_requests (
                pairing_id TEXT NOT NULL,
                launch_request_id TEXT NOT NULL,
                execution_id TEXT NOT NULL UNIQUE,
                return_token TEXT NOT NULL UNIQUE,
                origin_conversation_id TEXT NOT NULL,
                origin_conversation_url TEXT NOT NULL,
                transcript_evidence_hash TEXT NOT NULL,
                account_evidence_hash TEXT NOT NULL,
                target_id TEXT NOT NULL,
                canonical_target_path TEXT NOT NULL,
                policy_revision TEXT NOT NULL,
                effective_tool_policy TEXT NOT NULL,
                effective_approval_policy TEXT NOT NULL,
                prompt_text TEXT NOT NULL,
                payload_digest TEXT NOT NULL,
                state TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (pairing_id, launch_request_id),
                FOREIGN KEY (pairing_id) REFERENCES pairings(pairing_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS launch_attempts (
                execution_id TEXT PRIMARY KEY,
                pairing_id TEXT NOT NULL,
                attempt_marked_at INTEGER NOT NULL,
                invoked_at INTEGER,
                orca_terminal_handle TEXT,
                orca_tab_id TEXT,
                orca_pane_key TEXT,
                orca_pty_id TEXT,
                state TEXT NOT NULL,
                failure_reason TEXT,
                FOREIGN KEY (execution_id) REFERENCES launch_requests(execution_id) ON DELETE CASCADE,
                FOREIGN KEY (pairing_id) REFERENCES pairings(pairing_id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS replay_tombstones (
                pairing_id TEXT NOT NULL,
                launch_request_id TEXT NOT NULL,
                execution_id TEXT NOT NULL,
                payload_digest TEXT NOT NULL,
                tombstoned_at INTEGER NOT NULL,
                PRIMARY KEY (pairing_id, launch_request_id)
            );
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn set_expected_extension_id(&self, extension_id: &str) -> Result<(), PairingError> {
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let existing: Option<String> = tx
            .query_row(
                "SELECT value FROM host_config WHERE key = 'expected_extension_id'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        match existing {
            Some(curr) => {
                if curr != extension_id {
                    return Err(PairingError::StorageError(format!(
                        "Host already configured with expected extension ID '{}'; cannot overwrite with '{}'",
                        curr, extension_id
                    )));
                }
            }
            None => {
                tx.execute(
                    "INSERT INTO host_config (key, value) VALUES ('expected_extension_id', ?1)",
                    params![extension_id],
                )
                .map_err(|e| PairingError::StorageError(e.to_string()))?;
            }
        }

        tx.commit()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn get_expected_extension_id(&self) -> Result<Option<String>, PairingError> {
        let conn = self.conn.lock();
        let res: Option<String> = conn
            .query_row(
                "SELECT value FROM host_config WHERE key = 'expected_extension_id'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;
        Ok(res)
    }

    pub fn create_bootstrap(
        &self,
        pairing_id: &str,
        bootstrap_token: &str,
        browser: &str,
        profile_id: &str,
        targets: &[TargetRecord],
        policy: &PolicyRecord,
    ) -> Result<(), rusqlite::Error> {
        let now = now_epoch_secs();
        let token_hash = hash_secret(bootstrap_token);

        let mut conn = self.conn.lock();
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        tx.execute(
            r#"
            INSERT INTO pairings (
                pairing_id, bootstrap_token_hash, pairing_secret_hash,
                browser, profile_id, status, policy_revision,
                created_at, updated_at
            ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, ?7, ?7)
            "#,
            params![
                pairing_id,
                token_hash,
                browser,
                profile_id,
                PairingStatus::Pending.as_str(),
                policy.policy_revision,
                now
            ],
        )?;

        for target in targets {
            tx.execute(
                r#"
                INSERT INTO targets (pairing_id, target_id, canonical_path, name)
                VALUES (?1, ?2, ?3, ?4)
                "#,
                params![
                    pairing_id,
                    target.target_id,
                    target.canonical_path,
                    target.name
                ],
            )?;
        }

        tx.execute(
            r#"
            INSERT INTO policies (pairing_id, policy_revision, tool_policy, approval_policy)
            VALUES (?1, ?2, ?3, ?4)
            "#,
            params![
                pairing_id,
                policy.policy_revision,
                policy.tool_policy,
                policy.approval_policy
            ],
        )?;

        tx.commit()?;
        Ok(())
    }

    pub fn delete_pairing(&self, pairing_id: &str) -> Result<(), rusqlite::Error> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM pairings WHERE pairing_id = ?1",
            params![pairing_id],
        )?;
        Ok(())
    }

    pub fn get_pairing_status(&self, pairing_id: &str) -> Result<PairingStatus, PairingError> {
        let conn = self.conn.lock();
        let status_str: Option<String> = conn
            .query_row(
                "SELECT status FROM pairings WHERE pairing_id = ?1",
                params![pairing_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        match status_str {
            Some(s) => PairingStatus::parse(&s)
                .ok_or_else(|| PairingError::StorageError("unknown status".into())),
            None => Err(PairingError::NotFound),
        }
    }

    pub fn activate_bootstrap(
        &self,
        bootstrap_token: &str,
        profile_id: &str,
    ) -> Result<ActivatedPairing, PairingError> {
        let now = now_epoch_secs();
        let token_hash = hash_secret(bootstrap_token);

        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        // Find pairing with this bootstrap token hash
        let row: Option<(String, String, String, String)> = tx
            .query_row(
                r#"
                SELECT pairing_id, profile_id, status, policy_revision
                FROM pairings
                WHERE bootstrap_token_hash = ?1
                "#,
                params![token_hash],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let (pairing_id, stored_profile_id, status_str, policy_rev) = match row {
            Some(r) => r,
            None => return Err(PairingError::NotFound),
        };

        let status = PairingStatus::parse(&status_str)
            .ok_or_else(|| PairingError::StorageError("unknown status".into()))?;

        if status == PairingStatus::Revoked {
            return Err(PairingError::Retired);
        }
        if status != PairingStatus::Pending {
            return Err(PairingError::NotActive);
        }

        // Profile ID must match the one selected during native setup!
        if stored_profile_id != profile_id {
            return Err(PairingError::ProfileMismatch);
        }

        // Generate the pairing secret ONLY upon successful activation using CSPRNG
        let pairing_secret = generate_random_secret("rb_sec", 16)?;
        let secret_hash = hash_secret(&pairing_secret);

        // Atomic conditional update: exactly one winner under concurrency!
        let affected = tx
            .execute(
                r#"
                UPDATE pairings
                SET bootstrap_token_hash = NULL,
                    pairing_secret_hash = ?1,
                    status = ?2,
                    updated_at = ?3
                WHERE pairing_id = ?4
                  AND status = ?5
                  AND profile_id = ?6
                  AND bootstrap_token_hash = ?7
                "#,
                params![
                    secret_hash,
                    PairingStatus::Active.as_str(),
                    now,
                    pairing_id,
                    PairingStatus::Pending.as_str(),
                    profile_id,
                    token_hash
                ],
            )
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        if affected == 0 {
            return Err(PairingError::NotFound);
        }

        let targets = Self::get_targets_inner(&tx, &pairing_id)?;
        let policy = Self::get_policy_inner(&tx, &pairing_id, &policy_rev)?;

        tx.commit()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(ActivatedPairing {
            pairing_id,
            pairing_secret,
            profile_id: profile_id.to_string(),
            targets,
            policy,
        })
    }

    pub fn authenticate_pairing(
        &self,
        pairing_id: &str,
        pairing_secret: &str,
        profile_id: &str,
    ) -> Result<PairingContext, PairingError> {
        let conn = self.conn.lock();
        let row: Option<(Option<String>, String, String, String, String)> = conn
            .query_row(
                r#"
                SELECT pairing_secret_hash, browser, profile_id, status, policy_revision
                FROM pairings
                WHERE pairing_id = ?1
                "#,
                params![pairing_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let (stored_hash_opt, browser, stored_profile_id, status_str, policy_rev) = match row {
            Some(r) => r,
            None => return Err(PairingError::NotFound),
        };

        let status = PairingStatus::parse(&status_str)
            .ok_or_else(|| PairingError::StorageError("unknown status".into()))?;

        if status == PairingStatus::Revoked {
            return Err(PairingError::Retired);
        }
        if status == PairingStatus::Pending || status != PairingStatus::Active {
            return Err(PairingError::NotActive);
        }

        let stored_hash = match stored_hash_opt {
            Some(h) if !h.is_empty() => h,
            _ => return Err(PairingError::NotActive),
        };

        // Verify secret hash
        let given_hash = hash_secret(pairing_secret);
        if !constant_time_eq(&stored_hash, &given_hash) {
            return Err(PairingError::InvalidSecret);
        }

        // Verify profile isolation (N4): fail closed if stored profile_id is empty, then require exact equality
        if stored_profile_id.trim().is_empty() || stored_profile_id != profile_id {
            return Err(PairingError::ProfileMismatch);
        }

        let targets = Self::get_targets_inner(&conn, pairing_id)?;
        let policy = Self::get_policy_inner(&conn, pairing_id, &policy_rev)?;

        Ok(PairingContext {
            pairing_id: pairing_id.to_string(),
            profile_id: stored_profile_id,
            browser,
            status,
            policy_revision: policy_rev,
            targets,
            policy,
        })
    }

    pub fn revoke_pairing(
        &self,
        pairing_id: &str,
        pairing_secret: &str,
        profile_id: &str,
    ) -> Result<(), PairingError> {
        // Authenticate first before revoking
        let _ = self.authenticate_pairing(pairing_id, pairing_secret, profile_id)?;
        let now = now_epoch_secs();

        let conn = self.conn.lock();
        conn.execute(
            r#"
            UPDATE pairings
            SET status = ?1, bootstrap_token_hash = NULL, pairing_secret_hash = NULL, updated_at = ?2
            WHERE pairing_id = ?3
            "#,
            params![PairingStatus::Revoked.as_str(), now, pairing_id],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn revoke_pairing_admin(&self, pairing_id: &str) -> Result<(), PairingError> {
        let now = now_epoch_secs();
        let conn = self.conn.lock();
        let affected = conn
            .execute(
                r#"
                UPDATE pairings
                SET status = ?1, bootstrap_token_hash = NULL, pairing_secret_hash = NULL, updated_at = ?2
                WHERE pairing_id = ?3
                "#,
                params![PairingStatus::Revoked.as_str(), now, pairing_id],
            )
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        if affected == 0 {
            Err(PairingError::NotFound)
        } else {
            Ok(())
        }
    }

    pub fn get_targets(&self, pairing_id: &str) -> Result<Vec<TargetRecord>, PairingError> {
        let conn = self.conn.lock();
        Self::get_targets_inner(&conn, pairing_id)
    }

    pub fn get_policy(
        &self,
        pairing_id: &str,
        policy_revision: &str,
    ) -> Result<PolicyRecord, PairingError> {
        let conn = self.conn.lock();
        Self::get_policy_inner(&conn, pairing_id, policy_revision)
    }

    fn get_targets_inner(
        conn: &Connection,
        pairing_id: &str,
    ) -> Result<Vec<TargetRecord>, PairingError> {
        let mut stmt = conn
            .prepare(
                r#"
                SELECT target_id, canonical_path, name
                FROM targets
                WHERE pairing_id = ?1
                ORDER BY target_id ASC
                "#,
            )
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let rows = stmt
            .query_map(params![pairing_id], |r| {
                Ok(TargetRecord {
                    target_id: r.get(0)?,
                    canonical_path: r.get(1)?,
                    name: r.get(2)?,
                })
            })
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let mut targets = Vec::new();
        for row in rows {
            targets.push(row.map_err(|e| PairingError::StorageError(e.to_string()))?);
        }
        Ok(targets)
    }

    fn get_policy_inner(
        conn: &Connection,
        pairing_id: &str,
        policy_revision: &str,
    ) -> Result<PolicyRecord, PairingError> {
        conn.query_row(
            r#"
            SELECT policy_revision, tool_policy, approval_policy
            FROM policies
            WHERE pairing_id = ?1 AND policy_revision = ?2
            "#,
            params![pairing_id, policy_revision],
            |r| {
                Ok(PolicyRecord {
                    policy_revision: r.get(0)?,
                    tool_policy: r.get(1)?,
                    approval_policy: r.get(2)?,
                })
            },
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => PairingError::NotFound,
            _ => PairingError::StorageError(e.to_string()),
        })
    }

pub fn verify_git_target_identity(canonical_path_str: &str) -> Result<PathBuf, PairingError> {
    let raw_path = Path::new(canonical_path_str);
    if !raw_path.exists() || !raw_path.is_dir() {
        return Err(PairingError::TargetRevokedOrMismatched);
    }
    let canonical = raw_path
        .canonicalize()
        .map_err(|_| PairingError::TargetRevokedOrMismatched)?;

    let output = std::process::Command::new("git")
        .args(["-C", &canonical.to_string_lossy(), "rev-parse", "--show-toplevel"])
        .output()
        .map_err(|_| PairingError::TargetRevokedOrMismatched)?;

    if !output.status.success() {
        return Err(PairingError::TargetRevokedOrMismatched);
    }

    let toplevel_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if toplevel_str.is_empty() {
        return Err(PairingError::TargetRevokedOrMismatched);
    }

    let toplevel_canonical = PathBuf::from(toplevel_str)
        .canonicalize()
        .map_err(|_| PairingError::TargetRevokedOrMismatched)?;

    if toplevel_canonical != canonical {
        return Err(PairingError::TargetRevokedOrMismatched);
    }

    Ok(canonical)
}

    pub fn reserve_or_claim_launch(
        &self,
        params: &LaunchRequestParams,
    ) -> Result<LaunchClaimResult, PairingError> {
        let now = now_epoch_secs();
        let payload_digest = compute_payload_digest(
            &params.origin_conversation_id,
            &params.origin_conversation_url,
            &params.transcript_evidence_hash,
            &params.account_evidence_hash,
            &params.target_id,
            &params.policy_revision,
            &params.prompt_text,
        );

        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        // 1. Check if this (pairing_id, launch_request_id) already exists in retained launch_requests
        let existing: Option<(String, String, String, String, String, String, String, String)> = tx
            .query_row(
                r#"
                SELECT execution_id, return_token, canonical_target_path,
                       effective_tool_policy, effective_approval_policy,
                       prompt_text, payload_digest, state
                FROM launch_requests
                WHERE pairing_id = ?1 AND launch_request_id = ?2
                "#,
                params![&params.pairing_id, &params.launch_request_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        if let Some((
            execution_id,
            return_token,
            canonical_target_path,
            effective_tool_policy,
            effective_approval_policy,
            stored_prompt,
            stored_digest,
            state,
        )) = existing
        {
            // Same key + changed payload => explicit conflict, zero new side effect!
            if stored_digest != payload_digest {
                return Err(PairingError::PayloadConflict);
            }

            tx.commit()
                .map_err(|e| PairingError::StorageError(e.to_string()))?;

            return Ok(LaunchClaimResult {
                execution_id,
                return_token,
                canonical_target_path,
                effective_tool_policy,
                effective_approval_policy,
                prompt_text: stored_prompt,
                state,
                is_replayed: true,
            });
        }

        // 2. Check replay tombstone: if retained row was pruned, tombstone prevents re-launch
        let tombstone: Option<String> = tx
            .query_row(
                "SELECT payload_digest FROM replay_tombstones WHERE pairing_id = ?1 AND launch_request_id = ?2",
                params![&params.pairing_id, &params.launch_request_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        if let Some(tombstone_digest) = tombstone {
            if tombstone_digest != payload_digest {
                return Err(PairingError::PayloadConflict);
            } else {
                return Err(PairingError::ReplayTombstoned);
            }
        }

        // 3. New launch request: revalidate target and policy before allocating claim
        let target: Option<TargetRecord> = tx
            .query_row(
                "SELECT target_id, canonical_path, name FROM targets WHERE pairing_id = ?1 AND target_id = ?2",
                params![&params.pairing_id, &params.target_id],
                |r| {
                    Ok(TargetRecord {
                        target_id: r.get(0)?,
                        canonical_path: r.get(1)?,
                        name: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let target_rec = match target {
            Some(t) => t,
            None => return Err(PairingError::TargetNotFound),
        };

        // Canonical verification of target path: must resolve to SAME registered git worktree
        let canonical_verified = Self::verify_git_target_identity(&target_rec.canonical_path)?;
        let canonical_target_path = canonical_verified.to_string_lossy().to_string();

        let policy: Option<PolicyRecord> = tx
            .query_row(
                "SELECT policy_revision, tool_policy, approval_policy FROM policies WHERE pairing_id = ?1 AND policy_revision = ?2",
                params![&params.pairing_id, &params.policy_revision],
                |r| {
                    Ok(PolicyRecord {
                        policy_revision: r.get(0)?,
                        tool_policy: r.get(1)?,
                        approval_policy: r.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let policy_rec = match policy {
            Some(p) => p,
            None => return Err(PairingError::PolicyMismatch),
        };

        // Enforce supported policy values: fail closed on unsupported tool/approval policy
        if policy_rec.tool_policy.trim().is_empty() || policy_rec.approval_policy.trim().is_empty() {
            return Err(PairingError::PolicyUnsupported);
        }

        // Allocate opaque executionId and returnToken
        let execution_id = generate_random_secret("exec", 16)?;
        let return_token = generate_random_secret("ret", 24)?;

        tx.execute(
            r#"
            INSERT INTO launch_requests (
                pairing_id, launch_request_id, execution_id, return_token,
                origin_conversation_id, origin_conversation_url,
                transcript_evidence_hash, account_evidence_hash,
                target_id, canonical_target_path,
                policy_revision, effective_tool_policy, effective_approval_policy,
                prompt_text, payload_digest, state, created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?17
            )
            "#,
            params![
                &params.pairing_id,
                &params.launch_request_id,
                &execution_id,
                &return_token,
                &params.origin_conversation_id,
                &params.origin_conversation_url,
                &params.transcript_evidence_hash,
                &params.account_evidence_hash,
                &params.target_id,
                &canonical_target_path,
                &policy_rec.policy_revision,
                &policy_rec.tool_policy,
                &policy_rec.approval_policy,
                &params.prompt_text,
                &payload_digest,
                "claimed",
                now,
            ],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        // Keep tombstone/digest durably for every accepted key in the same acceptance transaction
        tx.execute(
            r#"
            INSERT OR REPLACE INTO replay_tombstones (
                pairing_id, launch_request_id, execution_id, payload_digest, tombstoned_at
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                &params.pairing_id,
                &params.launch_request_id,
                &execution_id,
                &payload_digest,
                now,
            ],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        tx.commit()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(LaunchClaimResult {
            execution_id,
            return_token,
            canonical_target_path,
            effective_tool_policy: policy_rec.tool_policy,
            effective_approval_policy: policy_rec.approval_policy,
            prompt_text: params.prompt_text.clone(),
            state: "claimed".to_string(),
            is_replayed: false,
        })
    }

    pub fn mark_launch_attempt(
        &self,
        execution_id: &str,
        pairing_id: &str,
    ) -> Result<bool, PairingError> {
        let now = now_epoch_secs();
        let mut conn = self.conn.lock();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        // Check if an attempt already exists
        let existing: Option<String> = tx
            .query_row(
                "SELECT state FROM launch_attempts WHERE execution_id = ?1",
                params![execution_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        if existing.is_some() {
            // Already attempted: exactly one attempt permitted!
            tx.commit()
                .map_err(|e| PairingError::StorageError(e.to_string()))?;
            return Ok(false);
        }

        tx.execute(
            r#"
            INSERT INTO launch_attempts (
                execution_id, pairing_id, attempt_marked_at, invoked_at,
                orca_terminal_handle, orca_tab_id, orca_pane_key, orca_pty_id,
                state, failure_reason
            ) VALUES (?1, ?2, ?3, NULL, NULL, NULL, NULL, NULL, 'unknown', NULL)
            "#,
            params![execution_id, pairing_id, now],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        tx.execute(
            "UPDATE launch_requests SET state = 'unknown', updated_at = ?1 WHERE execution_id = ?2",
            params![now, execution_id],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;
        tx.commit()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(true)
    }

    pub fn record_launch_start_evidence(
        &self,
        execution_id: &str,
        evidence: &AttemptEvidence,
    ) -> Result<(), PairingError> {
        let now = now_epoch_secs();
        let conn = self.conn.lock();

        conn.execute(
            r#"
            UPDATE launch_attempts
            SET invoked_at = ?1,
                orca_terminal_handle = ?2,
                orca_tab_id = ?3,
                orca_pane_key = ?4,
                orca_pty_id = ?5
            WHERE execution_id = ?6
            "#,
            params![
                now,
                evidence.orca_terminal_handle.as_deref(),
                evidence.orca_tab_id.as_deref(),
                evidence.orca_pane_key.as_deref(),
                evidence.orca_pty_id.as_deref(),
                execution_id,
            ],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn record_launch_started(
        &self,
        execution_id: &str,
        evidence: &AttemptEvidence,
    ) -> Result<(), PairingError> {
        let now = now_epoch_secs();
        let conn = self.conn.lock();

        conn.execute(
            r#"
            UPDATE launch_attempts
            SET invoked_at = ?1,
                orca_terminal_handle = ?2,
                orca_tab_id = ?3,
                orca_pane_key = ?4,
                orca_pty_id = ?5,
                state = 'started'
            WHERE execution_id = ?6
            "#,
            params![
                now,
                evidence.orca_terminal_handle.as_deref(),
                evidence.orca_tab_id.as_deref(),
                evidence.orca_pane_key.as_deref(),
                evidence.orca_pty_id.as_deref(),
                execution_id,
            ],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        conn.execute(
            "UPDATE launch_requests SET state = 'started', updated_at = ?1 WHERE execution_id = ?2",
            params![now, execution_id],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn record_launch_uncertain(
        &self,
        execution_id: &str,
        evidence: Option<&AttemptEvidence>,
        reason: &str,
    ) -> Result<(), PairingError> {
        let now = now_epoch_secs();
        let conn = self.conn.lock();

        let handle = evidence.and_then(|e| e.orca_terminal_handle.as_deref());
        let tab_id = evidence.and_then(|e| e.orca_tab_id.as_deref());
        let pane_key = evidence.and_then(|e| e.orca_pane_key.as_deref());
        let pty_id = evidence.and_then(|e| e.orca_pty_id.as_deref());

        conn.execute(
            r#"
            UPDATE launch_attempts
            SET invoked_at = ?1,
                orca_terminal_handle = COALESCE(?2, orca_terminal_handle),
                orca_tab_id = COALESCE(?3, orca_tab_id),
                orca_pane_key = COALESCE(?4, orca_pane_key),
                orca_pty_id = COALESCE(?5, orca_pty_id),
                state = 'unknown',
                failure_reason = ?6
            WHERE execution_id = ?7
            "#,
            params![now, handle, tab_id, pane_key, pty_id, reason, execution_id],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        conn.execute(
            "UPDATE launch_requests SET state = 'unknown', updated_at = ?1 WHERE execution_id = ?2",
            params![now, execution_id],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn record_launch_pre_spawn_failure(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<(), PairingError> {
        let now = now_epoch_secs();
        let conn = self.conn.lock();

        conn.execute(
            "UPDATE launch_attempts SET state = 'failed', failure_reason = ?1 WHERE execution_id = ?2",
            params![reason, execution_id],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        conn.execute(
            "UPDATE launch_requests SET state = 'failed', updated_at = ?1 WHERE execution_id = ?2",
            params![now, execution_id],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn record_launch_failure(
        &self,
        execution_id: &str,
        reason: &str,
    ) -> Result<(), PairingError> {
        self.record_launch_pre_spawn_failure(execution_id, reason)
    }

    pub fn record_replay_tombstone(
        &self,
        pairing_id: &str,
        launch_request_id: &str,
        execution_id: &str,
        payload_digest: &str,
    ) -> Result<(), PairingError> {
        let now = now_epoch_secs();
        let conn = self.conn.lock();

        conn.execute(
            r#"
            INSERT OR REPLACE INTO replay_tombstones (
                pairing_id, launch_request_id, execution_id, payload_digest, tombstoned_at
            ) VALUES (?1, ?2, ?3, ?4, ?5)
            "#,
            params![pairing_id, launch_request_id, execution_id, payload_digest, now],
        )
        .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn get_launch_summaries(
        &self,
        pairing_id: &str,
    ) -> Result<Vec<LaunchSummary>, PairingError> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare(
                r#"
                SELECT lr.launch_request_id, lr.execution_id, lr.origin_conversation_id,
                       lr.origin_conversation_url, lr.target_id, lr.policy_revision,
                       lr.state, lr.created_at, la.attempt_marked_at, la.orca_terminal_handle
                FROM launch_requests lr
                LEFT JOIN launch_attempts la ON lr.execution_id = la.execution_id
                WHERE lr.pairing_id = ?1
                ORDER BY lr.created_at DESC
                "#,
            )
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let rows = stmt
            .query_map(params![pairing_id], |r| {
                Ok(LaunchSummary {
                    launch_request_id: r.get(0)?,
                    execution_id: r.get(1)?,
                    origin_conversation_id: r.get(2)?,
                    origin_conversation_url: r.get(3)?,
                    target_id: r.get(4)?,
                    policy_revision: r.get(5)?,
                    state: r.get(6)?,
                    created_at: r.get(7)?,
                    attempt_marked_at: r.get(8)?,
                    orca_terminal_handle: r.get(9)?,
                })
            })
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let mut list = Vec::new();
        for row in rows {
            list.push(row.map_err(|e| PairingError::StorageError(e.to_string()))?);
        }
        Ok(list)
    }

    pub fn get_launch_request_by_id(
        &self,
        pairing_id: &str,
        launch_request_id: &str,
    ) -> Result<Option<LaunchSummary>, PairingError> {
        let conn = self.conn.lock();
        let summary: Option<LaunchSummary> = conn
            .query_row(
                r#"
                SELECT lr.launch_request_id, lr.execution_id, lr.origin_conversation_id,
                       lr.origin_conversation_url, lr.target_id, lr.policy_revision,
                       lr.state, lr.created_at, la.attempt_marked_at, la.orca_terminal_handle
                FROM launch_requests lr
                LEFT JOIN launch_attempts la ON lr.execution_id = la.execution_id
                WHERE lr.pairing_id = ?1 AND lr.launch_request_id = ?2
                "#,
                params![pairing_id, launch_request_id],
                |r| {
                    Ok(LaunchSummary {
                        launch_request_id: r.get(0)?,
                        execution_id: r.get(1)?,
                        origin_conversation_id: r.get(2)?,
                        origin_conversation_url: r.get(3)?,
                        target_id: r.get(4)?,
                        policy_revision: r.get(5)?,
                        state: r.get(6)?,
                        created_at: r.get(7)?,
                        attempt_marked_at: r.get(8)?,
                        orca_terminal_handle: r.get(9)?,
                    })
                },
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(summary)
    }

}
