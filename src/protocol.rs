use serde::{Deserialize, Serialize};

/// Compact request: [op, ...args] or {"op": N, ...params} or {"batch": [...]}
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Request {
    Single(Op),
    Batch { batch: Vec<Op> },
}

#[derive(Debug, Default, Deserialize)]
pub struct Op {
    /// Opcode number
    pub c: u8,
    /// Path argument (files, directories)
    #[serde(default)]
    pub p: Option<String>,
    /// Secondary path or pattern
    #[serde(default)]
    pub q: Option<String>,
    /// Content / command string
    #[serde(default)]
    pub s: Option<String>,
    /// Integer arg (offset, limit, signal, depth)
    #[serde(default)]
    pub n: Option<i64>,
    /// Secondary integer arg
    #[serde(default)]
    pub n2: Option<i64>,
    /// String array arg (args, edits)
    #[serde(default)]
    pub a: Option<Vec<String>>,
    /// Key-value pairs (env vars)
    #[serde(default)]
    pub kv: Option<std::collections::HashMap<String, String>>,
    /// Glob/filter pattern
    #[serde(default)]
    pub g: Option<String>,
}

/// Out-of-band metadata about a response, consumed by the dispatch layer
/// and downstream analytics consumers (the MCP server today, anything else
/// that wraps `ops::dispatch` tomorrow) to classify what happened during
/// dispatch (plugin redirect, exec output filtering, read deduplication).
/// These flags must be set at the source site rather than reconstructed by
/// substring-matching the serialized JSON, which is brittle and produces
/// false positives when string fields happen to contain the same tokens.
///
/// The field is `#[serde(skip)]` so it never reaches the wire — it is
/// transport-agnostic state shared between the handler and its in-process
/// caller, never sent to clients on the socket or stdio paths.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResponseMeta {
    /// True when the request was satisfied by a registered tool plugin
    /// instead of raw exec (e.g. `git status` short-circuited through the
    /// git plugin).
    pub redirect_via_plugin: bool,
    /// True when an exec response went through `exec_filter` semantic
    /// compression (test-runner summary, build "ok", install "ok", etc.).
    pub filter_applied: bool,
    /// True when a read of an unchanged file was satisfied from
    /// `session.read_cache` and returned the compact `{"unchanged": true}`
    /// payload instead of the file body.
    pub read_dedup: bool,
    /// Character count of the raw/uncompressed content that was suppressed
    /// by dedup or filtering. Zero when no compression occurred. Used by
    /// the analytics layer to compute `saved_tokens` and `savings_pct`.
    pub unfiltered_chars: usize,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
    #[serde(skip)]
    pub meta: ResponseMeta,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            d: Some(data),
            e: None,
            m: None,
            meta: ResponseMeta::default(),
        }
    }

    pub fn err(code: u32, msg: &str) -> Self {
        Self {
            ok: false,
            d: None,
            e: Some(code),
            m: Some(msg.to_string()),
            meta: ResponseMeta::default(),
        }
    }

    /// Mark this response as the product of a tool-plugin redirect.
    pub fn redirect_via_plugin(mut self) -> Self {
        self.meta.redirect_via_plugin = true;
        self
    }

    /// Mark this response as having gone through semantic exec filtering.
    pub fn filter_applied(mut self) -> Self {
        self.meta.filter_applied = true;
        self
    }

    /// Mark this response as a read-cache deduplication hit.
    pub fn read_dedup(mut self) -> Self {
        self.meta.read_dedup = true;
        self
    }

    /// Record the raw character count that was suppressed by dedup or
    /// filtering so the analytics layer can compute token savings.
    pub fn with_unfiltered_chars(mut self, chars: usize) -> Self {
        self.meta.unfiltered_chars = chars;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_op() {
        let json = r#"{"c": 0, "p": "foo.txt"}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Single(op) => {
                assert_eq!(op.c, 0);
                assert_eq!(op.p.as_deref(), Some("foo.txt"));
            }
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn parse_batch() {
        let json = r#"{"batch": [{"c": 0, "p": "a.txt"}, {"c": 1, "p": "b.txt", "s": "hi"}]}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Batch { batch } => {
                assert_eq!(batch.len(), 2);
                assert_eq!(batch[0].c, 0);
                assert_eq!(batch[1].s.as_deref(), Some("hi"));
            }
            _ => panic!("expected Batch"),
        }
    }

    #[test]
    fn parse_minimal_op() {
        let json = r#"{"c": 255}"#;
        let req: Request = serde_json::from_str(json).unwrap();
        match req {
            Request::Single(op) => {
                assert_eq!(op.c, 255);
                assert!(op.p.is_none());
                assert!(op.s.is_none());
                assert!(op.n.is_none());
                assert!(op.a.is_none());
                assert!(op.kv.is_none());
            }
            _ => panic!("expected Single"),
        }
    }

    #[test]
    fn op_default() {
        let op = Op::default();
        assert_eq!(op.c, 0);
        assert!(op.p.is_none());
    }

    #[test]
    fn response_ok_serialization() {
        let resp = Response::ok(serde_json::json!({"x": 1}));
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], true);
        assert_eq!(json["d"]["x"], 1);
        assert!(json.get("e").is_none());
        assert!(json.get("m").is_none());
    }

    #[test]
    fn response_err_serialization() {
        let resp = Response::err(3, "bad arg");
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["ok"], false);
        assert_eq!(json["e"], 3);
        assert_eq!(json["m"], "bad arg");
        assert!(json.get("d").is_none());
    }

    #[test]
    fn response_meta_defaults_all_false() {
        let resp = Response::ok(serde_json::json!({}));
        assert!(!resp.meta.redirect_via_plugin);
        assert!(!resp.meta.filter_applied);
        assert!(!resp.meta.read_dedup);
        let err = Response::err(1, "x");
        assert_eq!(err.meta, ResponseMeta::default());
    }

    #[test]
    fn response_meta_builders_set_individual_flags() {
        let r = Response::ok(serde_json::json!({})).redirect_via_plugin();
        assert!(r.meta.redirect_via_plugin);
        assert!(!r.meta.filter_applied);
        assert!(!r.meta.read_dedup);

        let r = Response::ok(serde_json::json!({})).filter_applied();
        assert!(!r.meta.redirect_via_plugin);
        assert!(r.meta.filter_applied);
        assert!(!r.meta.read_dedup);

        let r = Response::ok(serde_json::json!({})).read_dedup();
        assert!(!r.meta.redirect_via_plugin);
        assert!(!r.meta.filter_applied);
        assert!(r.meta.read_dedup);
    }

    #[test]
    fn response_meta_builders_compose() {
        // Hypothetical case where multiple flags would apply simultaneously
        // — confirm the builders chain without clobbering each other.
        let r = Response::ok(serde_json::json!({}))
            .redirect_via_plugin()
            .filter_applied()
            .read_dedup();
        assert!(r.meta.redirect_via_plugin);
        assert!(r.meta.filter_applied);
        assert!(r.meta.read_dedup);
    }

    #[test]
    fn response_meta_is_skipped_in_wire_format() {
        // The meta field must not leak onto the wire — clients never see it.
        let resp = Response::ok(serde_json::json!({"x": 1})).redirect_via_plugin();
        let serialized = serde_json::to_string(&resp).unwrap();
        assert!(!serialized.contains("meta"));
        assert!(!serialized.contains("redirect_via_plugin"));
    }
}

