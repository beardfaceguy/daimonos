//! The coordination SQLite store (ADR-009 D1, D6). Modeled on
//! `src/kgl/store.rs`: `Connection::open` + WAL + busy-timeout, `Result`-typed
//! throughout, with a `#[cfg(test)] open_in_memory` constructor. One store per
//! workspace; every daimonos process opens the same file directly.
//!
//! Slice 1 (#1058) implements the `agent` identity table and its methods. The
//! full schema (message/recipient/reservation) is created up-front so later
//! slices add methods, not migrations.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::time::Duration;

/// Bump when the on-disk schema changes in an incompatible way. Recorded in a
/// one-row `schema_meta` table so a future format change is detected, not
/// mis-parsed (mirrors `session_store::SESSION_PERSIST_VERSION`).
pub const SCHEMA_VERSION: u32 = 1;

/// One registered agent identity (ADR-009 D2/D6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRecord {
    pub name: String,
    pub session_id: Option<String>,
    pub program: Option<String>,
    pub model: Option<String>,
    pub task: Option<String>,
    pub inception_ts: String,
    pub last_seen_ts: String,
}

pub struct CoordinationStore {
    conn: Connection,
}

impl CoordinationStore {
    /// Open (creating parent dirs if needed) with an explicit SQLite
    /// busy-timeout. Enables WAL so concurrent daimonos processes read/write
    /// without blocking; the busy timeout makes a contended writer wait briefly
    /// instead of erroring `SQLITE_BUSY` (ADR-009 D1/D7).
    pub fn open_with(path: &Path, busy_timeout_ms: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).context("open coordination sqlite store")?;
        // WAL + NORMAL synchronous: matches analytics.rs. Best-effort — a VFS
        // that rejects WAL must not be fatal.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        let _ = conn.pragma_update(None, "synchronous", "NORMAL");
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// In-memory store for tests. No WAL (`:memory:` doesn't support it); the
    /// busy timeout still applies.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(Duration::from_millis(5_000))?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    /// Create tables if absent and record the schema version. Idempotent:
    /// safe to call on every open. All statements are `IF NOT EXISTS`, so two
    /// processes racing to initialize the same file both succeed.
    fn init(&self) -> Result<()> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_meta (
                    id            INTEGER PRIMARY KEY CHECK (id = 1),
                    version       INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS agent (
                    id            INTEGER PRIMARY KEY,
                    name          TEXT NOT NULL UNIQUE,
                    session_id    TEXT,
                    program       TEXT,
                    model         TEXT,
                    task          TEXT,
                    inception_ts  TEXT NOT NULL,
                    last_seen_ts  TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS message (
                    id           INTEGER PRIMARY KEY,
                    thread_id    INTEGER,
                    reply_to     INTEGER,
                    sender       TEXT NOT NULL,
                    subject      TEXT NOT NULL,
                    body         TEXT NOT NULL,
                    importance   TEXT NOT NULL DEFAULT 'normal',
                    ack_required INTEGER NOT NULL DEFAULT 0,
                    created_ts   TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS message_thread ON message(thread_id, created_ts);
                 CREATE TABLE IF NOT EXISTS recipient (
                    message_id  INTEGER NOT NULL,
                    agent_name  TEXT NOT NULL,
                    kind        TEXT NOT NULL,
                    read_ts     TEXT,
                    ack_ts      TEXT,
                    PRIMARY KEY (message_id, agent_name)
                 );
                 CREATE INDEX IF NOT EXISTS recipient_inbox ON recipient(agent_name, read_ts);
                 CREATE TABLE IF NOT EXISTS reservation (
                    id           INTEGER PRIMARY KEY,
                    agent_name   TEXT NOT NULL,
                    pattern      TEXT NOT NULL,
                    exclusive    INTEGER NOT NULL DEFAULT 1,
                    reason       TEXT,
                    created_ts   TEXT NOT NULL,
                    expires_ts   TEXT NOT NULL,
                    released_ts  TEXT
                 );
                 CREATE INDEX IF NOT EXISTS reservation_active
                    ON reservation(agent_name, released_ts, expires_ts);",
            )
            .context("init coordination schema")?;
        // Record the version once (id is pinned to 1 by the CHECK).
        self.conn
            .execute(
                "INSERT OR IGNORE INTO schema_meta (id, version) VALUES (1, ?1)",
                params![SCHEMA_VERSION],
            )
            .context("record schema version")?;
        Ok(())
    }

    /// The recorded schema version, if any. `Ok(None)` on a store that predates
    /// the meta row (treated by callers as "not this version"). Public API for
    /// future migration/compat checks; exercised by the store's own tests.
    #[allow(dead_code)]
    pub fn schema_version(&self) -> Result<Option<u32>> {
        let v = self
            .conn
            .query_row("SELECT version FROM schema_meta WHERE id = 1", [], |row| {
                row.get::<_, u32>(0)
            })
            .ok();
        Ok(v)
    }

    // ---- identity (ADR-009 D2) ----

    /// Register (or refresh) an agent. Idempotent by `name`: re-registering an
    /// existing name updates its profile + `last_seen_ts` and keeps the
    /// original `inception_ts`; a new name inserts a row. Returns the resulting
    /// record. `now` is an RFC-3339 timestamp supplied by the host (the store
    /// never invents time — mirrors KGL).
    pub fn register_agent(
        &self,
        name: &str,
        session_id: Option<&str>,
        program: Option<&str>,
        model: Option<&str>,
        task: Option<&str>,
        now: &str,
    ) -> Result<AgentRecord> {
        // Upsert: on conflict keep inception_ts, refresh the rest. COALESCE lets
        // a re-register with omitted fields preserve the prior value rather than
        // nulling it.
        self.conn
            .execute(
                "INSERT INTO agent
                    (name, session_id, program, model, task, inception_ts, last_seen_ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(name) DO UPDATE SET
                    session_id  = COALESCE(excluded.session_id, agent.session_id),
                    program     = COALESCE(excluded.program, agent.program),
                    model       = COALESCE(excluded.model, agent.model),
                    task        = COALESCE(excluded.task, agent.task),
                    last_seen_ts = excluded.last_seen_ts",
                params![name, session_id, program, model, task, now],
            )
            .context("register agent")?;
        self.agent(name)?
            .context("agent row missing immediately after register")
    }

    /// Fetch one agent by name, or `None` if absent. Panic-free.
    pub fn agent(&self, name: &str) -> Result<Option<AgentRecord>> {
        let rec = self
            .conn
            .query_row(
                "SELECT name, session_id, program, model, task, inception_ts, last_seen_ts
                 FROM agent WHERE name = ?1",
                params![name],
                Self::row_to_agent,
            )
            .ok();
        Ok(rec)
    }

    /// All agents, most-recently-seen first, bounded by `limit` (ADR-009: reads
    /// are always bounded). A non-positive limit is clamped to 1.
    pub fn list_agents(&self, limit: i64) -> Result<Vec<AgentRecord>> {
        let limit = limit.max(1);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT name, session_id, program, model, task, inception_ts, last_seen_ts
                 FROM agent ORDER BY last_seen_ts DESC, name ASC LIMIT ?1",
            )
            .context("prepare list_agents")?;
        let rows = stmt
            .query_map(params![limit], Self::row_to_agent)
            .context("query list_agents")?;
        // Collect defensively: skip any row that fails to decode rather than
        // propagating a panic (constraint 2). In practice our own schema never
        // produces a decode error, but a corrupt/hand-edited DB must not abort.
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(rec) => out.push(rec),
                Err(_) => continue,
            }
        }
        Ok(out)
    }

    fn row_to_agent(row: &rusqlite::Row<'_>) -> rusqlite::Result<AgentRecord> {
        Ok(AgentRecord {
            name: row.get(0)?,
            session_id: row.get(1)?,
            program: row.get(2)?,
            model: row.get(3)?,
            task: row.get(4)?,
            inception_ts: row.get(5)?,
            last_seen_ts: row.get(6)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> CoordinationStore {
        CoordinationStore::open_in_memory().unwrap()
    }

    #[test]
    fn schema_version_recorded_on_open() {
        let s = store();
        assert_eq!(s.schema_version().unwrap(), Some(SCHEMA_VERSION));
    }

    #[test]
    fn register_then_fetch_round_trips() {
        let s = store();
        let rec = s
            .register_agent(
                "BlueLake",
                Some("sess-1"),
                Some("codex-cli"),
                Some("gpt5"),
                Some("auth refactor"),
                "2026-07-24T00:00:00Z",
            )
            .unwrap();
        assert_eq!(rec.name, "BlueLake");
        assert_eq!(rec.session_id.as_deref(), Some("sess-1"));
        assert_eq!(rec.inception_ts, "2026-07-24T00:00:00Z");
        assert_eq!(rec.last_seen_ts, "2026-07-24T00:00:00Z");

        let fetched = s.agent("BlueLake").unwrap().unwrap();
        assert_eq!(fetched, rec);
    }

    #[test]
    fn reregister_updates_last_seen_and_keeps_inception_and_name() {
        let s = store();
        s.register_agent(
            "GreenCastle",
            None,
            None,
            None,
            None,
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        let again = s
            .register_agent(
                "GreenCastle",
                Some("sess-2"),
                None,
                None,
                Some("new task"),
                "2026-07-24T01:00:00Z",
            )
            .unwrap();
        // Same identity (name unchanged), inception preserved, last_seen bumped.
        assert_eq!(again.name, "GreenCastle");
        assert_eq!(again.inception_ts, "2026-07-24T00:00:00Z");
        assert_eq!(again.last_seen_ts, "2026-07-24T01:00:00Z");
        assert_eq!(again.session_id.as_deref(), Some("sess-2"));
        assert_eq!(again.task.as_deref(), Some("new task"));
        // Exactly one row — re-register updated, not inserted.
        assert_eq!(s.list_agents(100).unwrap().len(), 1);
    }

    #[test]
    fn reregister_with_omitted_fields_preserves_prior_values() {
        let s = store();
        s.register_agent(
            "Ridge",
            Some("sess-x"),
            Some("claude-code"),
            Some("opus"),
            Some("orig"),
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        // Re-register with all-None profile fields must not null them out.
        let after = s
            .register_agent("Ridge", None, None, None, None, "2026-07-24T02:00:00Z")
            .unwrap();
        assert_eq!(after.program.as_deref(), Some("claude-code"));
        assert_eq!(after.model.as_deref(), Some("opus"));
        assert_eq!(after.session_id.as_deref(), Some("sess-x"));
        assert_eq!(after.last_seen_ts, "2026-07-24T02:00:00Z");
    }

    #[test]
    fn list_agents_orders_by_last_seen_desc() {
        let s = store();
        s.register_agent("First", None, None, None, None, "2026-07-24T00:00:00Z")
            .unwrap();
        s.register_agent("Second", None, None, None, None, "2026-07-24T05:00:00Z")
            .unwrap();
        s.register_agent("Third", None, None, None, None, "2026-07-24T03:00:00Z")
            .unwrap();
        let names: Vec<String> = s
            .list_agents(100)
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, vec!["Second", "Third", "First"]);
    }

    #[test]
    fn list_agents_respects_limit_and_clamps_nonpositive() {
        let s = store();
        for i in 0..5 {
            s.register_agent(
                &format!("Agent{i}"),
                None,
                None,
                None,
                None,
                &format!("2026-07-24T0{i}:00:00Z"),
            )
            .unwrap();
        }
        assert_eq!(s.list_agents(2).unwrap().len(), 2);
        // A zero/negative limit must not error or return everything unbounded.
        assert_eq!(s.list_agents(0).unwrap().len(), 1);
    }

    #[test]
    fn fetch_unknown_agent_is_none_not_error() {
        let s = store();
        assert!(s.agent("nobody").unwrap().is_none());
    }

    #[test]
    fn empty_store_lists_nothing() {
        let s = store();
        assert!(s.list_agents(100).unwrap().is_empty());
    }

    #[test]
    fn two_connections_to_one_wal_file_see_each_others_writes() {
        // ADR-009 D1/D7: the shared-file model. Two independent connections to
        // one on-disk WAL DB (as two daimonos processes would) must observe
        // each other's committed writes.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.db");
        let a = CoordinationStore::open_with(&path, 5_000).unwrap();
        let b = CoordinationStore::open_with(&path, 5_000).unwrap();

        a.register_agent("FromA", None, None, None, None, "2026-07-24T00:00:00Z")
            .unwrap();
        b.register_agent("FromB", None, None, None, None, "2026-07-24T00:01:00Z")
            .unwrap();

        // Each connection sees both rows.
        let names_a: Vec<String> = a
            .list_agents(100)
            .unwrap()
            .into_iter()
            .map(|x| x.name)
            .collect();
        let names_b: Vec<String> = b
            .list_agents(100)
            .unwrap()
            .into_iter()
            .map(|x| x.name)
            .collect();
        assert!(names_a.contains(&"FromA".to_string()) && names_a.contains(&"FromB".to_string()));
        assert!(names_b.contains(&"FromA".to_string()) && names_b.contains(&"FromB".to_string()));
    }

    #[test]
    fn reopen_existing_file_preserves_rows_and_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.db");
        {
            let s = CoordinationStore::open_with(&path, 5_000).unwrap();
            s.register_agent("Persist", None, None, None, None, "2026-07-24T00:00:00Z")
                .unwrap();
        }
        // Reopen: schema init is idempotent, prior data survives.
        let s2 = CoordinationStore::open_with(&path, 5_000).unwrap();
        assert_eq!(s2.schema_version().unwrap(), Some(SCHEMA_VERSION));
        assert_eq!(s2.list_agents(100).unwrap().len(), 1);
    }
}
