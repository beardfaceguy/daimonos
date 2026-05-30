//! KGL v0 — core data model for the code + OS knowledge-graph layer.
//!
//! Nodes are content-addressed definitions; edges are typed relations; intent
//! and provenance are non-hashed metadata. This module defines only the data
//! shapes. Extraction lives in [`crate::kgl::substrate`] and per-substrate
//! backends; storage arrives in a later build phase.

use serde::{Deserialize, Serialize};

/// Which agent-first language a node was sourced from. Kept open so Tacit/Zero
/// backends can slot in behind the same [`crate::kgl::substrate::Substrate`]
/// trait without touching the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SubstrateKind {
    X07,
    Tacit,
    Zero,
    /// Rust source surfaced via a derived graphify graph (see `substrate_graphify`).
    Rust,
    /// The daimonos OS runtime itself — source of observed nodes/edges (sessions,
    /// live state). These are observed ground truth, not authored or derived.
    Daimonos,
}

/// The kind of definition a node represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeKind {
    Function,
    Type,
    Module,
    Const,
    /// An agent session that acted on the system (observed-provenance node).
    Session,
}

/// A node in the knowledge graph: a single content-addressed definition.
/// `hash` is identity; `name` is advisory display only and never identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefNode {
    pub hash: String,
    pub kind: NodeKind,
    pub name: Option<String>,
    pub substrate: SubstrateKind,
    /// Where the def text lives, so an agent *can* open source but shouldn't need to.
    pub file: Option<String>,
    /// Location within the file (e.g. a JSON Pointer like `/decls/0`).
    pub span: Option<String>,
}

/// A typed relation. The `to` field is either another node `hash` or a resource
/// URN (e.g. `file:///workspace/x`, `secret:DB_PW`, `x07mod:std.io`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Calls,
    DependsOn,
    Mutates,
    Reads,
}

/// How an edge was established — lets a reading agent distinguish
/// "the compiler proved this" from "an agent claimed this".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Derivation {
    /// Free from the substrate's own structure (import/reference edges).
    Derived,
    /// Computed by KGL's own static analysis.
    Inferred,
    /// Asserted by the authoring agent via the metadata channel.
    Declared,
}

/// A typed edge between a node and another node or a resource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: EdgeKind,
    pub derivation: Derivation,
    /// 1.0 for `Derived`; lower for `Inferred`/`Declared`.
    pub confidence: f32,
}

/// Coarse effect facts used to seed `mutates`/`reads` edges. No substrate
/// distinguishes reads from writes — that split is KGL's own analysis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectFacts {
    pub touches_io: bool,
    pub mutates_state: bool,
}

/// Non-hashed metadata: why a def exists — the thing not derivable from code.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    /// REQUIRED for KGL-completeness: one line on why this def exists.
    pub purpose: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_questions: Vec<String>,
}

/// Non-hashed metadata: who authored a def and under what assumptions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub authored_by: String,
    pub session_id: String,
    /// ISO-8601, supplied by the host. KGL never invents time.
    pub timestamp: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assumptions: Vec<String>,
    /// Hashes of definitions this one replaces.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
}