/// Opcode constants
pub mod op {
    pub const READ: u8 = 0;
    pub const WRITE: u8 = 1;
    pub const PATCH: u8 = 2;
    /// Append content to a file, creating it if absent. Distinct opcode from
    /// WRITE so the agent can grow a large file across bounded calls without
    /// re-emitting it (item 4 of the large-output UX work).
    pub const APPEND: u8 = 27;
    pub const LS: u8 = 3;
    pub const STAT: u8 = 4;
    pub const GLOB: u8 = 5;
    pub const GREP: u8 = 6;
    pub const FIND: u8 = 7;
    pub const EXEC: u8 = 8;
    pub const BG: u8 = 9;
    pub const POLL: u8 = 10;
    pub const KILL: u8 = 11;
    pub const SNAP: u8 = 12;
    pub const RESTORE: u8 = 13;
    pub const SNAP_LIST: u8 = 25;
    pub const SNAP_DELETE: u8 = 26;
    pub const DIFF: u8 = 14;
    #[deprecated(note = "git moved to tool plugin system — use TOOL_RUN with tool_id 'git'")]
    #[allow(dead_code)]
    pub const GIT: u8 = 15;
    pub const ENV_SET: u8 = 16;
    pub const ENV_GET: u8 = 17;
    pub const SESSION: u8 = 18;
    /// Agent-to-agent coordination ("agent mail", ADR-009). One opcode carries
    /// a JSON body in `s` (a `{verb, ...}` object) decoded by `ops::coord`; the
    /// compact positional `Op` fields are too narrow for rich mail payloads.
    pub const COORD: u8 = 19;
    pub const TOOL_RUN: u8 = 20;
    pub const LINT_REPAIR: u8 = 21;
    pub const TOOL_PIPELINE: u8 = 22;
    pub const TOOL_REGISTER: u8 = 23;
    pub const TOOL_LIST: u8 = 24;
    pub const SCHEMA: u8 = 255;
}
