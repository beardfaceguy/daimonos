use serde::{Deserialize, Serialize};

/// Compact request: [op, ...args] or {"op": N, ...params} or {"batch": [...]}
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Request {
    Single(Op),
    Batch { batch: Vec<Op> },
}

#[derive(Debug, Deserialize)]
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

#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub e: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub m: Option<String>,
}

impl Response {
    pub fn ok(data: serde_json::Value) -> Self {
        Self {
            ok: true,
            d: Some(data),
            e: None,
            m: None,
        }
    }

    pub fn err(code: u32, msg: &str) -> Self {
        Self {
            ok: false,
            d: None,
            e: Some(code),
            m: Some(msg.to_string()),
        }
    }
}

/// Opcode constants
pub mod op {
    pub const READ: u8 = 0;
    pub const WRITE: u8 = 1;
    pub const PATCH: u8 = 2;
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
    pub const DIFF: u8 = 14;
    pub const GIT: u8 = 15;
    pub const ENV_SET: u8 = 16;
    pub const ENV_GET: u8 = 17;
    pub const SESSION: u8 = 18;
    pub const SCHEMA: u8 = 255;
}
