//! SQLite side-store for the KGL graph (v0). The graph is external and keyed by
//! content hash, so it survives the substrate being swapped or churning. Reuses
//! daimonos's bundled `rusqlite`.
//!
//! `populate` does a full re-scan (no incremental maintenance — that is the
//! deferred Differential-Dataflow problem; see research notes). Agent-`Declared`
//! metadata (intent/provenance) and edges are preserved across re-population for
//! as long as the def's content hash survives; when a def changes, its hash
//! changes, the old node is pruned, and the agent re-attaches to the new hash.
//!
//! Assumes one workspace/root per store (per-workspace `.kgl/kgl.db`).

use crate::config::KglConfig;
use crate::kgl::model::{
    DefNode, Derivation, Edge, EdgeKind, Intent, NodeKind, Provenance, SubstrateKind,
};
use crate::kgl::substrate::Substrate;
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::Duration;

/// Edge-traversal direction for `neighbors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

/// A node plus its non-hashed metadata, as returned by queries.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub node: DefNode,
    pub intent: Option<Intent>,
    pub provenance: Option<Provenance>,
    pub touches_io: bool,
    pub mutates_state: bool,
}

/// A KGL-completeness violation (see [`KglStore::check_completeness`]).
#[derive(Debug, Clone)]
pub struct Violation {
    pub hash: String,
    pub name: Option<String>,
    pub reason: String,
}

pub struct KglStore {
    conn: Connection,
}

