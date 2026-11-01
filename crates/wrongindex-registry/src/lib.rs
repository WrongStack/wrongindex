//! Service registry (`<data-root>/registry.db`, ADR WI-ADR-006).
//!
//! Holds registrations, durable job admission, and idempotency receipts. It is
//! deliberately separate from every project's `index.db`: no service tables are
//! ever added to the shared index. Transactions here never span an index
//! database; committed index metadata stays authoritative for publication.

pub mod identity;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde_json::Value;
use wrongindex_contracts::{
    BASELINE_PROFILE_ID, Backend, ErrorBody, ErrorCode, JobKind, JobRecord, JobState,
    ProjectRecord, RegisterProjectParams, WatchPolicy,
};

pub use identity::{PathIdentity, resolve_identity};

pub const REGISTRY_SCHEMA_VERSION: i64 = 1;

/// Idempotency receipts expire after this long.
pub const RECEIPT_TTL: Duration = Duration::from_secs(24 * 60 * 60);

const MIGRATIONS: &[(i64, &str, &str)] = &[(
    1,
    "initial registry schema",
    r#"
    CREATE TABLE registry_metadata (
      key TEXT PRIMARY KEY,
      value TEXT NOT NULL
    );
    CREATE TABLE projects (
      project_id TEXT PRIMARY KEY,
      display_name TEXT NOT NULL,
      checkout_root TEXT NOT NULL,
      canonical_checkout_key TEXT NOT NULL,
      index_dir TEXT NOT NULL,
      canonical_index_key TEXT NOT NULL UNIQUE,
      repository_identity TEXT,
      compatibility_profile TEXT NOT NULL,
      watch_policy TEXT NOT NULL CHECK (watch_policy IN ('session','pinned','manual','disabled')),
      backend TEXT NOT NULL CHECK (backend IN ('legacy','native','unsupported')),
      configuration_revision INTEGER NOT NULL DEFAULT 1,
      registered_at_ms INTEGER NOT NULL
    );
    CREATE INDEX idx_projects_checkout ON projects(canonical_checkout_key);
    CREATE TABLE project_runtime (
      project_id TEXT PRIMARY KEY REFERENCES projects(project_id) ON DELETE CASCADE,
      owner_epoch INTEGER NOT NULL DEFAULT 0,
      dirty_revision INTEGER NOT NULL DEFAULT 0,
      last_published_generation INTEGER,
      reconciliation_reason TEXT,
      updated_at_ms INTEGER NOT NULL
    );
    CREATE TABLE jobs (
      job_id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL REFERENCES projects(project_id) ON DELETE CASCADE,
      kind TEXT NOT NULL,
      params_json TEXT NOT NULL DEFAULT '{}',
      state TEXT NOT NULL CHECK (state IN ('queued','running','succeeded','failed','canceled')),
      attempt INTEGER NOT NULL DEFAULT 0,
      requested_revision INTEGER,
      committed_generation INTEGER,
      admitted_at_ms INTEGER NOT NULL,
      started_at_ms INTEGER,
      completed_at_ms INTEGER,
      result_json TEXT,
      error_json TEXT
    );
    CREATE INDEX idx_jobs_project ON jobs(project_id, admitted_at_ms);
    CREATE INDEX idx_jobs_state ON jobs(state);
    CREATE TABLE job_receipts (
      client_name TEXT NOT NULL,
      idempotency_key TEXT NOT NULL,
      job_id TEXT NOT NULL REFERENCES jobs(job_id) ON DELETE CASCADE,
      expires_at_ms INTEGER NOT NULL,
      PRIMARY KEY (client_name, idempotency_key)
    );
    CREATE TABLE project_errors (
      project_id TEXT NOT NULL REFERENCES projects(project_id) ON DELETE CASCADE,
      code TEXT NOT NULL,
      detail TEXT NOT NULL,
      first_seen_ms INTEGER NOT NULL,
      last_seen_ms INTEGER NOT NULL,
      occurrences INTEGER NOT NULL DEFAULT 1,
      recovered INTEGER NOT NULL DEFAULT 0,
      PRIMARY KEY (project_id, code, detail)
    );
    CREATE TABLE ownership_transitions (
      id INTEGER PRIMARY KEY,
      project_id TEXT NOT NULL REFERENCES projects(project_id) ON DELETE CASCADE,
      old_owner TEXT,
      new_owner TEXT,
      epoch INTEGER NOT NULL,
      phase TEXT NOT NULL,
      compatibility_profile TEXT NOT NULL,
      evidence_json TEXT,
      at_ms INTEGER NOT NULL
    );
    "#,
)];

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn db_err(e: rusqlite::Error) -> ErrorBody {
    use rusqlite::ffi::ErrorCode as Sqlite;
    let code = match &e {
        rusqlite::Error::SqliteFailure(f, _) => match f.code {
            Sqlite::DiskFull => ErrorCode::DiskFull,
            Sqlite::DatabaseBusy | Sqlite::DatabaseLocked => ErrorCode::DatabaseBusy,
            Sqlite::DatabaseCorrupt | Sqlite::NotADatabase => ErrorCode::DatabaseCorrupt,
            Sqlite::ReadOnly => ErrorCode::ReadOnly,
            _ => ErrorCode::Internal,
        },
        _ => ErrorCode::Internal,
    };
    ErrorBody::new(code, format!("registry: {e}"))
}

