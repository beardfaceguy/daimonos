//! WP17 (FR-15): offline validation that a served or exported tool schema is a
//! full, self-contained JSON Schema draft 2020-12 document (ADR-030).
//!
//! Context: docs/adr/030-self-contained-full-schemas.md,
//! docs/contexts/reviewer-runtime/CONTEXT.md, and the reviewer-runtime
//! INTERFACES.md WP17 rows.
//!
//! This module is the static gate for the opt-in full-schema mode. It consumes
//! the WP05 reusable Draft 2020-12 validator ([`crate::schema_validation`])
//! rather than a second hand-written walker (ADR-030 / ADR-024): compilation
//! there already refuses external references (file/HTTP/other-scheme `$ref`,
//! with the crate's resolver features disabled) and any schema that is not a
//! usable 2020-12 document, without touching the network or filesystem.
//!
//! On top of compilation this adds the two static checks ADR-030 names that a
//! 2020-12 *compiler* does not reject on its own, because each is valid JSON
//! Schema yet unusable in a tool definition for the vendors dbunk targets
//! (OpenAI Chat Completions, Anthropic Messages, OpenRouter):
//!
//! - **Recursive reference cycles** — a `$ref` that, followed through local
//!   definitions, returns to a definition already on the path. Recursive
//!   schemas are the main example the PM named (A to Q-FR-15-2).
//! - **Definition-name collisions** — two owned tool schemas that each define a
//!   `$defs` entry under the same name would clash when a caller combines them
//!   (FR-15 R3). Names must be unique across the composed catalog.
//!
//! Every failure maps to the run reason `schema_unusable`, detected before any
//! model request (PM A to Q-FR-15-2). A schema that is valid, self-contained,
//! and non-recursive but which a *particular endpoint* rejects is NOT a
//! `schema_unusable`: that surfaces at the capability probe as
//! `model_unsupported` (WP09), never a guessed startup blacklist (ADR-030).
//!
//! The caller's own worker and final-answer schemas (WP05 `--final-output`) are
//! validated and offered unchanged through WP05's validator; this module
//! governs the native/served tool catalog, not the final-answer schema.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::schema_validation::{CompiledSchema, SchemaError};

/// Internal bound on static-analysis work for one schema. A document that
/// exceeds it is treated as statically unusable rather than explored forever;
/// the native tool schemas are tiny and well under this cap.
const MAX_ANALYSIS_STEPS: usize = 200_000;

/// A served/exported tool schema that cannot be used locally, detected before
/// any model request or external I/O (ADR-030). Every variant maps to the run
/// reason [`SchemaContractError::REASON`] (`schema_unusable`).
#[derive(Debug)]
pub enum SchemaContractError {
    /// The schema needs a reference that is not embedded in the document
    /// (another file, a URL, or any non-local scheme). Refused without a fetch.
    ExternalReference { tool: String, detail: String },
    /// The schema is not a usable Draft 2020-12 document for another reason
    /// (malformed, unknown keyword shape, invalid dialect).
    Invalid { tool: String, detail: String },
    /// A local `$ref` forms a recursive cycle through the document's
    /// definitions. Valid JSON Schema, but unusable in a tool definition.
    Recursive { tool: String, pointer: String },
    /// Two tools define a `$defs`/`definitions` entry under the same name, which
    /// would clash when a caller combines the schemas (FR-15 R3).
    DefinitionCollision {
        name: String,
        first_tool: String,
        second_tool: String,
    },
}

impl SchemaContractError {
    /// The single run reason every full-schema static failure reports
    /// (INTERFACES.md: `schema_unusable` before a request).
    pub const REASON: &'static str = "schema_unusable";

    /// The stable run reason for this failure. Always [`Self::REASON`].
    pub fn reason(&self) -> &'static str {
        Self::REASON
    }
}

impl std::fmt::Display for SchemaContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaContractError::ExternalReference { tool, detail } => write!(
                f,
                "tool `{tool}` schema requires an external reference: {detail}"
            ),
            SchemaContractError::Invalid { tool, detail } => {
                write!(f, "tool `{tool}` schema is not valid 2020-12: {detail}")
            }
            SchemaContractError::Recursive { tool, pointer } => write!(
                f,
                "tool `{tool}` schema has a recursive reference cycle at {pointer}"
            ),
            SchemaContractError::DefinitionCollision {
                name,
                first_tool,
                second_tool,
            } => write!(
                f,
                "definition name `{name}` is defined by both `{first_tool}` and `{second_tool}`"
            ),
        }
    }
}

impl std::error::Error for SchemaContractError {}

/// A tool schema that has passed the full-schema static gate (ADR-030): it is a
/// valid, self-contained, non-recursive Draft 2020-12 document. Construction is
/// the proof; the compiled validator is not retained because the gate only
/// needs the yes/no decision before serving.
#[derive(Debug)]
pub struct ValidatedToolSchema {
    name: String,
}

