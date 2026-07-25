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

/// Message importance levels (ADR-009 D3). Free-form-tolerant on read, but
/// `send_message` normalizes an input to one of these (unknown -> `Normal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Importance {
    Low,
    Normal,
    High,
    Urgent,
}

impl Importance {
    /// Parse a caller-supplied importance, defaulting unknown/empty to Normal.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Importance::Low,
            "high" => Importance::High,
            "urgent" => Importance::Urgent,
            _ => Importance::Normal,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Importance::Low => "low",
            Importance::Normal => "normal",
            Importance::High => "high",
            Importance::Urgent => "urgent",
        }
    }

    /// The canonical importance values, in ascending rank order. Single source
    /// of truth for the tool-schema `enum` so the three coordination schemas
    /// can't drift from `parse`/`as_str` (codeJung finding).
    pub fn schema_values() -> &'static [&'static str] {
        &["low", "normal", "high", "urgent"]
    }
}

/// The outcome of a `send_message`: the new message id and the thread it
/// belongs to (a fresh message threads under its own id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    pub message_id: i64,
    pub thread_id: i64,
    pub recipients: Vec<String>,
}

/// One message as stored (sender-side / thread view; no per-recipient state).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    pub id: i64,
    pub thread_id: i64,
    pub reply_to: Option<i64>,
    pub sender: String,
    pub subject: String,
    pub body: String,
    pub importance: String,
    pub ack_required: bool,
    pub created_ts: String,
}

/// One inbox entry: a message joined with the *reading agent's* per-recipient
/// delivery state (`kind`/`read_ts`/`ack_ts`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxEntry {
    pub message: MessageRecord,
    /// 'to' or 'cc' for this recipient.
    pub kind: String,
    pub read_ts: Option<String>,
    pub ack_ts: Option<String>,
}

/// Filters for `fetch_inbox` (all optional; ADR-009 D3).
#[derive(Debug, Clone, Default)]
pub struct InboxFilter {
    pub unread_only: bool,
    /// Restrict to messages at or above this importance, by rank.
    pub min_importance: Option<Importance>,
    /// Only messages created strictly after this RFC-3339 timestamp.
    pub since: Option<String>,
}

/// Bounded unread-mail summary used by cooperative notifications (#1063).
/// Carries metadata only — never subject/body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadSummary {
    pub count: u64,
    pub highest_importance: String,
    pub newest_message_id: i64,
}

/// One advisory file reservation (ADR-009 D4/D6). A soft "I'm working here"
/// signal, NOT a lock: it never blocks a write. `pattern` is an opaque glob
/// (never resolved against the filesystem). A reservation is *active* while it
/// is unreleased and its `expires_ts` is in the future.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationRecord {
    pub id: i64,
    pub agent_name: String,
    pub pattern: String,
    pub exclusive: bool,
    pub reason: Option<String>,
    pub created_ts: String,
    pub expires_ts: String,
    pub released_ts: Option<String>,
}

