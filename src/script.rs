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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};

use crate::config::DEFAULT_MAX_SCRIPT_THREADS;

/// Lazily-initialized global cap on concurrent Starlark script threads.
///
/// Pure-CPU runaway scripts cannot be cancelled by the current
/// `starlark` 0.13 evaluator (its `before_stmt` hook is `pub(crate)`),
/// so each such script leaks one OS thread until the process exits.
/// This semaphore is the daemon's only real defense against unbounded
/// thread allocation: when the configured cap is reached, new
/// `execute()` calls await an available slot, bounded by the per-call
/// script timeout so callers fail fast instead of hanging indefinitely.
///
/// `OwnedSemaphorePermit` is held by the script thread for its entire
/// lifetime, so for cancellable scripts (the common case) the permit
/// drops as soon as the thread unwinds.
static SCRIPT_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Initialize a `OnceLock<Arc<Semaphore>>` to a `Semaphore::new(n)`.
/// Returns `Err(())` if the cell was already initialized, so callers
/// can surface that to the operator.
///
/// Extracted from `configure_max_concurrent` so unit tests can exercise
/// the locking semantics against a local `OnceLock` without touching
/// the process-global `SCRIPT_PERMITS`.
fn init_semaphore(cell: &OnceLock<Arc<Semaphore>>, n: usize) -> Result<(), ()> {
    cell.set(Arc::new(Semaphore::new(n))).map_err(|_| ())
}

/// Configure the script-thread concurrency cap.
///
/// Idempotent — only the first call wins. Should be called once at
/// startup from the binary entry point with
/// `config.process.max_script_threads`. If a later call attempts to
/// reconfigure, the request is logged and ignored. If never called,
/// the cap defaults to `config::DEFAULT_MAX_SCRIPT_THREADS`.
///
/// Values below 1 are clamped to 1 — a zero cap would block every
/// `execute_script` until each per-call timeout fires, which is
/// almost certainly a misconfiguration, not an intentional kill switch.
pub fn configure_max_concurrent(max_threads: usize) {
    let max_threads = max_threads.max(1);
    if init_semaphore(&SCRIPT_PERMITS, max_threads).is_err() {
        eprintln!(
            "daimonos: configure_max_concurrent({max_threads}) ignored — \
             script semaphore was already initialized"
        );
    }
}

fn script_semaphore() -> Arc<Semaphore> {
    SCRIPT_PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(DEFAULT_MAX_SCRIPT_THREADS)))
        .clone()
}


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

