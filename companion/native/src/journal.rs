use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};
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
    StorageError(String),
}

impl std::fmt::Display for PairingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PairingError::NotFound => write!(f, "pairing_not_found"),
            PairingError::InvalidSecret => write!(f, "invalid_pairing_secret"),
            PairingError::ProfileMismatch => write!(f, "profile_mismatch"),
            PairingError::Retired => write!(f, "pairing_retired"),
            PairingError::StorageError(e) => write!(f, "storage_error: {}", e),
        }
    }
}

impl std::error::Error for PairingError {}

pub struct Journal {
    conn: Connection,
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

impl Journal {
    pub fn open(path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS pairings (
                pairing_id TEXT PRIMARY KEY,
                bootstrap_token TEXT UNIQUE,
                pairing_secret TEXT,
                pairing_secret_hash TEXT NOT NULL,
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
            "#,
        )?;
        Ok(Self { conn })
    }

    pub fn create_bootstrap(
        &self,
        pairing_id: &str,
        bootstrap_token: &str,
        pairing_secret: &str,
        browser: &str,
        profile_id: &str,
        targets: &[TargetRecord],
        policy: &PolicyRecord,
    ) -> Result<(), rusqlite::Error> {
        let now = now_epoch_secs();
        let secret_hash = hash_secret(pairing_secret);

        self.conn.execute(
            r#"
            INSERT INTO pairings (
                pairing_id, bootstrap_token, pairing_secret, pairing_secret_hash,
                browser, profile_id, status, policy_revision,
                created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?9)
            "#,
            params![
                pairing_id,
                bootstrap_token,
                pairing_secret,
                secret_hash,
                browser,
                profile_id,
                PairingStatus::Pending.as_str(),
                policy.policy_revision,
                now
            ],
        )?;

        for target in targets {
            self.conn.execute(
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

        self.conn.execute(
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

        Ok(())
    }

    pub fn get_pairing_status(&self, pairing_id: &str) -> Result<PairingStatus, PairingError> {
        let status_str: Option<String> = self
            .conn
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

        // Find pairing with this bootstrap token
        let row: Option<(String, String, String, String)> = self
            .conn
            .query_row(
                r#"
                SELECT pairing_id, pairing_secret, status, policy_revision
                FROM pairings
                WHERE bootstrap_token = ?1
                "#,
                params![bootstrap_token],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        let (pairing_id, pairing_secret, status_str, policy_rev) = match row {
            Some(r) => r,
            None => return Err(PairingError::NotFound),
        };

        let status = PairingStatus::parse(&status_str)
            .ok_or_else(|| PairingError::StorageError("unknown status".into()))?;

        if status == PairingStatus::Revoked {
            return Err(PairingError::Retired);
        }

        // Consume bootstrap token atomically and update profile_id if provided
        let affected = self
            .conn
            .execute(
                r#"
                UPDATE pairings
                SET bootstrap_token = NULL,
                    pairing_secret = NULL,
                    profile_id = ?1,
                    status = ?2,
                    updated_at = ?3
                WHERE pairing_id = ?4
                "#,
                params![
                    profile_id,
                    PairingStatus::Active.as_str(),
                    now,
                    pairing_id
                ],
            )
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        if affected == 0 {
            return Err(PairingError::NotFound);
        }

        let targets = self.get_targets(&pairing_id)?;
        let policy = self.get_policy(&pairing_id, &policy_rev)?;

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
        let row: Option<(String, String, String, String, String)> = self
            .conn
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

        let (stored_hash, browser, stored_profile_id, status_str, policy_rev) = match row {
            Some(r) => r,
            None => return Err(PairingError::NotFound),
        };

        let status = PairingStatus::parse(&status_str)
            .ok_or_else(|| PairingError::StorageError("unknown status".into()))?;

        if status == PairingStatus::Revoked {
            return Err(PairingError::Retired);
        }

        // Verify secret hash
        let given_hash = hash_secret(pairing_secret);
        if !constant_time_eq(&stored_hash, &given_hash) {
            return Err(PairingError::InvalidSecret);
        }

        // Verify profile isolation (N4)
        if !stored_profile_id.is_empty() && stored_profile_id != profile_id {
            return Err(PairingError::ProfileMismatch);
        }

        let targets = self.get_targets(pairing_id)?;
        let policy = self.get_policy(pairing_id, &policy_rev)?;

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

        self.conn
            .execute(
                r#"
                UPDATE pairings
                SET status = ?1, updated_at = ?2
                WHERE pairing_id = ?3
                "#,
                params![PairingStatus::Revoked.as_str(), now, pairing_id],
            )
            .map_err(|e| PairingError::StorageError(e.to_string()))?;

        Ok(())
    }

    pub fn revoke_pairing_admin(&self, pairing_id: &str) -> Result<(), PairingError> {
        let now = now_epoch_secs();
        let affected = self
            .conn
            .execute(
                r#"
                UPDATE pairings
                SET status = ?1, updated_at = ?2
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
        let mut stmt = self
            .conn
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

    pub fn get_policy(
        &self,
        pairing_id: &str,
        policy_revision: &str,
    ) -> Result<PolicyRecord, PairingError> {
        self.conn
            .query_row(
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
}