impl ValidatedToolSchema {
    /// The tool name this validated schema belongs to. Part of the validated
    /// surface (mirroring `schema_validation::CompiledSchema::is_valid`);
    /// exercised by tests and future full-schema callers that key off the
    /// validated name, while the current gate only needs the yes/no decision.
    #[allow(dead_code)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Validate one tool's full schema offline (ADR-030). Refuses external
    /// references and non-2020-12 documents (via the WP05 validator) and local
    /// recursive reference cycles. Never fetches a resource.
    pub fn validate(
        tool: &str,
        schema: &Value,
    ) -> Result<ValidatedToolSchema, SchemaContractError> {
        // 1. Compile with the WP05 Draft 2020-12 validator (ADR-024). This
        //    refuses external references (resolver features disabled, no I/O)
        //    and any document that is not usable 2020-12.
        CompiledSchema::compile(schema).map_err(|err| match err {
            SchemaError::ExternalReference(detail) => SchemaContractError::ExternalReference {
                tool: tool.to_string(),
                detail,
            },
            SchemaError::Invalid(detail) => SchemaContractError::Invalid {
                tool: tool.to_string(),
                detail,
            },
        })?;
        // 2. A 2020-12 compiler accepts recursive schemas; the full-schema mode
        //    does not (PM A to Q-FR-15-2). Reject local `$ref` cycles.
        detect_recursion(tool, schema)?;
        Ok(ValidatedToolSchema {
            name: tool.to_string(),
        })
    }

    /// Validate every schema in a composed catalog and reject cross-tool
    /// definition-name collisions (FR-15 R3). The slice order is preserved in
    /// the returned vector.
    pub fn validate_catalog(
        entries: &[(&str, &Value)],
    ) -> Result<Vec<ValidatedToolSchema>, SchemaContractError> {
        let mut out = Vec::with_capacity(entries.len());
        // Owner of each definition name seen so far, so a second tool defining
        // the same name is rejected rather than silently shadowing it when a
        // caller combines the schemas (FR-15 R3, ADR-030).
        let mut owners: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for (name, schema) in entries {
            out.push(ValidatedToolSchema::validate(name, schema)?);
            for def_name in definition_names(schema) {
                if let Some(first_tool) = owners.get(&def_name) {
                    if first_tool != name {
                        return Err(SchemaContractError::DefinitionCollision {
                            name: def_name,
                            first_tool: first_tool.clone(),
                            second_tool: name.to_string(),
                        });
                    }
                } else {
                    owners.insert(def_name, name.to_string());
                }
            }
        }
        Ok(out)
    }
}

/// The local JSON-pointer referenced by a local `$ref` fragment, or `None` when
/// the reference is not a document-local JSON pointer (an external scheme —
/// already refused at compile — or a plain-name `$anchor`, which the WP05
/// compiler validated as resolvable and which the native catalog does not use).
fn local_pointer(reference: &str) -> Option<&str> {
    let fragment = reference.strip_prefix('#')?;
    // `#` alone is the whole document; `#/...` is a JSON pointer. A `#name`
    // plain-name anchor is not a pointer and is not followed here.
    if fragment.is_empty() || fragment.starts_with('/') {
        Some(fragment)
    } else {
        None
    }
}

/// Detect a recursive local `$ref` cycle (ADR-030). Standard depth-first cycle
/// detection over the document's reference graph: structural descent carries
/// the active reference path unchanged, and only a `$ref` hop extends it. A hop
/// to a pointer already on the path is a cycle; a pointer fully explored with
/// no cycle through it is cached so a shared (diamond) definition is visited
/// once, keeping a DAG linear.
fn detect_recursion(tool: &str, root: &Value) -> Result<(), SchemaContractError> {
    let mut on_path: BTreeSet<String> = BTreeSet::new();
    let mut acyclic: BTreeSet<String> = BTreeSet::new();
    let mut steps: usize = 0;
    expand(tool, root, root, &mut on_path, &mut acyclic, &mut steps)
}

