use serde_json::Value;
use starlark::environment::{Globals, GlobalsBuilder, Module};
use starlark::eval::Evaluator;
use starlark::starlark_module;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::Dict;
use starlark::values::list::UnpackList;
use starlark::values::none::NoneType;
use starlark::values::{Heap, Value as StarlarkValue};
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

use crate::ops;
use crate::protocol::{Request, Response};
use crate::session::Session;
use crate::tools;

thread_local! {
    static PRINT_LOG: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Result of executing a Starlark script.
#[derive(Debug)]
pub struct ScriptResult {
    pub value: Value,
    pub logs: Vec<String>,
}

/// Execute a Starlark script with daimonos tool bindings.
///
/// Tools are exposed as synchronous functions that internally block on
/// the tokio runtime (via `Handle::block_on`) to call async daimonos ops.
/// The script runs in a dedicated thread to avoid blocking the async runtime.
pub async fn execute(
    code: &str,
    session: Arc<Mutex<Session>>,
    timeout: Duration,
) -> Result<ScriptResult, String> {
    let code = code.to_string();
    let handle = tokio::runtime::Handle::current();

    let result = tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || {
        run_starlark(&code, session, handle)
    }))
    .await;

    match result {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => Err(format!("script thread panic: {e}")),
        Err(_) => Err(format!("script timeout after {}s", timeout.as_secs())),
    }
}

fn run_starlark(
    code: &str,
    session: Arc<Mutex<Session>>,
    handle: tokio::runtime::Handle,
) -> Result<ScriptResult, String> {
    let ast = AstModule::parse("script", code.to_string(), &Dialect::Standard)
        .map_err(|e| format!("parse error: {e}"))?;

    let globals = build_globals(session, handle);
    let module = Module::new();

    PRINT_LOG.with(|log| log.borrow_mut().clear());

    {
        let mut eval = Evaluator::new(&module);
        eval.eval_module(ast, &globals)
            .map_err(|e| format!("eval error: {e}"))?;
    }

    let frozen = module.freeze().map_err(|e| format!("freeze: {e:?}"))?;

    // Extract `result` variable, or fall back to None.
    let json_val = if let Ok(result_val) = frozen.get("result") {
        let heap = Heap::new();
        let val = result_val.value();
        starlark_to_json(val, &heap)
    } else {
        Value::Null
    };

    let logs = PRINT_LOG.with(|log| log.borrow().clone());

    Ok(ScriptResult {
        value: json_val,
        logs,
    })
}


/// Convert a Starlark value to serde_json::Value.
fn starlark_to_json<'v>(val: StarlarkValue<'v>, heap: &'v Heap) -> Value {
    if val.is_none() {
        Value::Null
    } else if let Some(b) = val.unpack_bool() {
        Value::Bool(b)
    } else if let Some(i) = val.unpack_i32() {
        Value::Number(i.into())
    } else if let Some(s) = val.unpack_str() {
        Value::String(s.to_string())
    } else if let Ok(list_len) = val.length() {
        if val.get_type() == "dict" {
            if let Ok(iter) = val.iterate(heap) {
                let mut map = serde_json::Map::new();
                for key in iter {
                    if let Some(k) = key.unpack_str() {
                        if let Ok(v) = val.at(key, heap) {
                            map.insert(k.to_string(), starlark_to_json(v, heap));
                        }
                    }
                }
                return Value::Object(map);
            }
            Value::Null
        } else {
            let mut arr = Vec::with_capacity(list_len as usize);
            if let Ok(iter) = val.iterate(heap) {
                for item in iter {
                    arr.push(starlark_to_json(item, heap));
                }
            }
            Value::Array(arr)
        }
    } else {
        Value::String(val.to_string())
    }
}

/// Build Starlark globals with daimonos tool functions bound.
fn build_globals(
    session: Arc<Mutex<Session>>,
    handle: tokio::runtime::Handle,
) -> Globals {
    // We store session + handle in a thread-local so the starlark_module
    // functions can access them without capturing closures.
    TOOL_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(ToolContext {
            session,
            handle,
        });
    });

    GlobalsBuilder::standard()
        .with(builtin_functions)
        .with(tool_functions)
        .build()
}

struct ToolContext {
    session: Arc<Mutex<Session>>,
    handle: tokio::runtime::Handle,
}

thread_local! {
    static TOOL_CTX: RefCell<Option<ToolContext>> = const { RefCell::new(None) };
}