impl KglStore {
    /// Open with the default KGL busy-timeout. Prefer [`Self::open_with`] (or
    /// [`Self::open_workspace_with`]) on the hot paths so an operator-tuned
    /// `[kgl] busy_timeout_ms` is honored; this default-valued variant exists
    /// for tests and call sites without config in hand.
    #[cfg(test)]
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, KglConfig::default().busy_timeout_ms)
    }

    /// Open (creating parent dirs if needed) with an explicit SQLite
    /// busy-timeout. Enables WAL journaling so the autoindex watcher's writer
    /// and the query/assert readers can proceed concurrently; the busy timeout
    /// makes a contended connection wait briefly instead of erroring
    /// `SQLITE_BUSY`.
    pub fn open_with(path: &Path, busy_timeout_ms: u64) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).context("open kgl sqlite store")?;
        // WAL is a no-op for some VFS/back-ends; best-effort, never fatal.
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        conn.busy_timeout(Duration::from_millis(busy_timeout_ms))?;
        let s = Self { conn };
        s.init()?;
        Ok(s)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        // No WAL for `:memory:`; the busy timeout still applies if a future
        // shared-cache memory DB is used.
        conn.busy_timeout(Duration::from_millis(KglConfig::default().busy_timeout_ms))?;
        let s = Self { conn };
        s.init()?;
        Ok(s)
    }

    /// Open (creating if needed) the KGL store for a workspace at the canonical
    /// `<workspace>/.kgl/kgl.db`, using the default busy-timeout. Test-only
    /// convenience; production paths thread `[kgl]` config via
    /// [`Self::open_workspace_with`].
    #[cfg(test)]
    pub fn open_workspace(workspace: &Path) -> Result<Self> {
        Self::open_workspace_with(workspace, &KglConfig::default())
    }

    /// Open the workspace store honoring an operator-supplied `[kgl]` config.
    /// Single source of truth for the store path.
    pub fn open_workspace_with(workspace: &Path, cfg: &KglConfig) -> Result<Self> {
        Self::open_with(&workspace.join(".kgl").join("kgl.db"), cfg.busy_timeout_ms)
    }

    fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS kgl_node (
                hash TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                name TEXT,
                substrate TEXT NOT NULL,
                file TEXT,
                span TEXT,
                intent_json TEXT,
                provenance_json TEXT,
                touches_io INTEGER NOT NULL DEFAULT 0,
                mutates_state INTEGER NOT NULL DEFAULT 0,
                valid_as_of TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS kgl_edge (
                id INTEGER PRIMARY KEY,
                from_hash TEXT NOT NULL,
                to_ref TEXT NOT NULL,
                kind TEXT NOT NULL,
                derivation TEXT NOT NULL,
                confidence REAL NOT NULL,
                provenance_json TEXT
            );
            CREATE INDEX IF NOT EXISTS kgl_edge_from ON kgl_edge(from_hash, kind);
            CREATE INDEX IF NOT EXISTS kgl_edge_to ON kgl_edge(to_ref, kind);",
        )?;
        Ok(())
    }

    /// Full re-scan of `root` via `substrate`, written into the store.
    /// `run_stamp` marks nodes seen this run; nodes from prior runs (orphans)
    /// are pruned. Returns (node_count, edge_count) after the run.
    pub fn populate(
        &mut self,
        substrate: &dyn Substrate,
        root: &Path,
        run_stamp: &str,
    ) -> Result<(usize, usize)> {
        let idx = substrate.index(root)?;
        // The substrate being (re)scanned this run. The orphan prune below is
        // scoped to this substrate so re-indexing one substrate can never wipe
        // another's nodes (e.g. an x07 `index` on a graphify workspace) — and
        // observed `daimonos` nodes are likewise untouched.
        let active = enum_str(&substrate.kind());

        // Name -> hash maps for resolving structural-edge URNs to concrete hashes.
        let mut fn_by_name: HashMap<String, String> = HashMap::new();
        let mut mod_by_name: HashMap<String, String> = HashMap::new();
        for n in &idx.nodes {
            if let Some(name) = &n.name {
                match n.kind {
                    NodeKind::Function => {
                        fn_by_name.insert(name.clone(), n.hash.clone());
                    }
                    NodeKind::Module => {
                        mod_by_name.insert(name.clone(), n.hash.clone());
                    }
                    _ => {}
                }
            }
        }

        let tx = self.conn.transaction()?;

        // Recompute ONLY this substrate's structural edges. An edge's substrate
        // is that of its `from_hash` node, so scope the delete to nodes of the
        // active substrate. This preserves declared/observed edges AND other
        // substrates' derived/inferred edges — re-indexing x07 must not strip
        // graphify's structural edges (or vice-versa).
        tx.execute(
            "DELETE FROM kgl_edge
             WHERE derivation IN ('derived','inferred')
               AND from_hash IN (SELECT hash FROM kgl_node WHERE substrate = ?1)",
            params![active],
        )?;

        // Upsert nodes. intent_json/provenance_json are intentionally NOT in the
        // INSERT, so existing agent-authored metadata is preserved on conflict.
        for n in &idx.nodes {
            let eff = idx.effects.get(&n.hash).copied().unwrap_or_default();
            tx.execute(
                "INSERT INTO kgl_node
                    (hash,kind,name,substrate,file,span,touches_io,mutates_state,valid_as_of)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
                 ON CONFLICT(hash) DO UPDATE SET
                    kind=excluded.kind, name=excluded.name, substrate=excluded.substrate,
                    file=excluded.file, span=excluded.span,
                    touches_io=excluded.touches_io, mutates_state=excluded.mutates_state,
                    valid_as_of=excluded.valid_as_of",
                params![
                    n.hash,
                    enum_str(&n.kind),
                    n.name,
                    enum_str(&n.substrate),
                    n.file,
                    n.span,
                    eff.touches_io as i64,
                    eff.mutates_state as i64,
                    run_stamp,
                ],
            )?;
        }

        // Structural edges, with URN targets resolved to concrete hashes.
        for e in &idx.edges {
            let to = resolve_urn(&e.to, &fn_by_name, &mod_by_name);
            tx.execute(
                "INSERT INTO kgl_edge (from_hash,to_ref,kind,derivation,confidence,provenance_json)
                 VALUES (?1,?2,?3,?4,?5,NULL)",
                params![
                    e.from,
                    to,
                    enum_str(&e.kind),
                    enum_str(&e.derivation),
                    e.confidence as f64
                ],
            )?;
        }

        // Coarse fallback: a state-mutating def with no specific mutates/reads
        // edge gets an inferred `mutates io:unknown` so completeness rule (b) and
        // writers_of have something to bite on. Specific resource edges (BUILD 3
        // / declared) supersede this.
        for (hash, facts) in &idx.effects {
            if facts.mutates_state {
                let specific: i64 = tx.query_row(
                    "SELECT COUNT(*) FROM kgl_edge
                     WHERE from_hash=?1 AND kind IN ('mutates','reads') AND to_ref != 'io:unknown'",
                    params![hash],
                    |r| r.get(0),
                )?;
                if specific == 0 {
                    tx.execute(
                        "INSERT INTO kgl_edge (from_hash,to_ref,kind,derivation,confidence,provenance_json)
                         VALUES (?1,'io:unknown','mutates','inferred',0.5,NULL)",
                        params![hash],
                    )?;
                }
            }
        }

        // Prune orphans (nodes of THIS substrate not seen this run) and any
        // now-dangling edges. Scoping to `active` means a re-index of one
        // substrate never touches another's nodes, and observed `daimonos`
        // nodes (sessions/provenance) always survive a code re-index.
        tx.execute(
            "DELETE FROM kgl_node WHERE valid_as_of != ?1 AND substrate = ?2",
            params![run_stamp, active],
        )?;
        // Drop edges left dangling by the node prune, at BOTH ends:
        //  - `from_hash` always names a def node, so any missing one is dangling.
        //  - `to_ref` is a node hash/id for node-to-node edges (calls/depends_on)
        //    but a resource URN for reads/mutates (file://, secret:, io:, …) or an
        //    unresolved x07fn:/x07mod: target. Resource/URN refs carry a scheme
        //    separator (`:`); a `to_ref` without one is a node id, so prune it
        //    when its node is gone. This catches declared/observed edges whose
        //    target node was pruned, without deleting legitimate resource edges.
        tx.execute(
            "DELETE FROM kgl_edge WHERE from_hash NOT IN (SELECT hash FROM kgl_node)",
            [],
        )?;
        tx.execute(
            "DELETE FROM kgl_edge
             WHERE to_ref NOT LIKE '%:%'
               AND to_ref NOT IN (SELECT hash FROM kgl_node)",
            [],
        )?;

        tx.commit()?;

        let nodes: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM kgl_node", [], |r| r.get::<_, i64>(0))?
            as usize;
        let edges: usize = self
            .conn
            .query_row("SELECT COUNT(*) FROM kgl_edge", [], |r| r.get::<_, i64>(0))?
            as usize;
        Ok((nodes, edges))
    }

    // ---- metadata channel (store-only for substrates without a sidecar) ----

    pub fn set_intent(&self, hash: &str, intent: &Intent) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE kgl_node SET intent_json=?1 WHERE hash=?2",
            params![serde_json::to_string(intent)?, hash],
        )?;
        Ok(n > 0)
    }

    pub fn set_provenance(&self, hash: &str, p: &Provenance) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE kgl_node SET provenance_json=?1 WHERE hash=?2",
            params![serde_json::to_string(p)?, hash],
        )?;
        Ok(n > 0)
    }

    /// Record an agent-asserted edge (e.g. "this def reads file X").
    pub fn add_declared_edge(
        &self,
        from: &str,
        to: &str,
        kind: EdgeKind,
        prov: Option<&Provenance>,
    ) -> Result<()> {
        let pj = match prov {
            Some(p) => Some(serde_json::to_string(p)?),
            None => None,
        };
        // Atomic dedupe: identical declared edges must not pile up (an agent
        // re-asserting the same edge is a no-op), matching record_observation.
        self.conn.execute(
            "INSERT INTO kgl_edge (from_hash,to_ref,kind,derivation,confidence,provenance_json)
             SELECT ?1,?2,?3,'declared',1.0,?4
             WHERE NOT EXISTS (
                 SELECT 1 FROM kgl_edge
                 WHERE from_hash=?1 AND to_ref=?2 AND kind=?3 AND derivation='declared'
             )",
            params![from, to, enum_str(&kind), pj],
        )?;
        Ok(())
    }

    /// Record an OBSERVED action by an agent session: upsert a session node and
    /// add a reads/mutates edge to the touched resource URN. This is the
    /// observed-not-authored provenance KGL is uniquely positioned to capture
    /// (daimonos sees every tool call). Deduped per (session, resource, kind).
    pub fn record_observation(
        &self,
        session_id: &str,
        kind: EdgeKind,
        resource: &str,
        now: &str,
    ) -> Result<()> {
        let shash = format!("session:{session_id}");
        self.conn.execute(
            "INSERT INTO kgl_node (hash,kind,name,substrate,touches_io,mutates_state,valid_as_of)
             VALUES (?1,'session',?2,'daimonos',0,0,?3)
             ON CONFLICT(hash) DO UPDATE SET valid_as_of=excluded.valid_as_of",
            params![shash, session_id, now],
        )?;
        let prov = serde_json::to_string(&Provenance {
            authored_by: "daimonos:observed".into(),
            session_id: session_id.into(),
            timestamp: now.into(),
            assumptions: vec![],
            supersedes: vec![],
        })?;
        // Atomic dedupe: insert only if no identical observed edge exists (a
        // single statement, so concurrent observers can't double-insert).
        // derivation='observed' keeps these distinct from agent-`declared`
        // edges at the column level, not just via provenance_json.
        self.conn.execute(
            "INSERT INTO kgl_edge (from_hash,to_ref,kind,derivation,confidence,provenance_json)
             SELECT ?1,?2,?3,'observed',1.0,?4
             WHERE NOT EXISTS (
                 SELECT 1 FROM kgl_edge
                 WHERE from_hash=?1 AND to_ref=?2 AND kind=?3 AND derivation='observed'
             )",
            params![shash, resource, enum_str(&kind), prov],
        )?;
        Ok(())
    }

    // ---- query surface (the orientation API; see kgl_query MCP tool) ----

    pub fn node(&self, hash: &str) -> Result<Option<NodeRecord>> {
        let mut stmt = self.conn.prepare(NODE_SELECT_BY_HASH)?;
        let mut rows = stmt.query(params![hash])?;
        match rows.next()? {
            Some(row) => Ok(Some(row_to_record(row)?)),
            None => Ok(None),
        }
    }

    pub fn neighbors(
        &self,
        hash: &str,
        kind: Option<EdgeKind>,
        dir: Direction,
    ) -> Result<Vec<Edge>> {
        let mut edges = Vec::new();
        if dir == Direction::Out || dir == Direction::Both {
            edges.extend(self.edges_where("from_hash", hash)?);
        }
        if dir == Direction::In || dir == Direction::Both {
            edges.extend(self.edges_where("to_ref", hash)?);
        }
        if let Some(k) = kind {
            edges.retain(|e| e.kind == k);
        }
        Ok(edges)
    }

    /// Fuzzy orientation entry: nodes whose name or intent purpose contains `q`.
    /// `limit` caps the result set size; pass `usize::MAX` for effectively
    /// unlimited (internal/test callers that already scope their queries).
    pub fn find(&self, q: &str, limit: usize) -> Result<Vec<NodeRecord>> {
        if q.is_empty() {
            return Ok(vec![]);
        }
        // Escape SQL LIKE metacharacters so user input doesn't silently change
        // match semantics (e.g. "get%" must match literally, not "get anything").
        let escaped = q
            .to_lowercase()
            .replace('\\', r"\\")
            .replace('%', r"\%")
            .replace('_', r"\_");
        let needle = format!("%{escaped}%");
        let sql = format!(
            "{NODE_SELECT} WHERE lower(IFNULL(name,'')) LIKE ?1 ESCAPE '\\' \
             OR lower(IFNULL(intent_json,'')) LIKE ?1 ESCAPE '\\' \
             LIMIT ?2"
        );
        let lim = limit.min(i64::MAX as usize) as i64;
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![needle, lim], |r| Ok(row_to_record(r)))?;
        collect_records(rows)
    }

    /// Nodes that mutate a given resource URN — state-level blast radius.
    pub fn writers_of(&self, resource: &str) -> Result<Vec<NodeRecord>> {
        let sql = format!(
            "{NODE_SELECT} WHERE hash IN \
             (SELECT from_hash FROM kgl_edge WHERE to_ref=?1 AND kind='mutates')"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![resource], |r| Ok(row_to_record(r)))?;
        collect_records(rows)
    }

    /// Transitive set of nodes that (directly or indirectly) depend on / call
    /// `hash` — what breaks if it changes. Computed on demand (not materialized).
    /// `max_nodes` caps how many dependent nodes are collected before the BFS
    /// is truncated; pass `usize::MAX` for internal/test callers that need the
    /// full set on small graphs.
    pub fn blast_radius(&self, hash: &str, max_nodes: usize) -> Result<Vec<NodeRecord>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(hash.to_string());
        seen.insert(hash.to_string());
        let mut result_hashes: Vec<String> = Vec::new();

        while let Some(cur) = queue.pop_front() {
            if result_hashes.len() >= max_nodes {
                break;
            }
            let mut stmt = self.conn.prepare(
                "SELECT from_hash FROM kgl_edge \
                 WHERE to_ref=?1 AND kind IN ('calls','depends_on')",
            )?;
            let dependents: Vec<String> = stmt
                .query_map(params![cur], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            for d in dependents {
                if result_hashes.len() >= max_nodes {
                    break;
                }
                if seen.insert(d.clone()) {
                    result_hashes.push(d.clone());
                    queue.push_back(d);
                }
            }
        }

        let mut out = Vec::new();
        for h in result_hashes {
            if let Some(rec) = self.node(&h)? {
                out.push(rec);
            }
        }
        Ok(out)
    }

    /// Nodes carrying unresolved questions left by a prior authoring agent.
    pub fn open_questions(&self) -> Result<Vec<NodeRecord>> {
        let sql = format!("{NODE_SELECT} WHERE intent_json IS NOT NULL");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| Ok(row_to_record(r)))?;
        let recs = collect_records(rows)?;
        Ok(recs
            .into_iter()
            .filter(|r| {
                r.intent
                    .as_ref()
                    .is_some_and(|i| !i.open_questions.is_empty())
            })
            .collect())
    }

    /// KGL-completeness (SPEC §4): a node is complete iff it has a non-empty
    /// intent purpose AND, if effectful, has >=1 outgoing reads|mutates edge.
    pub fn check_completeness(&self) -> Result<Vec<Violation>> {
        let mut stmt = self.conn.prepare(NODE_SELECT)?;
        let rows = stmt.query_map([], |r| Ok(row_to_record(r)))?;
        let recs = collect_records(rows)?;

        let mut violations = Vec::new();
        for rec in recs {
            // Observed `daimonos` nodes (agent sessions, live state) are not
            // authored code defs and carry no intent.purpose by design — they
            // must not block the completeness/commit gate when observe is on.
            if rec.node.substrate == SubstrateKind::Daimonos {
                continue;
            }
            let purpose_ok = rec
                .intent
                .as_ref()
                .is_some_and(|i| !i.purpose.trim().is_empty());
            if !purpose_ok {
                violations.push(Violation {
                    hash: rec.node.hash.clone(),
                    name: rec.node.name.clone(),
                    reason: "missing intent.purpose".into(),
                });
            }
            if (rec.touches_io || rec.mutates_state)
                && !self.has_reachable_io_edge(&rec.node.hash)?
            {
                violations.push(Violation {
                    hash: rec.node.hash.clone(),
                    name: rec.node.name.clone(),
                    reason: "effectful def has no reads/mutates edge (directly or via any callee)"
                        .into(),
                });
            }
        }
        Ok(violations)
    }

    /// True if `start`, or any node reachable from it via `calls` or
    /// `depends_on` edges, has a reads/mutates edge. A def whose I/O happens
    /// entirely through callees or dependencies satisfies completeness rule (b):
    /// the touched state is still discoverable by following the dependency chain.
    /// Cycle-guarded.
    fn has_reachable_io_edge(&self, start: &str) -> Result<bool> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        seen.insert(start.to_string());
        queue.push_back(start.to_string());
        while let Some(cur) = queue.pop_front() {
            let direct: i64 = self.conn.query_row(
                "SELECT COUNT(*) FROM kgl_edge WHERE from_hash=?1 AND kind IN ('reads','mutates')",
                params![cur],
                |r| r.get(0),
            )?;
            if direct > 0 {
                return Ok(true);
            }
            // Walk both `calls` and `depends_on` — I/O can be reached through
            // either edge kind (a module that depends_on an I/O helper is just as
            // reachable as one that calls it directly).
            let mut stmt = self.conn.prepare(
                "SELECT to_ref FROM kgl_edge WHERE from_hash=?1 AND kind IN ('calls','depends_on')",
            )?;
            let callees: Vec<String> = stmt
                .query_map(params![cur], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            for c in callees {
                if seen.insert(c.clone()) {
                    queue.push_back(c);
                }
            }
        }
        Ok(false)
    }

    fn edges_where(&self, col: &str, val: &str) -> Result<Vec<Edge>> {
        let sql = format!(
            "SELECT from_hash,to_ref,kind,derivation,confidence FROM kgl_edge WHERE {col}=?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![val], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, f64>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (from, to, kind, derivation, confidence) = row?;
            out.push(Edge {
                from,
                to,
                kind: enum_from(&kind).unwrap_or(EdgeKind::Calls),
                derivation: enum_from(&derivation).unwrap_or(Derivation::Derived),
                confidence: confidence as f32,
            });
        }
        Ok(out)
    }
}

