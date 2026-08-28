//! Rebuildable SQLite metadata catalog for daemon-owned sessions (task 1336).
//!
//! Conversation JSON remains authoritative. This catalog exists only to make
//! discovery bounded; every attach still validates and loads the JSON payload.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::session_store::PersistedWrite;

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub enum CatalogOpen {
    Ready(SessionCatalog),
    NewerSchema { found: i64 },
}

#[derive(Debug, Clone)]
pub struct SessionCatalog {
    path: PathBuf,
    busy_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogMutation {
    pub session_id: String,
    pub workspace_id: String,
    pub model: Option<String>,
    pub updated_at_unix_ns: u64,
    pub preview: Option<String>,
    pub message_count: Option<usize>,
    pub writer_instance_id: String,
    pub generation: u64,
    pub deleted: bool,
    pub observed_at_unix_ms: u64,
    /// Reconciliation verified the current authoritative payload state.
    pub authoritative_observation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    pub session_id: String,
    pub workspace_id: String,
    pub model: Option<String>,
    pub updated_at_unix_ns: u64,
    pub preview: Option<String>,
    pub message_count: Option<usize>,
    pub deleted: bool,
}

#[derive(Default)]
struct PendingState {
    mutations: std::collections::BTreeMap<String, CatalogMutation>,
    mark_incomplete: bool,
}

pub struct SessionCatalogWriter {
    catalog: SessionCatalog,
    workspace_id: String,
    writer_instance_id: String,
    max_pending: usize,
    batch_size: usize,
    max_preview_bytes: usize,
    generation: AtomicU64,
    pending: Mutex<PendingState>,
    wake: tokio::sync::mpsc::Sender<()>,
    in_flight: AtomicUsize,
    healthy: AtomicBool,
    quiet: tokio::sync::Notify,
}

impl SessionCatalogWriter {
    pub fn start(
        catalog: SessionCatalog,
        workspace_id: String,
        max_pending: usize,
        batch_size: usize,
        max_preview_bytes: usize,
    ) -> Arc<Self> {
        let (wake, receiver) = tokio::sync::mpsc::channel(1);
        let writer = Arc::new(Self {
            catalog,
            workspace_id,
            writer_instance_id: uuid::Uuid::new_v4().to_string(),
            max_pending: max_pending.max(1),
            batch_size: batch_size.max(1),
            max_preview_bytes: max_preview_bytes.max(1),
            generation: AtomicU64::new(0),
            pending: Mutex::new(PendingState::default()),
            wake,
            in_flight: AtomicUsize::new(0),
            healthy: AtomicBool::new(true),
            quiet: tokio::sync::Notify::new(),
        });
        tokio::spawn(run_writer(Arc::downgrade(&writer), receiver));
        writer
    }

    pub fn enqueue_saved(&self, write: PersistedWrite) {
        let preview =
            write.summary.first_user_line.as_deref().and_then(|line| {
                crate::session_store::normalize_preview(line, self.max_preview_bytes)
            });
        self.enqueue(CatalogMutation {
            session_id: write.summary.id,
            workspace_id: self.workspace_id.clone(),
            model: Some(write.summary.model),
            updated_at_unix_ns: write.updated_at_unix_ns,
            preview,
            message_count: Some(write.summary.message_count),
            writer_instance_id: self.writer_instance_id.clone(),
            generation: self.generation.fetch_add(1, Ordering::AcqRel) + 1,
            deleted: false,
            observed_at_unix_ms: now_unix_ms(),
            authoritative_observation: false,
        });
    }

    pub fn enqueue_deleted(&self, session_id: &str) {
        let now_ms = now_unix_ms();
        self.enqueue(CatalogMutation {
            session_id: session_id.to_string(),
            workspace_id: self.workspace_id.clone(),
            model: None,
            updated_at_unix_ns: now_ms.saturating_mul(1_000_000),
            preview: None,
            message_count: None,
            writer_instance_id: self.writer_instance_id.clone(),
            generation: self.generation.fetch_add(1, Ordering::AcqRel) + 1,
            deleted: true,
            observed_at_unix_ms: now_ms,
            authoritative_observation: false,
        });
    }

