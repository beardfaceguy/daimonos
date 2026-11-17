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
    /// A `$ref`/`$dynamicRef` the WP05 validator accepted that the detector
    /// cannot resolve inside the document (integrator decision D41, fail
    /// closed). Refused without any fetch; the detail names the reference.
    UnresolvableReference { tool: String, detail: String },
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
    /// node base URI in `root`.
    fn build(root: &Value) -> SchemaIndex {
        let mut idx = SchemaIndex {
            resources: std::collections::BTreeMap::new(),
            anchors: std::collections::BTreeMap::new(),
            dynamic_anchors: std::collections::BTreeMap::new(),
            node_base: std::collections::BTreeMap::new(),
        };
        // A root without `$id` resolves document-local pointers against the
        // empty default base.
        idx.resources.insert(String::new(), String::new());
        idx.walk(root, String::new(), String::new());
        idx
    }

    fn walk(&mut self, node: &Value, base: String, pointer: String) {
        match node {
            Value::Object(map) => {
                let mut cur_base = base;
                if let Some(Value::String(id)) = map.get("$id") {
                    let (id_nofrag, _) = split_fragment(id);
                    cur_base = resolve_uri(&cur_base, id_nofrag);
                    self.resources.insert(cur_base.clone(), pointer.clone());
                }
                self.node_base.insert(pointer.clone(), cur_base.clone());
                if let Some(Value::String(a)) = map.get("$anchor") {
                    self.anchors
                        .insert((cur_base.clone(), a.clone()), pointer.clone());
                }
                if let Some(Value::String(d)) = map.get("$dynamicAnchor") {
                    self.dynamic_anchors
                        .insert((cur_base.clone(), d.clone()), pointer.clone());
                    // A `$dynamicAnchor` also answers a lexical plain-name `$ref`.
                    self.anchors
                        .entry((cur_base.clone(), d.clone()))
                        .or_insert_with(|| pointer.clone());
                }
                for (key, value) in map {
                    let child = format!("{pointer}/{}", escape_token(key));
                    self.walk(value, cur_base.clone(), child);
                }
            }
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    let child = format!("{pointer}/{i}");
                    self.walk(item, base.clone(), child);
                }
            }
            _ => {}
        }
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
fn detect_recursion(tool: &str, root: &Value) -> Result<(), SchemaContractError> {
    let index = SchemaIndex::build(root);
    let mut on_path: BTreeSet<String> = BTreeSet::new();
    let mut acyclic: BTreeSet<String> = BTreeSet::new();
    let mut steps: usize = 0;
    expand(
        tool,
        &index,
        root,
        root,
        "",
        &mut on_path,
        &mut acyclic,
        &mut steps,
    )
}

#[allow(clippy::too_many_arguments)]
fn expand(
    tool: &str,
    index: &SchemaIndex,
    root: &Value,
    node: &Value,
    pointer: &str,
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
            // Reference edges at this node: a static `$ref` and a `$dynamicRef`
            // (treated as an edge to its `$dynamicAnchor`, T1.3).
            let mut edges: Vec<(&str, bool)> = Vec::new();
            if let Some(Value::String(reference)) = map.get("$ref") {
                edges.push((reference, false));
            }
            if let Some(Value::String(reference)) = map.get("$dynamicRef") {
                edges.push((reference, true));
            }
            for (reference, dynamic) in edges {
                match index.resolve(root, pointer, reference, dynamic) {
                    RefEdge::To(target) => {
                        if on_path.contains(&target) {
                            return Err(SchemaContractError::Recursive {
                                tool: tool.to_string(),
                                pointer: format!("#{target}"),
                            });
                        }
                        if !acyclic.contains(&target) {
                            if let Some(target_node) = root.pointer(&target) {
                                on_path.insert(target.clone());
                                expand(
                                    tool,
                                    index,
                                    root,
                                    target_node,
                                    &target,
                                    on_path,
                                    acyclic,
                                    steps,
                                )?;
                                on_path.remove(&target);
                                acyclic.insert(target);
                            }
                        }
                    }
                    // Fail closed (integrator D41): a reference the WP05
                    // validator accepted that we cannot resolve inside the
                    // document is refused, named, and never fetched.
                    RefEdge::Unresolvable(detail) => {
                        return Err(SchemaContractError::UnresolvableReference {
                            tool: tool.to_string(),
                            detail,
                        });
                    }
                }
            }
            for (key, value) in map {
                if key == "$ref" || key == "$dynamicRef" {
                    continue;
                }
                let child = format!("{pointer}/{}", escape_token(key));
                expand(tool, index, root, value, &child, on_path, acyclic, steps)?;
            }
            Ok(())
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let child = format!("{pointer}/{i}");
                expand(tool, index, root, item, &child, on_path, acyclic, steps)?;
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
        // T2 / integrator decision D41 (fail closed): the WP05 validator accepts
        // this schema — the `$ref`-shaped object is the *data* value of a
        // `const` keyword, not a reference — but the recursion detector walks
        // the document structurally and meets a `#/...` it cannot resolve in
        // the document. Rather than guess or fetch, full-schema mode refuses it
        // as `schema_unusable` and names the reference. (This is a deliberate
        // over-approximation in the safe direction; see issues-wp17.md.)
        let schema = json!({
            "type": "object",
            "properties": {
                "y": { "const": { "$ref": "#/does/not/exist" } }
            }
        });
        // Precondition: the WP05 validator accepts it (const data is not a ref).
        crate::schema_validation::CompiledSchema::compile(&schema)
            .expect("WP05 validator accepts a const whose data looks like a $ref");
        let err = ValidatedToolSchema::validate("failclosed", &schema)
            .expect_err("an unresolvable reference must be refused, not fetched or guessed");
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
}