const NODE_SELECT: &str = "SELECT hash,kind,name,substrate,file,span,intent_json,provenance_json,touches_io,mutates_state FROM kgl_node";
const NODE_SELECT_BY_HASH: &str = "SELECT hash,kind,name,substrate,file,span,intent_json,provenance_json,touches_io,mutates_state FROM kgl_node WHERE hash=?1";

fn row_to_record(row: &rusqlite::Row) -> rusqlite::Result<NodeRecord> {
    let hash: String = row.get(0)?;
    let kind: String = row.get(1)?;
    let name: Option<String> = row.get(2)?;
    let substrate: String = row.get(3)?;
    let file: Option<String> = row.get(4)?;
    let span: Option<String> = row.get(5)?;
    let intent_json: Option<String> = row.get(6)?;
    let provenance_json: Option<String> = row.get(7)?;
    let touches_io: i64 = row.get(8)?;
    let mutates_state: i64 = row.get(9)?;

    Ok(NodeRecord {
        node: DefNode {
            hash,
            kind: enum_from(&kind).unwrap_or(NodeKind::Function),
            name,
            substrate: enum_from(&substrate).unwrap_or(SubstrateKind::X07),
            file,
            span,
        },
        intent: intent_json.and_then(|s| serde_json::from_str(&s).ok()),
        provenance: provenance_json.and_then(|s| serde_json::from_str(&s).ok()),
        touches_io: touches_io != 0,
        mutates_state: mutates_state != 0,
    })
}

