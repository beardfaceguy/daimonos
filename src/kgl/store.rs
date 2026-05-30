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
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(path).context("open kgl sqlite store")?;
        let s = Self { conn };
        s.init()?;
        Ok(s)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let s = Self { conn };
        s.init()?;
        Ok(s)
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

        // Recomputed each run; Declared edges are preserved.
        tx.execute(
            "DELETE FROM kgl_edge WHERE derivation IN ('derived','inferred')",
            [],
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

        // Prune orphans (nodes not seen this run) and any now-dangling edges.
        tx.execute("DELETE FROM kgl_node WHERE valid_as_of != ?1", params![run_stamp])?;
        tx.execute(
            "DELETE FROM kgl_edge WHERE from_hash NOT IN (SELECT hash FROM kgl_node)",
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
        self.conn.execute(
            "INSERT INTO kgl_edge (from_hash,to_ref,kind,derivation,confidence,provenance_json)
             VALUES (?1,?2,?3,'declared',1.0,?4)",
            params![from, to, enum_str(&kind), pj],
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
    pub fn find(&self, q: &str) -> Result<Vec<NodeRecord>> {
        let needle = format!("%{}%", q.to_lowercase());
        let sql = format!(
            "{NODE_SELECT} WHERE lower(IFNULL(name,'')) LIKE ?1 \
             OR lower(IFNULL(intent_json,'')) LIKE ?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params![needle], |r| Ok(row_to_record(r)))?;
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
    pub fn blast_radius(&self, hash: &str) -> Result<Vec<NodeRecord>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = VecDeque::new();
        queue.push_back(hash.to_string());
        seen.insert(hash.to_string());
        let mut result_hashes: Vec<String> = Vec::new();

        while let Some(cur) = queue.pop_front() {
            let mut stmt = self.conn.prepare(
                "SELECT from_hash FROM kgl_edge \
                 WHERE to_ref=?1 AND kind IN ('calls','depends_on')",
            )?;
            let dependents: Vec<String> = stmt
                .query_map(params![cur], |r| r.get::<_, String>(0))?
                .filter_map(|r| r.ok())
                .collect();
            for d in dependents {
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
            .filter(|r| r.intent.as_ref().is_some_and(|i| !i.open_questions.is_empty()))
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

    /// True if `start`, or any node reachable from it via `calls` edges, has a
    /// reads/mutates edge. A def whose I/O happens entirely through callees thus
    /// satisfies completeness rule (b): the touched state is still discoverable
    /// by following the call chain. Cycle-guarded.
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
            let mut stmt = self
                .conn
                .prepare("SELECT to_ref FROM kgl_edge WHERE from_hash=?1 AND kind='calls'")?;
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

fn resolve_urn(
    to: &str,
    fns: &HashMap<String, String>,
    mods: &HashMap<String, String>,
) -> String {
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
        let (nodes, _edges) = store.populate(&X07Substrate, tmp.path(), "run1").unwrap();
        assert_eq!(nodes, 3); // module + 2 functions

        // the `calls` URN x07fn:checked_add resolved to checked_add's real hash
        let add = store.find("add").unwrap();
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
        store.populate(&X07Substrate, tmp.path(), "run1").unwrap();

        let add = store.find("add").unwrap();
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

        let (n1, e1) = store.populate(&X07Substrate, tmp.path(), "run2").unwrap();
        let (n2, e2) = store.populate(&X07Substrate, tmp.path(), "run3").unwrap();
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
        store.populate(&X07Substrate, tmp.path(), "run1").unwrap();
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
        store.populate(&X07Substrate, tmp.path(), "r1").unwrap();
        // Document every node so only effect-rule (b) violations could remain.
        for rec in store.find("").unwrap() {
            store
                .set_intent(
                    &rec.node.hash,
                    &Intent {
                        purpose: "documented".into(),
                        rationale: None,
                        open_questions: vec![],
                    },
                )
                .unwrap();
        }
        let v = store.check_completeness().unwrap();
        assert!(v.is_empty(), "expected no violations, got {v:?}");
    }
}