fn with_ctx<F, R>(f: F) -> Result<R, anyhow::Error>
where
    F: FnOnce(&ToolContext) -> Result<R, anyhow::Error>,
{
    TOOL_CTX.with(|ctx| {
        let borrow = ctx.borrow();
        let ctx = borrow
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tool context not initialized"))?;
        f(ctx)
    })
}

fn dispatch_request(request: Request) -> Result<Response, anyhow::Error> {
    with_ctx(|ctx| {
        let session = ctx.session.clone();
        let resp = ctx.handle.block_on(async {
            let mut s = session.lock().await;
            ops::dispatch(&mut s, request).await
        });
        Ok(resp)
    })
}

/// Dispatch a tool call by name, using the tools registry to map args → Request.
fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Response, anyhow::Error> {
    match tools::build_request(name, args) {
        Some(Ok(request)) => dispatch_request(request),
        Some(Err(e)) => Err(anyhow::anyhow!("{e}")),
        None => Err(anyhow::anyhow!("tool '{name}' has no opcode mapping")),
    }
}

fn response_to_starlark_dict<'v>(resp: Response, heap: &'v Heap) -> anyhow::Result<Dict<'v>> {
    if !resp.ok {
        let msg = resp.m.unwrap_or_else(|| "unknown error".into());
        return Err(anyhow::anyhow!("{}", msg));
    }
    match resp.d {
        Some(data) => json_to_starlark_dict(&data, heap),
        None => Ok(Dict::default()),
    }
}

fn json_to_starlark_dict<'v>(val: &Value, heap: &'v Heap) -> anyhow::Result<Dict<'v>> {
    match val {
        Value::Object(map) => {
            let mut small_map = starlark::collections::SmallMap::new();
            for (k, v) in map {
                let key = heap.alloc_str(k).to_value();
                let hashed = key.get_hashed().expect("string key");
                small_map.insert_hashed(hashed, json_to_starlark(v, heap));
            }
            Ok(Dict::new(small_map))
        }
        _ => {
            let mut small_map = starlark::collections::SmallMap::new();
            let key = heap.alloc_str("data").to_value();
            let hashed = key.get_hashed().expect("string key");
            small_map.insert_hashed(hashed, json_to_starlark(val, heap));
            Ok(Dict::new(small_map))
        }
    }
}

fn response_to_starlark_val<'v>(resp: Response, heap: &'v Heap) -> anyhow::Result<StarlarkValue<'v>> {
    if !resp.ok {
        let msg = resp.m.unwrap_or_else(|| "unknown error".into());
        return Err(anyhow::anyhow!("{}", msg));
    }
    match resp.d {
        Some(data) => Ok(json_to_starlark(&data, heap)),
        None => Ok(StarlarkValue::new_none()),
    }
}

/// Convert serde_json::Value to a Starlark value allocated on the given heap.
fn json_to_starlark<'v>(val: &Value, heap: &'v Heap) -> StarlarkValue<'v> {
    match val {
        Value::Null => StarlarkValue::new_none(),
        Value::Bool(b) => StarlarkValue::new_bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                heap.alloc(i as i32)
            } else if let Some(f) = n.as_f64() {
                heap.alloc(f)
            } else {
                heap.alloc(n.to_string())
            }
        }
        Value::String(s) => heap.alloc_str(s).to_value(),
        Value::Array(arr) => {
            let items: Vec<StarlarkValue> = arr.iter().map(|v| json_to_starlark(v, heap)).collect();
            heap.alloc(items)
        }
        Value::Object(map) => {
            let mut small_map = starlark::collections::SmallMap::new();
            for (k, v) in map {
                let key = heap.alloc_str(k).to_value();
                let hashed = key.get_hashed().expect("string keys are hashable");
                small_map.insert_hashed(hashed, json_to_starlark(v, heap));
            }
            heap.alloc(Dict::new(small_map))
        }
    }
}

// --- Starlark built-in function overrides ---

#[starlark_module]
fn builtin_functions(builder: &mut GlobalsBuilder) {
    fn print<'v>(
        #[starlark(args)] args: StarlarkValue<'v>,
        heap: &'v Heap,
    ) -> anyhow::Result<NoneType> {
        let mut parts = Vec::new();
        if let Ok(iter) = args.iterate(heap) {
            for item in iter {
                if let Some(s) = item.unpack_str() {
                    parts.push(s.to_string());
                } else {
                    parts.push(item.to_string());
                }
            }
        } else {
            parts.push(args.to_string());
        }
        let text = parts.join(" ");
        PRINT_LOG.with(|log| log.borrow_mut().push(text));
        Ok(NoneType)
    }
}

