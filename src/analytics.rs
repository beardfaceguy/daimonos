use chrono::Utc;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Estimate token count from character length (same heuristic as RTK).
pub fn estimate_tokens(chars: usize) -> u64 {
    (chars as f64 / 4.0).ceil() as u64
}

/// Anonymize a command string to first 3 words for privacy.
fn anonymize_command(cmd: &str) -> String {
    cmd.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub command: Option<String>,
    pub request_tokens: u64,
    pub response_tokens: u64,
    pub saved_tokens: i64,
    pub savings_pct: f64,
    pub exec_time_ms: u64,
    pub was_redirect: bool,
    pub was_filtered: bool,
    pub read_dedup: bool,
    pub batch_size: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct SessionStats {
    pub total_calls: u64,
    pub total_request_tokens: u64,
    pub total_response_tokens: u64,
    pub total_saved_tokens: i64,
    pub redirect_hits: u64,
    pub filter_hits: u64,
    pub dedup_hits: u64,
    pub batch_calls: u64,
    pub per_tool: HashMap<String, ToolStats>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ToolStats {
    pub calls: u64,
    pub response_tokens: u64,
    pub saved_tokens: i64,
    pub avg_exec_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct HistorySummary {
    pub days: u64,
    pub total_calls: u64,
    pub total_request_tokens: u64,
    pub total_response_tokens: u64,
    pub total_saved_tokens: i64,
    pub sessions: u64,
    pub top_tools: Vec<ToolSavings>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSavings {
    pub tool: String,
    pub calls: u64,
    pub saved_tokens: i64,
    pub avg_savings_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DailyStats {
    pub date: String,
    pub calls: u64,
    pub response_tokens: u64,
    pub saved_tokens: i64,
}

pub struct AnalyticsStore {
    db: Mutex<Connection>,
    session_id: String,
    session_stats: Mutex<SessionStats>,
    retention_days: u64,
    /// In-flight asynchronous SQLite writes. The MCP layer fires `record`
    /// from a `spawn_blocking` task to keep the request hot path off the
    /// SQLite mutex; the idle watchdog needs to know when those tasks have
    /// drained before calling `std::process::exit(0)`, otherwise the last
    /// few tool calls of a session never make it to disk.
    pending_writes: Arc<AtomicUsize>,
}

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS tool_calls (
    id INTEGER PRIMARY KEY,
    timestamp TEXT NOT NULL,
    session_id TEXT NOT NULL,
    tool_name TEXT NOT NULL,
    command TEXT,
    request_tokens INTEGER NOT NULL,
    response_tokens INTEGER NOT NULL,
    saved_tokens INTEGER NOT NULL,
    savings_pct REAL NOT NULL,
    exec_time_ms INTEGER NOT NULL,
    was_redirect INTEGER NOT NULL DEFAULT 0,
    was_filtered INTEGER NOT NULL DEFAULT 0,
    read_dedup INTEGER NOT NULL DEFAULT 0,
    batch_size INTEGER NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_tc_timestamp ON tool_calls(timestamp);
CREATE INDEX IF NOT EXISTS idx_tc_tool ON tool_calls(tool_name);
CREATE INDEX IF NOT EXISTS idx_tc_session ON tool_calls(session_id);
";

impl AnalyticsStore {
    pub fn new(db_path: &Path, retention_days: u64) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create analytics dir: {e}"))?;
        }

        let conn = Connection::open(db_path)
            .map_err(|e| format!("open analytics db: {e}"))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("pragma: {e}"))?;

        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("schema migration: {e}"))?;

        let session_id = uuid::Uuid::new_v4().to_string();

        Ok(Self {
            db: Mutex::new(conn),
            session_id,
            session_stats: Mutex::new(SessionStats::default()),
            retention_days,
            pending_writes: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn open_readonly(db_path: &Path) -> Result<Self, String> {
        let conn = Connection::open_with_flags(
            db_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| format!("open analytics db (readonly): {e}"))?;

        Ok(Self {
            db: Mutex::new(conn),
            session_id: String::new(),
            session_stats: Mutex::new(SessionStats::default()),
            retention_days: 0,
            pending_writes: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub fn record(&self, rec: &ToolCallRecord) {
        // Update in-memory session stats
        {
            let mut stats = self.session_stats.lock().unwrap();
            stats.total_calls += 1;
            stats.total_request_tokens += rec.request_tokens;
            stats.total_response_tokens += rec.response_tokens;
            stats.total_saved_tokens += rec.saved_tokens;
            if rec.was_redirect {
                stats.redirect_hits += 1;
            }
            if rec.was_filtered {
                stats.filter_hits += 1;
            }
            if rec.read_dedup {
                stats.dedup_hits += 1;
            }
            if rec.batch_size > 1 {
                stats.batch_calls += 1;
            }

            let tool = stats
                .per_tool
                .entry(rec.tool_name.clone())
                .or_default();
            tool.calls += 1;
            tool.response_tokens += rec.response_tokens;
            tool.saved_tokens += rec.saved_tokens;
            let prev_total_ms = tool.avg_exec_ms * (tool.calls - 1);
            tool.avg_exec_ms = (prev_total_ms + rec.exec_time_ms) / tool.calls;
        }

        // Persist to SQLite
        let now = Utc::now().to_rfc3339();
        let cmd = rec.command.as_deref().map(anonymize_command);
        let db = self.db.lock().unwrap();

        let _ = db.execute(
            "INSERT INTO tool_calls (timestamp, session_id, tool_name, command,
             request_tokens, response_tokens, saved_tokens, savings_pct,
             exec_time_ms, was_redirect, was_filtered, read_dedup, batch_size)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                now,
                self.session_id,
                rec.tool_name,
                cmd,
                rec.request_tokens as i64,
                rec.response_tokens as i64,
                rec.saved_tokens,
                rec.savings_pct,
                rec.exec_time_ms as i64,
                rec.was_redirect as i32,
                rec.was_filtered as i32,
                rec.read_dedup as i32,
                rec.batch_size as i32,
            ],
        );

        // Auto-cleanup old records (probabilistic: ~1% of inserts)
        if self.retention_days > 0 {
            let should_cleanup: bool = {
                let stats = self.session_stats.lock().unwrap();
                stats.total_calls % 100 == 0
            };
            if should_cleanup {
                let cutoff = Utc::now()
                    - chrono::Duration::days(self.retention_days as i64);
                let _ = db.execute(
                    "DELETE FROM tool_calls WHERE timestamp < ?1",
                    params![cutoff.to_rfc3339()],
                );
            }
        }
    }

    pub fn session_summary(&self) -> SessionStats {
        self.session_stats.lock().unwrap().clone()
    }

    /// Spawn an asynchronous SQLite write for this record. Tracks the task
    /// in `pending_writes` so a subsequent `wait_until_quiet` can drain the
    /// queue before process exit. Use this from request paths instead of
    /// firing your own `spawn_blocking` — bare `spawn_blocking` calls can be
    /// dropped on `std::process::exit` and silently lose writes.
    pub fn record_async(self: &Arc<Self>, rec: ToolCallRecord) {
        self.pending_writes.fetch_add(1, Ordering::SeqCst);
        let me = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            me.record(&rec);
            me.pending_writes.fetch_sub(1, Ordering::SeqCst);
        });
    }

    /// Number of `record_async` tasks that have been spawned but have not
    /// yet completed their SQLite write. Useful as a shutdown gate.
    pub fn pending_writes(&self) -> usize {
        self.pending_writes.load(Ordering::SeqCst)
    }

    /// Block until every in-flight `record_async` task completes its SQLite
    /// write or `timeout` elapses. Returns true if the queue drained, false
    /// if the deadline hit first. Polls because the writes happen on the
    /// blocking pool — there's no future to await directly.
    pub async fn wait_until_quiet(&self, timeout: Duration) -> bool {
        if self.pending_writes() == 0 {
            return true;
        }
        let deadline = Instant::now() + timeout;
        let poll_interval = Duration::from_millis(20);
        loop {
            if self.pending_writes() == 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(poll_interval).await;
        }
    }

    pub fn history_summary(&self, days: u64) -> Result<HistorySummary, String> {
        let db = self.db.lock().unwrap();
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let (total_calls, total_req, total_resp, total_saved, sessions): (
            u64, u64, u64, i64, u64,
        ) = db
            .query_row(
                "SELECT COALESCE(COUNT(*), 0),
                        COALESCE(SUM(request_tokens), 0),
                        COALESCE(SUM(response_tokens), 0),
                        COALESCE(SUM(saved_tokens), 0),
                        COALESCE(COUNT(DISTINCT session_id), 0)
                 FROM tool_calls WHERE timestamp >= ?1",
                params![cutoff_str],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as u64,
                        row.get::<_, i64>(1)? as u64,
                        row.get::<_, i64>(2)? as u64,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)? as u64,
                    ))
                },
            )
            .map_err(|e| format!("history query: {e}"))?;

        let mut stmt = db
            .prepare(
                "SELECT tool_name, COUNT(*) as cnt,
                        SUM(saved_tokens) as saved,
                        AVG(savings_pct) as avg_pct
                 FROM tool_calls WHERE timestamp >= ?1
                 GROUP BY tool_name ORDER BY saved DESC LIMIT 10",
            )
            .map_err(|e| format!("tool breakdown: {e}"))?;

        let top_tools = stmt
            .query_map(params![cutoff_str], |row| {
                Ok(ToolSavings {
                    tool: row.get(0)?,
                    calls: row.get::<_, i64>(1)? as u64,
                    saved_tokens: row.get(2)?,
                    avg_savings_pct: row.get(3)?,
                })
            })
            .map_err(|e| format!("tool map: {e}"))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(HistorySummary {
            days,
            total_calls,
            total_request_tokens: total_req,
            total_response_tokens: total_resp,
            total_saved_tokens: total_saved,
            sessions,
            top_tools,
        })
    }

    pub fn daily_trend(&self, days: u64) -> Result<Vec<DailyStats>, String> {
        let db = self.db.lock().unwrap();
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let mut stmt = db
            .prepare(
                "SELECT DATE(timestamp) as day, COUNT(*),
                        SUM(response_tokens), SUM(saved_tokens)
                 FROM tool_calls WHERE timestamp >= ?1
                 GROUP BY day ORDER BY day",
            )
            .map_err(|e| format!("daily trend: {e}"))?;

        let rows = stmt
            .query_map(params![cutoff_str], |row| {
                Ok(DailyStats {
                    date: row.get(0)?,
                    calls: row.get::<_, i64>(1)? as u64,
                    response_tokens: row.get::<_, i64>(2)? as u64,
                    saved_tokens: row.get(3)?,
                })
            })
            .map_err(|e| format!("daily map: {e}"))?
            .filter_map(|r| r.ok())
            .collect();

        Ok(rows)
    }

    pub fn db_path(&self) -> Option<PathBuf> {
        let db = self.db.lock().unwrap();
        db.path().map(|p| PathBuf::from(p))
    }

    /// Format a CLI-friendly stats report.
    pub fn format_stats_report(&self, days: u64) -> String {
        let mut out = String::new();

        // Session stats
        let session = self.session_summary();
        if session.total_calls > 0 {
            out.push_str("=== Current Session ===\n");
            out.push_str(&format!(
                "  Calls: {}  Request tokens: {}  Response tokens: {}  Saved: {}\n",
                session.total_calls,
                session.total_request_tokens,
                session.total_response_tokens,
                session.total_saved_tokens
            ));
            out.push_str(&format!(
                "  Redirects (L1): {}  Filters (L2): {}  Dedup hits: {}\n\n",
                session.redirect_hits, session.filter_hits, session.dedup_hits
            ));
        }

        // History
        if let Ok(history) = self.history_summary(days) {
            out.push_str(&format!("=== Last {days} Days ===\n"));
            out.push_str(&format!(
                "  Sessions: {}  Total calls: {}  Tokens saved: {}\n",
                history.sessions, history.total_calls, history.total_saved_tokens
            ));

            if !history.top_tools.is_empty() {
                out.push_str("\n  Top tools by savings:\n");
                for t in &history.top_tools {
                    out.push_str(&format!(
                        "    {:<25} {:>6} calls  {:>+8} tokens  ({:.1}% avg)\n",
                        t.tool, t.calls, t.saved_tokens, t.avg_savings_pct
                    ));
                }
            }
        }

        // Daily trend
        if let Ok(daily) = self.daily_trend(7) {
            if !daily.is_empty() {
                out.push_str("\n  Last 7 days:\n");
                for d in &daily {
                    out.push_str(&format!(
                        "    {}  {:>5} calls  {:>8} resp tokens  {:>+8} saved\n",
                        d.date, d.calls, d.response_tokens, d.saved_tokens
                    ));
                }
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_store() -> (TempDir, AnalyticsStore) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("test_analytics.db");
        let store = AnalyticsStore::new(&db_path, 90).unwrap();
        (dir, store)
    }

    fn sample_record(tool: &str) -> ToolCallRecord {
        ToolCallRecord {
            tool_name: tool.to_string(),
            command: None,
            request_tokens: 100,
            response_tokens: 50,
            saved_tokens: 30,
            savings_pct: 37.5,
            exec_time_ms: 15,
            was_redirect: false,
            was_filtered: false,
            read_dedup: false,
            batch_size: 1,
        }
    }

    #[test]
    fn estimate_tokens_heuristic() {
        assert_eq!(estimate_tokens(0), 0);
        assert_eq!(estimate_tokens(4), 1);
        assert_eq!(estimate_tokens(5), 2);
        assert_eq!(estimate_tokens(100), 25);
        assert_eq!(estimate_tokens(3), 1);
    }

    #[test]
    fn anonymize_short_command() {
        assert_eq!(anonymize_command("cargo test"), "cargo test");
        assert_eq!(
            anonymize_command("cargo test --package foo --lib"),
            "cargo test --package"
        );
        assert_eq!(anonymize_command(""), "");
    }

    #[test]
    fn record_and_session_summary() {
        let (_dir, store) = test_store();

        store.record(&sample_record("read_file"));
        store.record(&sample_record("read_file"));
        store.record(&ToolCallRecord {
            tool_name: "exec".into(),
            command: Some("cargo test --lib".into()),
            request_tokens: 200,
            response_tokens: 80,
            saved_tokens: 120,
            savings_pct: 60.0,
            exec_time_ms: 50,
            was_redirect: true,
            was_filtered: false,
            read_dedup: false,
            batch_size: 1,
        });

        let stats = store.session_summary();
        assert_eq!(stats.total_calls, 3);
        assert_eq!(stats.total_request_tokens, 400);
        assert_eq!(stats.total_response_tokens, 180);
        assert_eq!(stats.total_saved_tokens, 180);
        assert_eq!(stats.redirect_hits, 1);
        assert_eq!(stats.per_tool.len(), 2);
        assert_eq!(stats.per_tool["read_file"].calls, 2);
        assert_eq!(stats.per_tool["exec"].saved_tokens, 120);
    }

    #[test]
    fn record_dedup_and_filter_flags() {
        let (_dir, store) = test_store();

        store.record(&ToolCallRecord {
            read_dedup: true,
            was_filtered: true,
            batch_size: 3,
            ..sample_record("read_file")
        });

        let stats = store.session_summary();
        assert_eq!(stats.dedup_hits, 1);
        assert_eq!(stats.filter_hits, 1);
        assert_eq!(stats.batch_calls, 1);
    }

    #[test]
    fn history_summary_returns_data() {
        let (_dir, store) = test_store();

        for _ in 0..5 {
            store.record(&sample_record("edit_file"));
        }
        store.record(&sample_record("search"));

        let history = store.history_summary(30).unwrap();
        assert_eq!(history.total_calls, 6);
        assert_eq!(history.sessions, 1);
        assert!(history.top_tools.len() <= 10);
        assert_eq!(history.top_tools[0].tool, "edit_file");
        assert_eq!(history.top_tools[0].calls, 5);
    }

    #[test]
    fn daily_trend_groups_by_date() {
        let (_dir, store) = test_store();

        store.record(&sample_record("read_file"));
        store.record(&sample_record("write_file"));

        let daily = store.daily_trend(7).unwrap();
        assert_eq!(daily.len(), 1); // all recorded today
        assert_eq!(daily[0].calls, 2);
    }

    #[test]
    fn empty_db_returns_zeros() {
        let (_dir, store) = test_store();

        let history = store.history_summary(30).unwrap();
        assert_eq!(history.total_calls, 0);
        assert_eq!(history.sessions, 0);

        let daily = store.daily_trend(7).unwrap();
        assert!(daily.is_empty());

        let session = store.session_summary();
        assert_eq!(session.total_calls, 0);
    }

    #[test]
    fn format_stats_report_not_empty() {
        let (_dir, store) = test_store();
        store.record(&sample_record("read_file"));

        let report = store.format_stats_report(30);
        assert!(report.contains("Current Session"));
        assert!(report.contains("Calls: 1"));
    }

    #[test]
    fn retention_cleanup_works() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("cleanup_test.db");
        let store = AnalyticsStore::new(&db_path, 90).unwrap();

        // Insert an old record directly
        {
            let db = store.db.lock().unwrap();
            db.execute(
                "INSERT INTO tool_calls (timestamp, session_id, tool_name,
                 request_tokens, response_tokens, saved_tokens, savings_pct,
                 exec_time_ms)
                 VALUES ('2020-01-01T00:00:00Z', 'old', 'old_tool', 10, 10, 0, 0.0, 1)",
                [],
            )
            .unwrap();
        }

        // Record 100 calls to trigger cleanup (runs at call % 100 == 0)
        for i in 0..100 {
            store.record(&sample_record(&format!("tool_{i}")));
        }

        let history = store.history_summary(36500).unwrap();
        // The old 2020 record should have been cleaned up
        let has_old: bool = {
            let db = store.db.lock().unwrap();
            db.query_row(
                "SELECT COUNT(*) FROM tool_calls WHERE session_id = 'old'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
                > 0
        };
        assert!(!has_old, "old records should be cleaned up");
        assert!(history.total_calls >= 100);
    }

    // --- Async-write drain (vikunja #248) ---
    //
    // Ensures the spawn_blocking-based `record_async` path tracks in-flight
    // writes so `wait_until_quiet` can be used as a shutdown gate. Without
    // this, the idle watchdog would `std::process::exit(0)` while the
    // blocking pool still held unsent INSERTs and the trailing tool calls
    // of a session would never reach disk.

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn pending_writes_starts_at_zero() {
        let (_dir, store) = test_store();
        assert_eq!(store.pending_writes(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn record_async_drains_to_disk_with_wait_until_quiet() {
        let (_dir, store) = test_store();
        let store = Arc::new(store);

        for i in 0..20 {
            store.record_async(sample_record(&format!("tool_{i}")));
        }

        let drained = store
            .wait_until_quiet(Duration::from_secs(5))
            .await;
        assert!(drained, "wait_until_quiet must succeed within budget");
        assert_eq!(
            store.pending_writes(),
            0,
            "no writes should remain pending after drain"
        );

        // All 20 records must have actually landed in SQLite.
        let history = store.history_summary(1).unwrap();
        assert_eq!(
            history.total_calls, 20,
            "wait_until_quiet must guarantee writes are durable"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wait_until_quiet_returns_immediately_when_idle() {
        let (_dir, store) = test_store();
        let start = Instant::now();
        let drained = store
            .wait_until_quiet(Duration::from_secs(1))
            .await;
        assert!(drained);
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "no-op drain must be effectively instant; took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wait_until_quiet_times_out_when_writes_stuck() {
        let (_dir, store) = test_store();
        let store = Arc::new(store);

        // Simulate a stuck write by directly bumping the pending counter
        // without ever producing a corresponding decrement. This isolates
        // the timeout logic from the SQLite path.
        store.pending_writes.fetch_add(1, Ordering::SeqCst);

        let start = Instant::now();
        let drained = store
            .wait_until_quiet(Duration::from_millis(100))
            .await;
        assert!(!drained, "must report failure when deadline elapses");
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "must respect the timeout budget"
        );
        assert_eq!(store.pending_writes(), 1, "pending count must be unchanged");

        // Cleanup so the test doesn't leak the bumped counter.
        store.pending_writes.fetch_sub(1, Ordering::SeqCst);
    }
}