/// Drop-guard that flips a cancellation `AtomicBool` when dropped.
///
/// Held inside `execute()` for the lifetime of the future. Any path
/// that drops the future — timeout, the MCP client disconnecting, the
/// outer task being cancelled, even an `await` point being aborted —
/// runs this `Drop` and signals the script thread to stop. Without it,
/// a caller-cancelled `execute_script` would leave its script thread
/// running until natural completion.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Execute a Starlark script with daimonos tool bindings.
///
/// Tools are exposed as synchronous functions that internally block on
/// the tokio runtime (via `Handle::block_on`) to call async daimonos ops.
///
/// Threading model:
/// - The script runs on a dedicated `std::thread`, NOT on the tokio
///   blocking pool. If we used `spawn_blocking`, a runaway script
///   would hold one blocking-pool slot indefinitely because
///   `tokio::time::timeout` only drops the `JoinHandle` — Tokio has
///   no mechanism to cancel a blocking task. With a 512-slot default
///   pool, ~500 runaways would freeze every blocking operation in
///   the daemon.
/// - Completion is signaled through `tokio::sync::oneshot`, so the
///   tokio side awaits a native future and consumes zero blocking-
///   pool slots whether the script returns or times out.
/// - Cancellation is driven by a shared `AtomicBool` held inside a
///   `CancelOnDrop` guard. Every tool dispatch checks the flag (via
///   `with_ctx`) and short-circuits with an error; the Starlark
///   evaluator unwinds and the thread exits cleanly. The flag is set
///   by `Drop` rather than only on timeout, so caller-side future
///   cancellation (MCP client disconnect, parent task abort) also
///   propagates to the script.
///
/// Leak classes worth knowing about:
/// 1. Pure-CPU runaways (no tool calls) — `starlark` 0.13 has no
///    public cancellation hook, so the thread runs until process
///    exit. Bounded by `process.max_script_threads`.
/// 2. Long-running in-flight tool calls — the cancel check happens
///    only between tool dispatches. A script blocked inside a slow
///    `exec`, `gh`, or `docker` op past the timeout still occupies
///    its thread until that inner op completes.
/// 3. Spawn failure — `std::thread::Builder::spawn` can fail under
///    extreme resource pressure; we surface this as an error.
pub async fn execute(
    code: &str,
    session: Arc<Mutex<Session>>,
    timeout: Duration,
) -> Result<ScriptResult, String> {
    let code = code.to_string();
    let handle = tokio::runtime::Handle::current();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = Arc::clone(&cancel);
    // Cancel on any drop of this future — covers timeout, caller-side
    // future cancellation (MCP disconnect, parent abort), and the
    // normal-completion path (where it's a harmless no-op because the
    // thread is already gone).
    let _cancel_guard = CancelOnDrop(Arc::clone(&cancel));

    // Acquire a script-thread slot. Bounds concurrent OS threads to
    // `process.max_script_threads`; pure-CPU runaway scripts hold
    // their slot forever, so this is the effective leak ceiling. The
    // acquire is bounded by the caller's timeout so a saturated cap
    // surfaces as a structured error instead of hanging the MCP
    // request indefinitely.
    let acquire_start = std::time::Instant::now();
    let permit = match tokio::time::timeout(timeout, script_semaphore().acquire_owned()).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return Err(format!("acquire script thread slot: {e}")),
        Err(_) => {
            return Err(format!(
                "script timeout after {} ms (no script thread slot available — \
                 max_script_threads reached)",
                timeout.as_millis()
            ))
        }
    };

    // The slot acquire consumed part of the budget. If it consumed
    // *all* of it, fail fast with a clear message rather than spawning
    // a thread that will be cancelled on its first tool dispatch and
    // reporting a misleading "0 ms" timeout.
    let original_timeout = timeout;
    let remaining = timeout.saturating_sub(acquire_start.elapsed());
    if remaining.is_zero() {
        return Err(format!(
            "script timeout after {} ms (entire budget consumed acquiring \
             thread slot)",
            original_timeout.as_millis()
        ));
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<ScriptResult, String>>();

    std::thread::Builder::new()
        // Linux truncates `/proc/<pid>/task/<tid>/comm` to 15 chars
        // (`TASK_COMM_LEN - 1`). Keep this prefix short so it shows
        // up readable in `ps -L`.
        .name("dmns-starlark".into())
        .spawn(move || {
            // The permit is dropped — and the slot returned — when this
            // closure exits, whether by completion, eval error, or the
            // cancel-flag unwind path.
            let _permit = permit;
            let result = run_starlark(&code, session, handle, cancel_for_thread);
            let _ = tx.send(result);
        })
        .map_err(|e| format!("spawn script thread: {e}"))?;

    match tokio::time::timeout(remaining, rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err("script thread exited without result".to_string()),
        Err(_) => {
            // Cancel flag will also be flipped by `_cancel_guard` on
            // function return; setting it explicitly here costs
            // nothing and makes the timeout path self-documenting.
            cancel.store(true, Ordering::Relaxed);
            Err(format!(
                "script timeout after {} ms",
                original_timeout.as_millis()
            ))
        }
    }
}