pub struct Registry {
    conn: Connection,
}

/// Outcome of startup recovery for jobs that were running when the previous
/// daemon instance stopped.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    pub requeued: Vec<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Admission {
    pub job: JobRecord,
    pub deduplicated: bool,
}

impl Registry {
    pub fn open(path: &Path) -> Result<Self, ErrorBody> {
        let conn = Connection::open(path).map_err(db_err)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, ErrorBody> {
        Self::init(Connection::open_in_memory().map_err(db_err)?)
    }

    fn init(mut conn: Connection) -> Result<Self, ErrorBody> {
        conn.busy_timeout(Duration::from_secs(5)).map_err(db_err)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        )
        .map_err(db_err)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS registry_migrations (
               version INTEGER PRIMARY KEY,
               description TEXT NOT NULL,
               applied_at_ms INTEGER NOT NULL
             );",
        )
        .map_err(db_err)?;
        let current: i64 = conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM registry_migrations",
                [],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if current > REGISTRY_SCHEMA_VERSION {
            return Err(ErrorBody::new(
                ErrorCode::ProfileUnsupported,
                format!(
                    "registry schema {current} is newer than this build ({REGISTRY_SCHEMA_VERSION}); refusing to open"
                ),
            ));
        }
        for (version, description, sql) in MIGRATIONS {
            if *version <= current {
                continue;
            }
            // Each migration and its history row commit together; an
            // interrupted migration leaves the previous version intact.
            let tx = conn
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_err)?;
            tx.execute_batch(sql).map_err(db_err)?;
            tx.execute(
                "INSERT INTO registry_migrations(version, description, applied_at_ms) VALUES (?1, ?2, ?3)",
                params![version, description, now_ms()],
            )
            .map_err(db_err)?;
            tx.commit().map_err(db_err)?;
        }
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<i64, ErrorBody> {
        self.conn
            .query_row("SELECT MAX(version) FROM registry_migrations", [], |r| {
                r.get(0)
            })
            .map_err(db_err)
    }

    // ─── Projects ───────────────────────────────────────────────────────────

    /// Register a project. Same index + same checkout returns the existing
    /// registration; same index + different checkout is
    /// `INDEX_IDENTITY_CONFLICT`. The source tree and index are never touched.
    pub fn register_project(
        &mut self,
        p: &RegisterProjectParams,
    ) -> Result<(ProjectRecord, bool), ErrorBody> {
        let checkout = resolve_identity(&p.checkout_root)?;
        if !checkout.exists || !Path::new(&checkout.display).is_dir() {
            return Err(ErrorBody::new(
                ErrorCode::ProjectUnavailable,
                format!(
                    "checkout root '{}' is not an existing directory",
                    checkout.display
                ),
            ));
        }
        let index = resolve_identity(&p.index_dir)?;
        if identity::is_unc(&index.display) {
            return Err(ErrorBody::new(
                ErrorCode::InvalidRequest,
                format!(
                    "index directory '{}' is a network location; SQLite WAL indexes must live on local storage",
                    index.display
                ),
            ));
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let existing = tx
            .query_row(
                &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE canonical_index_key = ?1"),
                params![index.key],
                project_from_row,
            )
            .optional()
            .map_err(db_err)?;
        if let Some(existing) = existing {
            if existing.canonical_checkout_key == checkout.key {
                return Ok((existing, false));
            }
            return Err(ErrorBody::new(
                ErrorCode::IndexIdentityConflict,
                format!(
                    "index '{}' is already registered for checkout '{}' (project {})",
                    index.display, existing.checkout_root, existing.project_id
                ),
            ));
        }

        let record = ProjectRecord {
            project_id: format!("p_{}", uuid::Uuid::new_v4().simple()),
            display_name: p.display_name.clone().unwrap_or_else(|| {
                Path::new(&checkout.display)
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| checkout.display.clone())
            }),
            checkout_root: checkout.display.clone(),
            canonical_checkout_key: checkout.key.clone(),
            index_dir: index.display.clone(),
            canonical_index_key: index.key.clone(),
            repository_identity: p.repository_identity.clone(),
            compatibility_profile: BASELINE_PROFILE_ID.to_string(),
            watch_policy: p.watch_policy,
            // M1 has no indexing backend attached yet.
            backend: Backend::Unsupported,
            configuration_revision: 1,
            registered_at_ms: now_ms(),
        };
        tx.execute(
            "INSERT INTO projects (project_id, display_name, checkout_root, canonical_checkout_key,
               index_dir, canonical_index_key, repository_identity, compatibility_profile,
               watch_policy, backend, configuration_revision, registered_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                record.project_id,
                record.display_name,
                record.checkout_root,
                record.canonical_checkout_key,
                record.index_dir,
                record.canonical_index_key,
                record.repository_identity,
                record.compatibility_profile,
                record.watch_policy.as_str(),
                record.backend.as_str(),
                record.configuration_revision,
                record.registered_at_ms,
            ],
        )
        .map_err(db_err)?;
        tx.execute(
            "INSERT INTO project_runtime(project_id, updated_at_ms) VALUES (?1, ?2)",
            params![record.project_id, record.registered_at_ms],
        )
        .map_err(db_err)?;
        tx.commit().map_err(db_err)?;
        Ok((record, true))
    }

    pub fn list_projects(&self) -> Result<Vec<ProjectRecord>, ErrorBody> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {PROJECT_COLUMNS} FROM projects ORDER BY registered_at_ms, project_id"
            ))
            .map_err(db_err)?;
        let rows = stmt.query_map([], project_from_row).map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn get_project(&self, project_id: &str) -> Result<ProjectRecord, ErrorBody> {
        self.conn
            .query_row(
                &format!("SELECT {PROJECT_COLUMNS} FROM projects WHERE project_id = ?1"),
                params![project_id],
                project_from_row,
            )
            .optional()
            .map_err(db_err)?
            .ok_or_else(|| {
                ErrorBody::new(
                    ErrorCode::ProjectNotFound,
                    format!("no project '{project_id}'"),
                )
            })
    }

    /// Remove a registration. Source and index files are never deleted
    /// (INV-10); job history for the project is dropped with it.
    pub fn detach_project(&mut self, project_id: &str) -> Result<ProjectRecord, ErrorBody> {
        let project = self.get_project(project_id)?;
        let active: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM jobs WHERE project_id = ?1 AND state IN ('queued','running')",
                params![project_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if active > 0 {
            return Err(ErrorBody::new(
                ErrorCode::OwnerConflict,
                format!(
                    "project '{project_id}' has {active} active job(s); wait or cancel before detaching"
                ),
            ));
        }
        self.conn
            .execute(
                "DELETE FROM projects WHERE project_id = ?1",
                params![project_id],
            )
            .map_err(db_err)?;
        Ok(project)
    }

    // ─── Jobs ───────────────────────────────────────────────────────────────

    /// Durably admit a job. The returned record is committed before the caller
    /// acknowledges the request. A matching unexpired idempotency receipt
    /// returns the earlier job instead of admitting a new one.
    pub fn admit_job(
        &mut self,
        project_id: &str,
        kind: JobKind,
        params_json: &Value,
        client_name: &str,
        idempotency_key: Option<&str>,
    ) -> Result<Admission, ErrorBody> {
        let now = now_ms();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;
        tx.execute(
            "DELETE FROM job_receipts WHERE expires_at_ms <= ?1",
            params![now],
        )
        .map_err(db_err)?;
        let exists: bool = tx
            .query_row(
                "SELECT 1 FROM projects WHERE project_id = ?1",
                params![project_id],
                |_| Ok(true),
            )
            .optional()
            .map_err(db_err)?
            .unwrap_or(false);
        if !exists {
            return Err(ErrorBody::new(
                ErrorCode::ProjectNotFound,
                format!("no project '{project_id}'"),
            ));
        }
        if let Some(key) = idempotency_key {
            let prior: Option<String> = tx
                .query_row(
                    "SELECT job_id FROM job_receipts WHERE client_name = ?1 AND idempotency_key = ?2",
                    params![client_name, key],
                    |r| r.get(0),
                )
                .optional()
                .map_err(db_err)?;
            if let Some(job_id) = prior {
                let job = get_job_tx(&tx, &job_id)?;
                if job.project_id != project_id || job.kind != kind {
                    return Err(ErrorBody::new(
                        ErrorCode::InvalidRequest,
                        "idempotency key was already used for a different project or job kind",
                    ));
                }
                return Ok(Admission {
                    job,
                    deduplicated: true,
                });
            }
        }
        let job_id = format!("j_{}", uuid::Uuid::new_v4().simple());
        tx.execute(
            "INSERT INTO jobs(job_id, project_id, kind, params_json, state, admitted_at_ms)
             VALUES (?1, ?2, ?3, ?4, 'queued', ?5)",
            params![
                job_id,
                project_id,
                kind.as_str(),
                params_json.to_string(),
                now
            ],
        )
        .map_err(db_err)?;
        if let Some(key) = idempotency_key {
            tx.execute(
                "INSERT INTO job_receipts(client_name, idempotency_key, job_id, expires_at_ms)
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    client_name,
                    key,
                    job_id,
                    now + RECEIPT_TTL.as_millis() as i64
                ],
            )
            .map_err(db_err)?;
        }
        let job = get_job_tx(&tx, &job_id)?;
        tx.commit().map_err(db_err)?;
        Ok(Admission {
            job,
            deduplicated: false,
        })
    }

    pub fn get_job(&self, job_id: &str) -> Result<JobRecord, ErrorBody> {
        get_job_tx(&self.conn, job_id)
    }

    pub fn list_jobs(
        &self,
        project_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<JobRecord>, ErrorBody> {
        let limit = limit.clamp(1, 500) as i64;
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE (?1 IS NULL OR project_id = ?1)
                 ORDER BY admitted_at_ms DESC, job_id LIMIT ?2"
            ))
            .map_err(db_err)?;
        let rows = stmt
            .query_map(params![project_id, limit], job_from_row)
            .map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn latest_job(&self, project_id: &str) -> Result<Option<JobRecord>, ErrorBody> {
        Ok(self.list_jobs(Some(project_id), 1)?.into_iter().next())
    }

    /// Queued jobs in admission order (FIFO).
    pub fn queued_jobs(&self) -> Result<Vec<JobRecord>, ErrorBody> {
        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {JOB_COLUMNS} FROM jobs WHERE state = 'queued' ORDER BY admitted_at_ms, job_id"
            ))
            .map_err(db_err)?;
        let rows = stmt.query_map([], job_from_row).map_err(db_err)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(db_err)
    }

    pub fn count_jobs(&self, state: JobState) -> Result<usize, ErrorBody> {
        self.conn
            .query_row(
                "SELECT count(*) FROM jobs WHERE state = ?1",
                params![state.as_str()],
                |r| r.get::<_, i64>(0),
            )
            .map(|n| n as usize)
            .map_err(db_err)
    }

    /// Transition queued -> running. Returns `false` if the job was no longer
    /// queued (canceled or already claimed).
    pub fn mark_running(&mut self, job_id: &str) -> Result<bool, ErrorBody> {
        let n = self
            .conn
            .execute(
                "UPDATE jobs SET state = 'running', attempt = attempt + 1, started_at_ms = ?2
                 WHERE job_id = ?1 AND state = 'queued'",
                params![job_id, now_ms()],
            )
            .map_err(db_err)?;
        Ok(n == 1)
    }

    pub fn complete_job(
        &mut self,
        job_id: &str,
        outcome: Result<Value, ErrorBody>,
    ) -> Result<JobRecord, ErrorBody> {
        let (state, result, error) = match outcome {
            Ok(v) => (JobState::Succeeded, Some(v.to_string()), None),
            Err(e) => (
                JobState::Failed,
                None,
                Some(serde_json::to_string(&e).unwrap_or_default()),
            ),
        };
        self.conn
            .execute(
                "UPDATE jobs SET state = ?2, completed_at_ms = ?3, result_json = ?4, error_json = ?5
                 WHERE job_id = ?1 AND state = 'running'",
                params![job_id, state.as_str(), now_ms(), result, error],
            )
            .map_err(db_err)?;
        self.get_job(job_id)
    }

    /// Startup recovery: a job left `running` by a previous instance is
    /// requeued when its kind is idempotent and side-effect free (`check`),
    /// otherwise failed with an explicit interruption error. Never assumes the
    /// interrupted run completed.
    pub fn recover_interrupted(&mut self) -> Result<Recovery, ErrorBody> {
        let mut recovery = Recovery::default();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_err)?;
        let running: Vec<(String, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT job_id, kind FROM jobs WHERE state = 'running' ORDER BY admitted_at_ms",
                )
                .map_err(db_err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db_err)?;
            rows.collect::<Result<_, _>>().map_err(db_err)?
        };
        for (job_id, kind) in running {
            if kind == JobKind::Check.as_str() {
                tx.execute(
                    "UPDATE jobs SET state = 'queued', started_at_ms = NULL WHERE job_id = ?1",
                    params![job_id],
                )
                .map_err(db_err)?;
                recovery.requeued.push(job_id);
            } else {
                let err = ErrorBody::new(
                    ErrorCode::Internal,
                    "interrupted by daemon restart before completion was recorded",
                );
                tx.execute(
                    "UPDATE jobs SET state = 'failed', completed_at_ms = ?2, error_json = ?3 WHERE job_id = ?1",
                    params![job_id, now_ms(), serde_json::to_string(&err).unwrap_or_default()],
                )
                .map_err(db_err)?;
                recovery.failed.push(job_id);
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(recovery)
    }
}