fn collect_records(
    rows: impl Iterator<Item = rusqlite::Result<rusqlite::Result<NodeRecord>>>,
) -> Result<Vec<NodeRecord>> {
    let mut out = Vec::new();
    for r in rows {
        out.push(r??);
    }
    Ok(out)
}

fn resolve_urn(to: &str, fns: &HashMap<String, String>, mods: &HashMap<String, String>) -> String {
    if let Some(n) = to.strip_prefix("x07fn:") {
        if let Some(h) = fns.get(n) {
            return h.clone();
        }
    }
    if let Some(n) = to.strip_prefix("x07mod:") {
        if let Some(h) = mods.get(n) {
            return h.clone();
        }
    }
    to.to_string()
}

/// Serialize a fieldless serde enum to its canonical lowercase string (single
/// source of truth = the serde `rename_all`), avoiding a hand-maintained map.
fn enum_str<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|x| x.as_str().map(String::from))
        .unwrap_or_default()
}

fn enum_from<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(Value::String(s.to_string())).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kgl::model::EdgeKind;
    use crate::kgl::substrate_x07::X07Substrate;
    use std::io::Write;
    use std::path::Path;

    fn fixture(dir: &Path) {
        let mut f = std::fs::File::create(dir.join("math.x07.json")).unwrap();
        f.write_all(
            br#"{
                "module_id":"math.core","schema_version":"1.0","kind":"library",
                "imports":["std.io"],
                "decls":[
                    {"kind":"defn","name":"add",
                     "params":[{"name":"a","ty":"i64"}],"result":"i64",
                     "body":[{"kind":"call","callee":"checked_add"}]},
                    {"kind":"defn","name":"checked_add",
                     "params":[{"name":"a","ty":"i64"}],"result":"i64",
                     "effects":["Mut"]}
                ]
            }"#,
        )
        .unwrap();
    }

    #[test]
    fn populate_resolves_urns_and_seeds_effects() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        let mut store = KglStore::open_in_memory().unwrap();
        let (nodes, _edges) = store
            .populate(&X07Substrate::default(), tmp.path(), "run1")
            .unwrap();
        assert_eq!(nodes, 3); // module + 2 functions

        // the `calls` URN x07fn:checked_add resolved to checked_add's real hash
        let add = store.find("add", usize::MAX).unwrap();
        let add_fn = add
            .iter()
            .find(|r| r.node.name.as_deref() == Some("add"))
            .unwrap();
        let calls = store
            .neighbors(&add_fn.node.hash, Some(EdgeKind::Calls), Direction::Out)
            .unwrap();
        assert_eq!(calls.len(), 1);
        let checked = store.node(&calls[0].to).unwrap().unwrap();
        assert_eq!(checked.node.name.as_deref(), Some("checked_add"));

        // checked_add mutates_state -> seeded inferred mutates edge
        let muts = store
            .neighbors(&checked.node.hash, Some(EdgeKind::Mutates), Direction::Out)
            .unwrap();
        assert_eq!(muts.len(), 1);
    }

    #[test]
    fn repopulate_is_idempotent_and_preserves_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        let mut store = KglStore::open_in_memory().unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "run1")
            .unwrap();

        let add = store.find("add", usize::MAX).unwrap();
        let add_hash = add
            .iter()
            .find(|r| r.node.name.as_deref() == Some("add"))
            .unwrap()
            .node
            .hash
            .clone();
        store
            .set_intent(
                &add_hash,
                &Intent {
                    purpose: "add two integers".into(),
                    rationale: None,
                    open_questions: vec!["overflow policy?".into()],
                },
            )
            .unwrap();

        let (n1, e1) = store
            .populate(&X07Substrate::default(), tmp.path(), "run2")
            .unwrap();
        let (n2, e2) = store
            .populate(&X07Substrate::default(), tmp.path(), "run3")
            .unwrap();
        assert_eq!((n1, e1), (n2, e2)); // idempotent counts

        // metadata survived re-population (hash unchanged)
        let rec = store.node(&add_hash).unwrap().unwrap();
        assert_eq!(rec.intent.unwrap().purpose, "add two integers");
        let oq = store.open_questions().unwrap();
        assert!(oq.iter().any(|r| r.node.hash == add_hash));
    }

    #[test]
    fn completeness_flags_missing_purpose_and_effectful_nodes() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        let mut store = KglStore::open_in_memory().unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "run1")
            .unwrap();
        // No intent set anywhere -> every node violates purpose.
        let v = store.check_completeness().unwrap();
        assert!(v.iter().any(|x| x.reason.contains("purpose")));
    }

    #[test]
    fn transitive_effect_satisfies_completeness() {
        // `outer` is effectful only via its call to `inner`, which does the
        // actual read. `outer` has no direct reads/mutates edge — it must still
        // be considered complete (regression for the transitive-effect fix).
        let tmp = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(tmp.path().join("m.x07.json")).unwrap();
        f.write_all(
            br#"{
                "module_id":"m","schema_version":"1.0","kind":"library","imports":[],
                "decls":[
                    {"kind":"defn","name":"outer","params":[],"result":"Unit","effects":["IO"],
                     "body":[{"kind":"call","callee":"inner"}]},
                    {"kind":"defn","name":"inner","params":[],"result":"Bytes","effects":["IO"],
                     "body":[{"kind":"call","callee":"read_file","args":["/data"]}]}
                ]
            }"#,
        )
        .unwrap();
        let mut store = KglStore::open_in_memory().unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "r1")
            .unwrap();
        // Document every node so only effect-rule (b) violations could remain.
        // Use check_completeness() to enumerate all nodes rather than find(""),
        // since find("") is intentionally blocked (empty query matches nothing).
        let intent = Intent {
            purpose: "documented".into(),
            rationale: None,
            open_questions: vec![],
        };
        for v in store.check_completeness().unwrap() {
            store.set_intent(&v.hash, &intent).unwrap();
        }
        let v = store.check_completeness().unwrap();
        assert!(v.is_empty(), "expected no violations, got {v:?}");
    }

    #[test]
    fn observation_records_session_and_writers() {
        let store = KglStore::open_in_memory().unwrap();
        store
            .record_observation("sess-7", EdgeKind::Mutates, "file:///ws/a.rs", "t0")
            .unwrap();
        store
            .record_observation("sess-7", EdgeKind::Reads, "file:///ws/b.rs", "t0")
            .unwrap();
        // dedupe: identical observation again -> still one edge
        store
            .record_observation("sess-7", EdgeKind::Mutates, "file:///ws/a.rs", "t1")
            .unwrap();

        let w = store.writers_of("file:///ws/a.rs").unwrap();
        assert!(w.iter().any(|r| r.node.name.as_deref() == Some("sess-7")));

        let sh = "session:sess-7".to_string();
        let edges = store.neighbors(&sh, None, Direction::Out).unwrap();
        assert_eq!(edges.len(), 2); // one mutates + one reads (deduped)
    }

    #[test]
    fn reindex_preserves_observations() {
        // Observed session nodes/edges must survive a code re-index (prune guard).
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        let mut store = KglStore::open_in_memory().unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "r1")
            .unwrap();
        store
            .record_observation("sess-1", EdgeKind::Mutates, "file:///ws/x", "t0")
            .unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "r2")
            .unwrap(); // re-index code
        let w = store.writers_of("file:///ws/x").unwrap();
        assert!(
            w.iter().any(|r| r.node.name.as_deref() == Some("sess-1")),
            "observation should survive code re-index"
        );
    }

    #[test]
    fn reindex_one_substrate_preserves_another() {
        // Blocker regression: an x07 `index` over a graphify-derived graph must
        // NOT wipe the graphify (substrate=rust) nodes or their agent metadata.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("graphify-out")).unwrap();
        std::fs::write(
            tmp.path().join("graphify-out").join("graph.json"),
            r#"{"nodes":[
                {"id":"n_foo","label":".foo()","file_type":"code","source_file":"src/a.rs"},
                {"id":"n_bar","label":".bar()","file_type":"code","source_file":"src/a.rs"}
            ],"links":[
                {"relation":"calls","source":"n_foo","target":"n_bar","confidence_score":1.0}
            ]}"#,
        )
        .unwrap();
        let mut store = KglStore::open_in_memory().unwrap();
        store
            .populate(
                &crate::kgl::substrate_graphify::GraphifySubstrate,
                tmp.path(),
                "g1",
            )
            .unwrap();
        store
            .set_intent(
                "n_foo",
                &Intent {
                    purpose: "does foo".into(),
                    rationale: None,
                    open_questions: vec![],
                },
            )
            .unwrap();

        // Re-index with x07 (no *.x07.json present -> empty scan). The prune
        // must leave the graphify node, its intent, AND its structural edge
        // intact — re-indexing one substrate must not strip another's edges.
        store
            .populate(&X07Substrate::default(), tmp.path(), "x1")
            .unwrap();

        let rec = store.node("n_foo").unwrap();
        assert!(rec.is_some(), "graphify node wiped by an x07 re-index");
        assert_eq!(rec.unwrap().intent.unwrap().purpose, "does foo");
        let calls = store
            .neighbors("n_foo", Some(EdgeKind::Calls), Direction::Out)
            .unwrap();
        assert!(
            calls.iter().any(|e| e.to == "n_bar"),
            "graphify structural edge stripped by an x07 re-index"
        );
    }

    #[test]
    fn prune_removes_edges_to_pruned_nodes_but_keeps_resource_edges() {
        // A declared edge to another node must be removed when that node is
        // pruned, but a declared edge to a resource URN must survive.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::File::create(tmp.path().join("m.x07.json"))
            .unwrap()
            .write_all(
                br#"{"module_id":"m","schema_version":"1.0","kind":"library","imports":[],
                    "decls":[
                      {"kind":"defn","name":"foo","params":[],"result":"Unit","body":[]},
                      {"kind":"defn","name":"bar","params":[],"result":"Unit","body":[]}
                    ]}"#,
            )
            .unwrap();
        let mut store = KglStore::open_in_memory().unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "r1")
            .unwrap();
        let foo = store
            .find("foo", usize::MAX)
            .unwrap()
            .into_iter()
            .find(|r| r.node.name.as_deref() == Some("foo"))
            .unwrap()
            .node
            .hash;
        let bar = store
            .find("bar", usize::MAX)
            .unwrap()
            .into_iter()
            .find(|r| r.node.name.as_deref() == Some("bar"))
            .unwrap()
            .node
            .hash;
        // foo -> bar (node target) and foo -> secret:KEY (resource target)
        store
            .add_declared_edge(&foo, &bar, EdgeKind::DependsOn, None)
            .unwrap();
        store
            .add_declared_edge(&foo, "secret:KEY", EdgeKind::Reads, None)
            .unwrap();

        // Re-index a workspace where `bar` is gone -> bar node pruned.
        std::fs::write(
            tmp.path().join("m.x07.json"),
            r#"{"module_id":"m","schema_version":"1.0","kind":"library","imports":[],
               "decls":[{"kind":"defn","name":"foo","params":[],"result":"Unit","body":[]}]}"#,
        )
        .unwrap();
        store
            .populate(&X07Substrate::default(), tmp.path(), "r2")
            .unwrap();

        assert!(store.node(&bar).unwrap().is_none(), "bar should be pruned");
        let out = store.neighbors(&foo, None, Direction::Out).unwrap();
        assert!(
            !out.iter().any(|e| e.to == bar),
            "declared edge to pruned node should be removed"
        );
        assert!(
            out.iter().any(|e| e.to == "secret:KEY"),
            "declared edge to a resource URN must survive the prune"
        );
    }

    // ---------------------------------------------------------------------
    // Substrate-isolation invariant.
    //
    // The "destructive prune crossing substrate boundaries" data-loss class
    // has recurred across multiple rewrites, landing at a new line each time.
    // Rather than pin a test to one statement, the helper below asserts the
    // *property* directly: re-indexing ONE substrate must
    // leave every row that doesn't belong to it byte-for-byte unchanged. Any
    // future destructive op that crosses a substrate boundary, or wipes
    // declared/observed/agent data, trips this regardless of where in
    // `populate` it lives. A mock substrate lets the invariant run over
    // arbitrary (and hypothetical future) substrate kinds.
    // ---------------------------------------------------------------------

    struct MockSubstrate {
        kind: SubstrateKind,
        nodes: Vec<DefNode>,
        edges: Vec<Edge>,
        effects: HashMap<String, crate::kgl::model::EffectFacts>,
    }
    impl crate::kgl::substrate::Substrate for MockSubstrate {
        fn kind(&self) -> SubstrateKind {
            self.kind
        }
        fn index(&self, _root: &Path) -> Result<crate::kgl::substrate::IndexResult> {
            Ok(crate::kgl::substrate::IndexResult {
                nodes: self.nodes.clone(),
                edges: self.edges.clone(),
                effects: self.effects.clone(),
            })
        }
    }

    fn mock_node(hash: &str, name: &str, sub: SubstrateKind) -> DefNode {
        DefNode {
            hash: hash.into(),
            kind: NodeKind::Function,
            name: Some(name.into()),
            substrate: sub,
            file: Some("src/x".into()),
            span: None,
        }
    }

    fn sub_str(k: SubstrateKind) -> String {
        serde_json::to_value(k)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    /// (hash, substrate, full-row-string) for every node, ordered by hash.
    fn dump_nodes(store: &KglStore) -> Vec<(String, String, String)> {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT hash,kind,name,substrate,file,span,intent_json,provenance_json,\
                 touches_io,mutates_state,valid_as_of FROM kgl_node ORDER BY hash",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let hash: String = r.get(0)?;
                let sub: String = r.get(3)?;
                let row = format!(
                    "{:?}",
                    (
                        &hash,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        &sub,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                        r.get::<_, i64>(8)?,
                        r.get::<_, i64>(9)?,
                        r.get::<_, String>(10)?,
                    )
                );
                Ok((hash, sub, row))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// (from_hash, full-row-string) for every edge, deterministically ordered.
    fn dump_edges(store: &KglStore) -> Vec<(String, String)> {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT from_hash,to_ref,kind,derivation,confidence,provenance_json \
                 FROM kgl_edge ORDER BY from_hash,to_ref,kind,derivation",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                let from: String = r.get(0)?;
                let row = format!(
                    "{:?}",
                    (
                        &from,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, f64>(4)?,
                        r.get::<_, Option<String>>(5)?,
                    )
                );
                Ok((from, row))
            })
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// Seed `preserved` (+ agent-authored + observed data), re-index
    /// `reindexed`, then assert nothing outside `reindexed` changed.
    fn assert_substrate_isolation(preserved: SubstrateKind, reindexed: SubstrateKind) {
        assert_ne!(preserved, reindexed);
        // MockSubstrate::index ignores the root, so no real workspace is needed.
        let root = Path::new("/");
        let mut store = KglStore::open_in_memory().unwrap();

        // Seed: 2 preserved nodes, a derived structural edge, and (via effects)
        // an inferred io edge.
        let mut effects = HashMap::new();
        effects.insert(
            "p2".to_string(),
            crate::kgl::model::EffectFacts {
                touches_io: true,
                mutates_state: true,
            },
        );
        let seed = MockSubstrate {
            kind: preserved,
            nodes: vec![
                mock_node("p1", "p_one", preserved),
                mock_node("p2", "p_two", preserved),
            ],
            edges: vec![Edge {
                from: "p1".into(),
                to: "p2".into(),
                kind: EdgeKind::Calls,
                derivation: Derivation::Derived,
                confidence: 1.0,
            }],
            effects,
        };
        store.populate(&seed, root, "seed").unwrap();

        // Agent-authored + observed data hung off the preserved nodes.
        store
            .set_intent(
                "p1",
                &Intent {
                    purpose: "why p1".into(),
                    rationale: None,
                    open_questions: vec![],
                },
            )
            .unwrap();
        store
            .set_provenance(
                "p1",
                &Provenance {
                    authored_by: "agent".into(),
                    session_id: "s1".into(),
                    timestamp: "t".into(),
                    assumptions: vec![],
                    supersedes: vec![],
                },
            )
            .unwrap();
        store
            .add_declared_edge("p1", "p2", EdgeKind::DependsOn, None)
            .unwrap();
        store
            .add_declared_edge("p1", "secret:DB", EdgeKind::Reads, None)
            .unwrap();
        store
            .record_observation("sess-x", EdgeKind::Mutates, "file:///w/a.rs", "t0")
            .unwrap();

        // Snapshot everything — none of it belongs to `reindexed` yet.
        let nodes_before: Vec<String> = dump_nodes(&store)
            .into_iter()
            .map(|(_, _, row)| row)
            .collect();
        let edges_before: Vec<String> =
            dump_edges(&store).into_iter().map(|(_, row)| row).collect();

        // Re-index the OTHER substrate (disjoint node namespace + its own edge).
        let reidx = MockSubstrate {
            kind: reindexed,
            nodes: vec![
                mock_node("q1", "q_one", reindexed),
                mock_node("q2", "q_two", reindexed),
            ],
            edges: vec![Edge {
                from: "q1".into(),
                to: "q2".into(),
                kind: EdgeKind::Calls,
                derivation: Derivation::Derived,
                confidence: 1.0,
            }],
            effects: HashMap::new(),
        };
        store.populate(&reidx, root, "reidx").unwrap();

        // Strip the reindexed substrate's own rows; the remainder MUST equal
        // the pre-reindex snapshot exactly.
        let b_sub = sub_str(reindexed);
        let all_nodes = dump_nodes(&store);
        let b_hashes: HashSet<String> = all_nodes
            .iter()
            .filter(|(_, sub, _)| *sub == b_sub)
            .map(|(h, _, _)| h.clone())
            .collect();
        let nodes_after: Vec<String> = all_nodes
            .into_iter()
            .filter(|(_, sub, _)| *sub != b_sub)
            .map(|(_, _, row)| row)
            .collect();
        let edges_after: Vec<String> = dump_edges(&store)
            .into_iter()
            .filter(|(from, _)| !b_hashes.contains(from))
            .map(|(_, row)| row)
            .collect();

        assert_eq!(
            nodes_before, nodes_after,
            "re-indexing {reindexed:?} mutated or dropped a {preserved:?}/daimonos NODE"
        );
        assert_eq!(
            edges_before, edges_after,
            "re-indexing {reindexed:?} mutated or dropped a \
             {preserved:?}/daimonos/declared/observed EDGE"
        );
    }

    #[test]
    fn reindex_isolates_rust_from_x07() {
        assert_substrate_isolation(SubstrateKind::Rust, SubstrateKind::X07);
    }

    #[test]
    fn reindex_isolates_x07_from_rust() {
        assert_substrate_isolation(SubstrateKind::X07, SubstrateKind::Rust);
    }

    #[test]
    fn reindex_isolates_future_substrate_kinds() {
        // Hypothetical future substrates behind the same trait must isolate too,
        // so a new backend can't silently reintroduce the cross-substrate wipe.
        assert_substrate_isolation(SubstrateKind::X07, SubstrateKind::Tacit);
        assert_substrate_isolation(SubstrateKind::Tacit, SubstrateKind::Zero);
        assert_substrate_isolation(SubstrateKind::Zero, SubstrateKind::Rust);
    }

    #[test]
    fn observed_edges_use_observed_derivation() {
        // W4: observed edges are distinguishable from agent-declared ones at the
        // derivation column, not just via provenance_json.
        let store = KglStore::open_in_memory().unwrap();
        store
            .record_observation("s1", EdgeKind::Mutates, "file:///ws/a", "t0")
            .unwrap();
        let edges = store.neighbors("session:s1", None, Direction::Out).unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].derivation, Derivation::Observed);
    }

    #[test]
    fn completeness_ignores_observed_nodes() {
        // W7: observed session/daimonos nodes have no intent.purpose by design
        // and must not block the completeness/commit gate.
        let store = KglStore::open_in_memory().unwrap();
        store
            .record_observation("s1", EdgeKind::Mutates, "file:///ws/a", "t0")
            .unwrap();
        let v = store.check_completeness().unwrap();
        assert!(
            !v.iter().any(|x| x.hash.starts_with("session:")),
            "observed session node should not be a completeness violation"
        );
    }
}
