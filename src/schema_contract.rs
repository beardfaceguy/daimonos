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

use std::collections::BTreeMap;

use serde_json::Value;

use crate::schema_validation::{CompiledSchema, SchemaError};

/// Internal bound on static-analysis work for one schema. A document that
/// exceeds it is treated as statically unusable (`schema_unusable`, exit 20 —
/// never a signal) rather than explored forever. The cycle search is iterative
/// (WP17 r5 / W1, issues D-WP17-13) and its work is linear in the document
/// size (each reference target is explored once), so this cap only guards a
/// genuinely pathological document with millions of schema positions. It is
/// set far above any realistic tool schema and above the deepest chains the
/// integrator exercised (a 100000-definition `$ref` chain visits ~400k nodes);
/// the native tool schemas are tiny.
const MAX_ANALYSIS_STEPS: usize = 10_000_000;

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
    /// A `$ref`/`$dynamicRef` the WP05 validator accepted that the detector
    /// cannot resolve inside the document (integrator decision D41, fail
    /// closed). Refused without any fetch; the detail names the reference.
    UnresolvableReference { tool: String, detail: String },
    /// Two schema positions declare the same resource identifier (`$id`
    /// resolving to one URI) or the same anchor (`$anchor`/`$dynamicAnchor`
    /// under one base URI) at different locations. The static index refuses to
    /// replace an entry (integrator decision D46 rule 2): a reference would be
    /// ambiguous, so the document is unusable. The detail names the duplicate.
    DuplicateIdentifier { tool: String, detail: String },
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
            SchemaContractError::UnresolvableReference { tool, detail } => write!(
                f,
                "tool `{tool}` schema has a reference that cannot be resolved inside the document: {detail}"
            ),
            SchemaContractError::DuplicateIdentifier { tool, detail } => write!(
                f,
                "tool `{tool}` schema declares a duplicate identifier: {detail}"
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

/// One schema in a composed catalog, tagged by provenance (S3 / integrator
/// decision D39). `owned` is true for a definition dbunk itself composes
/// (native tools, worker tools): only owned definition names participate in the
/// cross-tool collision namespace (ADR-030 "when composing owned definitions").
/// A supplied (caller-owned final-answer or registered) schema is validated for
/// self-containment but is NEVER refused because of a definition name.
pub struct CatalogSchema<'a> {
    /// The tool the schema belongs to (for error attribution).
    pub tool: &'a str,
    /// The full tool input schema to validate.
    pub schema: &'a Value,
    /// Whether dbunk owns this definition (participates in the collision
    /// namespace). Supplied schemas pass `false`.
    pub owned: bool,
}

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
        entries: &[CatalogSchema],
    ) -> Result<Vec<ValidatedToolSchema>, SchemaContractError> {
        let mut out = Vec::with_capacity(entries.len());
        // Owner of each OWNED definition name seen so far, so a second owned tool
        // defining the same name is rejected rather than silently shadowing it
        // when a caller combines the schemas (FR-15 R3, ADR-030).
        let mut owners: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for entry in entries {
            let CatalogSchema {
                tool,
                schema,
                owned,
            } = *entry;
            out.push(ValidatedToolSchema::validate(tool, schema)?);
            // D39 / ADR-030: only OWNED definitions share a namespace. A supplied
            // schema is validated for self-containment above but its definition
            // names are never recorded or checked, so it is never refused
            // because of a definition name.
            if !owned {
                continue;
            }
            for def_name in definition_names(schema) {
                if let Some(first_tool) = owners.get(&def_name) {
                    if first_tool != tool {
                        return Err(SchemaContractError::DefinitionCollision {
                            name: def_name,
                            first_tool: first_tool.clone(),
                            second_tool: tool.to_string(),
                        });
                    }
                } else {
                    owners.insert(def_name, tool.to_string());
                }
            }
        }
        Ok(out)
    }
}

/// WP17 r2 (Gap 1 / D39): recursion-only check for a *supplied* caller-owned
/// schema (the WP05 `--final-output` final-answer schema, and any other
/// caller-supplied tool schema) under full-schema mode. ADR-030: caller-owned
/// worker and final-answer schemas are validated and offered unchanged, yet a
/// local recursive `$ref` cycle is still refused as `schema_unusable` before any
/// model request (PM A to Q-FR-15-2).
///
/// Only the recursion cycle is checked here, deliberately NOT the cross-tool
/// definition-name collision: that rule is scoped to OWNED definitions dbunk
/// composes (integrator decision D39, ADR-030 "when composing owned
/// definitions"); a supplied schema is never refused because of a definition
/// name. External references and non-2020-12 validity are already refused on the
/// WP05 final-output path by [`CompiledSchema::compile`]; this adds only the
/// cycle check that a 2020-12 compiler accepts on its own.
pub fn reject_recursive_supplied_schema(
    tool: &str,
    schema: &Value,
) -> Result<(), SchemaContractError> {
    detect_recursion(tool, schema)
}

/// The outcome of resolving a single `$ref`/`$dynamicRef` reference against the
/// document (ADR-030: "proper resource semantics, not a string-prefix check").
enum RefEdge {
    /// The reference resolves to a node inside the document, named by its
    /// canonical JSON pointer from the document root.
    To(String),
    /// A reference the WP05 validator accepted that this detector cannot resolve
    /// inside the document. It is named, never fetched (integrator D41,
    /// fail-closed). Carried as a human-readable detail; whether it is refused
    /// is decided by the caller.
    Unresolvable(String),
}

/// A static index of one schema document's JSON-Schema resources and anchors,
/// built once before cycle detection so every in-document reference form
/// resolves to a canonical pointer with correct 2020-12 URI/anchor semantics
/// (ADR-030). It records nothing outside the supplied document and never
/// performs I/O.
struct SchemaIndex {
    /// Base URI (no fragment) of each resource -> its canonical pointer. The
    /// document root is always present (under its `$id` base, or the empty
    /// default base when it declares none).
    resources: std::collections::BTreeMap<String, String>,
    /// (resource base URI, plain name) -> canonical pointer, for `$anchor` and
    /// (lexically) `$dynamicAnchor`.
    anchors: std::collections::BTreeMap<(String, String), String>,
    /// (resource base URI, plain name) -> canonical pointer for `$dynamicAnchor`
    /// only, the lexical target of a `$dynamicRef`.
    dynamic_anchors: std::collections::BTreeMap<(String, String), String>,
    /// Canonical pointer -> the base URI in force at that node (after applying
    /// its own `$id`), so a reference found there resolves against the right
    /// base.
    node_base: std::collections::BTreeMap<String, String>,
}

impl SchemaIndex {
    /// Index every resource (`$id`), anchor (`$anchor`/`$dynamicAnchor`), and
    /// node base URI in `root`, visiting only the JSON positions that are
    /// schemas (integrator decision D46). An identifier found in DATA
    /// (`const`/`enum`/`default`/`examples`) is never indexed; an unknown
    /// keyword is read conservatively (fail closed). The index never replaces
    /// an entry: a duplicate `$id`/anchor at a second location is refused.
    fn build(tool: &str, root: &Value) -> Result<SchemaIndex, SchemaContractError> {
        let mut idx = SchemaIndex {
            resources: std::collections::BTreeMap::new(),
            anchors: std::collections::BTreeMap::new(),
            dynamic_anchors: std::collections::BTreeMap::new(),
            node_base: std::collections::BTreeMap::new(),
        };
        // A root without `$id` resolves document-local pointers against the
        // empty default base.
        idx.resources.insert(String::new(), String::new());
        // The document root is always a schema position.
        idx.index_schema(tool, root, String::new(), String::new())?;
        Ok(idx)
    }

