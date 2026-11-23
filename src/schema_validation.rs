//! Reusable full Draft 2020-12 schema validation (ADR-024).
//!
//! Context: docs/contexts/reviewer-runtime/CONTEXT.md and
//! docs/adr/024-validated-terminal-output.md.
//!
//! Compiles the caller's schema **unchanged** with the real `jsonschema`
//! Draft 2020-12 validator, format assertion on, and no external resolver.
//! With the crate's default resolver features disabled, any reference that
//! would require file/HTTP (or any non-local) retrieval is refused at compile
//! time without performing I/O. The compiled validator backs final-output
//! argument validation (ADR-024) so no second hand-written walker ever exists.

use jsonschema::{Draft, Validator};
use serde_json::Value;

/// Fixed internal cap on how many validation error strings are surfaced for
/// one instance. WP05's `--final-output` caller interface does not expose this
/// bound (see INTERFACES.md L43); it is an internal safety cap, not a caller or
/// config control. The reserved `reviewer.*` configuration groups
/// (INTERFACES.md L286-290) may later expose a `max_validation_errors` config
/// cap under package-level configuration review; until then this value is used
/// unconditionally by `FinalOutputSpec::compile` and direct module callers.
pub const DEFAULT_MAX_VALIDATION_ERRORS: usize = 32;

/// A schema that cannot be used locally, detected before any model request or
/// external I/O. Distinct from a valid schema that an instance merely fails.
#[derive(Debug)]
pub enum SchemaError {
    /// The schema needs a reference that is not embedded in the supplied
    /// document (file/HTTP/other scheme or otherwise unresolvable). Refused
    /// without any fetch. Maps to the `schema_unusable` run reason.
    ExternalReference(String),
    /// The schema is not a usable Draft 2020-12 document for another reason
    /// (malformed, unknown keyword shape, invalid dialect). Maps to
    /// `schema_unusable`.
    Invalid(String),
}

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SchemaError::ExternalReference(msg) => {
                write!(f, "schema requires an external reference: {msg}")
            }
            SchemaError::Invalid(msg) => write!(f, "invalid schema: {msg}"),
        }
    }
}

impl std::error::Error for SchemaError {}

/// A compiled, reusable Draft 2020-12 validator over one caller schema.
pub struct CompiledSchema {
    validator: Validator,
}

impl CompiledSchema {
    /// Compile `schema` once with Draft 2020-12 semantics and format
    /// assertion. Returns [`SchemaError`] without touching the network or
    /// filesystem. A reference requiring retrieval surfaces as
    /// [`SchemaError::ExternalReference`]; every other compilation failure is
    /// [`SchemaError::Invalid`].
    pub fn compile(schema: &Value) -> Result<CompiledSchema, SchemaError> {
        let validator = jsonschema::options()
            .with_draft(Draft::Draft202012)
            .should_validate_formats(true)
            .build(schema)
            .map_err(classify_build_error)?;
        Ok(CompiledSchema { validator })
    }

    /// True when `instance` satisfies the schema. Part of the reusable
    /// validator surface (ADR-024); WP05 validates through `validate`, so this
    /// convenience is exercised by tests and future full-schema callers.
    #[allow(dead_code)]
    pub fn is_valid(&self, instance: &Value) -> bool {
        self.validator.is_valid(instance)
    }

    /// Validate `instance`, returning formatted, path-qualified error strings
    /// bounded by `max_errors`. An empty vector means the instance is valid.
    pub fn validate(&self, instance: &Value, max_errors: usize) -> Vec<String> {
        let mut out = Vec::new();
        for error in self.validator.iter_errors(instance) {
            if out.len() >= max_errors {
                out.push(format!("(additional errors omitted; showing {max_errors})"));
                break;
            }
            let path = error.instance_path().to_string();
            if path.is_empty() {
                out.push(error.to_string());
            } else {
                out.push(format!("{path}: {error}"));
            }
        }
        out
    }
}