/// A conflict surfaced by `reserve_paths` / `check_conflicts`: the caller's
/// `pattern` overlaps an active exclusive reservation `held_by` another agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationConflict {
    pub pattern: String,
    pub held_by: String,
    pub conflicting_pattern: String,
    pub reservation_id: i64,
}

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
                 CREATE INDEX IF NOT EXISTS agent_session_id ON agent(session_id, last_seen_ts);
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
        // Detect an incompatible on-disk version rather than mis-parsing it
        // (ADR-009 D6 / #1053 lesson 3 spirit): if a version row already exists
        // and differs from ours, refuse to open. `open_with`'s caller
        // (`ops::coord`) turns this Err into a soft error, so a version skew
        // fails open — it never panics or corrupts data.
        if let Some(existing) = self.schema_version()? {
            if existing != SCHEMA_VERSION {
                anyhow::bail!(
                    "coordination store schema version {existing} != supported {SCHEMA_VERSION}"
                );
            }
        } else {
            // Fresh (or pre-meta) store: record our version once. The CHECK
            // pins id=1, and OR IGNORE keeps two racing initializers safe.
            self.conn
                .execute(
                    "INSERT OR IGNORE INTO schema_meta (id, version) VALUES (1, ?1)",
                    params![SCHEMA_VERSION],
                )
                .context("record schema version")?;
        }
        Ok(())
    }

    /// The recorded schema version, if any. `Ok(None)` on a store that predates
    /// the meta row (treated by callers as "not this version"). Public API for
    /// future migration/compat checks; consulted by `init` to reject an
    /// incompatible on-disk version.
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

    /// Find the most-recent agent registered to a runtime session id. Used to
    /// restore notification identity after ACP session reload; bounded to one
    /// indexed-style lookup and panic-free.
    pub fn agent_name_for_session(&self, session_id: &str) -> Result<Option<String>> {
        match self.conn.query_row(
            "SELECT name FROM agent WHERE session_id = ?1 ORDER BY last_seen_ts DESC LIMIT 1",
            params![session_id],
            |row| row.get(0),
        ) {
            Ok(name) => Ok(Some(name)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("lookup coordination identity by session"),
        }
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

    // ---- messaging (ADR-009 D3) ----

    /// Send a directed message. `to`/`cc` are agent names; **at least one
    /// recipient is required** (there is deliberately NO broadcast — the spam
    /// vector agent_mail omits). When `reply_to` names a parent message, the
    /// new message inherits that parent's thread (or, if the parent had none,
    /// the parent's id becomes the thread root); otherwise the new message
    /// starts its own thread (thread_id = its own id). Duplicate names across
    /// to+cc collapse to one recipient row (`to` wins). The sender is never
    /// auto-added as a recipient. Returns the message id + thread id.
    ///
    /// All writes happen in one transaction so a partial send can't leave a
    /// message with no recipients.
    #[allow(clippy::too_many_arguments)]
    pub fn send_message(
        &self,
        sender: &str,
        to: &[String],
        cc: &[String],
        subject: &str,
        body: &str,
        importance: Importance,
        ack_required: bool,
        reply_to: Option<i64>,
        now: &str,
    ) -> Result<SendReceipt> {
        // De-dupe recipients, `to` taking precedence over `cc`, and drop the
        // sender if they addressed themselves. Order is preserved for a stable
        // recipient list in the receipt.
        let mut seen = std::collections::HashSet::new();
        let mut recipients: Vec<(String, &'static str)> = Vec::new();
        for name in to {
            if name != sender && seen.insert(name.clone()) {
                recipients.push((name.clone(), "to"));
            }
        }
        for name in cc {
            if name != sender && seen.insert(name.clone()) {
                recipients.push((name.clone(), "cc"));
            }
        }
        if recipients.is_empty() {
            anyhow::bail!("send_message requires at least one recipient (no broadcast)");
        }

        // Resolve the parent's thread up front (a bounded single-row read), so
        // the transaction below is pure writes.
        let inherited_thread = match reply_to {
            Some(pid) => Some(self.thread_id_of(pid)?.unwrap_or(pid)),
            None => None,
        };

        let tx = self.conn.unchecked_transaction().context("begin send tx")?;
        tx.execute(
            "INSERT INTO message
                (thread_id, reply_to, sender, subject, body, importance, ack_required, created_ts)
             VALUES (NULL, ?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                reply_to,
                sender,
                subject,
                body,
                importance.as_str(),
                ack_required as i64,
                now
            ],
        )
        .context("insert message")?;
        let message_id = tx.last_insert_rowid();
        // A fresh message threads under its own id; a reply inherits the parent
        // thread. Set it now that we know the id.
        let thread_id = inherited_thread.unwrap_or(message_id);
        tx.execute(
            "UPDATE message SET thread_id = ?1 WHERE id = ?2",
            params![thread_id, message_id],
        )
        .context("set thread_id")?;
        for (name, kind) in &recipients {
            tx.execute(
                "INSERT INTO recipient (message_id, agent_name, kind) VALUES (?1, ?2, ?3)",
                params![message_id, name, kind],
            )
            .context("insert recipient")?;
        }
        tx.commit().context("commit send tx")?;

        Ok(SendReceipt {
            message_id,
            thread_id,
            recipients: recipients.into_iter().map(|(n, _)| n).collect(),
        })
    }

    /// Fetch one message by id (no per-recipient state), or `None` if absent.
    /// Single-row read; panic-free. Used by `reply_message` to default the
    /// reply's recipient to the parent's sender.
    pub fn message(&self, message_id: i64) -> Result<Option<MessageRecord>> {
        // Distinguish "no such message" (Ok(None)) from a real DB fault (Err):
        // mapping every failure to Ok(None) via `.ok()` would report a broken
        // store as "message not found", defeating the caller's fail-open
        // classification (codeJung finding).
        match self.conn.query_row(
            "SELECT id, thread_id, reply_to, sender, subject, body,
                    importance, ack_required, created_ts
             FROM message WHERE id = ?1",
            params![message_id],
            Self::row_to_message,
        ) {
            Ok(rec) => Ok(Some(rec)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("lookup message"),
        }
    }

    /// The thread id of a message, or `None` if the message doesn't exist.
    /// Single-row read; never recurses. A real DB fault propagates as `Err`
    /// (only a genuine "no such row" is `Ok(None)`), so a broken store is not
    /// silently mistaken for a new-thread root.
    fn thread_id_of(&self, message_id: i64) -> Result<Option<i64>> {
        match self.conn.query_row(
            "SELECT thread_id FROM message WHERE id = ?1",
            params![message_id],
            |row| row.get::<_, Option<i64>>(0),
        ) {
            Ok(tid) => Ok(tid),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e).context("lookup thread id"),
        }
    }

    /// An agent's inbox, newest-first, bounded by `limit` (clamped to >= 1).
    /// Filters (all optional): `unread_only`, `min_importance` (by rank),
    /// `since` (created strictly after). This is a single indexed join with a
    /// hard LIMIT — no recursion, no unbounded scan.
    pub fn fetch_inbox(
        &self,
        agent: &str,
        filter: &InboxFilter,
        limit: i64,
    ) -> Result<Vec<InboxEntry>> {
        let limit = limit.max(1);
        // Build the WHERE incrementally with bound params (no string interp of
        // user data). Importance rank is computed inline so the comparison is
        // ordinal, not lexical.
        let mut sql = String::from(
            "SELECT m.id, m.thread_id, m.reply_to, m.sender, m.subject, m.body,
                    m.importance, m.ack_required, m.created_ts,
                    r.kind, r.read_ts, r.ack_ts
             FROM recipient r JOIN message m ON m.id = r.message_id
             WHERE r.agent_name = ?1",
        );
        if filter.unread_only {
            sql.push_str(" AND r.read_ts IS NULL");
        }
        if filter.since.is_some() {
            sql.push_str(" AND m.created_ts > ?2");
        }
        if let Some(min) = filter.min_importance {
            // Inline the rank floor as a literal integer (not user input).
            sql.push_str(&format!(
                " AND {} >= {}",
                importance_rank_sql("m.importance"),
                importance_rank(min)
            ));
        }
        sql.push_str(" ORDER BY m.created_ts DESC, m.id DESC LIMIT ?3");

        let mut stmt = self.conn.prepare(&sql).context("prepare fetch_inbox")?;
        // `since` is optional but the placeholder ?2 must always bind; pass an
        // empty string when unused (the clause referencing it is only added
        // when Some, so an empty bind is inert otherwise).
        let since_bind = filter.since.clone().unwrap_or_default();
        let rows = stmt
            .query_map(params![agent, since_bind, limit], Self::row_to_inbox_entry)
            .context("query fetch_inbox")?;
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(entry) => out.push(entry),
                Err(_) => continue,
            }
        }
        Ok(out)
    }

    /// Metadata-only unread summary newer than `after_message_id`. The query is
    /// bounded to `scan_cap` rows and returns None when no new unread mail
    /// exists. No subjects/bodies are selected.
    pub fn unread_summary(
        &self,
        agent: &str,
        after_message_id: i64,
        scan_cap: i64,
    ) -> Result<Option<UnreadSummary>> {
        let scan_cap = scan_cap.clamp(1, 1_000);
        let mut stmt = self.conn.prepare(
            "SELECT m.id, m.importance
             FROM recipient r JOIN message m ON m.id = r.message_id
             WHERE r.agent_name = ?1 AND r.read_ts IS NULL AND m.id > ?2
             ORDER BY m.id DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![agent, after_message_id, scan_cap], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut count = 0u64;
        let mut newest = 0i64;
        let mut highest = Importance::Low;
        for (id, importance) in rows.flatten() {
            count += 1;
            newest = newest.max(id);
            let parsed = Importance::parse(&importance);
            if importance_rank(parsed) > importance_rank(highest) {
                highest = parsed;
            }
        }
        Ok((count > 0).then(|| UnreadSummary {
            count,
            highest_importance: highest.as_str().to_string(),
            newest_message_id: newest,
        }))
    }

    /// Mark one message read for `agent`. Idempotent: a second call keeps the
    /// original `read_ts`. Returns the effective read timestamp, or `None` if
    /// the agent is not a recipient of that message.
    pub fn mark_read(&self, agent: &str, message_id: i64, now: &str) -> Result<Option<String>> {
        self.conn
            .execute(
                "UPDATE recipient SET read_ts = ?1
                 WHERE message_id = ?2 AND agent_name = ?3 AND read_ts IS NULL",
                params![now, message_id, agent],
            )
            .context("mark_read")?;
        self.recipient_read_ts(agent, message_id)
    }

    /// Acknowledge one message for `agent` (also marks it read). Idempotent:
    /// a second call keeps the original `ack_ts`. Returns `(read_ts, ack_ts)`,
    /// or `None` if the agent is not a recipient.
    pub fn acknowledge(
        &self,
        agent: &str,
        message_id: i64,
        now: &str,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        self.conn
            .execute(
                "UPDATE recipient
                    SET read_ts = COALESCE(read_ts, ?1),
                        ack_ts  = COALESCE(ack_ts, ?1)
                 WHERE message_id = ?2 AND agent_name = ?3",
                params![now, message_id, agent],
            )
            .context("acknowledge")?;
        let row = self
            .conn
            .query_row(
                "SELECT read_ts, ack_ts FROM recipient
                 WHERE message_id = ?1 AND agent_name = ?2",
                params![message_id, agent],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .ok();
        Ok(row)
    }

    fn recipient_read_ts(&self, agent: &str, message_id: i64) -> Result<Option<String>> {
        let ts = self
            .conn
            .query_row(
                "SELECT read_ts FROM recipient WHERE message_id = ?1 AND agent_name = ?2",
                params![message_id, agent],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten();
        Ok(ts)
    }

    /// All messages in a thread, oldest-first, bounded by `cap` (clamped to at
    /// least 1). This is a flat indexed select on `thread_id` with a hard
    /// LIMIT. It deliberately does not walk the `reply_to` chain recursively,
    /// so a long or self-referential reply chain can never overflow the stack
    /// (ADR-009 D3/D7; #1053 lesson 1).
    pub fn thread(&self, thread_id: i64, cap: i64) -> Result<Vec<MessageRecord>> {
        let cap = cap.max(1);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, thread_id, reply_to, sender, subject, body,
                        importance, ack_required, created_ts
                 FROM message WHERE thread_id = ?1
                 ORDER BY created_ts ASC, id ASC LIMIT ?2",
            )
            .context("prepare thread")?;
        let rows = stmt
            .query_map(params![thread_id, cap], Self::row_to_message)
            .context("query thread")?;
        let mut out = Vec::new();
        for row in rows {
            match row {
                Ok(m) => out.push(m),
                Err(_) => continue,
            }
        }
        Ok(out)
    }

    fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRecord> {
        Ok(MessageRecord {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            reply_to: row.get(2)?,
            sender: row.get(3)?,
            subject: row.get(4)?,
            body: row.get(5)?,
            importance: row.get(6)?,
            ack_required: row.get::<_, i64>(7)? != 0,
            created_ts: row.get(8)?,
        })
    }

    fn row_to_inbox_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<InboxEntry> {
        Ok(InboxEntry {
            message: MessageRecord {
                id: row.get(0)?,
                thread_id: row.get(1)?,
                reply_to: row.get(2)?,
                sender: row.get(3)?,
                subject: row.get(4)?,
                body: row.get(5)?,
                importance: row.get(6)?,
                ack_required: row.get::<_, i64>(7)? != 0,
                created_ts: row.get(8)?,
            },
            kind: row.get(9)?,
            read_ts: row.get(10)?,
            ack_ts: row.get(11)?,
        })
    }

    // ---- advisory file reservations (ADR-009 D4) ----

    /// Claim advisory reservations on `patterns` for `agent`, each expiring at
    /// `expires_ts`. Returns the granted reservations plus any conflicts with
    /// *other* agents' active exclusive reservations (symmetric glob overlap).
    /// Reservations are advisory: a conflict is reported, never enforced, and
    /// the reservation is still granted (agents cooperate). `scan_cap` bounds
    /// how many existing active reservations are examined for conflicts; the
    /// returned `scan_truncated` flag is true when the active set exceeded the
    /// cap (so an empty `conflicts` is not a guaranteed all-clear — codeJung
    /// finding).
    #[allow(clippy::too_many_arguments)]
    pub fn reserve_paths(
        &self,
        agent: &str,
        patterns: &[String],
        exclusive: bool,
        reason: Option<&str>,
        created_ts: &str,
        expires_ts: &str,
        scan_cap: i64,
    ) -> Result<(Vec<ReservationRecord>, Vec<ReservationConflict>, bool)> {
        // Conflicts are computed against the pre-existing active set, before we
        // insert (so a batch doesn't self-conflict).
        let (existing, scan_truncated) =
            self.active_reservations_excluding(agent, created_ts, scan_cap)?;
        let mut conflicts = Vec::new();
        for pat in patterns {
            for other in &existing {
                if other.exclusive && patterns_overlap(pat, &other.pattern) {
                    conflicts.push(ReservationConflict {
                        pattern: pat.clone(),
                        held_by: other.agent_name.clone(),
                        conflicting_pattern: other.pattern.clone(),
                        reservation_id: other.id,
                    });
                }
            }
        }

        let tx = self
            .conn
            .unchecked_transaction()
            .context("begin reserve tx")?;
        let mut granted = Vec::with_capacity(patterns.len());
        for pat in patterns {
            tx.execute(
                "INSERT INTO reservation
                    (agent_name, pattern, exclusive, reason, created_ts, expires_ts, released_ts)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL)",
                params![agent, pat, exclusive as i64, reason, created_ts, expires_ts],
            )
            .context("insert reservation")?;
            let id = tx.last_insert_rowid();
            granted.push(ReservationRecord {
                id,
                agent_name: agent.to_string(),
                pattern: pat.clone(),
                exclusive,
                reason: reason.map(String::from),
                created_ts: created_ts.to_string(),
                expires_ts: expires_ts.to_string(),
                released_ts: None,
            });
        }
        tx.commit().context("commit reserve tx")?;
        Ok((granted, conflicts, scan_truncated))
    }

    /// Check `patterns` against other agents' active exclusive reservations
    /// WITHOUT mutating anything (read-only pre-edit guard). Ignores the
    /// caller's own reservations. Returns `(conflicts, scan_truncated)`;
    /// `scan_truncated` is true when more active foreign reservations existed
    /// than `scan_cap`, so an empty `conflicts` is not a guaranteed all-clear.
    pub fn check_conflicts(
        &self,
        agent: &str,
        patterns: &[String],
        now: &str,
        scan_cap: i64,
    ) -> Result<(Vec<ReservationConflict>, bool)> {
        let (existing, scan_truncated) =
            self.active_reservations_excluding(agent, now, scan_cap)?;
        let mut conflicts = Vec::new();
        for pat in patterns {
            for other in &existing {
                if other.exclusive && patterns_overlap(pat, &other.pattern) {
                    conflicts.push(ReservationConflict {
                        pattern: pat.clone(),
                        held_by: other.agent_name.clone(),
                        conflicting_pattern: other.pattern.clone(),
                        reservation_id: other.id,
                    });
                }
            }
        }
        Ok((conflicts, scan_truncated))
    }

    /// List an agent's own active (unreleased, unexpired) reservations,
    /// bounded. `now` is the RFC-3339 cutoff for expiry.
    pub fn list_reservations(
        &self,
        agent: &str,
        now: &str,
        limit: i64,
    ) -> Result<Vec<ReservationRecord>> {
        let limit = limit.max(1);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, agent_name, pattern, exclusive, reason, created_ts, expires_ts, released_ts
                 FROM reservation
                 WHERE agent_name = ?1 AND released_ts IS NULL AND expires_ts > ?2
                 ORDER BY created_ts DESC, id DESC LIMIT ?3",
            )
            .context("prepare list_reservations")?;
        let rows = stmt
            .query_map(params![agent, now, limit], Self::row_to_reservation)
            .context("query list_reservations")?;
        let mut out = Vec::new();
        for r in rows.flatten() {
            out.push(r);
        }
        Ok(out)
    }

    /// Extend the caller's own active reservations' `expires_ts` to
    /// `new_expires_ts`. Only the caller's unreleased, unexpired reservations
    /// are touched. Returns the number renewed.
    pub fn renew_reservations(
        &self,
        agent: &str,
        now: &str,
        new_expires_ts: &str,
    ) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "UPDATE reservation SET expires_ts = ?1
                 WHERE agent_name = ?2 AND released_ts IS NULL AND expires_ts > ?3",
                params![new_expires_ts, agent, now],
            )
            .context("renew_reservations")?;
        Ok(n)
    }

    /// Release the caller's own *unreleased* reservations (sets `released_ts`).
    /// When `patterns` is empty, releases all of the caller's unreleased
    /// reservations; otherwise only those whose pattern exactly matches one of
    /// `patterns`. This intentionally finalizes unreleased-but-expired rows too
    /// (benign cleanup) — the count reflects rows actually transitioned, so it
    /// does not claim to touch only unexpired reservations. Returns the number
    /// released. Idempotent.
    pub fn release_reservations(
        &self,
        agent: &str,
        patterns: &[String],
        now: &str,
    ) -> Result<usize> {
        if patterns.is_empty() {
            let n = self
                .conn
                .execute(
                    "UPDATE reservation SET released_ts = ?1
                     WHERE agent_name = ?2 AND released_ts IS NULL",
                    params![now, agent],
                )
                .context("release_reservations (all)")?;
            return Ok(n);
        }
        let mut released = 0usize;
        let tx = self
            .conn
            .unchecked_transaction()
            .context("begin release tx")?;
        for pat in patterns {
            released += tx
                .execute(
                    "UPDATE reservation SET released_ts = ?1
                     WHERE agent_name = ?2 AND pattern = ?3 AND released_ts IS NULL",
                    params![now, agent, pat],
                )
                .context("release_reservations (pattern)")?;
        }
        tx.commit().context("commit release tx")?;
        Ok(released)
    }

    /// Prune reservations that expired before `cutoff` (housekeeping). Bounded
    /// single DELETE; returns the number removed. Optional — expired rows are
    /// already inert because every read filters on `expires_ts`, so no verb
    /// exposes this yet; it's a tested maintenance primitive for a future
    /// background-prune task.
    #[allow(dead_code)]
    pub fn prune_expired_reservations(&self, cutoff: &str) -> Result<usize> {
        let n = self
            .conn
            .execute(
                "DELETE FROM reservation WHERE expires_ts <= ?1",
                params![cutoff],
            )
            .context("prune_expired_reservations")?;
        Ok(n)
    }

    /// Active reservations held by agents OTHER than `agent`, bounded by
    /// `scan_cap`. "Active" = unreleased and not yet expired at `now`. Used for
    /// conflict detection; the caller does the in-memory glob overlap so there
    /// is no recursion and no filesystem walk. Returns `(rows, truncated)`
    /// where `truncated` is true if more than `scan_cap` active foreign
    /// reservations exist (we query one extra to detect it) — so callers can
    /// surface that an empty conflict list may be incomplete.
    fn active_reservations_excluding(
        &self,
        agent: &str,
        now: &str,
        scan_cap: i64,
    ) -> Result<(Vec<ReservationRecord>, bool)> {
        let scan_cap = scan_cap.max(1);
        // Fetch one extra row to detect truncation without a separate COUNT.
        let fetch = scan_cap.saturating_add(1);
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, agent_name, pattern, exclusive, reason, created_ts, expires_ts, released_ts
                 FROM reservation
                 WHERE agent_name <> ?1 AND released_ts IS NULL AND expires_ts > ?2
                 ORDER BY id DESC LIMIT ?3",
            )
            .context("prepare active_reservations")?;
        let rows = stmt
            .query_map(params![agent, now, fetch], Self::row_to_reservation)
            .context("query active_reservations")?;
        let mut out = Vec::new();
        for r in rows.flatten() {
            out.push(r);
        }
        let truncated = out.len() as i64 > scan_cap;
        if truncated {
            out.truncate(scan_cap as usize);
        }
        Ok((out, truncated))
    }

    fn row_to_reservation(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReservationRecord> {
        Ok(ReservationRecord {
            id: row.get(0)?,
            agent_name: row.get(1)?,
            pattern: row.get(2)?,
            exclusive: row.get::<_, i64>(3)? != 0,
            reason: row.get(4)?,
            created_ts: row.get(5)?,
            expires_ts: row.get(6)?,
            released_ts: row.get(7)?,
        })
    }
}