    fn enqueue(&self, mutation: CatalogMutation) {
        if !self.is_healthy() {
            return;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !pending.mutations.contains_key(&mutation.session_id)
            && pending.mutations.len() >= self.max_pending
        {
            if !pending.mark_incomplete {
                tracing::warn!(
                    target: "daimonos::session_catalog",
                    event = "catalog_pending_saturated",
                    pending = pending.mutations.len(),
                    max_pending = self.max_pending,
                    "catalog mutation dropped; reconciliation required"
                );
            }
            pending.mark_incomplete = true;
        } else {
            pending
                .mutations
                .insert(mutation.session_id.clone(), mutation);
        }
        drop(pending);
        let _ = self.wake.try_send(());
    }

    pub fn pending_count(&self) -> usize {
        let pending = self
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        pending
            .mutations
            .len()
            .saturating_add(usize::from(pending.mark_incomplete))
            .saturating_add(self.in_flight.load(Ordering::Acquire))
    }

    pub async fn wait_until_quiet(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let notified = self.quiet.notified();
            if self.pending_count() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.pending_count() == 0;
            }
        }
    }

    #[cfg(test)]
    pub fn catalog(&self) -> &SessionCatalog {
        &self.catalog
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    pub fn max_preview_bytes(&self) -> usize {
        self.max_preview_bytes
    }
}

async fn run_writer(
    writer: Weak<SessionCatalogWriter>,
    mut receiver: tokio::sync::mpsc::Receiver<()>,
) {
    while receiver.recv().await.is_some() {
        let Some(writer) = writer.upgrade() else {
            break;
        };
        loop {
            let (batch, mark_incomplete) = {
                let mut pending = writer
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let keys = pending
                    .mutations
                    .keys()
                    .take(writer.batch_size)
                    .cloned()
                    .collect::<Vec<_>>();
                let batch = keys
                    .into_iter()
                    .filter_map(|key| pending.mutations.remove(&key))
                    .collect::<Vec<_>>();
                let mark_incomplete = std::mem::take(&mut pending.mark_incomplete);
                if !batch.is_empty() || mark_incomplete {
                    writer
                        .in_flight
                        .fetch_add(batch.len().max(1), Ordering::AcqRel);
                }
                (batch, mark_incomplete)
            };
            if batch.is_empty() && !mark_incomplete {
                writer.quiet.notify_waiters();
                break;
            }
            let catalog = writer.catalog.clone();
            let workspace_id = writer.workspace_id.clone();
            let weight = batch.len().max(1);
            let failed = tokio::task::spawn_blocking(move || {
                let mut failed = false;
                let mut needs_reconciliation = mark_incomplete;
                for mutation in batch {
                    match catalog.apply(&mutation) {
                        Ok(true) => {}
                        Ok(false) => needs_reconciliation = true,
                        Err(_) => failed = true,
                    }
                }
                if needs_reconciliation && catalog.mark_incomplete(&workspace_id).is_err() {
                    failed = true;
                }
                failed
            })
            .await
            .unwrap_or(true);
            writer.in_flight.fetch_sub(weight, Ordering::AcqRel);
            if failed {
                writer.healthy.store(false, Ordering::Release);
                tracing::warn!(
                    target: "daimonos::session_catalog",
                    event = "catalog_write_failed",
                    "session catalog disabled until restart/reconciliation"
                );
            }
            writer.quiet.notify_waiters();
        }
        drop(writer);
    }
}

impl SessionCatalog {
    pub fn open(path: PathBuf, busy_timeout: Duration) -> anyhow::Result<CatalogOpen> {
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                std::fs::create_dir_all(parent)?;
            }
            let metadata = std::fs::metadata(parent)?;
            let effective_uid = unsafe { libc::geteuid() };
            anyhow::ensure!(
                metadata.uid() == effective_uid,
                "session catalog parent is not owned by the current user"
            );
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(busy_timeout)?;
        let has_schema_meta = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = 'schema_meta'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if has_schema_meta {
            let version = connection
                .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?;
            if version.is_some_and(|version| version > SCHEMA_VERSION) {
                return Ok(CatalogOpen::NewerSchema {
                    found: version.unwrap_or_default(),
                });
            }
        }
        configure_connection(&connection, &path)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_meta (
                 id      INTEGER PRIMARY KEY CHECK (id = 1),
                 version INTEGER NOT NULL
             );",
        )?;
        let version = transaction
            .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?;
        if version.is_some_and(|version| version < SCHEMA_VERSION) {
            transaction.execute_batch(
                "DROP TABLE IF EXISTS session_catalog;
                 DROP TABLE IF EXISTS workspace_state;
                 DROP TABLE IF EXISTS reconcile_lease;",
            )?;
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_catalog (
                 session_id          TEXT PRIMARY KEY,
                 workspace_id        TEXT NOT NULL,
                 model               TEXT,
                 updated_at_unix_ns  INTEGER NOT NULL,
                 preview             TEXT,
                 message_count       INTEGER,
                 writer_instance_id  TEXT NOT NULL,
                 generation          INTEGER NOT NULL,
                 deleted             INTEGER NOT NULL DEFAULT 0,
                 observed_at_unix_ms INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS session_catalog_workspace_order
                 ON session_catalog(
                     workspace_id,
                     deleted,
                     updated_at_unix_ns DESC,
                     session_id ASC
                 );
             CREATE TABLE IF NOT EXISTS workspace_state (
                 workspace_id TEXT PRIMARY KEY,
                 complete     INTEGER NOT NULL DEFAULT 0,
                 cursor       TEXT
             );
             CREATE TABLE IF NOT EXISTS reconcile_lease (
                 id              INTEGER PRIMARY KEY CHECK (id = 1),
                 owner           TEXT NOT NULL,
                 expires_unix_ms INTEGER NOT NULL
             );
             INSERT INTO schema_meta(id, version) VALUES(1, 1)
                 ON CONFLICT(id) DO UPDATE SET version = excluded.version;",
        )?;
        transaction.commit()?;
        Ok(CatalogOpen::Ready(Self { path, busy_timeout }))
    }

