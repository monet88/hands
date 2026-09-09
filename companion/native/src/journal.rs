use std::path::Path;
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
}