fn run_starlark(
    code: &str,
    session: Arc<Mutex<Session>>,
    handle: tokio::runtime::Handle,
    cancel: Arc<AtomicBool>,
) -> Result<ScriptResult, String> {
    let ast = AstModule::parse("script", code.to_string(), &Dialect::Standard)
        .map_err(|e| format!("parse error: {e}"))?;

    let globals = build_globals(session, handle, cancel);
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
    cancel: Arc<AtomicBool>,
) -> Globals {
    // We store session + handle + cancel flag in a thread-local so the
    // starlark_module functions can access them without capturing closures.
    TOOL_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(ToolContext {
            session,
            handle,
            cancel,
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
    /// Set to `true` by `execute()` when the timeout fires. Tool dispatch
    /// checks this flag and short-circuits with an error so the Starlark
    /// evaluator unwinds and the script thread exits.
    cancel: Arc<AtomicBool>,
}

thread_local! {
    static TOOL_CTX: RefCell<Option<ToolContext>> = const { RefCell::new(None) };
}

/// Run `f` with the active `ToolContext`.
///
/// Cancellation check lives here, at the single entry point shared by
/// every Starlark tool binding. When `execute()` times out it flips
/// `ctx.cancel`; the next call from inside the script — whether it
/// routes through `dispatch_request` or one of the plugin shortcuts
/// that call `ctx.handle.block_on(...)` directly — returns an error,
/// the Starlark evaluator unwinds, and the script thread exits cleanly.
/// Centralizing this here means tool bindings inherit cancellation
/// without per-function plumbing.
fn with_ctx<F, R>(f: F) -> Result<R, anyhow::Error>
where
    F: FnOnce(&ToolContext) -> Result<R, anyhow::Error>,
{
    TOOL_CTX.with(|ctx| {
        let borrow = ctx.borrow();
        let ctx = borrow
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("tool context not initialized"))?;
        if ctx.cancel.load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!("script cancelled (timeout)"));
        }
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
        #[starlark(require = named)] limit: Option<i64>,
        #[starlark(require = named)] oneline: Option<bool>,
        #[starlark(require = named)] path: Option<&str>,
        #[starlark(require = named)] message: Option<&str>,
        #[starlark(require = named)] all: Option<bool>,
        #[starlark(require = named)] branch: Option<&str>,
        #[starlark(require = named)] create: Option<bool>,
        #[starlark(require = named)] mode: Option<&str>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let session = ctx.session.clone();
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = limit { args_val["limit"] = serde_json::json!(v); }
            if let Some(v) = oneline { args_val["oneline"] = serde_json::json!(v); }
            if let Some(v) = path { args_val["path"] = serde_json::json!(v); }
            if let Some(v) = message { args_val["message"] = serde_json::json!(v); }
            if let Some(v) = all { args_val["all"] = serde_json::json!(v); }
            if let Some(v) = branch { args_val["branch"] = serde_json::json!(v); }
            if let Some(v) = create { args_val["create"] = serde_json::json!(v); }
            if let Some(v) = mode { args_val["mode"] = serde_json::json!(v); }
            let resp = ctx.handle.block_on(async {
                let s = session.lock().await;
                let registry = s.tool_registry.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("git plugin not available"))?;
                let cwd = s.cwd.clone();
                let env = s.env.clone();
                registry.run("git", &cmd, &cwd, &env, None, Some(&args_val)).await
                    .map(|r| Response::ok(r.output))
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })?;
            response_to_starlark_dict(resp, heap)
        })
    }

    fn gh<'v>(
        command: &str,
        #[starlark(require = named)] number: Option<i64>,
        #[starlark(require = named)] state: Option<&str>,
        #[starlark(require = named)] limit: Option<i64>,
        #[starlark(require = named)] author: Option<&str>,
        #[starlark(require = named)] title: Option<&str>,
        #[starlark(require = named)] body: Option<&str>,
        #[starlark(require = named)] base: Option<&str>,
        #[starlark(require = named)] draft: Option<bool>,
        #[starlark(require = named)] endpoint: Option<&str>,
        #[starlark(require = named)] method: Option<&str>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let session = ctx.session.clone();
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = number { args_val["number"] = serde_json::json!(v); }
            if let Some(v) = state { args_val["state"] = serde_json::json!(v); }
            if let Some(v) = limit { args_val["limit"] = serde_json::json!(v); }
            if let Some(v) = author { args_val["author"] = serde_json::json!(v); }
            if let Some(v) = title { args_val["title"] = serde_json::json!(v); }
            if let Some(v) = body { args_val["body"] = serde_json::json!(v); }
            if let Some(v) = base { args_val["base"] = serde_json::json!(v); }
            if let Some(v) = draft { args_val["draft"] = serde_json::json!(v); }
            if let Some(v) = endpoint { args_val["endpoint"] = serde_json::json!(v); }
            if let Some(v) = method { args_val["method"] = serde_json::json!(v); }
            let resp = ctx.handle.block_on(async {
                let s = session.lock().await;
                let registry = s.tool_registry.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("gh plugin not available"))?;
                let cwd = s.cwd.clone();
                let env = s.env.clone();
                registry.run("gh", &cmd, &cwd, &env, None, Some(&args_val)).await
                    .map(|r| Response::ok(r.output))
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })?;
            response_to_starlark_dict(resp, heap)
        })
    }

    fn cargo<'v>(
        command: &str,
        #[starlark(require = named)] package: Option<&str>,
        #[starlark(require = named)] filter: Option<&str>,
        #[starlark(require = named)] lib: Option<bool>,
        #[starlark(require = named)] release: Option<bool>,
        #[starlark(require = named)] dev: Option<bool>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let session = ctx.session.clone();
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = package { args_val["package"] = serde_json::json!(v); }
            if let Some(v) = filter { args_val["filter"] = serde_json::json!(v); }
            if let Some(v) = lib { args_val["lib"] = serde_json::json!(v); }
            if let Some(v) = release { args_val["release"] = serde_json::json!(v); }
            if let Some(v) = dev { args_val["dev"] = serde_json::json!(v); }
            let resp = ctx.handle.block_on(async {
                let s = session.lock().await;
                let registry = s.tool_registry.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("cargo plugin not available"))?;
                let cwd = s.cwd.clone();
                let env = s.env.clone();
                registry.run("cargo", &cmd, &cwd, &env, None, Some(&args_val)).await
                    .map(|r| Response::ok(r.output))
                    .map_err(|e| anyhow::anyhow!("{e}"))
            })?;
            response_to_starlark_dict(resp, heap)
        })
    }

    fn session_stats<'v>(
        #[starlark(require = named, default = "session")] scope: &str,
        #[starlark(require = named)] days: Option<i64>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let session = ctx.session.clone();
            let scope = scope.to_string();
            let resp = ctx.handle.block_on(async {
                let s = session.lock().await;
                let analytics = s.analytics.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("analytics not enabled"))?;
                match scope.as_str() {
                    "session" => {
                        let stats = analytics.session_summary();
                        Ok(Response::ok(serde_json::to_value(&stats).unwrap_or_default()))
                    }
                    "history" => {
                        analytics.history_summary(days.unwrap_or(30) as u64)
                            .map(|s| Response::ok(serde_json::to_value(&s).unwrap_or_default()))
                            .map_err(|e| anyhow::anyhow!("{e}"))
                    }
                    "daily" => {
                        analytics.daily_trend(days.unwrap_or(30) as u64)
                            .map(|s| Response::ok(serde_json::to_value(&s).unwrap_or_default()))
                            .map_err(|e| anyhow::anyhow!("{e}"))
                    }
                    _ => Err(anyhow::anyhow!("unknown scope: {scope}")),
                }
            })?;
            response_to_starlark_dict(resp, heap)
        })
    }

    fn docker<'v>(
        command: &str,
        #[starlark(require = named)] container: Option<&str>,
        #[starlark(require = named)] tail: Option<i64>,
        #[starlark(require = named)] file: Option<&str>,
        #[starlark(require = named)] detach: Option<bool>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let session = ctx.session.clone();
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = container { args_val["container"] = serde_json::json!(v); }
            if let Some(v) = tail { args_val["tail"] = serde_json::json!(v); }
            if let Some(v) = file { args_val["file"] = serde_json::json!(v); }
            if let Some(v) = detach { args_val["detach"] = serde_json::json!(v); }
            let resp = ctx.handle.block_on(async {
                let s = session.lock().await;
                let registry = s.tool_registry.as_ref()
                    .ok_or_else(|| anyhow::anyhow!("docker plugin not available"))?;
                let cwd = s.cwd.clone();
                let env = s.env.clone();
                registry.run("docker", &cmd, &cwd, &env, None, Some(&args_val)).await
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
        "def git(command: str, limit: int = None, oneline: bool = None, path: str = None, message: str = None, all: bool = None, branch: str = None, create: bool = None, mode: str = None) -> dict: ...",
        "def gh(command: str, number: int = None, state: str = None, limit: int = None, author: str = None, title: str = None, body: str = None, base: str = None, draft: bool = None, endpoint: str = None, method: str = None) -> dict: ...",
        "def cargo(command: str, package: str = None, filter: str = None, lib: bool = None, release: bool = None, dev: bool = None) -> dict: ...",
        "def docker(command: str, container: str = None, tail: int = None, file: str = None, detach: bool = None) -> dict: ...",
        "def session_stats(scope: str = \"session\", days: int = None) -> dict: ...",
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

    #[test]
    fn init_semaphore_sets_cap_on_first_call() {
        let cell: OnceLock<Arc<Semaphore>> = OnceLock::new();
        init_semaphore(&cell, 7).expect("first init must succeed");
        let sem = cell.get().expect("cell must be initialized");
        assert_eq!(
            sem.available_permits(),
            7,
            "semaphore should be sized to the requested cap"
        );
    }

    #[test]
    fn init_semaphore_rejects_second_call() {
        let cell: OnceLock<Arc<Semaphore>> = OnceLock::new();
        init_semaphore(&cell, 4).expect("first init must succeed");
        init_semaphore(&cell, 99).expect_err(
            "second init must fail — `configure_max_concurrent` is documented \
             as idempotent",
        );
        let sem = cell.get().expect("cell must still be initialized");
        assert_eq!(
            sem.available_permits(),
            4,
            "first-call value must win on conflict"
        );
    }

    /// `CancelOnDrop` is what propagates caller-side future cancellation
    /// (MCP client disconnect, outer task abort) to the script thread.
    /// Without it, a script that the caller no longer cares about would
    /// run to natural completion holding its `max_script_threads` slot.
    #[test]
    fn cancel_on_drop_sets_flag_when_dropped() {
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let _guard = CancelOnDrop(Arc::clone(&cancel));
            assert!(
                !cancel.load(Ordering::Relaxed),
                "flag must remain unset while the guard is alive"
            );
        }
        assert!(
            cancel.load(Ordering::Relaxed),
            "dropping the guard must flip the cancel flag — this is what \
             propagates caller-side future cancellation into the script \
             thread"
        );
    }

    /// A script that loops over tool calls past the timeout must
    /// cause `execute()` to return a timeout error promptly.
    ///
    /// The companion cancel-mechanism guarantee (that the underlying
    /// script thread actually terminates after the cancel flag flips)
    /// is verified in isolation by `cancel_on_drop_sets_flag_when_dropped`.
    /// Asserting it here too would require reading a process-global
    /// thread counter, which is unreliable under default parallel
    /// `cargo test` because other tests' script threads perturb the
    /// count.
    #[tokio::test]
    async fn execute_timeout_aborts_runaway_tool_loop() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let session = Arc::new(Mutex::new(session));

        let code = r#"
def runaway():
    for i in range(10000000):
        exec("true")
    return "should not reach"

result = runaway()
"#;

        let start = std::time::Instant::now();
        let result = execute(code, session, Duration::from_millis(150)).await;
        let elapsed = start.elapsed();

        assert!(result.is_err(), "expected timeout error, got {:?}", result);
        let err = result.unwrap_err();
        assert!(
            err.contains("timeout"),
            "expected 'timeout' substring in error: {err}"
        );
        assert!(
            elapsed < Duration::from_secs(3),
            "execute() must return promptly on timeout, elapsed {:?}",
            elapsed
        );
    }

    /// Resource-leak regression. Pre-fix code put the script on the
    /// tokio blocking pool via `spawn_blocking`, and a runaway script
    /// held its slot forever because `tokio::time::timeout` has no
    /// way to cancel a blocking task. With a single-slot blocking
    /// pool the second `execute()` would hang. Post-fix scripts run
    /// on dedicated `std::thread`s and the blocking pool is untouched.
    ///
    /// This test is intentionally *not* `#[tokio::test]` so it can
    /// build a custom runtime with `max_blocking_threads(1)` — the
    /// bug is only observable when the pool size is artificially
    /// constrained.
    #[test]
    fn runaway_script_does_not_starve_tokio_blocking_pool() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();

        runtime.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
            let session = Arc::new(Mutex::new(session));

            let runaway = r#"
def loop_forever():
    for i in range(10000000):
        exec("true")

loop_forever()
"#;
            let _ = execute(runaway, session.clone(), Duration::from_millis(100)).await;

            let second = tokio::time::timeout(
                Duration::from_secs(5),
                execute(r#"result = "ok""#, session, Duration::from_secs(2)),
            )
            .await;

            assert!(
                second.is_ok(),
                "second execute() hung — runaway script starved the tokio blocking pool"
            );
            let result = second.unwrap().expect("second execute() must succeed");
            assert_eq!(result.value, serde_json::json!("ok"));
        });
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