    /// Record one resource `$id`, refusing a second `$id` that resolves to a
    /// URI already claimed at a different location (D46 rule 2, never replace).
    fn insert_resource(
        &mut self,
        tool: &str,
        uri: String,
        pointer: String,
    ) -> Result<(), SchemaContractError> {
        if let Some(existing) = self.resources.get(&uri) {
            if existing != &pointer {
                return Err(SchemaContractError::DuplicateIdentifier {
                    tool: tool.to_string(),
                    detail: format!("`$id` `{uri}` is declared at both #{existing} and #{pointer}"),
                });
            }
        }
        self.resources.insert(uri, pointer);
        Ok(())
    }

    /// Record one `$anchor`, refusing a second anchor with the same base URI
    /// and name at a different location (D46 rule 2, never replace).
    fn insert_anchor(
        &mut self,
        tool: &str,
        base: String,
        name: String,
        pointer: String,
    ) -> Result<(), SchemaContractError> {
        let key = (base, name);
        if let Some(existing) = self.anchors.get(&key) {
            if existing != &pointer {
                return Err(SchemaContractError::DuplicateIdentifier {
                    tool: tool.to_string(),
                    detail: format!(
                        "anchor `{}` under base `{}` is declared at both #{existing} and #{pointer}",
                        key.1, key.0
                    ),
                });
            }
        }
        self.anchors.insert(key, pointer);
        Ok(())
    }

    /// Record one `$dynamicAnchor`, refusing a duplicate (D46 rule 2). It also
    /// answers a lexical plain-name `$ref` (fallback into `anchors`, a
    /// convenience index that is not re-checked for duplication here).
    fn insert_dynamic_anchor(
        &mut self,
        tool: &str,
        base: String,
        name: String,
        pointer: String,
    ) -> Result<(), SchemaContractError> {
        let key = (base, name);
        if let Some(existing) = self.dynamic_anchors.get(&key) {
            if existing != &pointer {
                return Err(SchemaContractError::DuplicateIdentifier {
                    tool: tool.to_string(),
                    detail: format!(
                        "dynamic anchor `{}` under base `{}` is declared at both #{existing} and #{pointer}",
                        key.1, key.0
                    ),
                });
            }
        }
        self.dynamic_anchors.insert(key.clone(), pointer.clone());
        self.anchors.entry(key).or_insert(pointer);
        Ok(())
    }

    /// Index the identifiers a schema object declares (`$id`/`$anchor`/
    /// `$dynamicAnchor`) and record its node base URI; return the base URI in
    /// force inside it.
    fn index_identifiers(
        &mut self,
        tool: &str,
        map: &serde_json::Map<String, Value>,
        base: String,
        pointer: &str,
    ) -> Result<String, SchemaContractError> {
        let mut cur_base = base;
        if let Some(Value::String(id)) = map.get("$id") {
            let (id_nofrag, _) = split_fragment(id);
            cur_base = resolve_uri(&cur_base, id_nofrag);
            self.insert_resource(tool, cur_base.clone(), pointer.to_string())?;
        }
        self.node_base.insert(pointer.to_string(), cur_base.clone());
        if let Some(Value::String(a)) = map.get("$anchor") {
            self.insert_anchor(tool, cur_base.clone(), a.clone(), pointer.to_string())?;
        }
        if let Some(Value::String(d)) = map.get("$dynamicAnchor") {
            self.insert_dynamic_anchor(tool, cur_base.clone(), d.clone(), pointer.to_string())?;
        }
        Ok(cur_base)
    }

    /// Index a SCHEMA position (D46 typed traversal). Only schema children are
    /// descended as schemas; data keywords are skipped; unknown keywords are
    /// read conservatively.
    fn index_schema(
        &mut self,
        tool: &str,
        node: &Value,
        base: String,
        pointer: String,
    ) -> Result<(), SchemaContractError> {
        let Value::Object(map) = node else {
            return Ok(());
        };
        let cur_base = self.index_identifiers(tool, map, base, &pointer)?;
        for (key, value) in map {
            let child = format!("{pointer}/{}", escape_token(key));
            match classify_keyword(key) {
                KeywordKind::Reference | KeywordKind::Data => {}
                KeywordKind::Schema => self.index_schema(tool, value, cur_base.clone(), child)?,
                KeywordKind::SchemaArray => {
                    self.index_schema_array(tool, value, cur_base.clone(), &child)?
                }
                KeywordKind::SchemaMap => {
                    self.index_schema_map(tool, value, cur_base.clone(), &child)?
                }
                KeywordKind::Items => {
                    if value.is_array() {
                        self.index_schema_array(tool, value, cur_base.clone(), &child)?
                    } else {
                        self.index_schema(tool, value, cur_base.clone(), child)?
                    }
                }
                KeywordKind::Dependencies => {
                    self.index_dependencies(tool, value, cur_base.clone(), &child)?
                }
                KeywordKind::Other => {
                    self.index_conservative(tool, value, cur_base.clone(), child)?
                }
            }
        }
        Ok(())
    }

    /// Index every element of an array-of-schemas keyword as a schema.
    fn index_schema_array(
        &mut self,
        tool: &str,
        value: &Value,
        base: String,
        pointer: &str,
    ) -> Result<(), SchemaContractError> {
        if let Value::Array(items) = value {
            for (i, item) in items.iter().enumerate() {
                self.index_schema(tool, item, base.clone(), format!("{pointer}/{i}"))?;
            }
        }
        Ok(())
    }

    /// Index every value of a map-from-names-to-schemas keyword as a schema.
    /// Each key is a NAME, never a keyword.
    fn index_schema_map(
        &mut self,
        tool: &str,
        value: &Value,
        base: String,
        pointer: &str,
    ) -> Result<(), SchemaContractError> {
        if let Value::Object(entries) = value {
            for (name, sub) in entries {
                self.index_schema(
                    tool,
                    sub,
                    base.clone(),
                    format!("{pointer}/{}", escape_token(name)),
                )?;
            }
        }
        Ok(())
    }