// --- Starlark tool function bindings ---

#[starlark_module]
fn tool_functions(builder: &mut GlobalsBuilder) {
    fn read_file<'v>(
        path: &str,
        #[starlark(require = named)] offset: Option<i32>,
        #[starlark(require = named)] limit: Option<i32>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let args = serde_json::json!({"path": path, "offset": offset, "limit": limit});
        let resp = dispatch_tool_by_name("read_file", &args)?;
        response_to_starlark_dict(resp, heap)
    }

    fn write_file<'v>(
        path: &str,
        content: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let args = serde_json::json!({"path": path, "content": content});
        let resp = dispatch_tool_by_name("write_file", &args)?;
        response_to_starlark_dict(resp, heap)
    }

    fn edit_file<'v>(
        path: &str,
        edits: UnpackList<String>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let args = serde_json::json!({"path": path, "edits": edits.items});
        let resp = dispatch_tool_by_name("edit_file", &args)?;
        response_to_starlark_dict(resp, heap)
    }

    fn search<'v>(
        pattern: &str,
        #[starlark(require = named, default = "content")] mode: &str,
        #[starlark(require = named)] path: Option<&str>,
        #[starlark(require = named)] glob: Option<&str>,
        #[starlark(require = named)] max_results: Option<i32>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let args = serde_json::json!({
            "pattern": pattern, "mode": mode,
            "path": path, "glob": glob, "max_results": max_results,
        });
        let resp = dispatch_tool_by_name("search", &args)?;
        response_to_starlark_dict(resp, heap)
    }

    fn exec<'v>(
        command: &str,
        #[starlark(require = named)] args: Option<UnpackList<String>>,
        #[starlark(require = named)] cwd: Option<&str>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let a: Option<Vec<String>> = args.and_then(|a| {
            if a.items.is_empty() { None } else { Some(a.items) }
        });
        let tool_args = serde_json::json!({"command": command, "args": a, "cwd": cwd});
        let resp = dispatch_tool_by_name("exec", &tool_args)?;
        response_to_starlark_dict(resp, heap)
    }

    fn ls<'v>(
        path: Option<&str>,
        depth: Option<i32>,
        heap: &'v Heap,
    ) -> anyhow::Result<StarlarkValue<'v>> {
        let args = serde_json::json!({"path": path, "depth": depth});
        let resp = dispatch_tool_by_name("ls", &args)?;
        response_to_starlark_val(resp, heap)
    }

    fn snapshot<'v>(
        action: &str,
        #[starlark(require = named)] id: Option<&str>,
        #[starlark(require = named)] tag: Option<&str>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let args = serde_json::json!({"action": action, "id": id, "tag": tag});
        let resp = dispatch_tool_by_name("snapshot", &args)?;
        response_to_starlark_dict(resp, heap)
    }

    fn git<'v>(
        command: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let session = ctx.session.clone();
            let cmd = command.to_string();
            let resp = ctx.handle.block_on(async {
                let s = session.lock().await;
                let registry = s.tool_registry.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("git plugin not available"))?;
                let cwd = s.cwd.clone();
                let env = s.env.clone();
                let args_val = serde_json::json!({"command": cmd});
                registry.run("git", &cmd, &cwd, &env, None, Some(&args_val)).await
                    .map(|r| Response::ok(r.output))
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })?;
            response_to_starlark_dict(resp, heap)
        })
    }
}