    fn connection(&self) -> anyhow::Result<Connection> {
        let connection = open_connection(&self.path, self.busy_timeout)?;
        let version = connection
            .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                row.get::<_, i64>(0)
            })
            .optional()?;
        anyhow::ensure!(
            version == Some(SCHEMA_VERSION),
            "session catalog schema changed while in use"
        );
        Ok(connection)
    }

    /// Apply one latest-wins mutation. Tombstones remain materialized so an
    /// older in-flight upsert cannot resurrect a deleted payload.
    pub fn apply(&self, mutation: &CatalogMutation) -> anyhow::Result<bool> {
        // Cross-writer mtime ordering is valid only because mutations are
        // minted after the atomic rename's real metadata is observed. Equal
        // time favors deletion; clock rollback falls back to reconciliation.
        let connection = self.connection()?;
        let changed = connection.execute(
            "INSERT INTO session_catalog(
                 session_id, workspace_id, model, updated_at_unix_ns, preview,
                 message_count, writer_instance_id, generation, deleted,
                 observed_at_unix_ms
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(session_id) DO UPDATE SET
                 workspace_id = excluded.workspace_id,
                 model = excluded.model,
                 updated_at_unix_ns = excluded.updated_at_unix_ns,
                 preview = excluded.preview,
                 message_count = excluded.message_count,
                 writer_instance_id = excluded.writer_instance_id,
                 generation = excluded.generation,
                 deleted = excluded.deleted,
                 observed_at_unix_ms = excluded.observed_at_unix_ms
             WHERE
                 (
                     excluded.writer_instance_id = session_catalog.writer_instance_id
                     AND excluded.generation >= session_catalog.generation
                 )
                 OR (
                     excluded.writer_instance_id != session_catalog.writer_instance_id
                     AND (
                         excluded.updated_at_unix_ns > session_catalog.updated_at_unix_ns
                         OR (
                             excluded.updated_at_unix_ns = session_catalog.updated_at_unix_ns
                             AND excluded.deleted > session_catalog.deleted
                         )
                     )
                 )
                 OR ?11 = 1",
            params![
                mutation.session_id,
                mutation.workspace_id,
                mutation.model,
                to_sql_u64(mutation.updated_at_unix_ns),
                mutation.preview,
                mutation.message_count.map(to_sql_usize),
                mutation.writer_instance_id,
                to_sql_u64(mutation.generation),
                i64::from(mutation.deleted),
                to_sql_u64(mutation.observed_at_unix_ms),
                i64::from(mutation.authoritative_observation),
            ],
        )?;
        Ok(changed > 0)
    }

    pub fn rows(&self, workspace_id: &str, limit: usize) -> anyhow::Result<Vec<CatalogRow>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id, workspace_id, model, updated_at_unix_ns,
                    preview, message_count, deleted
             FROM session_catalog
             WHERE workspace_id = ?1 AND deleted = 0
             ORDER BY updated_at_unix_ns DESC, session_id ASC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![workspace_id, to_sql_usize(limit)], |row| {
            Ok(CatalogRow {
                session_id: row.get(0)?,
                workspace_id: row.get(1)?,
                model: row.get(2)?,
                updated_at_unix_ns: from_sql_u64(row.get(3)?),
                preview: row.get(4)?,
                message_count: row
                    .get::<_, Option<i64>>(5)?
                    .and_then(|value| usize::try_from(value).ok()),
                deleted: row.get::<_, i64>(6)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    #[cfg(test)]
    pub fn row(&self, session_id: &str) -> anyhow::Result<Option<CatalogRow>> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT session_id, workspace_id, model, updated_at_unix_ns,
                        preview, message_count, deleted
                 FROM session_catalog WHERE session_id = ?1",
                [session_id],
                |row| {
                    Ok(CatalogRow {
                        session_id: row.get(0)?,
                        workspace_id: row.get(1)?,
                        model: row.get(2)?,
                        updated_at_unix_ns: from_sql_u64(row.get(3)?),
                        preview: row.get(4)?,
                        message_count: row
                            .get::<_, Option<i64>>(5)?
                            .and_then(|value| usize::try_from(value).ok()),
                        deleted: row.get::<_, i64>(6)? != 0,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn ids_after(
        &self,
        workspace_id: &str,
        after_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<String>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            "SELECT session_id FROM session_catalog
             WHERE workspace_id = ?1 AND deleted = 0
                 AND session_id > COALESCE(?2, '')
             ORDER BY session_id ASC LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![workspace_id, after_id, to_sql_usize(limit)],
            |row| row.get::<_, String>(0),
        )?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn mark_incomplete(&self, workspace_id: &str) -> anyhow::Result<()> {
        self.set_workspace_state(workspace_id, false, None)
    }

    pub fn set_workspace_state(
        &self,
        workspace_id: &str,
        complete: bool,
        cursor: Option<&str>,
    ) -> anyhow::Result<()> {
        let connection = self.connection()?;
        connection.execute(
            "INSERT INTO workspace_state(workspace_id, complete, cursor)
             VALUES(?1, ?2, ?3)
             ON CONFLICT(workspace_id) DO UPDATE SET
                 complete = excluded.complete,
                 cursor = excluded.cursor",
            params![workspace_id, i64::from(complete), cursor],
        )?;
        Ok(())
    }

    pub fn workspace_state(&self, workspace_id: &str) -> anyhow::Result<(bool, Option<String>)> {
        let connection = self.connection()?;
        Ok(connection
            .query_row(
                "SELECT complete, cursor FROM workspace_state WHERE workspace_id = ?1",
                [workspace_id],
                |row| Ok((row.get::<_, i64>(0)? != 0, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((false, None)))
    }

    pub fn try_acquire_reconcile_lease(
        &self,
        owner: &str,
        now_unix_ms: u64,
        lease_duration: Duration,
    ) -> anyhow::Result<bool> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_expiry = transaction
            .query_row(
                "SELECT expires_unix_ms FROM reconcile_lease WHERE id = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(from_sql_u64);
        if current_expiry.is_some_and(|expiry| expiry > now_unix_ms) {
            transaction.rollback()?;
            return Ok(false);
        }
        let lease_ms = lease_duration.as_millis().min(u128::from(u64::MAX)) as u64;
        let expires = now_unix_ms.saturating_add(lease_ms);
        transaction.execute(
            "INSERT INTO reconcile_lease(id, owner, expires_unix_ms)
             VALUES(1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET
                 owner = excluded.owner,
                 expires_unix_ms = excluded.expires_unix_ms",
            params![owner, to_sql_u64(expires)],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    pub fn purge_tombstones(
        &self,
        observed_before_unix_ms: u64,
        limit: usize,
    ) -> anyhow::Result<usize> {
        let connection = self.connection()?;
        Ok(connection.execute(
            "DELETE FROM session_catalog
             WHERE session_id IN (
                 SELECT session_id FROM session_catalog
                 WHERE deleted = 1 AND observed_at_unix_ms <= ?1
                 ORDER BY session_id ASC LIMIT ?2
             )",
            params![to_sql_u64(observed_before_unix_ms), to_sql_usize(limit)],
        )?)
    }
}

fn open_connection(path: &Path, busy_timeout: Duration) -> anyhow::Result<Connection> {
    let connection = Connection::open(path)?;
    configure_connection(&connection, path)?;
    connection.busy_timeout(busy_timeout)?;
    Ok(connection)
}

fn configure_connection(connection: &Connection, path: &Path) -> anyhow::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let _ = connection.pragma_update(None, "journal_mode", "WAL");
    let _ = connection.pragma_update(None, "synchronous", "NORMAL");
    Ok(())
}

fn to_sql_u64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn to_sql_usize(value: usize) -> i64 {
    value.min(i64::MAX as usize) as i64
}

fn from_sql_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionSummary;
    use std::os::unix::fs::PermissionsExt;

    fn mutation(
        id: &str,
        writer: &str,
        generation: u64,
        updated: u64,
        deleted: bool,
    ) -> CatalogMutation {
        CatalogMutation {
            session_id: id.to_string(),
            workspace_id: "workspace".to_string(),
            model: (!deleted).then(|| "model".to_string()),
            updated_at_unix_ns: updated,
            preview: (!deleted).then(|| "preview".to_string()),
            message_count: (!deleted).then_some(2),
            writer_instance_id: writer.to_string(),
            generation,
            deleted,
            observed_at_unix_ms: updated / 1_000_000,
            authoritative_observation: false,
        }
    }

    fn catalog() -> (tempfile::TempDir, SessionCatalog) {
        let directory = tempfile::tempdir().unwrap();
        let catalog = match SessionCatalog::open(
            directory.path().join("catalog.sqlite"),
            Duration::from_secs(1),
        )
        .unwrap()
        {
            CatalogOpen::Ready(catalog) => catalog,
            CatalogOpen::NewerSchema { .. } => panic!("fresh catalog"),
        };
        (directory, catalog)
    }

    fn persisted(id: &str, model: &str, updated: u64) -> PersistedWrite {
        PersistedWrite {
            summary: SessionSummary {
                id: id.to_string(),
                model: model.to_string(),
                message_count: 2,
                cwd: None,
                updated_at: None,
                first_user_line: Some("preview".to_string()),
            },
            updated_at_unix_ns: updated,
        }
    }

    #[test]
    fn same_writer_generation_is_latest_wins() {
        let (_directory, catalog) = catalog();
        assert!(catalog
            .apply(&mutation("s", "writer", 2, 20, false))
            .unwrap());
        assert!(!catalog
            .apply(&mutation("s", "writer", 1, 10, false))
            .unwrap());
        assert_eq!(catalog.row("s").unwrap().unwrap().updated_at_unix_ns, 20);
    }

    #[test]
    fn catalog_file_and_parent_are_owner_only() {
        let (directory, catalog) = catalog();
        assert_eq!(
            std::fs::metadata(directory.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&catalog.path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn materialized_tombstone_blocks_stale_resurrection() {
        let (_directory, catalog) = catalog();
        catalog
            .apply(&mutation("s", "writer", 1, 10, false))
            .unwrap();
        catalog
            .apply(&mutation("s", "writer", 2, 10, true))
            .unwrap();
        assert!(!catalog
            .apply(&mutation("s", "writer", 1, 10, false))
            .unwrap());
        assert!(catalog.row("s").unwrap().unwrap().deleted);
        assert!(catalog.rows("workspace", 10).unwrap().is_empty());
    }

    #[test]
    fn newer_cross_instance_observation_supersedes_old_writer() {
        let (_directory, catalog) = catalog();
        catalog.apply(&mutation("s", "old", 50, 10, false)).unwrap();
        assert!(catalog.apply(&mutation("s", "new", 1, 20, false)).unwrap());
        assert_eq!(catalog.row("s").unwrap().unwrap().updated_at_unix_ns, 20);
    }

    #[test]
    fn equal_time_cross_instance_tombstone_wins() {
        let (_directory, catalog) = catalog();
        catalog.apply(&mutation("s", "old", 1, 10, false)).unwrap();
        assert!(catalog.apply(&mutation("s", "new", 1, 10, true)).unwrap());
        assert!(!catalog
            .apply(&mutation("s", "third", 1, 10, false))
            .unwrap());
        assert!(catalog.row("s").unwrap().unwrap().deleted);
    }

    #[test]
    fn authoritative_reconciliation_can_restore_equal_time_recreate() {
        let (_directory, catalog) = catalog();
        catalog.apply(&mutation("s", "old", 1, 10, true)).unwrap();
        let mut recreated = mutation("s", "reconciler", 1, 10, false);
        recreated.authoritative_observation = true;
        assert!(catalog.apply(&recreated).unwrap());
        assert!(!catalog.row("s").unwrap().unwrap().deleted);
    }

    #[test]
    fn restarted_writer_cannot_resurrect_newer_tombstone() {
        let (_directory, catalog) = catalog();
        catalog
            .apply(&mutation("s", "deleter", 1, 20, true))
            .unwrap();
        assert!(!catalog
            .apply(&mutation("s", "restarted-stale-writer", 1, 10, false))
            .unwrap());
        assert!(catalog.row("s").unwrap().unwrap().deleted);
    }

    #[test]
    fn newer_schema_is_never_modified() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("catalog.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_meta(id INTEGER PRIMARY KEY, version INTEGER NOT NULL);
                 INSERT INTO schema_meta(id, version) VALUES(1, 999);
                 CREATE TABLE future_data(value TEXT);
                 INSERT INTO future_data(value) VALUES('keep');",
            )
            .unwrap();
        let mode_before = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        drop(connection);

        assert!(matches!(
            SessionCatalog::open(path.clone(), Duration::from_secs(1)).unwrap(),
            CatalogOpen::NewerSchema { found: 999 }
        ));
        let connection = Connection::open(path).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT value FROM future_data", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "keep"
        );
        assert_eq!(
            connection
                .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "delete"
        );
        assert_eq!(
            std::fs::metadata(connection.path().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode_before
        );
    }

    #[test]
    fn older_schema_is_rebuilt_without_touching_payload_files() {
        let directory = tempfile::tempdir().unwrap();
        let payload = directory.path().join("session.json");
        std::fs::write(&payload, b"authoritative").unwrap();
        let path = directory.path().join("catalog.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE schema_meta(id INTEGER PRIMARY KEY, version INTEGER NOT NULL);
                 INSERT INTO schema_meta(id, version) VALUES(1, 0);
                 CREATE TABLE session_catalog(old_value TEXT);",
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            SessionCatalog::open(path, Duration::from_secs(1)).unwrap(),
            CatalogOpen::Ready(_)
        ));
        assert_eq!(std::fs::read(payload).unwrap(), b"authoritative");
    }

    #[test]
    fn separate_connections_share_wal_catalog() {
        let (directory, first) = catalog();
        let second = match SessionCatalog::open(
            directory.path().join("catalog.sqlite"),
            Duration::from_secs(1),
        )
        .unwrap()
        {
            CatalogOpen::Ready(catalog) => catalog,
            CatalogOpen::NewerSchema { .. } => panic!("same schema"),
        };
        first.apply(&mutation("s", "one", 1, 10, false)).unwrap();
        assert_eq!(second.rows("workspace", 10).unwrap().len(), 1);
    }

    #[test]
    fn workspace_completeness_and_tombstone_retention_are_bounded() {
        let (_directory, catalog) = catalog();
        assert_eq!(catalog.workspace_state("workspace").unwrap(), (false, None));
        catalog
            .set_workspace_state("workspace", true, Some("cursor"))
            .unwrap();
        assert_eq!(
            catalog.workspace_state("workspace").unwrap(),
            (true, Some("cursor".to_string()))
        );
        catalog
            .apply(&mutation("a", "writer", 1, 10, true))
            .unwrap();
        catalog
            .apply(&mutation("b", "writer", 2, 20, true))
            .unwrap();
        assert_eq!(catalog.purge_tombstones(100, 1).unwrap(), 1);
        assert_eq!(catalog.purge_tombstones(100, 1).unwrap(), 1);
    }

    #[test]
    fn reconciliation_lease_bounds_all_processes_to_one_pass_per_interval() {
        let (_directory, catalog) = catalog();
        assert!(catalog
            .try_acquire_reconcile_lease("one", 100, Duration::from_millis(50))
            .unwrap());
        assert!(!catalog
            .try_acquire_reconcile_lease("two", 149, Duration::from_millis(50))
            .unwrap());
        assert!(catalog
            .try_acquire_reconcile_lease("two", 150, Duration::from_millis(50))
            .unwrap());
    }

    #[tokio::test]
    async fn writer_coalesces_same_session_and_marks_distinct_overflow_incomplete() {
        let (_directory, catalog) = catalog();
        catalog
            .set_workspace_state("workspace", true, None)
            .unwrap();
        let writer = SessionCatalogWriter::start(catalog, "workspace".to_string(), 1, 1, 64);
        writer.enqueue_saved(persisted("same", "old", 10));
        writer.enqueue_saved(persisted("same", "new", 20));
        writer.enqueue_saved(persisted("overflow", "model", 30));
        assert!(writer.wait_until_quiet(Duration::from_secs(1)).await);
        let row = writer.catalog().row("same").unwrap().unwrap();
        assert_eq!(row.model.as_deref(), Some("new"));
        assert_eq!(
            writer.catalog().workspace_state("workspace").unwrap(),
            (false, None)
        );
    }

    #[tokio::test]
    async fn writer_delete_materializes_tombstone_after_pending_save() {
        let (_directory, catalog) = catalog();
        let writer = SessionCatalogWriter::start(catalog, "workspace".to_string(), 4, 4, 64);
        writer.enqueue_saved(persisted("same", "model", 10));
        writer.enqueue_deleted("same");
        assert!(writer.wait_until_quiet(Duration::from_secs(1)).await);
        assert!(writer.catalog().row("same").unwrap().unwrap().deleted);
    }

    #[tokio::test]
    async fn rejected_cross_instance_write_marks_workspace_incomplete() {
        let (_directory, catalog) = catalog();
        catalog
            .set_workspace_state("workspace", true, None)
            .unwrap();
        catalog
            .apply(&mutation("same", "old", 1, 10, true))
            .unwrap();
        let writer = SessionCatalogWriter::start(catalog, "workspace".to_string(), 4, 4, 64);
        writer.enqueue_saved(persisted("same", "model", 10));
        assert!(writer.wait_until_quiet(Duration::from_secs(1)).await);
        assert_eq!(
            writer.catalog().workspace_state("workspace").unwrap(),
            (false, None)
        );
    }

    #[tokio::test]
    async fn rejected_cross_instance_write_repairs_before_completeness_returns() {
        let (_directory, catalog) = catalog();
        catalog
            .apply(&mutation("same", "old", 1, 10, false))
            .unwrap();
        catalog
            .set_workspace_state("workspace", true, None)
            .unwrap();
        let writer =
            SessionCatalogWriter::start(catalog.clone(), "workspace".to_string(), 4, 4, 64);
        writer.enqueue_saved(persisted("same", "new", 10));
        assert!(writer.wait_until_quiet(Duration::from_secs(1)).await);
        assert!(!catalog.workspace_state("workspace").unwrap().0);

        let mut repaired = mutation("same", "reconciler", 1, 10, false);
        repaired.model = Some("new".to_string());
        repaired.authoritative_observation = true;
        catalog.apply(&repaired).unwrap();
        catalog
            .set_workspace_state("workspace", true, None)
            .unwrap();
        assert_eq!(
            catalog.rows("workspace", 1).unwrap()[0].model.as_deref(),
            Some("new")
        );
        assert!(catalog.workspace_state("workspace").unwrap().0);
    }

    #[tokio::test]
    async fn writer_task_does_not_retain_writer_after_owner_drop() {
        let (_directory, catalog) = catalog();
        let writer = SessionCatalogWriter::start(catalog, "workspace".to_string(), 4, 4, 64);
        let weak = Arc::downgrade(&writer);
        drop(writer);
        tokio::task::yield_now().await;
        assert!(weak.upgrade().is_none());
    }
}