    /// Index the legacy `dependencies` keyword: each value is a subschema OR an
    /// array of required-property names (not a schema).
    fn index_dependencies(
        &mut self,
        tool: &str,
        value: &Value,
        base: String,
        pointer: &str,
    ) -> Result<(), SchemaContractError> {
        if let Value::Object(entries) = value {
            for (name, sub) in entries {
                if sub.is_object() {
                    self.index_schema(
                        tool,
                        sub,
                        base.clone(),
                        format!("{pointer}/{}", escape_token(name)),
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Conservative index walk of a value under an UNKNOWN keyword (fail closed,
    /// D46): descend into every key/element and still record any `$id`/anchor
    /// found, so an identifier hidden under a keyword the check does not model
    /// is never silently ignored.
    fn index_conservative(
        &mut self,
        tool: &str,
        node: &Value,
        base: String,
        pointer: String,
    ) -> Result<(), SchemaContractError> {
        match node {
            Value::Object(map) => {
                let cur_base = self.index_identifiers(tool, map, base, &pointer)?;
                for (key, value) in map {
                    let child = format!("{pointer}/{}", escape_token(key));
                    self.index_conservative(tool, value, cur_base.clone(), child)?;
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.index_conservative(tool, item, base.clone(), format!("{pointer}/{i}"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Resolve one reference found at `pointer` to a canonical target pointer
    /// (ADR-030). `dynamic` is true for a `$dynamicRef`. Every in-document form
    /// is resolved with proper URI/anchor semantics: a percent-/`~`-decoded
    /// JSON-pointer fragment (relative to the enclosing resource), a plain-name
    /// `$anchor` (or `$dynamicAnchor` for a `$dynamicRef`), the whole-resource
    /// `#`, and a `$ref` that names the document's or an embedded resource's
    /// `$id` (absolute or relative to the base, with or without a fragment). A
    /// reference that does not resolve inside the document is
    /// [`RefEdge::Unresolvable`] (fail closed, D41), never fetched.
    /// Context: docs/adr/030-self-contained-full-schemas.md.
    fn resolve(&self, root: &Value, pointer: &str, reference: &str, dynamic: bool) -> RefEdge {
        let base = self.node_base.get(pointer).cloned().unwrap_or_default();
        let (nofrag, frag) = split_fragment(reference);
        // Resolve the pre-fragment part against the enclosing base URI (T1.4):
        // an empty part keeps the current resource; otherwise it may name the
        // document's own `$id` or an embedded resource's `$id`, absolute or
        // relative to the base.
        let target_base = if nofrag.is_empty() {
            base
        } else {
            resolve_uri(&base, nofrag)
        };
        let resource_root = match self.resources.get(&target_base) {
            Some(root_ptr) => root_ptr.clone(),
            None => {
                return RefEdge::Unresolvable(format!(
                    "reference `{reference}` resolves to base URI `{target_base}`, which is not a resource embedded in the document"
                ))
            }
        };
        match frag {
            // JSON pointer: percent-decode first, then serde_json decodes the
            // RFC-6901 `~1`/`~0` escapes. The pointer is relative to the
            // enclosing resource root.
            Some(f) if f.starts_with('/') => {
                let target = format!("{resource_root}{}", percent_decode(f));
                if root.pointer(&target).is_some() {
                    RefEdge::To(target)
                } else {
                    RefEdge::Unresolvable(format!(
                        "JSON pointer reference `{reference}` does not resolve inside the document"
                    ))
                }
            }
            // Whole-resource reference (`#` or the resource's own base URI with
            // no fragment) targets the resource root.
            Some("") => RefEdge::To(resource_root),
            None => RefEdge::To(resource_root),
            // Plain-name fragment (T1.2): resolve through `$anchor` (and, for a
            // `$dynamicRef`, `$dynamicAnchor`) declared in the enclosing
            // resource, keyed by that resource's base URI.
            Some(name) => {
                let key = (target_base.clone(), percent_decode(name));
                let primary = if dynamic {
                    self.dynamic_anchors.get(&key)
                } else {
                    self.anchors.get(&key)
                };
                match primary.or_else(|| self.anchors.get(&key)) {
                    Some(ptr) => RefEdge::To(ptr.clone()),
                    None => RefEdge::Unresolvable(format!(
                        "plain-name anchor reference `{reference}` has no matching anchor in the document"
                    )),
                }
            }
        }
    }
}

/// Split a reference into its pre-fragment part and its fragment (without the
/// `#`). `None` fragment means the reference carried no `#` at all.
fn split_fragment(reference: &str) -> (&str, Option<&str>) {
    match reference.find('#') {
        Some(i) => (&reference[..i], Some(&reference[i + 1..])),
        None => (reference, None),
    }
}

/// Escape one object key as a single RFC-6901 JSON-pointer token (`~`->`~0`,
/// `/`->`~1`), so canonical pointers built here round-trip through
/// [`Value::pointer`].
fn escape_token(token: &str) -> String {
    token.replace('~', "~0").replace('/', "~1")
}

/// Percent-decode a URI fragment (RFC-3986 `%XX`). Invalid escapes are left
/// verbatim. The result is a JSON pointer whose `~1`/`~0` escapes are decoded
/// afterwards by [`Value::pointer`].
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal RFC-3986 reference resolution (sections 5.2-5.3), enough for the
/// self-contained `$id`/`$ref` base URIs full-schema mode meets. No crate is
/// added (ADR-030 forbids new dependencies on this path); only the relative
/// resolution the detector needs is implemented. `reference` must be the
/// pre-fragment part.
fn resolve_uri(base: &str, reference: &str) -> String {
    let b = parse_uri(base);
    let r = parse_uri(reference);
    let scheme;
    let authority;
    let path;
    let query;
    if r.scheme.is_some() {
        scheme = r.scheme;
        authority = r.authority;
        path = remove_dot_segments(&r.path);
        query = r.query;
    } else if r.authority.is_some() {
        scheme = b.scheme;
        authority = r.authority;
        path = remove_dot_segments(&r.path);
        query = r.query;
    } else if r.path.is_empty() {
        scheme = b.scheme;
        authority = b.authority;
        path = b.path;
        query = if r.query.is_some() { r.query } else { b.query };
    } else {
        scheme = b.scheme;
        authority = b.authority.clone();
        let merged = if r.path.starts_with('/') {
            r.path.clone()
        } else {
            merge_paths(&b.authority, &b.path, &r.path)
        };
        path = remove_dot_segments(&merged);
        query = r.query;
    }
    compose_uri(
        scheme.as_deref(),
        authority.as_deref(),
        &path,
        query.as_deref(),
    )
}

struct UriParts {
    scheme: Option<String>,
    authority: Option<String>,
    path: String,
    query: Option<String>,
}

fn parse_uri(s: &str) -> UriParts {
    let mut rest = s;
    let mut scheme = None;
    if let Some(colon) = rest.find(':') {
        let cand = &rest[..colon];
        let no_delim_before = !cand.contains(['/', '?', '#']);
        let valid = !cand.is_empty()
            && cand
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            && cand
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.');
        if no_delim_before && valid {
            scheme = Some(cand.to_string());
            rest = &rest[colon + 1..];
        }
    }
    let mut authority = None;
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        authority = Some(after[..end].to_string());
        rest = &after[end..];
    }
    let mut query = None;
    let path;
    if let Some(q) = rest.find('?') {
        path = rest[..q].to_string();
        query = Some(rest[q + 1..].to_string());
    } else {
        path = rest.to_string();
    }
    UriParts {
        scheme,
        authority,
        path,
        query,
    }
}

fn merge_paths(base_authority: &Option<String>, base_path: &str, ref_path: &str) -> String {
    if base_authority.is_some() && base_path.is_empty() {
        format!("/{ref_path}")
    } else if let Some(i) = base_path.rfind('/') {
        format!("{}{ref_path}", &base_path[..=i])
    } else {
        ref_path.to_string()
    }
}

fn remove_dot_segments(path: &str) -> String {
    let mut input = path.to_string();
    let mut out = String::new();
    while !input.is_empty() {
        if let Some(rest) = input.strip_prefix("../") {
            input = rest.to_string();
        } else if let Some(rest) = input.strip_prefix("./") {
            input = rest.to_string();
        } else if input == "/." {
            input = "/".to_string();
        } else if let Some(rest) = input.strip_prefix("/./") {
            input = format!("/{rest}");
        } else if input == "/.." {
            input = "/".to_string();
            pop_last_segment(&mut out);
        } else if let Some(rest) = input.strip_prefix("/../") {
            input = format!("/{rest}");
            pop_last_segment(&mut out);
        } else if input == "." || input == ".." {
            input.clear();
        } else {
            let start = if input.starts_with('/') { 1 } else { 0 };
            let next = input[start..]
                .find('/')
                .map(|i| start + i)
                .unwrap_or(input.len());
            out.push_str(&input[..next]);
            input = input[next..].to_string();
        }
    }
    out
}

fn pop_last_segment(out: &mut String) {
    if let Some(i) = out.rfind('/') {
        out.truncate(i);
    } else {
        out.clear();
    }
}

fn compose_uri(
    scheme: Option<&str>,
    authority: Option<&str>,
    path: &str,
    query: Option<&str>,
) -> String {
    let mut out = String::new();
    if let Some(s) = scheme {
        out.push_str(s);
        out.push(':');
    }
    if let Some(a) = authority {
        out.push_str("//");
        out.push_str(a);
    }
    out.push_str(path);
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
    }
    out
}

/// Detect a recursive reference cycle (ADR-030) across every in-document
/// reference form the detector resolves. Depth-first cycle detection over the
/// document's reference graph: structural descent carries the active reference
/// path unchanged, and only a `$ref`/`$dynamicRef` hop extends it. A hop to a
/// target already on the path is a cycle; a target fully explored with no cycle
/// through it is cached so a shared (diamond) definition is visited once,
/// keeping a DAG linear.
/// The depth-first colour of one reference-target node in the iterative cycle
/// search (WP17 r5 / W1). `Gray` is a node on the active path — a reference
/// back-edge to it is a recursive cycle; `Black` is a node fully explored with
/// no cycle through it (the diamond cache a shared definition reuses). A node
/// not in the map is unvisited (white).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    Gray,
    Black,
}

/// One entry of the explicit depth-first work list (WP17 r5 / W1). `Enter`
/// begins exploring a reference target (collecting its out-edges); the matching
/// `Exit`, pushed beneath its children, marks it fully explored.
enum Visit {
    Enter(String),
    Exit(String),
}

fn detect_recursion(tool: &str, root: &Value) -> Result<(), SchemaContractError> {
    let index = SchemaIndex::build(tool, root)?;
    let mut search = CycleSearch {
        tool,
        index: &index,
        root,
        pending: Vec::new(),
        steps: 0,
    };
    // Iterative depth-first cycle search over the document's reference graph
    // (WP17 r5 / W1, issues D-WP17-13). The previous version recursed through
    // every `$ref` hop (`follow_edges` -> `schema` -> `follow_edges` ...), so a
    // chain of N local definitions recursed N deep and overflowed the thread
    // stack for a large but finite, non-recursive schema. The work list lives
    // on the heap, so the reference depth no longer touches the stack; only the
    // per-node structural walk still recurses, and that is bounded by the
    // document's JSON nesting depth, which the JSON parser already caps. A node
    // is keyed by its canonical pointer (the document root is `""`).
    let mut color: BTreeMap<String, Color> = BTreeMap::new();
    let mut stack: Vec<Visit> = vec![Visit::Enter(String::new())];
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Enter(pointer) => {
                // Already fully explored, or already on the active path and
                // re-queued by a diamond edge: skip.
                if color.contains_key(&pointer) {
                    continue;
                }
                let Some(node) = root.pointer(&pointer) else {
                    // A target that resolves to no node: `resolve` already
                    // fails closed for a reference it cannot place in the
                    // document, so simply mark it explored.
                    color.insert(pointer, Color::Black);
                    continue;
                };
                color.insert(pointer.clone(), Color::Gray);
                // Collect this node's out-edges with a bounded structural walk
                // of its subtree; `schema` fills `pending` and never recurses
                // into a reference target itself.
                search.bump()?;
                search.pending.clear();
                search.schema(node, &pointer)?;
                let edges = std::mem::take(&mut search.pending);
                // Exit is pushed beneath the children so it runs only after the
                // whole subtree is explored (keeps `color` a correct path set).
                stack.push(Visit::Exit(pointer));
                for target in edges {
                    match color.get(&target) {
                        // A back-edge to a node still on the active path is the
                        // recursive cycle.
                        Some(Color::Gray) => {
                            return Err(SchemaContractError::Recursive {
                                tool: tool.to_string(),
                                pointer: format!("#{target}"),
                            });
                        }
                        // Already proven acyclic (diamond / shared definition).
                        Some(Color::Black) => {}
                        None => stack.push(Visit::Enter(target)),
                    }
                }
            }
            Visit::Exit(pointer) => {
                color.insert(pointer, Color::Black);
            }
        }
    }
    Ok(())
}

/// How a keyword's value is traversed inside a schema object (JSON Schema
/// 2020-12), per integrator decision D46 rule 1. A reference keyword has a
/// meaning only in a schema position; a data keyword is never followed; an
/// unknown keyword is read conservatively (fail closed). This classification is
/// written from the 2020-12 keyword vocabulary (core, applicator, unevaluated,
/// and the legacy `definitions`/`dependencies`/`additionalItems` forms).
enum KeywordKind {
    /// `$ref` / `$dynamicRef`: a reference edge.
    Reference,
    /// `const` / `enum` / `default` / `examples`: DATA, never a schema.
    Data,
    /// Value is a single subschema.
    Schema,
    /// Value is an array of subschemas.
    SchemaArray,
    /// Value is a map from names to subschemas.
    SchemaMap,
    /// `items`: an array value is an array of subschemas; otherwise a single
    /// subschema.
    Items,
    /// `dependencies`: a map from names to (subschema | array of names).
    Dependencies,
    /// Any other keyword: read conservatively (fail closed).
    Other,
}

/// Classify one object key as a 2020-12 schema keyword (D46 rule 1). Only used
/// when the key is a KEYWORD (a direct child of a schema object); inside a
/// name->schema map each key is a NAME and is never classified.
fn classify_keyword(key: &str) -> KeywordKind {
    match key {
        "$ref" | "$dynamicRef" => KeywordKind::Reference,
        "const" | "enum" | "default" | "examples" => KeywordKind::Data,
        "not"
        | "if"
        | "then"
        | "else"
        | "contains"
        | "additionalProperties"
        | "propertyNames"
        | "unevaluatedItems"
        | "unevaluatedProperties"
        | "contentSchema"
        | "additionalItems" => KeywordKind::Schema,
        "allOf" | "anyOf" | "oneOf" | "prefixItems" => KeywordKind::SchemaArray,
        "items" => KeywordKind::Items,
        "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
            KeywordKind::SchemaMap
        }
        "dependencies" => KeywordKind::Dependencies,
        _ => KeywordKind::Other,
    }
}

/// One document's reference-graph edge collector (ADR-030). A structural walk
/// of a single node's subtree ([`CycleSearch::schema`]) gathers every
/// `$ref`/`$dynamicRef` target found in it into [`Self::pending`] WITHOUT
/// following any of them; the iterative driver ([`detect_recursion`]) does the
/// depth-first cycle search over those edges on an explicit heap work list, so
/// the reference depth never touches the thread stack (WP17 r5 / W1). The
/// structural descent is typed (D46): only schema positions are followed as
/// schemas, data keywords are skipped, and an unknown keyword is walked
/// conservatively. The structural recursion is bounded by the document's JSON
/// nesting depth, which the JSON parser already caps.
struct CycleSearch<'a> {
    tool: &'a str,
    index: &'a SchemaIndex,
    root: &'a Value,
    /// Resolved reference targets collected from the subtree currently being
    /// walked; drained by the driver after each node's structural walk.
    pending: Vec<String>,
    /// Static-analysis step budget guard.
    steps: usize,
}

impl<'a> CycleSearch<'a> {
    fn bump(&mut self) -> Result<(), SchemaContractError> {
        self.steps += 1;
        if self.steps > MAX_ANALYSIS_STEPS {
            return Err(SchemaContractError::Invalid {
                tool: self.tool.to_string(),
                detail: "schema is too large to analyze statically".to_string(),
            });
        }
        Ok(())
    }

    /// Collect the `$ref`/`$dynamicRef` edges at one object node into
    /// [`Self::pending`], resolving each to its canonical target pointer. The
    /// edges are NOT followed here (the driver explores them iteratively); only
    /// a reference the detector cannot resolve inside the document is refused
    /// eagerly, fail-closed (integrator D41).
    fn follow_edges(
        &mut self,
        map: &serde_json::Map<String, Value>,
        pointer: &str,
    ) -> Result<(), SchemaContractError> {
        let mut edges: Vec<(String, bool)> = Vec::new();
        if let Some(Value::String(reference)) = map.get("$ref") {
            edges.push((reference.clone(), false));
        }
        if let Some(Value::String(reference)) = map.get("$dynamicRef") {
            edges.push((reference.clone(), true));
        }
        for (reference, dynamic) in edges {
            match self.index.resolve(self.root, pointer, &reference, dynamic) {
                // A reference target is always a schema position. Record it;
                // the driver places it in the reference graph and detects a
                // cycle (an edge back to a target on the active path).
                RefEdge::To(target) => self.pending.push(target),
                // Fail closed (integrator D41): a reference the WP05 validator
                // accepted that we cannot resolve inside the document is
                // refused, named, and never fetched.
                RefEdge::Unresolvable(detail) => {
                    return Err(SchemaContractError::UnresolvableReference {
                        tool: self.tool.to_string(),
                        detail,
                    });
                }
            }
        }
        Ok(())
    }

    /// Search a SCHEMA position (D46 typed traversal).
    fn schema(&mut self, node: &Value, pointer: &str) -> Result<(), SchemaContractError> {
        self.bump()?;
        let Value::Object(map) = node else {
            return Ok(());
        };
        self.follow_edges(map, pointer)?;
        for (key, value) in map {
            let child = format!("{pointer}/{}", escape_token(key));
            match classify_keyword(key) {
                KeywordKind::Reference | KeywordKind::Data => {}
                KeywordKind::Schema => self.schema(value, &child)?,
                KeywordKind::SchemaArray => self.schema_array(value, &child)?,
                KeywordKind::SchemaMap => self.schema_map(value, &child)?,
                KeywordKind::Items => {
                    if value.is_array() {
                        self.schema_array(value, &child)?
                    } else {
                        self.schema(value, &child)?
                    }
                }
                KeywordKind::Dependencies => self.dependencies(value, &child)?,
                KeywordKind::Other => self.conservative(value, &child)?,
            }
        }
        Ok(())
    }

    /// Search every element of an array-of-schemas keyword as a schema.
    fn schema_array(&mut self, value: &Value, pointer: &str) -> Result<(), SchemaContractError> {
        if let Value::Array(items) = value {
            for (i, item) in items.iter().enumerate() {
                self.schema(item, &format!("{pointer}/{i}"))?;
            }
        }
        Ok(())
    }

    /// Search every value of a map-from-names-to-schemas keyword as a schema.
    fn schema_map(&mut self, value: &Value, pointer: &str) -> Result<(), SchemaContractError> {
        if let Value::Object(entries) = value {
            for (name, sub) in entries {
                self.schema(sub, &format!("{pointer}/{}", escape_token(name)))?;
            }
        }
        Ok(())
    }

    /// Search the legacy `dependencies` keyword: only its subschema values.
    fn dependencies(&mut self, value: &Value, pointer: &str) -> Result<(), SchemaContractError> {
        if let Value::Object(entries) = value {
            for (name, sub) in entries {
                if sub.is_object() {
                    self.schema(sub, &format!("{pointer}/{}", escape_token(name)))?;
                }
            }
        }
        Ok(())
    }

    /// Conservative search of a value under an UNKNOWN keyword (fail closed,
    /// D46): descend every key/element and still follow any `$ref`/`$dynamicRef`
    /// found, so a cycle hidden under a keyword the check does not model is
    /// still refused.
    fn conservative(&mut self, node: &Value, pointer: &str) -> Result<(), SchemaContractError> {
        self.bump()?;
        match node {
            Value::Object(map) => {
                self.follow_edges(map, pointer)?;
                for (key, value) in map {
                    if key == "$ref" || key == "$dynamicRef" {
                        continue;
                    }
                    self.conservative(value, &format!("{pointer}/{}", escape_token(key)))?;
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.conservative(item, &format!("{pointer}/{i}"))?;
                }
            }
            _ => {}
        }
        Ok(())
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
    fn rejects_recursive_cycle_through_percent_encoded_pointer() {
        // T1.1: a JSON-pointer fragment is percent-decoded (then `~1`/`~0`)
        // before it is followed. The `$defs` key contains a space, written
        // `%20` in the pointer. A naive string-prefix check never decodes it,
        // so the self-reference cycle is missed; the detector must decode and
        // refuse it (ADR-030; integrator r3 T1).
        let schema = json!({
            "type": "object",
            "properties": { "child": { "$ref": "#/$defs/a%20b" } },
            "$defs": {
                "a b": {
                    "type": "object",
                    "properties": { "next": { "$ref": "#/$defs/a%20b" } }
                }
            }
        });
        let err = ValidatedToolSchema::validate("pct", &schema)
            .expect_err("a cycle through a percent-encoded pointer must be refused");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn accepts_nonrecursive_percent_encoded_pointer() {
        // T1.1 non-recursive: the same decoding resolves a non-recursive
        // pointer to a leaf, which is accepted (linear, not a cycle).
        let schema = json!({
            "type": "object",
            "properties": { "id": { "$ref": "#/$defs/a%20b" } },
            "$defs": { "a b": { "type": "string" } }
        });
        ValidatedToolSchema::validate("pct", &schema)
            .expect("a non-recursive percent-encoded pointer is accepted");
    }

    #[test]
    fn rejects_recursive_cycle_through_plain_name_anchor() {
        // T1.2: a plain-name fragment resolves through `$anchor`, against the
        // base URI of the enclosing resource. A cycle `#node -> #node` must be
        // refused (integrator r3 T1; the r2 detector followed only `#/...`).
        let schema = json!({
            "type": "object",
            "properties": { "child": { "$ref": "#node" } },
            "$defs": {
                "node": {
                    "$anchor": "node",
                    "type": "object",
                    "properties": { "next": { "$ref": "#node" } }
                }
            }
        });
        let err = ValidatedToolSchema::validate("anchor", &schema)
            .expect_err("a cycle through a plain-name $anchor must be refused");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn accepts_nonrecursive_plain_name_anchor() {
        // T1.2 non-recursive: a `#leaf` anchor referenced once resolves to a
        // leaf, accepted (linear).
        let schema = json!({
            "type": "object",
            "properties": { "id": { "$ref": "#leaf" } },
            "$defs": { "leaf": { "$anchor": "leaf", "type": "string" } }
        });
        ValidatedToolSchema::validate("anchor", &schema)
            .expect("a non-recursive $anchor reference is accepted");
    }

    #[test]
    fn rejects_recursive_cycle_through_dynamic_ref() {
        // T1.3: a `$dynamicRef` to a `$dynamicAnchor` is a reference edge. A
        // cycle through it must be refused (integrator r3 T1; the r2 detector
        // did not read `$dynamicRef` at all).
        let schema = json!({
            "type": "object",
            "properties": { "child": { "$dynamicRef": "#node" } },
            "$defs": {
                "node": {
                    "$dynamicAnchor": "node",
                    "type": "object",
                    "properties": { "next": { "$dynamicRef": "#node" } }
                }
            }
        });
        let err = ValidatedToolSchema::validate("dyn", &schema)
            .expect_err("a cycle through a $dynamicRef must be refused");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn accepts_nonrecursive_dynamic_ref() {
        // T1.3 non-recursive: a `$dynamicRef` to a leaf `$dynamicAnchor`
        // referenced once is accepted (linear).
        let schema = json!({
            "type": "object",
            "properties": { "id": { "$dynamicRef": "#leaf" } },
            "$defs": { "leaf": { "$dynamicAnchor": "leaf", "type": "string" } }
        });
        ValidatedToolSchema::validate("dyn", &schema)
            .expect("a non-recursive $dynamicRef is accepted");
    }

    #[test]
    fn rejects_recursive_cycle_through_document_id() {
        // T1.4: a `$ref` whose value is the document's own absolute `$id` names
        // the root resource. Pointing a property at the root is a cycle and
        // must be refused (integrator r3 T1; the r2 detector followed only `#`
        // fragments).
        let schema = json!({
            "$id": "https://example.com/root",
            "type": "object",
            "properties": { "loop": { "$ref": "https://example.com/root" } }
        });
        let err = ValidatedToolSchema::validate("idabs", &schema)
            .expect_err("a cycle through the document $id must be refused");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn rejects_recursive_cycle_through_embedded_resource_id_relative() {
        // T1.4: an embedded resource with a relative `$id` resolved against the
        // base URI; a `$ref` back to that relative id is a cycle.
        let schema = json!({
            "$id": "https://example.com/root",
            "type": "object",
            "properties": { "sub": { "$ref": "#/$defs/sub" } },
            "$defs": {
                "sub": {
                    "$id": "sub",
                    "type": "object",
                    "properties": { "back": { "$ref": "sub" } }
                }
            }
        });
        let err = ValidatedToolSchema::validate("idrel", &schema)
            .expect_err("a cycle through a relative embedded $id must be refused");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn accepts_nonrecursive_absolute_id_reference() {
        // T1.4 non-recursive: a `$ref` to an embedded resource's absolute `$id`
        // (a leaf, referenced once) is accepted.
        let schema = json!({
            "$id": "https://example.com/root",
            "type": "object",
            "properties": { "sub": { "$ref": "https://example.com/sub" } },
            "$defs": { "sub": { "$id": "https://example.com/sub", "type": "string" } }
        });
        ValidatedToolSchema::validate("idabs", &schema)
            .expect("a non-recursive absolute $id reference is accepted");
    }

    #[test]
    fn accepts_nonrecursive_id_reference_with_pointer_fragment() {
        // T1.4 non-recursive, "with a fragment": a `$ref` to an embedded
        // resource's `$id` plus a JSON-pointer fragment into that resource.
        let schema = json!({
            "$id": "https://example.com/root",
            "type": "object",
            "properties": { "x": { "$ref": "https://example.com/sub#/$defs/leaf" } },
            "$defs": {
                "sub": {
                    "$id": "https://example.com/sub",
                    "$defs": { "leaf": { "type": "string" } }
                }
            }
        });
        ValidatedToolSchema::validate("idfrag", &schema)
            .expect("a non-recursive $id-plus-fragment reference is accepted");
    }

    #[test]
    fn fail_closed_refuses_reference_it_cannot_resolve_in_document() {
        // T2 / integrator decision D41 (fail closed), as amended by D46: a
        // reference keyword has meaning only in a SCHEMA position. The detector
        // no longer treats a `$ref`-shaped object that is the DATA of a `const`
        // / `enum` / `default` / `examples` keyword as a reference (defect A).
        // The fail-closed branch still guards a reference in a real schema
        // position that the WP05 validator accepted but that the detector
        // cannot resolve inside the document: it is refused as `schema_unusable`
        // and named, never fetched or guessed. Exercised at the detector seam
        // (`detect_recursion`) because a real schema-position `$ref` that does
        // not resolve is refused earlier by the WP05 compiler on the public
        // path, so the detector's fail-closed branch is otherwise unobservable.
        let schema = json!({
            "type": "object",
            "properties": { "x": { "$ref": "#/does/not/exist" } }
        });
        let err = detect_recursion("failclosed", &schema)
            .expect_err("an unresolvable schema-position reference must be refused");
        assert!(
            matches!(err, SchemaContractError::UnresolvableReference { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
        // The detail names the offending reference (never fetched).
        assert!(
            err.to_string().contains("#/does/not/exist"),
            "detail must name the reference: {err}"
        );
    }

    #[test]
    fn catalog_accepts_distinct_definition_names() {
        let a = json!({ "type": "object", "$defs": { "alpha_id": { "type": "string" } } });
        let b = json!({ "type": "object", "$defs": { "beta_id": { "type": "string" } } });
        let entries = [
            CatalogSchema {
                tool: "alpha",
                schema: &a,
                owned: true,
            },
            CatalogSchema {
                tool: "beta",
                schema: &b,
                owned: true,
            },
        ];
        let validated = ValidatedToolSchema::validate_catalog(&entries).expect("distinct names");
        assert_eq!(validated.len(), 2);
    }

    #[test]
    fn catalog_rejects_colliding_definition_names_across_tools() {
        let a = json!({ "type": "object", "$defs": { "shared": { "type": "string" } } });
        let b = json!({ "type": "object", "$defs": { "shared": { "type": "number" } } });
        let entries = [
            CatalogSchema {
                tool: "alpha",
                schema: &a,
                owned: true,
            },
            CatalogSchema {
                tool: "beta",
                schema: &b,
                owned: true,
            },
        ];
        let err = ValidatedToolSchema::validate_catalog(&entries)
            .expect_err("a definition name defined by two tools must be refused");
        assert!(
            matches!(err, SchemaContractError::DefinitionCollision { .. }),
            "{err:?}"
        );
        assert_eq!(err.reason(), "schema_unusable");
    }

    #[test]
    fn catalog_accepts_supplied_defs_name_colliding_with_owned() {
        // S3 / integrator decision D39, ADR-030 ("when composing owned
        // definitions"): the cross-tool definition-name collision rule applies
        // to OWNED definitions only. A supplied (caller-owned) schema is never
        // refused because of a definition name, even when its `$defs` name
        // equals a name an owned tool defines.
        let owned = json!({ "type": "object", "$defs": { "shared": { "type": "string" } } });
        let supplied = json!({ "type": "object", "$defs": { "shared": { "type": "number" } } });
        let entries = [
            CatalogSchema {
                tool: "native_tool",
                schema: &owned,
                owned: true,
            },
            CatalogSchema {
                tool: "caller_final_answer",
                schema: &supplied,
                owned: false,
            },
        ];
        let validated = ValidatedToolSchema::validate_catalog(&entries)
            .expect("a supplied schema may reuse an owned definition name");
        assert_eq!(validated.len(), 2);
    }

    #[test]
    fn catalog_rejects_two_supplied_sharing_is_not_checked() {
        // Two supplied schemas sharing a definition name are also never refused
        // (only owned-vs-owned collisions are): a supplied name is outside the
        // owned namespace entirely (D39).
        let a = json!({ "type": "object", "$defs": { "shared": { "type": "string" } } });
        let b = json!({ "type": "object", "$defs": { "shared": { "type": "number" } } });
        let entries = [
            CatalogSchema {
                tool: "supplied_a",
                schema: &a,
                owned: false,
            },
            CatalogSchema {
                tool: "supplied_b",
                schema: &b,
                owned: false,
            },
        ];
        ValidatedToolSchema::validate_catalog(&entries)
            .expect("supplied definition names never collide");
    }

    // -----------------------------------------------------------------------
    // WP17 r4 (integrator decision D46): typed traversal. A reference keyword
    // has meaning only in a SCHEMA position; `const`/`enum`/`default`/
    // `examples` hold DATA and are never followed; an unknown keyword is read
    // conservatively (fail closed); the index never replaces an entry.
    // -----------------------------------------------------------------------

    // V1 (defect A, false refusal fixed): a `$ref`-shaped object that is the
    // DATA value of a known data keyword is NOT a reference and must not make
    // the schema recursive or unresolvable.

    #[test]
    fn v1_accepts_const_holding_a_ref_shaped_object() {
        let schema = json!({
            "type": "object",
            "properties": { "y": { "const": { "$ref": "#" } } }
        });
        ValidatedToolSchema::validate("c", &schema)
            .expect("const DATA that looks like a $ref is not a reference");
    }

    #[test]
    fn v1_accepts_enum_holding_a_ref_shaped_object() {
        let schema = json!({
            "type": "object",
            "properties": { "y": { "enum": [{ "$ref": "#" }] } }
        });
        ValidatedToolSchema::validate("e", &schema)
            .expect("enum DATA that looks like a $ref is not a reference");
    }

    #[test]
    fn v1_accepts_default_holding_a_ref_shaped_object() {
        let schema = json!({
            "type": "object",
            "properties": { "y": { "type": "object", "default": { "$ref": "#" } } }
        });
        ValidatedToolSchema::validate("d", &schema)
            .expect("default DATA that looks like a $ref is not a reference");
    }

    #[test]
    fn v1_accepts_examples_holding_a_ref_shaped_object() {
        let schema = json!({
            "type": "object",
            "properties": { "y": { "type": "object", "examples": [{ "$ref": "#" }] } }
        });
        ValidatedToolSchema::validate("x", &schema)
            .expect("examples DATA that looks like a $ref is not a reference");
    }

    #[test]
    fn v1_accepts_const_holding_an_unresolvable_ref_shaped_object() {
        // Data, so the `#/nowhere` it contains is never resolved: accepted.
        let schema = json!({
            "type": "object",
            "properties": { "y": { "const": { "$ref": "#/nowhere" } } }
        });
        ValidatedToolSchema::validate("c", &schema)
            .expect("an unresolvable $ref inside const DATA is not a reference");
    }

    #[test]
    fn v1_accepts_default_holding_an_external_ref_shaped_object() {
        // Data, so the `other.json` it contains is never treated as external.
        let schema = json!({
            "type": "object",
            "properties": { "y": { "type": "object", "default": { "$ref": "other.json" } } }
        });
        ValidatedToolSchema::validate("d", &schema)
            .expect("an external-looking $ref inside default DATA is not a reference");
    }

    // V2 (defect B, false accept fixed): the schema IS recursive and must be
    // refused even though a decoy `$id`/`$anchor` sits in DATA; a genuine
    // duplicate identifier in two schema positions is refused and named.

    #[test]
    fn v2_refuses_recursive_id_cycle_despite_decoy_default_id_data() {
        // `$defs/n` declares `$id` X and refers to X (self-recursive). A sibling
        // property carries `default: {"$id": X}` — DATA. The data `$id` must not
        // be indexed and must not mask the real resource, so the X -> X cycle is
        // still refused (before r4 the data `$id` replaced the real entry and
        // the schema was falsely accepted).
        let schema = json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/n" } },
            "$defs": {
                "n": {
                    "$id": "https://example.com/n",
                    "type": "object",
                    "properties": {
                        "loop": { "$ref": "https://example.com/n" },
                        "decoy": { "type": "object", "default": { "$id": "https://example.com/n" } }
                    }
                }
            }
        });
        let err = ValidatedToolSchema::validate("idb", &schema)
            .expect_err("a cycle through an embedded $id must be refused despite a decoy");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn v2_refuses_recursive_anchor_cycle_despite_decoy_default_anchor_data() {
        let schema = json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/n" } },
            "$defs": {
                "n": {
                    "$anchor": "node",
                    "type": "object",
                    "properties": {
                        "loop": { "$ref": "#node" },
                        "decoy": { "type": "object", "default": { "$anchor": "node" } }
                    }
                }
            }
        });
        let err = ValidatedToolSchema::validate("anb", &schema)
            .expect_err("a cycle through an $anchor must be refused despite a decoy");
        assert!(
            matches!(err, SchemaContractError::Recursive { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn v2_refuses_duplicate_id_in_two_schema_positions_and_names_it() {
        // Two real schema positions declare the same resolved `$id`: the index
        // refuses to replace the first entry (D46 rule 2) and the detail names
        // the duplicate. Exercised at the detector seam because the WP05
        // compiler may already reject a duplicate `$id`; here we pin that the
        // detector's index also refuses it and names the URI.
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "$id": "https://example.com/dup", "type": "string" },
                "b": { "$id": "https://example.com/dup", "type": "number" }
            }
        });
        let err = detect_recursion("dup", &schema)
            .expect_err("a duplicate $id in two schema positions must be refused");
        assert_eq!(err.reason(), "schema_unusable");
        assert!(
            err.to_string().contains("https://example.com/dup"),
            "detail must name the duplicate identifier: {err}"
        );
    }

    #[test]
    fn v2_accepts_same_anchor_name_in_two_resources_with_different_ids() {
        // The same anchor name under two DIFFERENT base URIs is two distinct
        // anchors, not a duplicate: it still passes (keyed by base URI + name).
        let schema = json!({
            "type": "object",
            "properties": {
                "a": { "$id": "https://example.com/a", "$anchor": "node", "type": "string" },
                "b": { "$id": "https://example.com/b", "$anchor": "node", "type": "number" }
            }
        });
        detect_recursion("two", &schema)
            .expect("the same anchor name in two resources is not a duplicate");
    }

    // V3 (guard, must pass before AND after r4): a recursive PROPERTY/DEFINITION
    // whose NAME equals a data keyword is still refused (the name is a NAME in a
    // name->schema map, never a keyword), and a cycle routed through every
    // schema keyword of D46 rule 1 is refused.

    #[test]
    fn v3_refuses_recursive_property_named_like_a_data_keyword() {
        for name in ["const", "enum", "default", "examples", "x-meta"] {
            let schema = json!({
                "type": "object",
                "properties": { name: { "$ref": "#/$defs/n" } },
                "$defs": {
                    "n": { "properties": { "next": { "$ref": "#/$defs/n" } } }
                }
            });
            let err = ValidatedToolSchema::validate("named", &schema).unwrap_err();
            assert!(
                matches!(err, SchemaContractError::Recursive { .. }),
                "a recursive property named `{name}` must be refused: {err:?}"
            );
        }
    }

    #[test]
    fn v3_refuses_recursive_definition_named_like_a_data_keyword() {
        for name in ["const", "enum", "default", "examples", "x-meta"] {
            // A `$defs` entry NAMED like a data keyword, self-recursive.
            let schema = json!({
                "properties": { "start": { "$ref": format!("#/$defs/{name}") } },
                "$defs": { name: { "properties": { "n": { "$ref": format!("#/$defs/{name}") } } } }
            });
            let err = ValidatedToolSchema::validate("defnamed", &schema).unwrap_err();
            assert!(
                matches!(err, SchemaContractError::Recursive { .. }),
                "a recursive $defs entry named `{name}` must be refused: {err:?}"
            );
        }
    }

    #[test]
    fn v3_refuses_a_cycle_through_every_schema_keyword() {
        // Table written from JSON Schema 2020-12 (core + applicator + unevaluated
        // + legacy forms D46 rule 1 names), NOT copied from the production list.
        // For each schema keyword, `$defs/n` references itself THROUGH that
        // keyword; the detector must refuse the n -> n cycle.
        //
        // kind: 0 = single subschema, 1 = array of subschemas, 2 = map
        // name->subschema, 3 = `dependencies` (map name->subschema | names).
        let self_ref = json!({ "$ref": "#/$defs/n" });
        let cases: &[(&str, u8)] = &[
            // single subschema
            ("not", 0),
            ("if", 0),
            ("then", 0),
            ("else", 0),
            ("items", 0),
            ("contains", 0),
            ("additionalProperties", 0),
            ("propertyNames", 0),
            ("unevaluatedItems", 0),
            ("unevaluatedProperties", 0),
            ("contentSchema", 0),
            ("additionalItems", 0),
            // array of subschemas
            ("allOf", 1),
            ("anyOf", 1),
            ("oneOf", 1),
            ("prefixItems", 1),
            ("items", 1),
            // map name->subschema
            ("properties", 2),
            ("patternProperties", 2),
            ("$defs", 2),
            ("definitions", 2),
            ("dependentSchemas", 2),
            // dependencies (schema form)
            ("dependencies", 3),
        ];
        for (keyword, kind) in cases {
            let body = match kind {
                0 => json!({ *keyword: self_ref.clone() }),
                1 => json!({ *keyword: [self_ref.clone()] }),
                _ => json!({ *keyword: { "sub": self_ref.clone() } }),
            };
            let schema = json!({
                "properties": { "start": { "$ref": "#/$defs/n" } },
                "$defs": { "n": body }
            });
            let err = detect_recursion("kw", &schema).unwrap_err();
            assert!(
                matches!(err, SchemaContractError::Recursive { .. }),
                "a cycle through `{keyword}` must be refused: {err:?}"
            );
        }
    }

    // V4: an unknown keyword is read conservatively (fail closed), so a
    // reference under it keeps the schema refused.

    #[test]
    fn v4_unknown_keyword_with_a_real_ref_stays_refused() {
        let schema = json!({
            "type": "object",
            "properties": { "a": { "type": "string" } },
            "x-meta": { "$ref": "#" }
        });
        let err = ValidatedToolSchema::validate("xm", &schema)
            .expect_err("a reference under an unknown keyword is read conservatively");
        assert_eq!(err.reason(), "schema_unusable");
    }

    /// Build a finite, NON-recursive `$ref` chain of `n` local definitions:
    /// `#/$defs/d0 -> properties.n -> $ref d1 -> ... -> d(n-1)`, whose last
    /// definition is a plain string leaf (no reference). The document root
    /// references `d0`. The chain is deep in reference hops but shallow in JSON
    /// nesting, so the only unbounded recursion it can provoke is the cycle
    /// search following the chain of references.
    fn deep_reference_chain(n: usize) -> Value {
        let mut defs = serde_json::Map::new();
        for i in 0..n {
            let def = if i + 1 == n {
                json!({ "type": "string" })
            } else {
                json!({
                    "type": "object",
                    "properties": { "n": { "$ref": format!("#/$defs/d{}", i + 1) } }
                })
            };
            defs.insert(format!("d{i}"), def);
        }
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/d0" } },
            "$defs": Value::Object(defs)
        })
    }

    /// WP17 r5 (W1, option a): a finite, non-recursive `$ref` chain of 5000
    /// local definitions must be analyzed without overflowing the thread stack
    /// (the check runs on a ~2 MiB tokio worker / test thread). With the
    /// recursive cycle search this call aborted the whole test binary with
    /// `fatal runtime error: stack overflow`; with the iterative work list it
    /// passes the check (the chain is self-contained and acyclic).
    #[test]
    fn accepts_a_deep_finite_reference_chain_without_stack_overflow() {
        let schema = deep_reference_chain(5000);
        ValidatedToolSchema::validate("deepchain", &schema)
            .expect("a finite non-recursive reference chain is not recursive");
    }

    /// WP17 r5 (W1): the same depth of reference hops, but closed into a cycle
    /// at the far end, is still refused as `schema_unusable` (the iterative
    /// driver detects the back-edge to an on-path target) rather than crashing
    /// or being missed.
    #[test]
    fn refuses_a_deep_reference_chain_that_closes_into_a_cycle() {
        let n = 5000;
        let mut defs = serde_json::Map::new();
        for i in 0..n {
            // Every definition references the next; the last references d0, so
            // the whole chain is one long recursive cycle.
            let target = (i + 1) % n;
            defs.insert(
                format!("d{i}"),
                json!({
                    "type": "object",
                    "properties": { "n": { "$ref": format!("#/$defs/d{target}") } }
                }),
            );
        }
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "root": { "$ref": "#/$defs/d0" } },
            "$defs": Value::Object(defs)
        });
        let err = ValidatedToolSchema::validate("deepcycle", &schema)
            .expect_err("a deep chain that closes into a cycle is recursive");
        assert_eq!(err.reason(), "schema_unusable");
    }
}