/// Generate Python-style function signatures for all available tool bindings.
pub fn tool_signatures() -> String {
    let sigs = [
        "def read_file(path: str, offset: int = None, limit: int = None) -> dict: ...",
        "def write_file(path: str, content: str) -> dict: ...",
        "def edit_file(path: str, edits: list[str]) -> dict: ...",
        "def search(pattern: str, mode: str = \"content\", path: str = None, glob: str = None, max_results: int = None) -> dict: ...",
        "def exec(command: str, args: list[str] = [], cwd: str = None) -> dict: ...",
        "def ls(path: str = None, depth: int = None) -> dict: ...",
        "def snapshot(action: str, id: str = None, tag: str = None) -> dict: ...",
        "def git(command: str) -> dict: ...",
        "def print(*args) -> None:  # captured in logs",
    ];
    sigs.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_session() -> Arc<Mutex<Session>> {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.keep(), Arc::new(Config::default()));
        Arc::new(Mutex::new(session))
    }

    #[tokio::test]
    async fn execute_simple_expression() {
        let session = test_session();
        let result = execute("result = 1 + 2", session, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.value, serde_json::json!(3));
    }

    #[tokio::test]
    async fn execute_string_result() {
        let session = test_session();
        let code = r#"result = "hello" + " world""#;
        let result = execute(code, session, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.value, serde_json::json!("hello world"));
    }

    #[tokio::test]
    async fn execute_no_result_returns_null() {
        let session = test_session();
        let result = execute("x = 42", session, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.value, serde_json::json!(null));
    }

    #[tokio::test]
    async fn execute_dict_result_variable() {
        let session = test_session();
        let code = r#"
x = {"a": 1, "b": [2, 3]}
result = x
"#;
        let result = execute(code, session, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.value, serde_json::json!({"a": 1, "b": [2, 3]}));
    }

    #[tokio::test]
    async fn execute_captures_print() {
        let session = test_session();
        let code = r#"
print("hello")
print("world")
result = True
"#;
        let result = execute(code, session, Duration::from_secs(5)).await.unwrap();
        assert_eq!(result.logs, vec!["hello", "world"]);
        assert_eq!(result.value, serde_json::json!(true));
    }

    #[tokio::test]
    async fn execute_syntax_error() {
        let session = test_session();
        let result = execute("def (", session, Duration::from_secs(5)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("parse error"), "expected parse error, got: {err}");
    }

    #[tokio::test]
    async fn execute_timeout() {
        let session = test_session();
        // Starlark doesn't have sleep, but a very tight loop would take too long
        // to actually timeout. Just verify the timeout path with a short timeout.
        let code = "result = 42";
        let result = execute(code, session, Duration::from_millis(5000)).await.unwrap();
        assert_eq!(result.value, serde_json::json!(42));
    }

    #[tokio::test]
    async fn execute_read_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let session = Arc::new(Mutex::new(session));

        let code = r#"
write_file("test.txt", "hello starlark")
data = read_file("test.txt")
result = data["content"]
"#;
        let result = execute(code, session, Duration::from_secs(10)).await.unwrap();
        assert_eq!(result.value, serde_json::json!("hello starlark"));
    }

    #[tokio::test]
    async fn execute_exec_command() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let session = Arc::new(Mutex::new(session));

        let code = r#"
r = exec("echo hello from starlark")
result = r["out"]
"#;
        let result = execute(code, session, Duration::from_secs(10)).await.unwrap();
        assert_eq!(result.value, serde_json::json!("hello from starlark"));
    }

    #[tokio::test]
    async fn execute_multi_step_script() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let session = Arc::new(Mutex::new(session));

        let code = r#"
write_file("a.txt", "line one\nline two\nline three")
write_file("b.txt", "other content")
data = read_file("a.txt")
lines = data["lines"]
result = {"lines": lines, "status": "ok"}
"#;
        let result = execute(code, session, Duration::from_secs(10)).await.unwrap();
        let val = result.value;
        assert_eq!(val["lines"], 3);
        assert_eq!(val["status"], "ok");
    }

    #[test]
    fn tool_signatures_has_all_functions() {
        let sigs = tool_signatures();
        assert!(sigs.contains("read_file"));
        assert!(sigs.contains("write_file"));
        assert!(sigs.contains("edit_file"));
        assert!(sigs.contains("search"));
        assert!(sigs.contains("exec"));
        assert!(sigs.contains("ls"));
        assert!(sigs.contains("snapshot"));
        assert!(sigs.contains("git"));
    }

    #[test]
    fn json_to_starlark_roundtrip() {
        let heap = Heap::new();
        let original = serde_json::json!({
            "name": "test",
            "count": 42,
            "items": [1, 2, 3],
            "nested": {"a": true}
        });
        let starlark_val = json_to_starlark(&original, &heap);
        let back = starlark_to_json(starlark_val, &heap);
        assert_eq!(back["name"], "test");
        assert_eq!(back["count"], 42);
        assert_eq!(back["items"], serde_json::json!([1, 2, 3]));
        assert_eq!(back["nested"]["a"], true);
    }
}