/// A build-time `Referencing` failure means a reference could not be resolved
/// from the supplied document; with resolver features disabled that is exactly
/// the "no external retrieval" refusal. Everything else is a malformed schema.
fn classify_build_error(error: jsonschema::ValidationError<'static>) -> SchemaError {
    let message = error.to_string();
    match error.kind() {
        jsonschema::error::ValidationErrorKind::Referencing(_) => {
            SchemaError::ExternalReference(message)
        }
        _ => SchemaError::Invalid(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn full_schema_enforces_type_combinator_and_format() {
        let schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {
                "when": { "type": "string", "format": "date-time" },
                "mode": { "enum": ["a", "b"] }
            },
            "required": ["when", "mode"],
            "allOf": [{ "required": ["when"] }],
            "additionalProperties": false
        });
        let compiled = CompiledSchema::compile(&schema).expect("valid schema");

        assert!(compiled.is_valid(&json!({
            "when": "2026-10-29T06:00:00Z",
            "mode": "a"
        })));

        // Bad date-time is rejected because format assertion is enabled.
        let errs = compiled.validate(
            &json!({ "when": "not-a-date", "mode": "a" }),
            DEFAULT_MAX_VALIDATION_ERRORS,
        );
        assert!(!errs.is_empty());

        // additionalProperties:false rejects an unexpected key.
        assert!(!compiled.is_valid(&json!({
            "when": "2026-10-29T06:00:00Z",
            "mode": "a",
            "extra": 1
        })));
    }

    #[test]
    fn conditional_unique_items_and_local_ref_enforced() {
        // AC-01.5: if/then, uniqueItems, and a local $ref each reject.
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": { "type": "string" },
                "tags": { "type": "array", "uniqueItems": true },
                "id": { "$ref": "#/$defs/id" }
            },
            "required": ["kind", "id"],
            "if": { "properties": { "kind": { "const": "tagged" } } },
            "then": { "required": ["tags"] },
            "$defs": { "id": { "type": "string", "minLength": 4 } }
        });
        let compiled = CompiledSchema::compile(&schema).expect("valid schema");
        // Valid: tagged with tags and a long-enough id.
        assert!(compiled.is_valid(&json!({
            "kind": "tagged", "tags": ["a", "b"], "id": "abcd"
        })));
        // if/then: kind=tagged but no tags.
        assert!(!compiled.is_valid(&json!({ "kind": "tagged", "id": "abcd" })));
        // uniqueItems: duplicate tag.
        assert!(!compiled.is_valid(&json!({
            "kind": "tagged", "tags": ["a", "a"], "id": "abcd"
        })));
        // local $ref: id too short.
        assert!(!compiled.is_valid(&json!({ "kind": "plain", "id": "a" })));
    }

    #[test]
    fn local_defs_reference_resolves() {
        let schema = json!({
            "type": "object",
            "properties": { "id": { "$ref": "#/$defs/uuid" } },
            "required": ["id"],
            "$defs": { "uuid": { "type": "string", "minLength": 4 } }
        });
        let compiled = CompiledSchema::compile(&schema).expect("local $ref compiles");
        assert!(compiled.is_valid(&json!({ "id": "abcd" })));
        assert!(!compiled.is_valid(&json!({ "id": "a" })));
    }

    #[test]
    fn http_reference_refused_without_fetch() {
        let schema = json!({
            "type": "object",
            "properties": { "x": { "$ref": "https://example.com/remote.json" } }
        });
        assert!(matches!(
            CompiledSchema::compile(&schema),
            Err(SchemaError::ExternalReference(_))
        ));
    }

    #[test]
    fn file_reference_refused_without_fetch() {
        let schema = json!({
            "$ref": "file:///etc/passwd"
        });
        assert!(matches!(
            CompiledSchema::compile(&schema),
            Err(SchemaError::ExternalReference(_))
        ));
    }
}
