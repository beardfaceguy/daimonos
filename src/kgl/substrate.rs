//! The substrate abstraction: KGL extracts a content-addressed graph from some
//! agent-first language but stays decoupled from which one. v0 ships an X07
//! backend ([`crate::kgl::substrate_x07`]); Tacit/Zero can be added behind the
//! same trait. The graph is external and keyed by content hash, so KGL survives
//! a substrate being swapped or churning underneath it.

use crate::kgl::model::{DefNode, Edge, EffectFacts, SubstrateKind};
use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;

/// Everything KGL extracts from a substrate in one pass over a workspace.
#[derive(Debug, Default)]
pub struct IndexResult {
    pub nodes: Vec<DefNode>,
    /// Structural edges the substrate gives for free (`calls`, `depends_on`).
    pub edges: Vec<Edge>,
    /// Coarse effect facts per node hash, for seeding `mutates`/`reads`.
    pub effects: HashMap<String, EffectFacts>,
}

/// A source of content-addressed definitions and their structural relations.
pub trait Substrate {
    fn kind(&self) -> SubstrateKind;

    /// Walk a workspace and extract nodes + free structural edges + effect facts.
    fn index(&self, root: &Path) -> Result<IndexResult>;
}

/// Canonical JSON serialization (RFC-8785 style: object keys sorted, no
/// insignificant whitespace) so a definition's content hash is stable across
/// formatting and key ordering. Arrays preserve order; objects are key-sorted.
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| {
                    let key = Value::String((*k).clone()).to_string();
                    format!("{}:{}", key, canonical_json(&map[*k]))
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => {
            let parts: Vec<String> = arr.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        // Scalars: serde_json's Display is already canonical for these.
        other => other.to_string(),
    }
}

/// Content hash of a JSON value = SHA-256 of its canonical form, hex-encoded.
/// SHA-256 reuses the hashing already in the tree (`sha2`, as `plugins::x07`
/// does); the spec permits substrate-native hashes, so a Tacit backend would
/// instead surface its own BLAKE3 definition hashes directly.
pub fn content_hash(v: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical_json(v).as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_is_key_order_independent() {
        let a: Value = serde_json::from_str(r#"{"x":1,"y":[2,3]}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"y":[2,3],"x":1}"#).unwrap();
        assert_eq!(content_hash(&a), content_hash(&b));
        assert_eq!(content_hash(&a).len(), 64);
    }

    #[test]
    fn array_order_is_significant() {
        let a: Value = serde_json::from_str(r#"[1,2]"#).unwrap();
        let b: Value = serde_json::from_str(r#"[2,1]"#).unwrap();
        assert_ne!(content_hash(&a), content_hash(&b));
    }
}