const PROJECT_COLUMNS: &str = "project_id, display_name, checkout_root, canonical_checkout_key, \
    index_dir, canonical_index_key, repository_identity, compatibility_profile, watch_policy, \
    backend, configuration_revision, registered_at_ms";

fn project_from_row(r: &Row<'_>) -> rusqlite::Result<ProjectRecord> {
    let policy: String = r.get(8)?;
    let backend: String = r.get(9)?;
    Ok(ProjectRecord {
        project_id: r.get(0)?,
        display_name: r.get(1)?,
        checkout_root: r.get(2)?,
        canonical_checkout_key: r.get(3)?,
        index_dir: r.get(4)?,
        canonical_index_key: r.get(5)?,
        repository_identity: r.get(6)?,
        compatibility_profile: r.get(7)?,
        watch_policy: WatchPolicy::parse(&policy).unwrap_or(WatchPolicy::Disabled),
        backend: Backend::parse(&backend).unwrap_or(Backend::Unsupported),
        configuration_revision: r.get(10)?,
        registered_at_ms: r.get(11)?,
    })
}

const JOB_COLUMNS: &str = "job_id, project_id, kind, state, attempt, admitted_at_ms, \
    started_at_ms, completed_at_ms, result_json, error_json";

fn job_from_row(r: &Row<'_>) -> rusqlite::Result<JobRecord> {
    let kind: String = r.get(2)?;
    let state: String = r.get(3)?;
    let result: Option<String> = r.get(8)?;
    let error: Option<String> = r.get(9)?;
    Ok(JobRecord {
        job_id: r.get(0)?,
        project_id: r.get(1)?,
        kind: JobKind::parse(&kind).unwrap_or(JobKind::Check),
        state: JobState::parse(&state).unwrap_or(JobState::Failed),
        attempt: r.get(4)?,
        admitted_at_ms: r.get(5)?,
        started_at_ms: r.get(6)?,
        completed_at_ms: r.get(7)?,
        result: result.and_then(|s| serde_json::from_str(&s).ok()),
        error: error.and_then(|s| serde_json::from_str(&s).ok()),
    })
}

