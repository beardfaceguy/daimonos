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

/// Compute `(saved_tokens, savings_pct)` from raw and filtered char counts.
///
/// Returns `(0, 0.0)` when `unfiltered_chars == 0` (no baseline available)
/// or when the filtered output is not smaller than the raw output.
pub fn compute_savings(unfiltered_chars: usize, response_chars: usize) -> (i64, f64) {
    if unfiltered_chars == 0 || response_chars >= unfiltered_chars {
        return (0, 0.0);
    }
    let raw_tokens = estimate_tokens(unfiltered_chars);
    let ret_tokens = estimate_tokens(response_chars);
    let saved = (raw_tokens - ret_tokens) as i64;
    let pct = saved as f64 / raw_tokens as f64 * 100.0;
    (saved, pct)
}

/// Read the `DAIMONOS_AGENT_SESSION_ID` environment variable if set to a
/// non-empty value. Used at MCP/socket startup so agents (e.g.
/// `claude --session-id $SID`) can pre-attach a runtime session
/// identifier that gets recorded with every analytics row.
pub fn read_agent_session_id_env() -> Option<String> {
    std::env::var("DAIMONOS_AGENT_SESSION_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Decoder for the row tuple shared between filtered and unfiltered
/// `history_summary` queries.
fn history_totals_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(u64, u64, u64, i64, u64)> {
    Ok((
        row.get::<_, i64>(0)? as u64,
        row.get::<_, i64>(1)? as u64,
        row.get::<_, i64>(2)? as u64,
        row.get::<_, i64>(3)?,
        row.get::<_, i64>(4)? as u64,
    ))
}

/// Build SQL for `history_summary_filtered`. Returns `(totals_sql,
/// breakdown_sql)`. When an external session id is supplied both queries
/// gain a second positional parameter (`?2`) for `external_session_id`.
fn build_filtered_history_sql(external_session_id: Option<&str>) -> (String, String) {
    let extra = if external_session_id.is_some() {
        " AND external_session_id = ?2"
    } else {
        ""
    };
    let totals = format!(
        "SELECT COALESCE(COUNT(*), 0),
                COALESCE(SUM(request_tokens), 0),
                COALESCE(SUM(response_tokens), 0),
                COALESCE(SUM(saved_tokens), 0),
                COALESCE(COUNT(DISTINCT session_id), 0)
         FROM tool_calls WHERE timestamp >= ?1{extra}"
    );
    let breakdown = format!(
        "SELECT tool_name, COUNT(*) as cnt,
                SUM(saved_tokens) as saved,
                AVG(savings_pct) as avg_pct
         FROM tool_calls WHERE timestamp >= ?1{extra}
         GROUP BY tool_name ORDER BY saved DESC LIMIT 10"
    );
    (totals, breakdown)
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
    /// Optional caller-supplied identifier for the agent-side runtime
    /// session (e.g. `claude --session-id <uuid>`). Persisted alongside
    /// each call so analytics can be correlated post-hoc with the
    /// agent's own usage logs (vikunja #43).
    pub external_session_id: Option<String>,
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

#[derive(Debug, Clone)]
pub struct AgentRunRecord {
    pub external_session_id: Option<String>,
    /// First 200 chars of the task (for reporting), never full task text.
    pub task_prefix: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
    pub stop_reason: String,
    pub turns: u32,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct AgentRunsSummary {
    pub total_runs: u64,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cache_read_tokens: u64,
    pub total_cache_write_tokens: u64,
    pub total_cost_usd: f64,
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
    batch_size INTEGER NOT NULL DEFAULT 1,
    external_session_id TEXT
);
CREATE INDEX IF NOT EXISTS idx_tc_timestamp ON tool_calls(timestamp);
CREATE INDEX IF NOT EXISTS idx_tc_tool ON tool_calls(tool_name);
CREATE INDEX IF NOT EXISTS idx_tc_session ON tool_calls(session_id);
CREATE TABLE IF NOT EXISTS agent_runs (
    id INTEGER PRIMARY KEY,
    timestamp TEXT NOT NULL,
    session_id TEXT NOT NULL,
    external_session_id TEXT,
    task_prefix TEXT NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_read_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    cost_usd REAL NOT NULL,
    stop_reason TEXT NOT NULL,
    turns INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_ar_timestamp ON agent_runs(timestamp);
CREATE INDEX IF NOT EXISTS idx_ar_session ON agent_runs(session_id);
";

/// Idempotently bring an existing analytics DB up to the current schema.
/// `CREATE TABLE IF NOT EXISTS` won't add columns to a pre-existing table,
/// so columns added after the initial release ship as ALTER TABLEs here.
/// Any "duplicate column name" error is treated as success (column already
/// exists) — every other SQLite error is surfaced.
fn migrate_schema(conn: &Connection) -> Result<(), String> {
    let migrations = [
        "ALTER TABLE tool_calls ADD COLUMN external_session_id TEXT",
        "CREATE INDEX IF NOT EXISTS idx_tc_external_session ON tool_calls(external_session_id)",
    ];
    for sql in migrations {
        if let Err(e) = conn.execute_batch(sql) {
            let msg = e.to_string();
            // SQLite returns "duplicate column name: …" when the column
            // is already present from a prior migration / fresh CREATE.
            if !msg.contains("duplicate column name") {
                return Err(format!("schema migration ({sql}): {msg}"));
            }
        }
    }
    Ok(())
}

impl AnalyticsStore {
    pub fn new(db_path: &Path, retention_days: u64) -> Result<Self, String> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create analytics dir: {e}"))?;
        }

        let conn = Connection::open(db_path).map_err(|e| format!("open analytics db: {e}"))?;

        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(|e| format!("pragma: {e}"))?;

        conn.execute_batch(SCHEMA_SQL)
            .map_err(|e| format!("schema migration: {e}"))?;

        migrate_schema(&conn)?;

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

    /// Acquire the in-memory stats mutex, recovering past poison rather
    /// than panicking. A poisoned mutex doesn't corrupt our data — the
    /// fields are plain numeric counters and HashMaps that survive a
    /// panic mid-update — so `into_inner()` returns the still-valid
    /// guard. See vikunja #254.
    fn stats_lock(&self) -> std::sync::MutexGuard<'_, SessionStats> {
        self.session_stats.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Same poison-tolerant pattern for the SQLite connection mutex.
    /// SQLite itself is durable on disk and the in-memory `Connection`
    /// state isn't sensitive to mid-statement poisoning (statements run
    /// to completion or surface a `Result` error to our code).
    fn db_lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn record(&self, rec: &ToolCallRecord) {
        // Update in-memory session stats
        {
            let mut stats = self.stats_lock();
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

            let tool = stats.per_tool.entry(rec.tool_name.clone()).or_default();
            tool.calls += 1;
            tool.response_tokens += rec.response_tokens;
            tool.saved_tokens += rec.saved_tokens;
            let prev_total_ms = tool.avg_exec_ms * (tool.calls - 1);
            tool.avg_exec_ms = (prev_total_ms + rec.exec_time_ms) / tool.calls;
        }

        // Persist to SQLite
        let now = Utc::now().to_rfc3339();
        let cmd = rec.command.as_deref().map(anonymize_command);
        let db = self.db_lock();

        let _ = db.execute(
            "INSERT INTO tool_calls (timestamp, session_id, tool_name, command,
             request_tokens, response_tokens, saved_tokens, savings_pct,
             exec_time_ms, was_redirect, was_filtered, read_dedup, batch_size,
             external_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
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
                rec.external_session_id.as_deref(),
            ],
        );

        // Auto-cleanup old records (probabilistic: ~1% of inserts)
        if self.retention_days > 0 {
            let should_cleanup: bool = {
                let stats = self.stats_lock();
                stats.total_calls.is_multiple_of(100)
            };
            if should_cleanup {
                let cutoff = Utc::now() - chrono::Duration::days(self.retention_days as i64);
                let _ = db.execute(
                    "DELETE FROM tool_calls WHERE timestamp < ?1",
                    params![cutoff.to_rfc3339()],
                );
            }
        }
    }

    pub fn session_summary(&self) -> SessionStats {
        self.stats_lock().clone()
    }

    /// Persist actual LLM usage from one completed agent session.
    /// Field names mirror the neutral `Usage` type — no provider vocabulary.
    pub fn record_agent_run(&self, rec: &AgentRunRecord) {
        let now = Utc::now().to_rfc3339();
        let db = self.db_lock();
        let _ = db.execute(
            "INSERT INTO agent_runs (timestamp, session_id, external_session_id,
             task_prefix, input_tokens, output_tokens, cache_read_tokens,
             cache_write_tokens, cost_usd, stop_reason, turns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                now,
                self.session_id,
                rec.external_session_id.as_deref(),
                rec.task_prefix,
                rec.input_tokens as i64,
                rec.output_tokens as i64,
                rec.cache_read_tokens as i64,
                rec.cache_write_tokens as i64,
                rec.cost_usd,
                rec.stop_reason,
                rec.turns as i32,
            ],
        );
    }

    pub fn agent_runs_summary(&self, days: u64) -> Result<AgentRunsSummary, String> {
        let db = self.db_lock();
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        let cutoff_str = cutoff.to_rfc3339();
        db.query_row(
            "SELECT COALESCE(COUNT(*), 0),
                    COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    COALESCE(SUM(cost_usd), 0.0)
             FROM agent_runs WHERE timestamp >= ?1",
            params![cutoff_str],
            |row| {
                Ok(AgentRunsSummary {
                    total_runs: row.get::<_, i64>(0)? as u64,
                    total_input_tokens: row.get::<_, i64>(1)? as u64,
                    total_output_tokens: row.get::<_, i64>(2)? as u64,
                    total_cache_read_tokens: row.get::<_, i64>(3)? as u64,
                    total_cache_write_tokens: row.get::<_, i64>(4)? as u64,
                    total_cost_usd: row.get::<_, f64>(5)?,
                })
            },
        )
        .map_err(|e| format!("agent_runs_summary: {e}"))
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
        self.history_summary_filtered(days, None)
    }

    /// Same as `history_summary` but optionally restricted to a single
    /// agent-runtime session via `external_session_id`. Useful for
    /// post-hoc correlation with claude/cursor session logs.
    pub fn history_summary_filtered(
        &self,
        days: u64,
        external_session_id: Option<&str>,
    ) -> Result<HistorySummary, String> {
        let db = self.db_lock();
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let (totals_sql, breakdown_sql) = build_filtered_history_sql(external_session_id);

        let (total_calls, total_req, total_resp, total_saved, sessions): (u64, u64, u64, i64, u64) =
            if let Some(ext) = external_session_id {
                db.query_row(&totals_sql, params![cutoff_str, ext], history_totals_row)
            } else {
                db.query_row(&totals_sql, params![cutoff_str], history_totals_row)
            }
            .map_err(|e| format!("history query: {e}"))?;

        let mut stmt = db
            .prepare(&breakdown_sql)
            .map_err(|e| format!("tool breakdown: {e}"))?;

        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<ToolSavings> {
            Ok(ToolSavings {
                tool: row.get(0)?,
                calls: row.get::<_, i64>(1)? as u64,
                saved_tokens: row.get(2)?,
                avg_savings_pct: row.get(3)?,
            })
        };

        let top_tools: Vec<ToolSavings> = if let Some(ext) = external_session_id {
            stmt.query_map(params![cutoff_str, ext], map_row)
        } else {
            stmt.query_map(params![cutoff_str], map_row)
        }
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
        self.daily_trend_filtered(days, None)
    }

    pub fn daily_trend_filtered(
        &self,
        days: u64,
        external_session_id: Option<&str>,
    ) -> Result<Vec<DailyStats>, String> {
        let db = self.db_lock();
        let cutoff = Utc::now() - chrono::Duration::days(days as i64);
        let cutoff_str = cutoff.to_rfc3339();

        let sql = if external_session_id.is_some() {
            "SELECT DATE(timestamp) as day, COUNT(*),
                    SUM(response_tokens), SUM(saved_tokens)
             FROM tool_calls WHERE timestamp >= ?1 AND external_session_id = ?2
             GROUP BY day ORDER BY day"
        } else {
            "SELECT DATE(timestamp) as day, COUNT(*),
                    SUM(response_tokens), SUM(saved_tokens)
             FROM tool_calls WHERE timestamp >= ?1
             GROUP BY day ORDER BY day"
        };

        let mut stmt = db.prepare(sql).map_err(|e| format!("daily trend: {e}"))?;

        let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<DailyStats> {
            Ok(DailyStats {
                date: row.get(0)?,
                calls: row.get::<_, i64>(1)? as u64,
                response_tokens: row.get::<_, i64>(2)? as u64,
                saved_tokens: row.get(3)?,
            })
        };

        let rows = if let Some(ext) = external_session_id {
            stmt.query_map(params![cutoff_str, ext], map_row)
        } else {
            stmt.query_map(params![cutoff_str], map_row)
        }
        .map_err(|e| format!("daily map: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

        Ok(rows)
    }

    pub fn db_path(&self) -> Option<PathBuf> {
        let db = self.db_lock();
        db.path().map(PathBuf::from)
    }

    /// Format a CLI-friendly stats report. Pass an `external_session_id`
    /// to restrict the history/daily blocks to a single agent-runtime
    /// session id; pass `None` for an unfiltered report. The
    /// current-session block is hidden when filtering, since the
    /// in-memory session stats aren't keyed by external session id —
    /// use the filtered history block below instead.
    pub fn format_stats_report_filtered(
        &self,
        days: u64,
        external_session_id: Option<&str>,
    ) -> String {
        let mut out = String::new();

        if let Some(ext) = external_session_id {
            out.push_str(&format!("=== Filter ===\n  external_session_id: {ext}\n\n"));
        } else {
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
        }

        if let Ok(history) = self.history_summary_filtered(days, external_session_id) {
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

        if let Ok(daily) = self.daily_trend_filtered(7, external_session_id) {
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

        if let Ok(ar) = self.agent_runs_summary(days) {
            if ar.total_runs > 0 {
                out.push_str(&format!("\n=== Agent Runs (last {days} days) ===\n"));
                out.push_str(&format!(
                    "  Runs: {}  Input: {}  Output: {}  Cache read: {}  Cache write: {}\n",
                    ar.total_runs,
                    ar.total_input_tokens,
                    ar.total_output_tokens,
                    ar.total_cache_read_tokens,
                    ar.total_cache_write_tokens,
                ));
                let denominator = ar.total_input_tokens + ar.total_cache_read_tokens;
                let hit_rate = if denominator > 0 {
                    ar.total_cache_read_tokens as f64 / denominator as f64 * 100.0
                } else {
                    0.0
                };
                out.push_str(&format!(
                    "  Cache hit rate: {:.1}%  Total cost: ${:.4}\n",
                    hit_rate, ar.total_cost_usd
                ));
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // --- compute_savings ---

    #[test]
    fn compute_savings_zero_when_no_baseline() {
        let (saved, pct) = compute_savings(0, 100);
        assert_eq!(saved, 0);
        assert_eq!(pct, 0.0);
    }

    #[test]
    fn compute_savings_zero_when_not_smaller() {
        let (saved, pct) = compute_savings(100, 100);
        assert_eq!(saved, 0);
        assert_eq!(pct, 0.0);
    }

    #[test]
    fn compute_savings_positive_when_compressed() {
        // 4000 raw chars ≈ 1000 tokens; 40 returned chars ≈ 10 tokens → 990 saved
        let (saved, pct) = compute_savings(4000, 40);
        assert!(saved > 0, "saved_tokens must be positive");
        assert!(pct > 0.0 && pct <= 100.0, "savings_pct must be in (0, 100]");
    }

    #[test]
    fn compute_savings_pct_matches_token_ratio() {
        // 800 raw chars = 200 tokens; 400 returned chars = 100 tokens → 50% saved
        let (saved, pct) = compute_savings(800, 400);
        assert_eq!(saved, 100);
        assert!((pct - 50.0).abs() < 0.01, "expected ~50%, got {pct}");
    }

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
            external_session_id: None,
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
            external_session_id: None,
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

    // --- Mutex poison recovery (vikunja #254) ---
    //
    // `Mutex::lock().unwrap()` panics for the rest of the process's life
    // once any holder of the same mutex has panicked. Because `record()`
    // is called from every MCP request handler, a single bug elsewhere
    // would poison the analytics mutexes and turn all subsequent tool
    // calls into 500s. These tests deliberately poison each mutex and
    // assert the next analytics call still completes.

    /// Poison a mutex via the canonical recipe: lock it on a separate
    /// thread, then panic while the guard is held. `thread::join()`
    /// swallows the panic so the test process survives.
    fn poison_via_thread<F: FnOnce() + Send + 'static>(f: F) {
        let _ = std::thread::spawn(f).join();
    }

    #[test]
    fn record_recovers_from_poisoned_stats_mutex() {
        let (_dir, store) = test_store();
        let store = Arc::new(store);
        let cloned = Arc::clone(&store);
        poison_via_thread(move || {
            let _g = cloned.session_stats.lock();
            panic!("intentional poison");
        });
        assert!(
            store.session_stats.is_poisoned(),
            "test setup: session_stats must be poisoned"
        );

        // The fix: record() must not panic on the poisoned mutex.
        store.record(&sample_record("read_file"));
        let summary = store.session_summary();
        assert_eq!(summary.total_calls, 1, "record must succeed past poison");
    }

    #[test]
    fn record_recovers_from_poisoned_db_mutex() {
        let (_dir, store) = test_store();
        let store = Arc::new(store);
        let cloned = Arc::clone(&store);
        poison_via_thread(move || {
            let _g = cloned.db.lock();
            panic!("intentional poison");
        });
        assert!(store.db.is_poisoned(), "test setup: db must be poisoned");

        // The fix: record() must not panic on the poisoned db mutex.
        store.record(&sample_record("read_file"));
        let summary = store.session_summary();
        assert_eq!(
            summary.total_calls, 1,
            "record must update in-memory stats even when db mutex is poisoned"
        );

        // The SQLite INSERT happens after the stats update, with its
        // Result discarded by `record()`. The in-memory assertion above
        // alone would pass even if poison recovery on `db_lock()` had
        // failed and the row was lost. Verify the row actually landed by
        // round-tripping through `history_summary` (which itself must
        // also recover from the still-poisoned mutex).
        let history = store
            .history_summary(1)
            .expect("history_summary must not panic on poisoned mutex");
        assert_eq!(
            history.total_calls, 1,
            "INSERT must have landed in SQLite after poison recovery"
        );
    }

    #[test]
    fn history_summary_recovers_from_poisoned_db_mutex() {
        let (_dir, store) = test_store();
        store.record(&sample_record("read_file"));
        let store = Arc::new(store);
        let cloned = Arc::clone(&store);
        poison_via_thread(move || {
            let _g = cloned.db.lock();
            panic!("intentional poison");
        });
        assert!(store.db.is_poisoned());

        // history_summary must complete (no panic) and report the
        // previously-recorded data.
        let history = store
            .history_summary(30)
            .expect("history_summary must not panic on poisoned mutex");
        assert!(history.total_calls >= 1);
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

        let report = store.format_stats_report_filtered(30, None);
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

        let drained = store.wait_until_quiet(Duration::from_secs(5)).await;
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
        let drained = store.wait_until_quiet(Duration::from_secs(1)).await;
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
        let drained = store.wait_until_quiet(Duration::from_millis(100)).await;
        assert!(!drained, "must report failure when deadline elapses");
        assert!(
            start.elapsed() >= Duration::from_millis(100),
            "must respect the timeout budget"
        );
        assert_eq!(store.pending_writes(), 1, "pending count must be unchanged");

        // Cleanup so the test doesn't leak the bumped counter.
        store.pending_writes.fetch_sub(1, Ordering::SeqCst);
    }

    // --- external_session_id correlation (vikunja #43) ---
    //
    // Threads an agent-runtime session id through every analytics row and
    // exposes it as a query filter so post-hoc correlation with
    // claude/cursor session logs is a single SQL `WHERE` clause.

    fn record_with_external(tool: &str, ext: Option<&str>) -> ToolCallRecord {
        ToolCallRecord {
            external_session_id: ext.map(String::from),
            ..sample_record(tool)
        }
    }

    #[test]
    fn record_persists_external_session_id_column() {
        let (_dir, store) = test_store();
        store.record(&record_with_external("read_file", Some("agent-sid-A")));
        store.record(&record_with_external("write_file", None));

        let db = store.db_lock();
        let rows: Vec<(String, Option<String>)> = db
            .prepare("SELECT tool_name, external_session_id FROM tool_calls ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "read_file");
        assert_eq!(rows[0].1.as_deref(), Some("agent-sid-A"));
        assert_eq!(rows[1].0, "write_file");
        assert_eq!(rows[1].1, None);
    }

    #[test]
    fn history_summary_filtered_isolates_one_external_session() {
        let (_dir, store) = test_store();
        for _ in 0..3 {
            store.record(&record_with_external("read_file", Some("sid-A")));
        }
        for _ in 0..5 {
            store.record(&record_with_external("read_file", Some("sid-B")));
        }
        store.record(&record_with_external("read_file", None));

        let unfiltered = store.history_summary(30).unwrap();
        assert_eq!(unfiltered.total_calls, 9);

        let only_a = store.history_summary_filtered(30, Some("sid-A")).unwrap();
        assert_eq!(only_a.total_calls, 3);

        let only_b = store.history_summary_filtered(30, Some("sid-B")).unwrap();
        assert_eq!(only_b.total_calls, 5);

        let nonexistent = store
            .history_summary_filtered(30, Some("does-not-exist"))
            .unwrap();
        assert_eq!(nonexistent.total_calls, 0);
    }

    #[test]
    fn daily_trend_filtered_isolates_one_external_session() {
        let (_dir, store) = test_store();
        store.record(&record_with_external("read_file", Some("sid-A")));
        store.record(&record_with_external("read_file", Some("sid-A")));
        store.record(&record_with_external("read_file", Some("sid-B")));

        let only_a = store.daily_trend_filtered(7, Some("sid-A")).unwrap();
        assert_eq!(only_a.len(), 1);
        assert_eq!(only_a[0].calls, 2);

        let only_b = store.daily_trend_filtered(7, Some("sid-B")).unwrap();
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].calls, 1);
    }

    #[test]
    fn format_stats_report_filtered_includes_filter_banner() {
        let (_dir, store) = test_store();
        store.record(&record_with_external("read_file", Some("sid-A")));
        store.record(&record_with_external("read_file", Some("sid-B")));

        let filtered = store.format_stats_report_filtered(30, Some("sid-A"));
        assert!(filtered.contains("external_session_id: sid-A"));
        // Filtered reports skip the in-memory "Current Session" block,
        // since session stats aren't keyed by external session id.
        assert!(!filtered.contains("Current Session"));

        let unfiltered = store.format_stats_report_filtered(30, None);
        assert!(unfiltered.contains("Current Session"));
        assert!(!unfiltered.contains("external_session_id:"));
    }

    /// Schema migration must be idempotent: a DB created with the current
    /// `SCHEMA_SQL` will still have `migrate_schema` run on every open,
    /// and the duplicate `ADD COLUMN` must not fail. (Older DBs created
    /// before the column existed get the column added on first open.)
    #[test]
    fn schema_migration_idempotent_on_fresh_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("idempotent.db");

        // Open once — runs SCHEMA_SQL (which already has the column) plus
        // migrate_schema (which tries to ADD COLUMN again and must
        // tolerate the "duplicate column" error).
        let _store = AnalyticsStore::new(&db_path, 90).unwrap();
        // Re-open — confirms the migration is safe to run a second time
        // against an already-current DB.
        let _store = AnalyticsStore::new(&db_path, 90).unwrap();
    }

    /// Older DBs (pre-vikunja-#43) won't have the column. Simulate by
    /// creating a table with the historical schema and confirm
    /// `AnalyticsStore::new` brings it forward without data loss.
    #[test]
    fn schema_migration_adds_column_to_legacy_db() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("legacy.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tool_calls (
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
                );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tool_calls (timestamp, session_id, tool_name,
                 request_tokens, response_tokens, saved_tokens, savings_pct,
                 exec_time_ms)
                 VALUES ('2024-01-01T00:00:00Z', 'old', 'read_file', 10, 5, 0, 0.0, 1)",
                [],
            )
            .unwrap();
        }

        let store = AnalyticsStore::new(&db_path, 90).unwrap();
        store.record(&record_with_external("write_file", Some("sid-new")));

        let history = store.history_summary(36500).unwrap();
        assert!(
            history.total_calls >= 2,
            "legacy row + new row must coexist after migration"
        );

        let only_new = store
            .history_summary_filtered(36500, Some("sid-new"))
            .unwrap();
        assert_eq!(only_new.total_calls, 1);
    }

    // --- agent_runs table (#877) ---

    fn sample_agent_run() -> AgentRunRecord {
        AgentRunRecord {
            external_session_id: None,
            task_prefix: "test task".into(),
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_write_tokens: 100,
            cost_usd: 0.0125,
            stop_reason: "end_turn".into(),
            turns: 3,
        }
    }

    #[test]
    fn record_agent_run_persists_to_db() {
        let (_dir, store) = test_store();
        store.record_agent_run(&sample_agent_run());

        let db = store.db_lock();
        let count: i64 = db
            .query_row("SELECT COUNT(*) FROM agent_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn record_agent_run_stores_all_token_fields() {
        let (_dir, store) = test_store();
        store.record_agent_run(&AgentRunRecord {
            input_tokens: 1111,
            output_tokens: 2222,
            cache_read_tokens: 333,
            cache_write_tokens: 44,
            ..sample_agent_run()
        });

        let db = store.db_lock();
        let (inp, out, cr, cw): (i64, i64, i64, i64) = db
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
                 FROM agent_runs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(inp, 1111);
        assert_eq!(out, 2222);
        assert_eq!(cr, 333);
        assert_eq!(cw, 44);
    }

    #[test]
    fn record_agent_run_stores_cost_and_stop_reason() {
        let (_dir, store) = test_store();
        store.record_agent_run(&AgentRunRecord {
            cost_usd: 0.5678,
            stop_reason: "max_tokens".into(),
            ..sample_agent_run()
        });

        let db = store.db_lock();
        let (cost, reason): (f64, String) = db
            .query_row(
                "SELECT cost_usd, stop_reason FROM agent_runs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!((cost - 0.5678).abs() < 1e-9);
        assert_eq!(reason, "max_tokens");
    }

    #[test]
    fn record_agent_run_stores_external_session_id() {
        let (_dir, store) = test_store();
        store.record_agent_run(&AgentRunRecord {
            external_session_id: Some("ext-session-xyz".into()),
            ..sample_agent_run()
        });

        let db = store.db_lock();
        let ext: Option<String> = db
            .query_row("SELECT external_session_id FROM agent_runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ext.as_deref(), Some("ext-session-xyz"));
    }

    #[test]
    fn agent_runs_summary_aggregates_correctly() {
        let (_dir, store) = test_store();
        store.record_agent_run(&AgentRunRecord {
            input_tokens: 1000,
            output_tokens: 500,
            cache_read_tokens: 200,
            cache_write_tokens: 100,
            cost_usd: 0.01,
            ..sample_agent_run()
        });
        store.record_agent_run(&AgentRunRecord {
            input_tokens: 2000,
            output_tokens: 1000,
            cache_read_tokens: 400,
            cache_write_tokens: 200,
            cost_usd: 0.02,
            ..sample_agent_run()
        });

        let summary = store.agent_runs_summary(30).unwrap();
        assert_eq!(summary.total_runs, 2);
        assert_eq!(summary.total_input_tokens, 3000);
        assert_eq!(summary.total_output_tokens, 1500);
        assert_eq!(summary.total_cache_read_tokens, 600);
        assert_eq!(summary.total_cache_write_tokens, 300);
        assert!((summary.total_cost_usd - 0.03).abs() < 1e-9);
    }

    #[test]
    fn agent_runs_summary_empty_returns_zeros() {
        let (_dir, store) = test_store();
        let summary = store.agent_runs_summary(30).unwrap();
        assert_eq!(summary.total_runs, 0);
        assert_eq!(summary.total_cost_usd, 0.0);
    }

    #[test]
    fn stats_report_includes_agent_runs_section() {
        let (_dir, store) = test_store();
        store.record_agent_run(&sample_agent_run());

        let report = store.format_stats_report_filtered(30, None);
        assert!(report.contains("Agent Runs"), "report must include Agent Runs section: {report}");
        assert!(report.contains("Runs: 1"));
    }

    #[test]
    fn stats_report_shows_cache_hit_rate() {
        let (_dir, store) = test_store();
        store.record_agent_run(&AgentRunRecord {
            input_tokens: 800,
            cache_read_tokens: 200,
            ..sample_agent_run()
        });

        let report = store.format_stats_report_filtered(30, None);
        // cache hit rate = 200 / (800 + 200) * 100 = 20.0%
        assert!(report.contains("20.0%"), "cache hit rate must appear in report: {report}");
    }

    #[test]
    fn stats_report_hides_agent_runs_section_when_no_runs() {
        let (_dir, store) = test_store();
        store.record(&sample_record("read_file"));

        let report = store.format_stats_report_filtered(30, None);
        assert!(!report.contains("Agent Runs"), "agent runs section must be hidden when no runs: {report}");
    }

    #[test]
    fn agent_runs_table_survives_reopen() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("reopen_test.db");

        {
            let store = AnalyticsStore::new(&db_path, 90).unwrap();
            store.record_agent_run(&sample_agent_run());
        }

        let store2 = AnalyticsStore::new(&db_path, 90).unwrap();
        let summary = store2.agent_runs_summary(30).unwrap();
        assert_eq!(summary.total_runs, 1, "agent run must survive DB reopen");
    }

    #[test]
    fn read_agent_session_id_env_returns_none_for_empty() {
        // Use a uniquely named test var so we don't fight the global
        // env namespace with concurrent tests. We're not exercising the
        // real `DAIMONOS_AGENT_SESSION_ID` here — that's covered by the
        // pytest layer where each subprocess has an isolated environment.
        // This unit test just pins the empty/whitespace handling.
        std::env::remove_var("DAIMONOS_AGENT_SESSION_ID");
        assert!(read_agent_session_id_env().is_none());

        std::env::set_var("DAIMONOS_AGENT_SESSION_ID", "");
        assert!(
            read_agent_session_id_env().is_none(),
            "empty string must be treated as unset"
        );

        std::env::set_var("DAIMONOS_AGENT_SESSION_ID", "   ");
        assert!(
            read_agent_session_id_env().is_none(),
            "whitespace-only must be treated as unset"
        );

        std::env::set_var("DAIMONOS_AGENT_SESSION_ID", "  abc-123  ");
        assert_eq!(
            read_agent_session_id_env().as_deref(),
            Some("abc-123"),
            "value must be trimmed"
        );

        std::env::remove_var("DAIMONOS_AGENT_SESSION_ID");
    }
}