fn expand(
    tool: &str,
    root: &Value,
    node: &Value,
    on_path: &mut BTreeSet<String>,
    acyclic: &mut BTreeSet<String>,
    steps: &mut usize,
) -> Result<(), SchemaContractError> {
    *steps += 1;
    if *steps > MAX_ANALYSIS_STEPS {
        return Err(SchemaContractError::Invalid {
            tool: tool.to_string(),
            detail: "schema is too large to analyze statically".to_string(),
        });
    }
    match node {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                if let Some(pointer) = local_pointer(reference) {
                    if on_path.contains(pointer) {
                        return Err(SchemaContractError::Recursive {
                            tool: tool.to_string(),
                            pointer: format!("#{pointer}"),
                        });
                    }
                    if !acyclic.contains(pointer) {
                        if let Some(target) = root.pointer(pointer) {
                            on_path.insert(pointer.to_string());
                            expand(tool, root, target, on_path, acyclic, steps)?;
                            on_path.remove(pointer);
                            acyclic.insert(pointer.to_string());
                        }
                    }
                }
            }
            for (key, value) in map {
                if key == "$ref" {
                    continue;
                }
                expand(tool, root, value, on_path, acyclic, steps)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                expand(tool, root, item, on_path, acyclic, steps)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The top-level definition names a schema declares under `$defs` and the
/// legacy `definitions` keyword, the namespace a caller would merge when
/// combining tool schemas (FR-15 R3).
fn definition_names(schema: &Value) -> Vec<String> {
    let mut names = Vec::new();
    for keyword in ["$defs", "definitions"] {
        if let Some(Value::Object(defs)) = schema.get(keyword) {
            names.extend(defs.keys().cloned());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_a_valid_self_contained_schema_with_local_refs() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "id": { "$ref": "#/$defs/id" },
                "tags": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["id"],
            "$defs": { "id": { "type": "string", "minLength": 4 } }
        });
        let v = ValidatedToolSchema::validate("sample", &schema).expect("valid schema");
        assert_eq!(v.name(), "sample");
    }

    #[test]
    fn rejects_external_file_reference_as_schema_unusable() {
        let schema = json!({ "$ref": "file:///etc/passwd" });
        let err = ValidatedToolSchema::validate("leaky", &schema)
            .expect_err("external file reference must be refused");
        assert!(
            matches!(err, SchemaContractError::ExternalReference { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn rejects_external_http_reference_as_schema_unusable() {
        let schema = json!({
            "type": "object",
            "properties": { "x": { "$ref": "https://example.com/remote.json" } }
        });
        let err = ValidatedToolSchema::validate("leaky", &schema)
            .expect_err("external http reference must be refused");
        assert!(
            matches!(err, SchemaContractError::ExternalReference { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn rejects_not_valid_2020_12_as_schema_unusable() {
        // `type` must be a string or array of strings, not a number.
        let schema = json!({ "type": 7 });
        let err = ValidatedToolSchema::validate("bad", &schema)
            .expect_err("a non-2020-12 document must be refused");
        assert!(
            matches!(err, SchemaContractError::Invalid { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn rejects_direct_recursive_reference_cycle() {
        // A node that references itself: a tree with recursive children.
        let schema = json!({
            "type": "object",
            "properties": { "child": { "$ref": "#/$defs/node" } },
            "$defs": {
                "node": {
                    "type": "object",
                    "properties": { "next": { "$ref": "#/$defs/node" } }
                }
            }
        });
        let err = ValidatedToolSchema::validate("tree", &schema)
            .expect_err("a recursive reference cycle must be refused");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn rejects_indirect_recursive_reference_cycle() {
        // a -> b -> a is still a cycle.
        let schema = json!({
            "$ref": "#/$defs/a",
            "$defs": {
                "a": { "properties": { "b": { "$ref": "#/$defs/b" } } },
                "b": { "properties": { "a": { "$ref": "#/$defs/a" } } }
            }
        });
        let err = ValidatedToolSchema::validate("mutual", &schema)
            .expect_err("a mutual reference cycle must be refused");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn accepts_a_shared_non_recursive_definition_referenced_twice() {
        // A diamond (two refs to one leaf) is a DAG, not a cycle: must pass.
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "$ref": "#/$defs/leaf" },
                "b": { "$ref": "#/$defs/leaf" }
            },
            "$defs": { "leaf": { "type": "string" } }
        });
        ValidatedToolSchema::validate("diamond", &schema).expect("a DAG is not recursive");
    }

    #[test]
    fn catalog_accepts_distinct_definition_names() {
        let a = json!({ "type": "object", "$defs": { "alpha_id": { "type": "string" } } });
        let b = json!({ "type": "object", "$defs": { "beta_id": { "type": "string" } } });
        let entries: [(&str, &Value); 2] = [("alpha", &a), ("beta", &b)];
        let validated = ValidatedToolSchema::validate_catalog(&entries).expect("distinct names");
        assert_eq!(validated.len(), 2);
    }

    #[test]
    fn catalog_rejects_colliding_definition_names_across_tools() {
        let a = json!({ "type": "object", "$defs": { "shared": { "type": "string" } } });
        let b = json!({ "type": "object", "$defs": { "shared": { "type": "number" } } });
        let entries: [(&str, &Value); 2] = [("alpha", &a), ("beta", &b)];
        let err = ValidatedToolSchema::validate_catalog(&entries)
            .expect_err("a definition name defined by two tools must be refused");
        assert!(
            matches!(err, SchemaContractError::DefinitionCollision { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }
}