fn get_job_tx(conn: &Connection, job_id: &str) -> Result<JobRecord, ErrorBody> {
    conn.query_row(
        &format!("SELECT {JOB_COLUMNS} FROM jobs WHERE job_id = ?1"),
        params![job_id],
        job_from_row,
    )
    .optional()
    .map_err(db_err)?
    .ok_or_else(|| ErrorBody::new(ErrorCode::JobNotFound, format!("no job '{job_id}'")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(root: &Path, index: &Path) -> RegisterProjectParams {
        RegisterProjectParams {
            checkout_root: root.display().to_string(),
            index_dir: index.display().to_string(),
            display_name: None,
            watch_policy: WatchPolicy::Manual,
            repository_identity: None,
        }
    }

    #[test]
    fn register_is_idempotent_and_detects_index_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let index = tmp.path().join("idx");
        let mut reg = Registry::open_in_memory().unwrap();

        let (first, created) = reg.register_project(&params(&a, &index)).unwrap();
        assert!(created);
        assert_eq!(first.display_name, "a");
        let (again, created) = reg.register_project(&params(&a, &index)).unwrap();
        assert!(!created);
        assert_eq!(first.project_id, again.project_id);

        let err = reg.register_project(&params(&b, &index)).unwrap_err();
        assert_eq!(err.code, ErrorCode::IndexIdentityConflict);

        // Same checkout, explicit different index: an intentional alternate registration.
        let (alt, created) = reg
            .register_project(&params(&a, &tmp.path().join("idx2")))
            .unwrap();
        assert!(created);
        assert_ne!(alt.project_id, first.project_id);
        assert_eq!(reg.list_projects().unwrap().len(), 2);
    }

    #[test]
    fn missing_checkout_is_unavailable() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = Registry::open_in_memory().unwrap();
        let err = reg
            .register_project(&params(&tmp.path().join("nope"), &tmp.path().join("i")))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::ProjectUnavailable);
    }

    #[test]
    fn detach_keeps_files_on_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("idx");
        std::fs::create_dir_all(&index).unwrap();
        std::fs::write(index.join("index.db"), b"x").unwrap();
        let mut reg = Registry::open_in_memory().unwrap();
        let (p, _) = reg.register_project(&params(tmp.path(), &index)).unwrap();
        reg.detach_project(&p.project_id).unwrap();
        assert!(index.join("index.db").exists());
        assert_eq!(
            reg.get_project(&p.project_id).unwrap_err().code,
            ErrorCode::ProjectNotFound
        );
    }

    #[test]
    fn admission_is_durable_and_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("registry.db");
        let project_id;
        let job_id;
        {
            let mut reg = Registry::open(&db).unwrap();
            let (p, _) = reg
                .register_project(&params(tmp.path(), &tmp.path().join("idx")))
                .unwrap();
            project_id = p.project_id;
            let a = reg
                .admit_job(&project_id, JobKind::Check, &json!({}), "cli", Some("k1"))
                .unwrap();
            assert!(!a.deduplicated);
            assert_eq!(a.job.state, JobState::Queued);
            let b = reg
                .admit_job(&project_id, JobKind::Check, &json!({}), "cli", Some("k1"))
                .unwrap();
            assert!(b.deduplicated);
            assert_eq!(a.job.job_id, b.job.job_id);
            // Same key from another client is independent.
            let c = reg
                .admit_job(&project_id, JobKind::Check, &json!({}), "gui", Some("k1"))
                .unwrap();
            assert_ne!(c.job.job_id, a.job.job_id);
            // Reusing a key for a different kind is rejected.
            let err = reg
                .admit_job(&project_id, JobKind::Full, &json!({}), "cli", Some("k1"))
                .unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidRequest);
            job_id = a.job.job_id;
        }
        let reg = Registry::open(&db).unwrap();
        assert_eq!(reg.get_job(&job_id).unwrap().state, JobState::Queued);
        assert_eq!(reg.queued_jobs().unwrap().len(), 2);
    }

    #[test]
    fn recovery_requeues_checks_and_fails_mutating_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = Registry::open_in_memory().unwrap();
        let (p, _) = reg
            .register_project(&params(tmp.path(), &tmp.path().join("idx")))
            .unwrap();
        let check = reg
            .admit_job(&p.project_id, JobKind::Check, &json!({}), "t", None)
            .unwrap()
            .job;
        let full = reg
            .admit_job(&p.project_id, JobKind::Full, &json!({}), "t", None)
            .unwrap()
            .job;
        assert!(reg.mark_running(&check.job_id).unwrap());
        assert!(reg.mark_running(&full.job_id).unwrap());
        assert!(!reg.mark_running(&full.job_id).unwrap(), "double claim");

        let rec = reg.recover_interrupted().unwrap();
        assert_eq!(rec.requeued, vec![check.job_id.clone()]);
        assert_eq!(rec.failed, vec![full.job_id.clone()]);
        assert_eq!(reg.get_job(&check.job_id).unwrap().state, JobState::Queued);
        let failed = reg.get_job(&full.job_id).unwrap();
        assert_eq!(failed.state, JobState::Failed);
        assert!(failed.error.is_some());
    }

    #[test]
    fn complete_records_result() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = Registry::open_in_memory().unwrap();
        let (p, _) = reg
            .register_project(&params(tmp.path(), &tmp.path().join("idx")))
            .unwrap();
        let job = reg
            .admit_job(&p.project_id, JobKind::Check, &json!({}), "t", None)
            .unwrap()
            .job;
        reg.mark_running(&job.job_id).unwrap();
        let done = reg
            .complete_job(&job.job_id, Ok(json!({"verdict": "compatible"})))
            .unwrap();
        assert_eq!(done.state, JobState::Succeeded);
        assert_eq!(done.attempt, 1);
        assert_eq!(done.result.unwrap()["verdict"], "compatible");
    }

    #[test]
    fn newer_registry_schema_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("registry.db");
        drop(Registry::open(&db).unwrap());
        Connection::open(&db)
            .unwrap()
            .execute(
                "INSERT INTO registry_migrations VALUES (99, 'future', 0)",
                [],
            )
            .unwrap();
        let err = Registry::open(&db).err().expect("must refuse");
        assert_eq!(err.code, ErrorCode::ProfileUnsupported);
    }
}