/// Symmetric glob overlap: true if `a` matches `b` as a glob, or `b` matches
/// `a`, or they are exactly equal. Patterns are opaque globs compared against
/// each other — never resolved against the filesystem, so this cannot probe
/// outside the workspace and does no directory walk (ADR-009 D4 / #1053
/// lesson 1). A pattern that fails to compile falls back to exact string
/// equality (still total and panic-free).
fn patterns_overlap(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let a_matches_b = glob::Pattern::new(a).map(|p| p.matches(b)).unwrap_or(false);
    let b_matches_a = glob::Pattern::new(b).map(|p| p.matches(a)).unwrap_or(false);
    a_matches_b || b_matches_a
}

/// Ordinal rank of an importance level, for `min_importance` filtering.
fn importance_rank(i: Importance) -> i64 {
    match i {
        Importance::Low => 0,
        Importance::Normal => 1,
        Importance::High => 2,
        Importance::Urgent => 3,
    }
}

/// A SQL `CASE` expression mapping an importance column to its ordinal rank, so
/// the `min_importance` comparison is ordinal rather than lexical. The argument
/// is a column name we control (never user input).
fn importance_rank_sql(col: &str) -> String {
    format!(
        "(CASE {col} WHEN 'low' THEN 0 WHEN 'normal' THEN 1 \
          WHEN 'high' THEN 2 WHEN 'urgent' THEN 3 ELSE 1 END)"
    )
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
    fn incompatible_schema_version_is_rejected_not_misparsed() {
        // A store written by a future/incompatible version must fail to open
        // (Err), which callers turn into a soft error — never silently opened
        // and mis-parsed (ADR-009 D6).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.db");
        {
            let s = CoordinationStore::open_with(&path, 5_000).unwrap();
            // Bump the recorded version out from under us.
            s.conn
                .execute(
                    "UPDATE schema_meta SET version = ?1 WHERE id = 1",
                    params![SCHEMA_VERSION + 1],
                )
                .unwrap();
        }
        let reopened = CoordinationStore::open_with(&path, 5_000);
        assert!(
            reopened.is_err(),
            "an incompatible schema version must be rejected on open"
        );
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

    // ---- messaging ----

    fn send(
        s: &CoordinationStore,
        sender: &str,
        to: &[&str],
        cc: &[&str],
        subject: &str,
        now: &str,
    ) -> SendReceipt {
        let to: Vec<String> = to.iter().map(|x| x.to_string()).collect();
        let cc: Vec<String> = cc.iter().map(|x| x.to_string()).collect();
        s.send_message(
            sender,
            &to,
            &cc,
            subject,
            "body",
            Importance::Normal,
            false,
            None,
            now,
        )
        .unwrap()
    }

    #[test]
    fn send_lands_in_recipient_inbox_not_sender() {
        let s = store();
        let r = send(
            &s,
            "BlueLake",
            &["GreenCastle"],
            &[],
            "hi",
            "2026-07-24T00:00:00Z",
        );
        assert_eq!(r.recipients, vec!["GreenCastle"]);

        let inbox = s
            .fetch_inbox("GreenCastle", &InboxFilter::default(), 100)
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].message.subject, "hi");
        assert_eq!(inbox[0].message.sender, "BlueLake");
        assert_eq!(inbox[0].kind, "to");
        assert!(inbox[0].read_ts.is_none());

        // Sender does not receive their own message.
        let sender_inbox = s
            .fetch_inbox("BlueLake", &InboxFilter::default(), 100)
            .unwrap();
        assert!(sender_inbox.is_empty());
    }

    #[test]
    fn cc_recipients_get_kind_cc_and_dedupe_with_to() {
        let s = store();
        // Overlap: GreenCastle in both to and cc -> single row, kind 'to' wins.
        s.send_message(
            "BlueLake",
            &["GreenCastle".into()],
            &["GreenCastle".into(), "Ridge".into()],
            "s",
            "b",
            Importance::Normal,
            false,
            None,
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        let gc = s
            .fetch_inbox("GreenCastle", &InboxFilter::default(), 10)
            .unwrap();
        assert_eq!(
            gc.len(),
            1,
            "overlapping recipient must collapse to one row"
        );
        assert_eq!(gc[0].kind, "to");
        let ridge = s.fetch_inbox("Ridge", &InboxFilter::default(), 10).unwrap();
        assert_eq!(ridge[0].kind, "cc");
    }

    #[test]
    fn send_requires_a_recipient_no_broadcast() {
        let s = store();
        // Empty to+cc -> error.
        let empty = s.send_message(
            "BlueLake",
            &[],
            &[],
            "s",
            "b",
            Importance::Normal,
            false,
            None,
            "t",
        );
        assert!(
            empty.is_err(),
            "a message with no recipients must be rejected"
        );
        // Addressing only yourself also yields zero recipients -> error.
        let self_only = s.send_message(
            "BlueLake",
            &["BlueLake".into()],
            &[],
            "s",
            "b",
            Importance::Normal,
            false,
            None,
            "t",
        );
        assert!(self_only.is_err());
    }

    #[test]
    fn inbox_filters_unread_importance_since() {
        let s = store();
        s.send_message(
            "A",
            &["Z".into()],
            &[],
            "low",
            "b",
            Importance::Low,
            false,
            None,
            "2026-07-24T00:00:01Z",
        )
        .unwrap();
        let high = s
            .send_message(
                "A",
                &["Z".into()],
                &[],
                "high",
                "b",
                Importance::High,
                false,
                None,
                "2026-07-24T00:00:02Z",
            )
            .unwrap();
        s.send_message(
            "A",
            &["Z".into()],
            &[],
            "urgent",
            "b",
            Importance::Urgent,
            false,
            None,
            "2026-07-24T00:00:03Z",
        )
        .unwrap();

        // Newest-first ordering.
        let all = s.fetch_inbox("Z", &InboxFilter::default(), 100).unwrap();
        let subjects: Vec<&str> = all.iter().map(|e| e.message.subject.as_str()).collect();
        assert_eq!(subjects, vec!["urgent", "high", "low"]);

        // min_importance = High -> only high + urgent.
        let f = InboxFilter {
            min_importance: Some(Importance::High),
            ..Default::default()
        };
        let hi = s.fetch_inbox("Z", &f, 100).unwrap();
        assert_eq!(hi.len(), 2);
        assert!(hi.iter().all(|e| e.message.subject != "low"));

        // since -> strictly-after.
        let f = InboxFilter {
            since: Some("2026-07-24T00:00:01Z".into()),
            ..Default::default()
        };
        let since = s.fetch_inbox("Z", &f, 100).unwrap();
        assert_eq!(since.len(), 2);
        assert!(since.iter().all(|e| e.message.subject != "low"));

        // Read the high message, then unread_only excludes it.
        s.mark_read("Z", high.message_id, "2026-07-24T01:00:00Z")
            .unwrap();
        let f = InboxFilter {
            unread_only: true,
            ..Default::default()
        };
        let unread = s.fetch_inbox("Z", &f, 100).unwrap();
        assert_eq!(unread.len(), 2);
        assert!(unread.iter().all(|e| e.message.subject != "high"));
    }

    #[test]
    fn unread_summary_is_metadata_only_bounded_and_watermarked() {
        let s = store();
        let one = s
            .send_message(
                "A",
                &["Z".into()],
                &[],
                "SUBJECT_SECRET",
                "BODY_SECRET",
                Importance::Normal,
                false,
                None,
                "2026-07-24T00:00:01Z",
            )
            .unwrap();
        let two = s
            .send_message(
                "A",
                &["Z".into()],
                &[],
                "ANOTHER_SECRET",
                "MORE_SECRET",
                Importance::Urgent,
                false,
                None,
                "2026-07-24T00:00:02Z",
            )
            .unwrap();
        let summary = s.unread_summary("Z", 0, 100).unwrap().unwrap();
        assert_eq!(summary.count, 2);
        assert_eq!(summary.highest_importance, "urgent");
        assert_eq!(summary.newest_message_id, two.message_id);
        // Watermark dedup: nothing newer than the newest surfaced id.
        assert!(s
            .unread_summary("Z", summary.newest_message_id, 100)
            .unwrap()
            .is_none());
        // Marking the first read leaves only the second in a fresh summary.
        s.mark_read("Z", one.message_id, "2026-07-24T01:00:00Z")
            .unwrap();
        let remaining = s.unread_summary("Z", 0, 100).unwrap().unwrap();
        assert_eq!(remaining.count, 1);
        assert_eq!(remaining.newest_message_id, two.message_id);
    }

    #[test]
    fn coordination_identity_recovers_by_session_id() {
        let s = store();
        s.register_agent(
            "BlueLake",
            Some("acp-session-1"),
            Some("daimonos"),
            None,
            None,
            "2026-07-24T00:00:00Z",
        )
        .unwrap();
        assert_eq!(
            s.agent_name_for_session("acp-session-1")
                .unwrap()
                .as_deref(),
            Some("BlueLake")
        );
        assert!(s.agent_name_for_session("missing").unwrap().is_none());
    }

    #[test]
    fn mark_read_and_acknowledge_are_idempotent_and_scoped() {
        let s = store();
        let r = send(&s, "A", &["Z"], &[], "s", "2026-07-24T00:00:00Z");
        // First read sets ts; second keeps it.
        let first = s
            .mark_read("Z", r.message_id, "2026-07-24T01:00:00Z")
            .unwrap();
        assert_eq!(first.as_deref(), Some("2026-07-24T01:00:00Z"));
        let second = s
            .mark_read("Z", r.message_id, "2026-07-24T02:00:00Z")
            .unwrap();
        assert_eq!(
            second.as_deref(),
            Some("2026-07-24T01:00:00Z"),
            "read_ts must not change"
        );

        // Acknowledge sets ack (and read if unset); idempotent.
        let ack = s
            .acknowledge("Z", r.message_id, "2026-07-24T03:00:00Z")
            .unwrap()
            .unwrap();
        assert_eq!(ack.1.as_deref(), Some("2026-07-24T03:00:00Z"));
        let ack2 = s
            .acknowledge("Z", r.message_id, "2026-07-24T04:00:00Z")
            .unwrap()
            .unwrap();
        assert_eq!(
            ack2.1.as_deref(),
            Some("2026-07-24T03:00:00Z"),
            "ack_ts must not change"
        );

        // A non-recipient gets None, not an error.
        assert!(s.mark_read("Nobody", r.message_id, "t").unwrap().is_none());
        assert!(s
            .acknowledge("Nobody", r.message_id, "t")
            .unwrap()
            .is_none());
    }

    #[test]
    fn reply_inherits_thread_and_new_message_starts_own_thread() {
        let s = store();
        let root = send(&s, "A", &["B"], &[], "root", "2026-07-24T00:00:00Z");
        // A fresh message threads under its own id.
        assert_eq!(root.thread_id, root.message_id);

        // B replies to A within the same thread.
        let reply = s
            .send_message(
                "B",
                &["A".into()],
                &[],
                "re",
                "b",
                Importance::Normal,
                false,
                Some(root.message_id),
                "2026-07-24T00:01:00Z",
            )
            .unwrap();
        assert_eq!(reply.thread_id, root.thread_id);

        // A reply to the reply still lands in the same root thread.
        let reply2 = s
            .send_message(
                "A",
                &["B".into()],
                &[],
                "re re",
                "b",
                Importance::Normal,
                false,
                Some(reply.message_id),
                "2026-07-24T00:02:00Z",
            )
            .unwrap();
        assert_eq!(reply2.thread_id, root.thread_id);

        let thread = s.thread(root.thread_id, 100).unwrap();
        assert_eq!(thread.len(), 3);
        // Oldest-first.
        assert_eq!(thread[0].subject, "root");
        assert_eq!(thread[2].subject, "re re");
    }

    #[test]
    fn thread_is_bounded_and_never_recurses_on_self_referential_chain() {
        let s = store();
        let root = send(&s, "A", &["B"], &[], "m0", "2026-07-24T00:00:00Z");
        // Build a long chain, each replying to the previous.
        let mut prev = root.message_id;
        for i in 1..50 {
            let r = s
                .send_message(
                    "A",
                    &["B".into()],
                    &[],
                    &format!("m{i}"),
                    "b",
                    Importance::Normal,
                    false,
                    Some(prev),
                    &format!("2026-07-24T00:{:02}:00Z", i),
                )
                .unwrap();
            assert_eq!(r.thread_id, root.thread_id);
            prev = r.message_id;
        }
        // A hard cap bounds the read regardless of chain length (no recursion).
        let capped = s.thread(root.thread_id, 10).unwrap();
        assert_eq!(capped.len(), 10, "thread read must honor the hard cap");
        let full = s.thread(root.thread_id, 1000).unwrap();
        assert_eq!(full.len(), 50);
    }

    #[test]
    fn messaging_survives_two_connections_to_one_wal_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.db");
        let a = CoordinationStore::open_with(&path, 5_000).unwrap();
        let b = CoordinationStore::open_with(&path, 5_000).unwrap();
        // A sends; B (a separate connection) reads it and acks; A sees the ack.
        let r = a
            .send_message(
                "A",
                &["B".into()],
                &[],
                "s",
                "b",
                Importance::High,
                true,
                None,
                "2026-07-24T00:00:00Z",
            )
            .unwrap();
        let inbox = b.fetch_inbox("B", &InboxFilter::default(), 10).unwrap();
        assert_eq!(inbox.len(), 1);
        b.acknowledge("B", r.message_id, "2026-07-24T01:00:00Z")
            .unwrap();
        // A re-reads via its own connection and sees B's ack.
        let seen = a.fetch_inbox("B", &InboxFilter::default(), 10).unwrap();
        assert_eq!(seen[0].ack_ts.as_deref(), Some("2026-07-24T01:00:00Z"));
    }

    // ---- reservations ----

    const FAR: &str = "2999-01-01T00:00:00Z"; // far-future expiry (active)

    fn reserve(
        s: &CoordinationStore,
        agent: &str,
        pats: &[&str],
        exclusive: bool,
    ) -> Vec<ReservationConflict> {
        let pats: Vec<String> = pats.iter().map(|x| x.to_string()).collect();
        s.reserve_paths(
            agent,
            &pats,
            exclusive,
            None,
            "2026-07-24T00:00:00Z",
            FAR,
            1000,
        )
        .unwrap()
        .1
    }

    #[test]
    fn reserve_is_visible_and_conflicts_only_with_other_agents_exclusive() {
        let s = store();
        let c = reserve(&s, "A", &["src/api/*.rs"], true);
        assert!(c.is_empty(), "first reservation has no conflict");
        assert_eq!(
            s.list_reservations("A", "2026-07-24T00:00:00Z", 100)
                .unwrap()
                .len(),
            1
        );

        // B reserving an overlapping glob sees a conflict against A.
        let c = reserve(&s, "B", &["src/api/users.rs"], true);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].held_by, "A");
        assert_eq!(c[0].conflicting_pattern, "src/api/*.rs");

        // A non-overlapping path is conflict-free.
        let c = reserve(&s, "B", &["docs/readme.md"], true);
        assert!(c.is_empty());
    }

    #[test]
    fn own_reservation_never_conflicts_with_self() {
        let s = store();
        reserve(&s, "A", &["src/**"], true);
        let c = reserve(&s, "A", &["src/api/users.rs"], true);
        assert!(
            c.is_empty(),
            "an agent never conflicts with its own reservations"
        );
    }

    #[test]
    fn shared_nonexclusive_reservations_do_not_conflict() {
        let s = store();
        reserve(&s, "A", &["src/api/*.rs"], false);
        let c = reserve(&s, "B", &["src/api/users.rs"], true);
        assert!(
            c.is_empty(),
            "a non-exclusive holder does not cause a conflict"
        );
    }

    #[test]
    fn expired_reservation_is_inert() {
        let s = store();
        s.reserve_paths(
            "A",
            &["src/*".into()],
            true,
            None,
            "2026-01-01T00:00:00Z",
            "2026-01-01T01:00:00Z",
            1000,
        )
        .unwrap();
        let (c, _) = s
            .check_conflicts("B", &["src/main.rs".into()], "2026-07-24T00:00:00Z", 1000)
            .unwrap();
        assert!(c.is_empty(), "an expired reservation must not conflict");
        assert!(s
            .list_reservations("A", "2026-07-24T00:00:00Z", 100)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn renew_extends_and_release_drops() {
        let s = store();
        s.reserve_paths(
            "A",
            &["src/*".into()],
            true,
            None,
            "2026-07-24T00:00:00Z",
            "2026-07-24T01:00:00Z",
            1000,
        )
        .unwrap();
        assert!(s
            .list_reservations("A", "2026-07-24T02:00:00Z", 100)
            .unwrap()
            .is_empty());
        let renewed = s
            .renew_reservations("A", "2026-07-24T00:30:00Z", FAR)
            .unwrap();
        assert_eq!(renewed, 1);
        assert_eq!(
            s.list_reservations("A", "2026-07-24T02:00:00Z", 100)
                .unwrap()
                .len(),
            1
        );
        let released = s
            .release_reservations("A", &[], "2026-07-24T03:00:00Z")
            .unwrap();
        assert_eq!(released, 1);
        assert!(s
            .list_reservations("A", "2026-07-24T03:00:00Z", 100)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn release_by_pattern_only_targets_that_pattern() {
        let s = store();
        reserve(&s, "A", &["src/a.rs", "src/b.rs"], true);
        let n = s
            .release_reservations("A", &["src/a.rs".into()], "2026-07-24T03:00:00Z")
            .unwrap();
        assert_eq!(n, 1);
        let remaining = s
            .list_reservations("A", "2026-07-24T03:00:00Z", 100)
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].pattern, "src/b.rs");
    }

    #[test]
    fn check_conflicts_is_read_only_and_ignores_own() {
        let s = store();
        reserve(&s, "A", &["src/api/*.rs"], true);
        let before = s
            .list_reservations("A", "2026-07-24T00:00:00Z", 100)
            .unwrap()
            .len();
        let (c, _) = s
            .check_conflicts(
                "A",
                &["src/api/users.rs".into()],
                "2026-07-24T00:00:00Z",
                1000,
            )
            .unwrap();
        assert!(c.is_empty());
        let after = s
            .list_reservations("A", "2026-07-24T00:00:00Z", 100)
            .unwrap()
            .len();
        assert_eq!(before, after, "check_conflicts must not mutate");
        let (c, _) = s
            .check_conflicts(
                "B",
                &["src/api/users.rs".into()],
                "2026-07-24T00:00:00Z",
                1000,
            )
            .unwrap();
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn prune_removes_expired_only() {
        let s = store();
        s.reserve_paths(
            "A",
            &["old/*".into()],
            true,
            None,
            "2026-01-01T00:00:00Z",
            "2026-01-01T01:00:00Z",
            1000,
        )
        .unwrap();
        s.reserve_paths(
            "A",
            &["new/*".into()],
            true,
            None,
            "2026-07-24T00:00:00Z",
            FAR,
            1000,
        )
        .unwrap();
        let pruned = s
            .prune_expired_reservations("2026-07-24T00:00:00Z")
            .unwrap();
        assert_eq!(pruned, 1, "only the expired reservation is pruned");
        assert_eq!(
            s.list_reservations("A", "2026-07-24T00:00:00Z", 100)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn scan_truncation_is_surfaced_when_active_set_exceeds_cap() {
        // codeJung (impact 7): with more active foreign reservations than the
        // scan cap, an empty conflict list must NOT be reported as all-clear —
        // the truncated flag tells the caller the scan was incomplete.
        let s = store();
        // Three other-agent reservations on non-overlapping paths.
        for i in 0..3 {
            s.reserve_paths(
                &format!("Other{i}"),
                &[format!("unrelated/dir{i}/*")],
                true,
                None,
                "2026-07-24T00:00:00Z",
                FAR,
                1000,
            )
            .unwrap();
        }
        // Scan cap of 2 < 3 active foreign reservations => truncated.
        let (conflicts, truncated) = s
            .check_conflicts("Me", &["src/main.rs".into()], "2026-07-24T00:00:00Z", 2)
            .unwrap();
        assert!(conflicts.is_empty(), "no overlap, so no conflicts found");
        assert!(
            truncated,
            "scan hit the cap, so truncation must be surfaced"
        );
        // A cap comfortably above the active set is not truncated.
        let (_c, truncated) = s
            .check_conflicts("Me", &["src/main.rs".into()], "2026-07-24T00:00:00Z", 100)
            .unwrap();
        assert!(!truncated, "a cap above the active set is a complete scan");
    }

    #[test]
    fn reservations_share_across_two_connections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.db");
        let a = CoordinationStore::open_with(&path, 5_000).unwrap();
        let b = CoordinationStore::open_with(&path, 5_000).unwrap();
        a.reserve_paths(
            "A",
            &["src/api/*.rs".into()],
            true,
            None,
            "2026-07-24T00:00:00Z",
            FAR,
            1000,
        )
        .unwrap();
        let (c, _) = b
            .check_conflicts(
                "B",
                &["src/api/users.rs".into()],
                "2026-07-24T00:00:00Z",
                1000,
            )
            .unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].held_by, "A");
    }
}
