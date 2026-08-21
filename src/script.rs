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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{Mutex, Semaphore};
use tracing::Instrument;

use crate::analytics::{self, ToolCallRecord};
use crate::config::DEFAULT_MAX_SCRIPT_THREADS;
use crate::ops;
use crate::protocol::{Request, Response};
use crate::providers::{
    CompleteOpts, ContentBlock, Context, LlmProvider, Message, StopReason, ThinkingLevel, Usage,
};
use crate::session::Session;
use crate::tools;

pub const DEFAULT_SCRIPT_TIMEOUT_SECS: u64 = 60;
/// Internal safety ceiling for model-supplied timeouts; not a tuning knob.
pub const MAX_SCRIPT_TIMEOUT_SECS: i64 = 3600;

pub fn bounded_timeout_secs(raw: Option<i64>) -> u64 {
    raw.filter(|seconds| *seconds > 0)
        .map(|seconds| seconds.min(MAX_SCRIPT_TIMEOUT_SECS) as u64)
        .unwrap_or(DEFAULT_SCRIPT_TIMEOUT_SECS)
}

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
/// Returns `false` if the cell was already initialized, so callers
/// can surface that to the operator.
///
/// Extracted from `configure_max_concurrent` so unit tests can exercise
/// the locking semantics against a local `OnceLock` without touching
/// the process-global `SCRIPT_PERMITS`.
fn init_semaphore(cell: &OnceLock<Arc<Semaphore>>, n: usize) -> bool {
    cell.set(Arc::new(Semaphore::new(n))).is_ok()
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
    if !init_semaphore(&SCRIPT_PERMITS, max_threads) {
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
///
/// Run a Starlark script, discarding the dispatched-op count. Most callers
/// (the MCP server, tests) don't need it; the agent loop uses
/// [`execute_with_op_count`] to feed the parent span's batch_size.
pub async fn execute(
    code: &str,
    session: Arc<Mutex<Session>>,
    timeout: Duration,
) -> Result<ScriptResult, String> {
    execute_with_op_count(code, session, timeout, Arc::new(AtomicUsize::new(0)), None).await
}

/// As [`execute`], but the caller owns the op counter so it can read the
/// number of tool ops the script dispatched even if the script errors
/// mid-run (used as the execute_script span's `daimonos.tool.batch_size`).
pub async fn execute_with_op_count(
    code: &str,
    session: Arc<Mutex<Session>>,
    timeout: Duration,
    op_count: Arc<AtomicUsize>,
    subcall: Option<SubcallEnv>,
) -> Result<ScriptResult, String> {
    let code = code.to_string();
    let handle = tokio::runtime::Handle::current();
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_for_thread = Arc::clone(&cancel);

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

    // Cancel on any drop of this future from this point forward —
    // covers timeout and caller-side future cancellation (MCP client
    // disconnect, parent task abort). Installed *after* permit
    // acquisition so the acquire-timeout error path doesn't flip a
    // flag no thread will ever read.
    let _cancel_guard = CancelOnDrop(Arc::clone(&cancel));

    // The slot acquire consumed part of the budget. If it consumed
    // *all* of it, fail fast with a clear message rather than spawning
    // a thread that will be cancelled on its first tool dispatch and
    // reporting a misleading "0 ms" timeout.
    let remaining = timeout.saturating_sub(acquire_start.elapsed());
    if remaining.is_zero() {
        return Err(format!(
            "script timeout after {} ms (entire budget consumed acquiring \
             thread slot)",
            timeout.as_millis()
        ));
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<ScriptResult, String>>();

    // Carry the caller's tracing context (the `agent.prompt` / `tool.call`
    // span, when execute_script runs under the internal agent loop) onto the
    // dedicated OS thread, so spans created during dispatch nest under it
    // instead of orphaning. A no-op when there is no active span.
    let parent_span = tracing::Span::current();

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
            // `in_scope` keeps the parent span active for exactly the
            // synchronous script run on this dedicated thread, rather than
            // holding an `enter()` guard across the whole closure.
            let result = parent_span.in_scope(|| {
                run_starlark(&code, session, handle, cancel_for_thread, op_count, subcall)
            });
            let _ = tx.send(result);
        })
        .map_err(|e| format!("spawn script thread: {e}"))?;

    match tokio::time::timeout(remaining, rx).await {
        Ok(Ok(r)) => r,
        // The sender is dropped without sending only if the script
        // thread panicked before reaching `tx.send`. Surface that
        // rather than the vaguer "exited without result".
        Ok(Err(_)) => Err("script thread panicked before producing a result".to_string()),
        // Cancel-on-drop will flip the flag when `_cancel_guard`
        // unwinds at function return; no explicit `cancel.store`
        // needed here.
        Err(_) => Err(format!("script timeout after {} ms", timeout.as_millis())),
    }
}

/// Pre-process Starlark code so that single/double-quoted string literals
/// containing literal newlines are converted to triple-quoted form.
///
/// Starlark (unlike Python) does not permit literal newlines inside `"..."` or
/// `'...'` strings. Models often produce such strings when embedding multi-line
/// file content. This normalization converts them before the Starlark parser
/// sees the code, turning confusing "unfinished string literal" parse errors
/// into valid scripts.
///
/// Triple-quoted strings already in the code are copied verbatim. Comments
/// (`# ...`) are skipped correctly. Backslash escape sequences are preserved.
///
/// If the content contains the same triple-quote sequence as the delimiter
/// (e.g. `"""` inside a `"..."` string), the other quote style is tried. If
/// both are present, literal newlines are backslash-escaped instead — this
/// is semantically imperfect but at least produces parseable code. That
/// edge case is extremely rare in practice.
fn normalize_string_literals(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(code.len() + 32);
    let mut i = 0;

    while i < n {
        let ch = chars[i];

        // Comments: copy everything to end of line.
        if ch == '#' {
            while i < n && chars[i] != '\n' {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if ch == '"' || ch == '\'' {
            let q = ch;

            // Triple-quoted string: copy verbatim until matching closing triple.
            if i + 2 < n && chars[i + 1] == q && chars[i + 2] == q {
                out.push(q);
                out.push(q);
                out.push(q);
                i += 3;
                while i < n {
                    if chars[i] == '\\' && i + 1 < n {
                        out.push(chars[i]);
                        out.push(chars[i + 1]);
                        i += 2;
                    } else if chars[i] == q && i + 2 < n && chars[i + 1] == q && chars[i + 2] == q {
                        out.push(q);
                        out.push(q);
                        out.push(q);
                        i += 3;
                        break;
                    } else {
                        out.push(chars[i]);
                        i += 1;
                    }
                }
                continue;
            }

            // Single-quoted string: collect content, watching for literal newlines.
            i += 1;
            let mut content: Vec<char> = Vec::new();
            let mut has_newline = false;

            'outer: loop {
                if i >= n {
                    break;
                }
                match chars[i] {
                    c if c == q => {
                        i += 1;
                        break;
                    }
                    '\n' => {
                        has_newline = true;
                        content.push('\n');
                        i += 1;
                        // Collect remaining content until closing quote.
                        loop {
                            if i >= n {
                                break 'outer;
                            }
                            if chars[i] == '\\' && i + 1 < n {
                                content.push('\\');
                                content.push(chars[i + 1]);
                                i += 2;
                            } else if chars[i] == q {
                                i += 1;
                                break 'outer;
                            } else {
                                content.push(chars[i]);
                                i += 1;
                            }
                        }
                    }
                    '\\' if i + 1 < n => {
                        content.push('\\');
                        content.push(chars[i + 1]);
                        i += 2;
                    }
                    c => {
                        content.push(c);
                        i += 1;
                    }
                }
            }

            if has_newline {
                let content_str: String = content.iter().collect();
                // Use the same quote style as the original. Collision with
                // triple-quote delimiters is impossible for valid inputs:
                // an unescaped `"` inside `"..."` always closes the string,
                // so `"""` can never appear in the extracted content.
                out.push(q);
                out.push(q);
                out.push(q);
                out.push_str(&content_str);
                out.push(q);
                out.push(q);
                out.push(q);
            } else {
                out.push(q);
                for c in &content {
                    out.push(*c);
                }
                out.push(q);
            }
            continue;
        }

        out.push(ch);
        i += 1;
    }

    out
}

/// Recover Python-shaped scripts that put control flow at module scope.
///
/// Standard Starlark rejects top-level `for`/`if` statements, but models
/// routinely emit them despite prompt guidance. Wrap only after that specific
/// parser error so valid scripts and unrelated syntax errors retain their
/// original semantics and diagnostics.
fn wrap_top_level_control_flow(code: &str) -> String {
    let mut wrapped = String::with_capacity(code.len() + 64);
    wrapped.push_str("def __daimonos_main__():\n");
    for line in code.lines() {
        wrapped.push_str("    ");
        wrapped.push_str(line);
        wrapped.push('\n');
    }
    wrapped.push_str("    return result\nresult = __daimonos_main__()\n");
    wrapped
}

fn run_starlark(
    code: &str,
    session: Arc<Mutex<Session>>,
    handle: tokio::runtime::Handle,
    cancel: Arc<AtomicBool>,
    op_count: Arc<AtomicUsize>,
    subcall: Option<SubcallEnv>,
) -> Result<ScriptResult, String> {
    let code = normalize_string_literals(code);
    let ast = match AstModule::parse("script", code.clone(), &Dialect::Standard) {
        Ok(ast) => ast,
        Err(error) if error.to_string().contains("cannot be used outside `def`") => {
            AstModule::parse(
                "script",
                wrap_top_level_control_flow(&code),
                &Dialect::Standard,
            )
            .map_err(|retry_error| format!("parse error: {retry_error}"))?
        }
        Err(error) => return Err(format!("parse error: {error}")),
    };

    let globals = build_globals(session, handle, cancel, op_count, subcall);
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
    op_count: Arc<AtomicUsize>,
    subcall: Option<SubcallEnv>,
) -> Globals {
    // We store session + handle + cancel flag in a thread-local so the
    // starlark_module functions can access them without capturing closures.
    TOOL_CTX.with(|ctx| {
        *ctx.borrow_mut() = Some(ToolContext {
            session,
            handle,
            subcall,
            cancel,
            op_count,
        });
    });

    GlobalsBuilder::standard()
        .with(builtin_functions)
        .with(tool_functions)
        .with(subcall_functions)
        .build()
}

/// Everything an in-script LLM sub-call (`llm_query` / `llm_query_batched`,
/// ADR-008) needs. Present only in agent/chat/ACP mode with a provider
/// attached and `process.script_llm_enabled` set; `None` disables the
/// builtins (they raise a clear error). Constructed by the execute_script
/// branch of the agent loop, which owns the same `usage`/`count` `Arc`s and
/// reads them back after the script finishes to fold sub-call spend into the
/// turn total.
pub struct SubcallEnv {
    pub provider: Arc<dyn LlmProvider>,
    pub model: String,
    pub max_tokens: u32,
    /// Total sub-calls a single script may issue (`llm_query` counts as 1,
    /// `llm_query_batched` as its prompt count).
    pub max_subcalls: usize,
    /// Max prompts a single `llm_query_batched` call may take.
    pub max_batch: usize,
    /// Accumulated usage across every sub-call this script issued; read by
    /// the caller after the run to fold into the agent turn's total.
    pub usage: Arc<std::sync::Mutex<Usage>>,
    /// Sub-calls issued so far, enforcing `max_subcalls`. Mutated only on the
    /// single script thread, so plain load/store is race-free.
    pub count: Arc<AtomicUsize>,
    /// Shared generation-ordinal sequence, so sub-call `llm.generation` spans
    /// interleave correctly with the agent's own generations (ADR-006).
    pub ordinal: Arc<AtomicU64>,
}

struct ToolContext {
    session: Arc<Mutex<Session>>,
    handle: tokio::runtime::Handle,
    /// In-script LLM sub-call environment (ADR-008); `None` when disabled.
    subcall: Option<SubcallEnv>,
    /// Set to `true` by `execute()` when the timeout fires. Tool dispatch
    /// checks this flag and short-circuits with an error so the Starlark
    /// evaluator unwinds and the script thread exits.
    cancel: Arc<AtomicBool>,
    /// Caller-owned counter of tool ops dispatched via `dispatch_request`,
    /// surfaced as the parent execute_script span's batch_size (ADR-006 D5).
    /// Owned by the caller so the count survives even when the script errors
    /// after dispatching some ops.
    op_count: Arc<AtomicUsize>,
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

fn request_command_hint(request: &Request) -> Option<String> {
    use crate::protocol::op;
    match request {
        Request::Single(opc) if opc.c == op::EXEC || opc.c == op::BG => opc.s.clone(),
        _ => None,
    }
}

fn request_batch_size(request: &Request) -> u32 {
    match request {
        Request::Single(_) => 1,
        Request::Batch { batch } => batch.len().max(1) as u32,
    }
}

fn response_chars(resp: &Response) -> usize {
    match &resp.d {
        Some(d) => serde_json::to_string(d).map(|s| s.len()).unwrap_or(0),
        None => resp.m.as_ref().map(|m| m.len()).unwrap_or(0),
    }
}

fn dispatch_request(request: Request, label: &str) -> Result<Response, anyhow::Error> {
    // `Request` is deserialize-only today; debug-form length is enough for the
    // coarse token-estimation heuristic used by analytics.
    let request_chars = format!("{request:?}").len();
    let command = request_command_hint(&request);
    let batch_size = request_batch_size(&request);
    let started = std::time::Instant::now();

    with_ctx(|ctx| {
        ctx.op_count.fetch_add(1, Ordering::Relaxed);
        // Child `tool.call` span for this script op. It nests under the
        // execute_script parent span propagated onto this thread (ADR-006 D4,
        // vikunja #1045) and reuses the same signals as the analytics record.
        // Metadata-only — no command/args/result bodies (D6).
        let child_span =
            crate::observability::ToolSpan::new(label, crate::observability::tool_kind(label));
        let session = ctx.session.clone();
        let resp = child_span.span().in_scope(|| {
            ctx.handle.block_on(async {
                let mut s = session.lock().await;
                let resp = ops::dispatch(&mut s, request).await;
                if let Some(analytics) = s.analytics.clone() {
                    let resp_chars = response_chars(&resp);
                    let (saved_tokens, savings_pct) =
                        analytics::compute_savings(resp.meta.unfiltered_chars, resp_chars);
                    let record = ToolCallRecord {
                        tool_name: format!("script:{label}"),
                        command,
                        request_tokens: analytics::estimate_tokens(request_chars),
                        response_tokens: analytics::estimate_tokens(resp_chars),
                        saved_tokens,
                        savings_pct,
                        exec_time_ms: started.elapsed().as_millis() as u64,
                        was_redirect: resp.meta.redirect_via_plugin,
                        was_filtered: resp.meta.filter_applied,
                        read_dedup: resp.meta.read_dedup,
                        batch_size,
                        external_session_id: s.external_session_id.clone(),
                    };
                    analytics.record_async(record);
                }
                resp
            })
        });
        let resp_chars = response_chars(&resp);
        let (saved_tokens_est, _) =
            analytics::compute_savings(resp.meta.unfiltered_chars, resp_chars);
        let status = if resp.ok {
            crate::observability::ToolStatus::Success
        } else {
            crate::observability::ToolStatus::Error
        };
        child_span.finish(
            status,
            crate::observability::ToolOutcome {
                request_tokens_est: analytics::estimate_tokens(request_chars),
                response_tokens_est: analytics::estimate_tokens(resp_chars),
                saved_tokens_est,
                redirect: resp.meta.redirect_via_plugin,
                filtered: resp.meta.filter_applied,
                read_dedup: resp.meta.read_dedup,
                batch_size: batch_size as u64,
            },
        );
        Ok(resp)
    })
}

/// Dispatch any native tool by name. Opcode-backed tools take the compact op
/// path; plugin and meta tools fall through to the same shared dispatcher used
/// by MCP and the agent loop. Keeping `tool("…")` universal prevents the model
/// from spending retry turns learning which names require dedicated bindings.
fn dispatch_tool_by_name(name: &str, args: &serde_json::Value) -> Result<Response, anyhow::Error> {
    let resp = match tools::build_request(name, args) {
        Some(Ok(request)) => dispatch_request(request, name)?,
        Some(Err(e)) => return Err(anyhow::anyhow!("{e}")),
        None => with_ctx(|ctx| {
            let started = std::time::Instant::now();
            let request_chars = serde_json::to_string(args).map(|s| s.len()).unwrap_or(0);
            ctx.handle.block_on(async {
                let mut session = ctx.session.lock().await;
                let Some((content, is_error, meta)) =
                    crate::mcp::dispatch_local_tool(&mut session, name, args).await
                else {
                    return Err(anyhow::anyhow!("unknown tool '{name}'"));
                };

                if let Some(analytics) = session.analytics.clone() {
                    let (saved_tokens, savings_pct) =
                        analytics::compute_savings(meta.unfiltered_chars, content.len());
                    analytics.record_async(ToolCallRecord {
                        tool_name: format!("script:{name}"),
                        command: args
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        request_tokens: analytics::estimate_tokens(request_chars),
                        response_tokens: analytics::estimate_tokens(content.len()),
                        saved_tokens,
                        savings_pct,
                        exec_time_ms: started.elapsed().as_millis() as u64,
                        was_redirect: meta.redirect_via_plugin,
                        was_filtered: meta.filter_applied,
                        read_dedup: meta.read_dedup,
                        batch_size: 1,
                        external_session_id: session.external_session_id.clone(),
                    });
                }

                let mut response = if is_error {
                    Response::err(5, &content)
                } else {
                    // Local tools return MCP text content: structured tools encode
                    // JSON, while intentionally textual meta tools (for example
                    // list_tool_signatures) remain a string under `data`.
                    let value = serde_json::from_str(&content)
                        .unwrap_or_else(|_| serde_json::Value::String(content));
                    Response::ok(value)
                };
                response.meta = meta;
                Ok(response)
            })
        })?,
    };
    // KGL observed-provenance: capture script-driven file ops the same way the
    // MCP-direct path does. Gated off by default; best-effort (never affects the
    // tool result). This is the Starlark layer feeding the graph.
    if resp.ok
        && matches!(name, "write_file" | "edit_file" | "read_file")
        && crate::kgl::observe::enabled()
    {
        record_script_observation(name, args);
    }
    Ok(resp)
}

/// Best-effort: record a script-driven file op as an observed KGL edge, reading
/// the session's workspace + id from the tool context.
fn record_script_observation(name: &str, args: &serde_json::Value) {
    let _ = with_ctx(|ctx| {
        let (ws, cwd, sid, kcfg) = ctx.handle.block_on(async {
            let s = ctx.session.lock().await;
            (
                s.workspace.clone(),
                s.cwd.clone(),
                s.external_session_id.clone(),
                s.cfg.kgl.clone(),
            )
        });
        let now = chrono::Utc::now().to_rfc3339();
        let sid = sid.unwrap_or_else(|| "unknown".to_string());
        let _ = crate::kgl::observe::record_file_op(&ws, &cwd, &sid, name, args, &now, &kcfg);
        Ok(())
    });
}

fn run_registry_tool(
    ctx: &ToolContext,
    tool_id: &str,
    command: &str,
    args_val: &Value,
) -> Result<Response, anyhow::Error> {
    let started = std::time::Instant::now();
    let request_chars = serde_json::to_string(args_val)
        .map(|s| s.len())
        .unwrap_or(0);

    let resp = ctx.handle.block_on(async {
        let s = ctx.session.lock().await;
        let cwd = s.cwd.clone();
        let env = s.env.clone();
        let analytics = s.analytics.clone();
        let external_session_id = s.external_session_id.clone();

        let resp = if let Some(registry) = s.tool_registry.as_ref() {
            match registry
                .run(tool_id, command, &cwd, &env, None, Some(args_val))
                .await
            {
                Ok(r) => Response::ok(r.output),
                Err(e) => Response::err(5, &e),
            }
        } else {
            Response::err(5, &format!("{tool_id} plugin not available"))
        };

        if let Some(analytics) = analytics {
            let resp_chars = response_chars(&resp);
            let (saved_tokens, savings_pct) =
                analytics::compute_savings(resp.meta.unfiltered_chars, resp_chars);
            let record = ToolCallRecord {
                tool_name: format!("script:{tool_id}"),
                command: Some(command.to_string()),
                request_tokens: analytics::estimate_tokens(request_chars),
                response_tokens: analytics::estimate_tokens(resp_chars),
                saved_tokens,
                savings_pct,
                exec_time_ms: started.elapsed().as_millis() as u64,
                was_redirect: resp.meta.redirect_via_plugin,
                was_filtered: resp.meta.filter_applied,
                read_dedup: resp.meta.read_dedup,
                batch_size: 1,
                external_session_id,
            };
            analytics.record_async(record);
        }

        resp
    });

    Ok(resp)
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

fn response_to_starlark_val<'v>(
    resp: Response,
    heap: &'v Heap,
) -> anyhow::Result<StarlarkValue<'v>> {
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

// --- In-script LLM sub-call bindings (ADR-008) ---

/// Concatenate the text blocks of a sub-call response. Thinking and any
/// (unused) tool blocks are dropped — a sub-call is a plain text completion.
fn subcall_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        if let ContentBlock::Text(t) = block {
            out.push_str(t);
        }
    }
    out
}

/// Reserve `n` sub-call slots against the per-script budget, or error.
///
/// `count` is mutated only here, and the builtins call this from the single
/// Starlark script thread *before* any fan-out — `llm_query_batched` reserves
/// its whole batch up front, and the concurrent per-prompt futures never touch
/// `count`. So the non-atomic load+store cannot race.
fn reserve_subcalls(env: &SubcallEnv, n: usize) -> Result<(), anyhow::Error> {
    let used = env.count.load(Ordering::Relaxed);
    if used + n > env.max_subcalls {
        return Err(anyhow::anyhow!(
            "llm sub-call budget exhausted: {used} used + {n} requested \
             > max_script_subcalls = {}",
            env.max_subcalls
        ));
    }
    env.count.store(used + n, Ordering::Relaxed);
    Ok(())
}

/// Classify a sub-call response into an error message, or `None` if it is a
/// usable completion. Only `EndTurn`/`MaxTokens` (and the tools-less
/// `ToolUse`) yield text — `MaxTokens` is truncated but still usable, whereas
/// a `Refusal`, `Aborted`, or `Error` must not masquerade as a successful
/// empty result.
fn classify_subcall_failure(resp: &crate::providers::LlmResponse) -> Option<String> {
    match resp.stop_reason {
        StopReason::EndTurn | StopReason::MaxTokens | StopReason::ToolUse => None,
        StopReason::Error => Some(
            resp.error_message
                .clone()
                .unwrap_or_else(|| "llm sub-call failed".to_string()),
        ),
        StopReason::Refusal => Some("llm sub-call refused".to_string()),
        StopReason::Aborted => Some("llm sub-call aborted".to_string()),
    }
}

/// Run one LLM sub-call: emit a `script_subcall` generation span, call the
/// provider, fold spend into the shared usage accumulator (regardless of
/// outcome — a failed call can still have burned input tokens), and return
/// the completion text or a per-call error string.
///
/// NOTE: an in-flight sub-call is not interrupted by the parent
/// `execute_script` timeout/cancel. Like a native tool op, cancellation is
/// observed *between* sandbox operations (in `with_ctx`), not mid-call, so a
/// slow provider call can outlive the turn. Callers needing a hard bound
/// should keep sub-call prompts small.
async fn run_one_subcall_async(env: &SubcallEnv, prompt: &str) -> Result<String, String> {
    let ordinal = env.ordinal.fetch_add(1, Ordering::Relaxed);
    let generation =
        crate::observability::GenerationSpan::new(crate::observability::GenerationMetadata {
            kind: "script_subcall",
            model: &env.model,
            max_tokens: env.max_tokens,
            thinking: ThinkingLevel::Off,
            temperature: None,
            ordinal,
            tools_exposed: 0,
            stable_prefix_len: 0,
        });
    let context = Context {
        messages: vec![Message::user(prompt)],
        system: None,
        tools: vec![],
        stable_prefix_len: 0,
    };
    let opts = CompleteOpts {
        model: env.model.clone(),
        max_tokens: env.max_tokens,
        thinking: ThinkingLevel::Off,
        temperature: None,
    };
    let resp = env
        .provider
        .complete(&context, &opts)
        .instrument(generation.span().clone())
        .await;
    // Recover a poisoned lock's inner value rather than silently dropping this
    // sub-call's spend (matches the caller-side fold-in in the agent loop).
    let mut acc = env.usage.lock().unwrap_or_else(|p| p.into_inner());
    *acc = crate::agent::accumulate_usage(std::mem::take(&mut *acc), resp.usage.clone());
    drop(acc);
    let text = subcall_text(&resp.content);
    let failure = classify_subcall_failure(&resp);
    generation.finish(&resp);
    match failure {
        Some(msg) => Err(msg),
        None => Ok(text),
    }
}

/// `llm_query` helper: drive a single sub-call to completion on the script
/// thread, surfacing any provider/refusal error as an `anyhow` error.
fn run_one_subcall(
    ctx: &ToolContext,
    env: &SubcallEnv,
    prompt: &str,
) -> Result<String, anyhow::Error> {
    ctx.handle
        .block_on(run_one_subcall_async(env, prompt))
        .map_err(|e| anyhow::anyhow!(e))
}

/// Build a `{ok, text, error}` outcome dict for one batched sub-call.
fn outcome_dict<'v>(
    heap: &'v Heap,
    ok: bool,
    text: Option<String>,
    error: Option<String>,
) -> Dict<'v> {
    let mut m = starlark::collections::SmallMap::new();
    let ok_key = heap.alloc_str("ok").to_value();
    m.insert_hashed(ok_key.get_hashed().expect("string key"), heap.alloc(ok));
    let text_key = heap.alloc_str("text").to_value();
    let text_val = match text {
        Some(t) => heap.alloc(t),
        None => StarlarkValue::new_none(),
    };
    m.insert_hashed(text_key.get_hashed().expect("string key"), text_val);
    let err_key = heap.alloc_str("error").to_value();
    let err_val = match error {
        Some(e) => heap.alloc(e),
        None => StarlarkValue::new_none(),
    };
    m.insert_hashed(err_key.get_hashed().expect("string key"), err_val);
    Dict::new(m)
}

const SUBCALL_DISABLED: &str = "in-script LLM sub-calls are off \
    (set process.script_llm_enabled) or no provider is attached \
    (one-shot / agent-cmd mode)";

#[starlark_module]
fn subcall_functions(builder: &mut GlobalsBuilder) {
    /// `llm_query(prompt) -> str`: one blocking LLM completion. Raises on
    /// provider error or when the sub-call budget is exhausted.
    fn llm_query(prompt: &str) -> anyhow::Result<String> {
        with_ctx(|ctx| {
            let env = ctx
                .subcall
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("llm_query unavailable: {SUBCALL_DISABLED}"))?;
            reserve_subcalls(env, 1)?;
            run_one_subcall(ctx, env, prompt)
        })
    }

    /// `llm_query_batched(prompts) -> list[{ok, text, error}]`: fan out one
    /// completion per prompt. Each prompt yields an outcome dict so a single
    /// failure does not abort the batch (ADR-008). Raises only for a disabled
    /// environment, an over-`max_batch` request, or an exhausted budget.
    fn llm_query_batched<'v>(
        prompts: UnpackList<String>,
        heap: &'v Heap,
    ) -> anyhow::Result<StarlarkValue<'v>> {
        with_ctx(|ctx| {
            let env = ctx.subcall.as_ref().ok_or_else(|| {
                anyhow::anyhow!("llm_query_batched unavailable: {SUBCALL_DISABLED}")
            })?;
            let prompts = prompts.items;
            if prompts.len() > env.max_batch {
                return Err(anyhow::anyhow!(
                    "llm_query_batched given {} prompts but max_script_subcall_batch = {}",
                    prompts.len(),
                    env.max_batch
                ));
            }
            reserve_subcalls(env, prompts.len())?;
            // Fan the prompts out concurrently on the script thread's runtime
            // — batching exists for latency (ADR-008). Results keep prompt
            // order; each yields an outcome dict so one failure does not abort
            // the batch.
            let outcomes: Vec<Result<String, String>> = ctx.handle.block_on(async {
                futures_util::future::join_all(
                    prompts.iter().map(|p| run_one_subcall_async(env, p)),
                )
                .await
            });
            let mut items = Vec::with_capacity(outcomes.len());
            for outcome in outcomes {
                let dict = match outcome {
                    Ok(text) => outcome_dict(heap, true, Some(text), None),
                    Err(e) => outcome_dict(heap, false, None, Some(e)),
                };
                items.push(heap.alloc(dict));
            }
            Ok(heap.alloc(items))
        })
    }
}

// --- Starlark tool function bindings ---

// The `#[starlark_module]` macro expands to a registration fn whose arity is
// the sum of all bound functions' params; clippy counts that synthetic total.
#[allow(clippy::too_many_arguments)]
#[starlark_module]
fn tool_functions(builder: &mut GlobalsBuilder) {
    /// Call any native tool by name with keyword arguments.
    ///
    /// The named bindings below cover a hand-picked subset, and that subset drifts
    /// from the tool registry as tools are added — the coordination family
    /// (`register_agent`, `send_message`, `fetch_inbox`, …) was documented in the
    /// MCP instructions while being absent here, so scripts calling it failed with
    /// `Variable register_agent not found` (vikunja 1112).
    ///
    /// This is the escape hatch that closes the class rather than one more entry
    /// in a list that has to be maintained by hand: opcode, plugin, and meta
    /// tools are all reachable through it, including tools added later.
    ///
    /// The tool name is positional-only: several tools take a `name` argument of
    /// their own (`register_agent(name=...)` above all), and a nameable positional
    /// would collide with it as "Argument `name` occurs more than once".
    fn tool<'v>(
        #[starlark(require = pos)] tool_name: &str,
        #[starlark(kwargs)] kwargs: StarlarkValue<'v>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let args = starlark_to_json(kwargs, heap);
        let resp = dispatch_tool_by_name(tool_name, &args)?;
        response_to_starlark_dict(resp, heap)
    }

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

    fn write_file<'v>(path: &str, content: &str, heap: &'v Heap) -> anyhow::Result<Dict<'v>> {
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
        args: Option<UnpackList<String>>,
        #[starlark(require = named)] cwd: Option<&str>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        let a: Option<Vec<String>> = args.and_then(|a| {
            if a.items.is_empty() {
                None
            } else {
                Some(a.items)
            }
        });
        let tool_args = serde_json::json!({"command": command, "args": a, "cwd": cwd});
        let resp = dispatch_tool_by_name("exec", &tool_args)?;
        response_to_starlark_dict(resp, heap)
    }

    fn ls<'v>(
        path: Option<&str>,
        depth: Option<i32>,
        #[starlark(require = named)] glob: Option<&str>,
        #[starlark(require = named)] r#type: Option<&str>,
        heap: &'v Heap,
    ) -> anyhow::Result<StarlarkValue<'v>> {
        let args = serde_json::json!({
            "path": path,
            "depth": depth,
            "glob": glob,
            "type": r#type,
        });
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
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = limit {
                args_val["limit"] = serde_json::json!(v);
            }
            if let Some(v) = oneline {
                args_val["oneline"] = serde_json::json!(v);
            }
            if let Some(v) = path {
                args_val["path"] = serde_json::json!(v);
            }
            if let Some(v) = message {
                args_val["message"] = serde_json::json!(v);
            }
            if let Some(v) = all {
                args_val["all"] = serde_json::json!(v);
            }
            if let Some(v) = branch {
                args_val["branch"] = serde_json::json!(v);
            }
            if let Some(v) = create {
                args_val["create"] = serde_json::json!(v);
            }
            if let Some(v) = mode {
                args_val["mode"] = serde_json::json!(v);
            }
            let resp = run_registry_tool(ctx, "git", &cmd, &args_val)?;
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
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = number {
                args_val["number"] = serde_json::json!(v);
            }
            if let Some(v) = state {
                args_val["state"] = serde_json::json!(v);
            }
            if let Some(v) = limit {
                args_val["limit"] = serde_json::json!(v);
            }
            if let Some(v) = author {
                args_val["author"] = serde_json::json!(v);
            }
            if let Some(v) = title {
                args_val["title"] = serde_json::json!(v);
            }
            if let Some(v) = body {
                args_val["body"] = serde_json::json!(v);
            }
            if let Some(v) = base {
                args_val["base"] = serde_json::json!(v);
            }
            if let Some(v) = draft {
                args_val["draft"] = serde_json::json!(v);
            }
            if let Some(v) = endpoint {
                args_val["endpoint"] = serde_json::json!(v);
            }
            if let Some(v) = method {
                args_val["method"] = serde_json::json!(v);
            }
            let resp = run_registry_tool(ctx, "gh", &cmd, &args_val)?;
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
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = package {
                args_val["package"] = serde_json::json!(v);
            }
            if let Some(v) = filter {
                args_val["filter"] = serde_json::json!(v);
            }
            if let Some(v) = lib {
                args_val["lib"] = serde_json::json!(v);
            }
            if let Some(v) = release {
                args_val["release"] = serde_json::json!(v);
            }
            if let Some(v) = dev {
                args_val["dev"] = serde_json::json!(v);
            }
            let resp = run_registry_tool(ctx, "cargo", &cmd, &args_val)?;
            response_to_starlark_dict(resp, heap)
        })
    }

    fn pytest<'v>(
        command: &str,
        #[starlark(require = named)] path: Option<&str>,
        #[starlark(require = named)] filter: Option<&str>,
        #[starlark(require = named)] markers: Option<&str>,
        #[starlark(require = named)] verbose: Option<bool>,
        #[starlark(require = named)] failfast: Option<bool>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = path {
                args_val["path"] = serde_json::json!(v);
            }
            if let Some(v) = filter {
                args_val["filter"] = serde_json::json!(v);
            }
            if let Some(v) = markers {
                args_val["markers"] = serde_json::json!(v);
            }
            if let Some(v) = verbose {
                args_val["verbose"] = serde_json::json!(v);
            }
            if let Some(v) = failfast {
                args_val["failfast"] = serde_json::json!(v);
            }
            let resp = run_registry_tool(ctx, "pytest", &cmd, &args_val)?;
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
                let analytics = s
                    .analytics
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("analytics not enabled"))?;
                match scope.as_str() {
                    "session" => {
                        let stats = analytics.session_summary();
                        Ok(Response::ok(
                            serde_json::to_value(&stats).unwrap_or_default(),
                        ))
                    }
                    "history" => analytics
                        .history_summary(days.unwrap_or(30) as u64)
                        .map(|s| Response::ok(serde_json::to_value(&s).unwrap_or_default()))
                        .map_err(|e| anyhow::anyhow!("{e}")),
                    "daily" => analytics
                        .daily_trend(days.unwrap_or(30) as u64)
                        .map(|s| Response::ok(serde_json::to_value(&s).unwrap_or_default()))
                        .map_err(|e| anyhow::anyhow!("{e}")),
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
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = container {
                args_val["container"] = serde_json::json!(v);
            }
            if let Some(v) = tail {
                args_val["tail"] = serde_json::json!(v);
            }
            if let Some(v) = file {
                args_val["file"] = serde_json::json!(v);
            }
            if let Some(v) = detach {
                args_val["detach"] = serde_json::json!(v);
            }
            let resp = run_registry_tool(ctx, "docker", &cmd, &args_val)?;
            response_to_starlark_dict(resp, heap)
        })
    }

    fn discord<'v>(
        command: &str,
        #[starlark(require = named)] guild_id: Option<&str>,
        #[starlark(require = named)] channel_id: Option<&str>,
        #[starlark(require = named)] query: Option<&str>,
        #[starlark(require = named)] limit: Option<i64>,
        #[starlark(require = named)] analytics_tag: Option<&str>,
        heap: &'v Heap,
    ) -> anyhow::Result<Dict<'v>> {
        with_ctx(|ctx| {
            let cmd = command.to_string();
            let mut args_val = serde_json::json!({"command": cmd});
            if let Some(v) = guild_id {
                args_val["guild_id"] = serde_json::json!(v);
            }
            if let Some(v) = channel_id {
                args_val["channel_id"] = serde_json::json!(v);
            }
            if let Some(v) = query {
                args_val["query"] = serde_json::json!(v);
            }
            if let Some(v) = limit {
                args_val["limit"] = serde_json::json!(v);
            }
            if let Some(v) = analytics_tag {
                args_val["analytics_tag"] = serde_json::json!(v);
            }
            let resp = run_registry_tool(ctx, "discord", &cmd, &args_val)?;
            response_to_starlark_dict(resp, heap)
        })
    }
}

/// Generate Python-style function signatures for all available tool bindings.
pub fn tool_signatures() -> String {
    let sigs = [
        "def read_file(path: str, *, offset: int = None, limit: int = None) -> dict: ...",
        "def write_file(path: str, content: str) -> dict: ...",
        "def edit_file(path: str, edits: list[str]) -> dict: ...",
        "def search(pattern: str, *, mode: str = \"content\", path: str = None, glob: str = None, max_results: int = None) -> dict: ...",
        "def exec(command: str, args: list[str] = [], *, cwd: str = None) -> dict: ...",
        "def ls(path: str = None, depth: int = None, *, glob: str = None, type: str = None) -> dict: ...",
        "def snapshot(action: str, *, id: str = None, tag: str = None) -> dict: ...",
        "def git(command: str, *, limit: int = None, oneline: bool = None, path: str = None, message: str = None, all: bool = None, branch: str = None, create: bool = None, mode: str = None) -> dict: ...",
        "def gh(command: str, *, number: int = None, state: str = None, limit: int = None, author: str = None, title: str = None, body: str = None, base: str = None, draft: bool = None, endpoint: str = None, method: str = None) -> dict: ...",
        "def cargo(command: str, *, package: str = None, filter: str = None, lib: bool = None, release: bool = None, dev: bool = None) -> dict: ...",
        "def pytest(command: str, *, path: str = None, filter: str = None, markers: str = None, verbose: bool = None, failfast: bool = None) -> dict: ...",
        "def docker(command: str, *, container: str = None, tail: int = None, file: str = None, detach: bool = None) -> dict: ...",
        "def discord(command: str, *, guild_id: str = None, channel_id: str = None, query: str = None, limit: int = None, analytics_tag: str = None) -> dict: ...",
        "def session_stats(*, scope: str = \"session\", days: int = None) -> dict: ...",
        "def print(*args) -> None:  # captured in logs",
        // The tool name is positional-only, so it must not be documented as a
        // keyword: `tool(name="register_agent")` fails, and several tools take a
        // `name` argument of their own that would collide with it.
        "def tool(tool_name: str, /, **kwargs) -> dict: ...  # any native tool, e.g. tool(\"register_agent\", name=\"BlueLake\")",
    ];
    // Every opcode-backed tool is listed below from the registry. Plugin/meta
    // tools also work through `tool()` even when they have a dedicated binding.
    // An earlier version
    // subtracted the named bindings above to avoid listing them twice, but that
    // needed a hand-maintained set of those names — reintroducing exactly the
    // drift this whole change removes. Listing all of them is redundant rather
    // than wrong: the named bindings are callable through `tool()` too, since
    // both route through `build_request`.
    let mut via_generic: Vec<&str> = tools::all_tools()
        .into_iter()
        .filter(|t| t.to_request.is_some())
        .map(|t| t.name)
        .collect();
    via_generic.sort_unstable();
    let mut out = sigs.join("\n");
    if !via_generic.is_empty() {
        out.push_str("\n# Callable as tool(\"<name>\", ...): ");
        out.push_str(&via_generic.join(", "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::AnalyticsStore;
    use crate::config::Config;
    use crate::tool_runner::{ToolCommand, ToolDescriptor, ToolPlugin, ToolRegistry, ToolResult};
    use std::collections::HashMap;
    use std::path::Path;

    fn test_session() -> Arc<Mutex<Session>> {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.keep(), Arc::new(Config::default()));
        Arc::new(Mutex::new(session))
    }

    #[test]
    fn script_timeout_defaults_and_clamps_invalid_values() {
        assert_eq!(bounded_timeout_secs(None), DEFAULT_SCRIPT_TIMEOUT_SECS);
        assert_eq!(bounded_timeout_secs(Some(-1)), DEFAULT_SCRIPT_TIMEOUT_SECS);
        assert_eq!(bounded_timeout_secs(Some(0)), DEFAULT_SCRIPT_TIMEOUT_SECS);
        assert_eq!(bounded_timeout_secs(Some(30)), 30);
        assert_eq!(
            bounded_timeout_secs(Some(MAX_SCRIPT_TIMEOUT_SECS + 1)),
            MAX_SCRIPT_TIMEOUT_SECS as u64
        );
    }

    /// vikunja 1112: the coordination tools were documented in the MCP
    /// instructions but never bound in the sandbox, so scripts calling them died
    /// with `Variable register_agent not found`. `tool(name, **kwargs)` reaches any
    /// native tool, which closes the class rather than adding one more
    /// hand-maintained binding that can drift from the registry.
    #[tokio::test]
    async fn generic_tool_binding_reaches_the_coordination_family() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.coordination.db_dir = Some(dir.path().join("coord-db").to_string_lossy().to_string());
        let session = Arc::new(Mutex::new(Session::new(
            dir.path().to_path_buf(),
            Arc::new(cfg),
        )));

        let code = concat!(
            "registered = tool(\"register_agent\", name=\"ScriptProbe\", program=\"test\")\n",
            "agents = tool(\"list_agents\")\n",
            "result = registered",
        );
        let result = execute(code, session, Duration::from_secs(10))
            .await
            .expect("coordination tools should be callable through tool()");

        // The registration receipt echoes the handle back.
        let text = serde_json::to_string(&result.value).unwrap();
        assert!(
            text.contains("ScriptProbe"),
            "expected the registration receipt to name the agent, got {text}"
        );
    }

    #[tokio::test]
    async fn generic_tool_binding_reaches_commandless_meta_tools() {
        let result = execute(
            "result = tool(\"list_tool_signatures\")",
            test_session(),
            Duration::from_secs(2),
        )
        .await
        .expect("commandless meta tools should use the shared local dispatcher");
        assert!(result.value["data"]
            .as_str()
            .is_some_and(|text| text.contains("def read_file")));
    }

    /// The failure mode this replaces: an unbound name is a Starlark resolution
    /// error, not a tool error, so it cannot be caught or worked around in-script.
    #[tokio::test]
    async fn unknown_tool_name_is_a_tool_error_not_a_missing_variable() {
        let session = test_session();
        let error = execute(
            "result = tool(\"no_such_tool\")",
            session,
            Duration::from_secs(5),
        )
        .await
        .expect_err("an unknown tool name should fail");
        assert!(
            error.contains("no_such_tool"),
            "error should name the tool, got {error}"
        );
        assert!(
            !error.contains("not found") || error.contains("opcode"),
            "should be a dispatch error, not a Starlark variable-resolution error: {error}"
        );
    }

    #[tokio::test]
    async fn execute_simple_expression() {
        let session = test_session();
        let result = execute("result = 1 + 2", session, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!(3));
    }

    #[tokio::test]
    async fn execute_normalizes_model_generated_top_level_control_flow() {
        let session = test_session();
        let code = r#"values = []
for value in range(3):
    values.append(value)
result = values"#;

        let result = execute(code, session, Duration::from_secs(5))
            .await
            .unwrap();

        assert_eq!(result.value, serde_json::json!([0, 1, 2]));
    }

    #[tokio::test]
    async fn execute_counts_dispatched_ops_for_batch_size() {
        // vikunja #1045: op_count is surfaced as the parent execute_script
        // span's batch_size. Two read_file dispatches -> op_count 2.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "A").unwrap();
        std::fs::write(dir.path().join("b.txt"), "B").unwrap();
        let session = Arc::new(Mutex::new(Session::new(
            dir.path().to_path_buf(),
            Arc::new(Config::default()),
        )));
        let op_count = Arc::new(AtomicUsize::new(0));
        let code = "_a = read_file(\"a.txt\")\n_b = read_file(\"b.txt\")\nresult = \"done\"";
        execute_with_op_count(
            code,
            session,
            Duration::from_secs(10),
            Arc::clone(&op_count),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            op_count.load(Ordering::Relaxed),
            2,
            "two read_file ops should be counted"
        );
    }

    #[tokio::test]
    async fn execute_string_result() {
        let session = test_session();
        let code = r#"result = "hello" + " world""#;
        let result = execute(code, session, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!("hello world"));
    }

    #[tokio::test]
    async fn execute_no_result_returns_null() {
        let session = test_session();
        let result = execute("x = 42", session, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!(null));
    }

    #[tokio::test]
    async fn execute_dict_result_variable() {
        let session = test_session();
        let code = r#"
x = {"a": 1, "b": [2, 3]}
result = x
"#;
        let result = execute(code, session, Duration::from_secs(5))
            .await
            .unwrap();
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
        let result = execute(code, session, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(result.logs, vec!["hello", "world"]);
        assert_eq!(result.value, serde_json::json!(true));
    }

    #[tokio::test]
    async fn execute_syntax_error() {
        let session = test_session();
        let result = execute("def (", session, Duration::from_secs(5)).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("parse error"),
            "expected parse error, got: {err}"
        );
    }

    #[tokio::test]
    async fn execute_timeout() {
        let session = test_session();
        // Starlark doesn't have sleep, but a very tight loop would take too long
        // to actually timeout. Just verify the timeout path with a short timeout.
        let code = "result = 42";
        let result = execute(code, session, Duration::from_millis(5000))
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!(42));
    }

    /// Regression: internal tool calls issued from `execute_script` must
    /// update analytics counters so `session_stats` / `workspace_info` remain
    /// a reliable verification signal when callers batch through scripts.
    #[tokio::test]
    async fn execute_script_records_internal_tool_calls_in_analytics() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let db_path = dir.path().join("script_analytics.db");
        let analytics = Arc::new(AnalyticsStore::new(&db_path, 30).unwrap());
        session.analytics = Some(analytics.clone());
        let session = Arc::new(Mutex::new(session));

        let code = r#"
write_file(path="hello.txt", content="hello")
read_file(path="hello.txt")
result = True
"#;

        let result = execute(code, session, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!(true));

        assert!(
            analytics.wait_until_quiet(Duration::from_secs(1)).await,
            "analytics writes should drain promptly"
        );
        let stats = analytics.session_summary();
        assert!(
            stats.total_calls >= 2,
            "expected at least write_file + read_file calls, got {}",
            stats.total_calls
        );
        assert!(
            stats.per_tool.contains_key("script:write_file"),
            "expected script:write_file in per_tool stats"
        );
        assert!(
            stats.per_tool.contains_key("script:read_file"),
            "expected script:read_file in per_tool stats"
        );
    }

    struct MockScriptToolPlugin {
        descriptor: ToolDescriptor,
    }

    #[async_trait::async_trait]
    impl ToolPlugin for MockScriptToolPlugin {
        fn descriptor(&self) -> &ToolDescriptor {
            &self.descriptor
        }

        async fn run_command_with_config(
            &self,
            command: &str,
            _cwd: &Path,
            _env: &HashMap<String, String>,
            _stdin_data: Option<&[u8]>,
            _args: Option<&serde_json::Value>,
            _process_cfg: &crate::config::ProcessConfig,
        ) -> Result<ToolResult, String> {
            Ok(ToolResult {
                tool: self.descriptor.id.clone(),
                command: command.to_string(),
                exit_code: 0,
                output: serde_json::json!({"ok": true}),
                stderr: String::new(),
            })
        }
    }

    #[tokio::test]
    async fn generic_tool_binding_dispatches_plugins_and_records_analytics() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let db_path = dir.path().join("script_analytics.db");
        let analytics = Arc::new(AnalyticsStore::new(&db_path, 30).unwrap());
        session.analytics = Some(analytics.clone());

        let mut commands = HashMap::new();
        commands.insert(
            "status".to_string(),
            ToolCommand {
                bin: "true".to_string(),
                args: vec![],
                output: "json".to_string(),
            },
        );
        let registry = Arc::new(ToolRegistry::new());
        registry
            .register(Arc::new(MockScriptToolPlugin {
                descriptor: ToolDescriptor {
                    id: "git".to_string(),
                    commands,
                    source_pattern: None,
                    manifest: None,
                    diagnostics_format: "json".to_string(),
                    supports_quickfix: false,
                    quickfix_format: None,
                },
            }))
            .await;
        session.tool_registry = Some(registry);
        let session = Arc::new(Mutex::new(session));

        let result = execute(
            r#"result = tool("git", command="status")"#,
            session,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(result.value, serde_json::json!({"ok": true}));

        assert!(
            analytics.wait_until_quiet(Duration::from_secs(1)).await,
            "analytics writes should drain promptly"
        );
        let stats = analytics.session_summary();
        assert!(
            stats.per_tool.contains_key("script:git"),
            "expected script:git analytics entry for plugin shortcut"
        );
    }

    #[test]
    fn init_semaphore_sets_cap_on_first_call() {
        let cell: OnceLock<Arc<Semaphore>> = OnceLock::new();
        assert!(init_semaphore(&cell, 7), "first init must succeed");
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
        assert!(init_semaphore(&cell, 4), "first init must succeed");
        assert!(
            !init_semaphore(&cell, 99),
            "second init must fail — `configure_max_concurrent` is documented \
             as idempotent"
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
            elapsed < Duration::from_millis(1500),
            "execute() must return promptly on timeout (within ~10x of the \
             150 ms request), elapsed {:?}. If this flakes under heavy CI load, \
             the bound was 3 s pre-vikunja-#251 and the prior commit tightened \
             it to 1 s; 1.5 s is the practical floor that still catches any \
             multi-second cancellation regression.",
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
            let first = execute(runaway, session.clone(), Duration::from_millis(100)).await;
            assert!(
                first.is_err(),
                "first execute() must time out — if it accidentally succeeds, \
                 the rest of the test is meaningless"
            );

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
        let result = execute(code, session, Duration::from_secs(10))
            .await
            .unwrap();
        assert_eq!(result.value, serde_json::json!("hello starlark"));
    }

    #[tokio::test]
    async fn execute_prompt_read_transform_write_idiom_is_runnable() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.txt"), "old_name").unwrap();
        let session = Arc::new(Mutex::new(Session::new(
            dir.path().to_path_buf(),
            Arc::new(Config::default()),
        )));
        let code = r#"
def main():
    c = read_file("config.txt")["content"]
    written = write_file("config.txt", c.replace("old_name", "new_name"))
    if not written.get("ok"):
        fail("write_file failed")
    tested = exec("true")
    if tested["exit"] != 0:
        fail("command failed")
    return {"ok": True, "test_exit": tested["exit"]}

result = main()
"#;

        let result = execute(code, session, Duration::from_secs(10))
            .await
            .unwrap();

        assert_eq!(
            result.value,
            serde_json::json!({"ok": true, "test_exit": 0})
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("config.txt")).unwrap(),
            "new_name"
        );
    }

    #[tokio::test]
    async fn execute_ls_accepts_documented_glob_and_type_filters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.path().join("drop.txt"), "ignore\n").unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        std::fs::write(dir.path().join("nested/also.rs"), "nested\n").unwrap();
        let session = Arc::new(Mutex::new(Session::new(
            dir.path().to_path_buf(),
            Arc::new(Config::default()),
        )));

        let result = execute(
            r#"result = ls(path=".", depth=1, glob="*.rs", type="f")"#,
            session,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        let names = result.value["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|entry| entry["n"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["keep.rs"]);
    }

    #[tokio::test]
    async fn execute_exec_accepts_positional_and_keyword_args() {
        let session = test_session();
        let result = execute(
            r#"
result = {
    "positional": exec("printf", ["%s", "hello"])["out"],
    "keyword": exec("printf", args=["%s", "world"])["out"],
}
"#,
            session,
            Duration::from_secs(10),
        )
        .await
        .unwrap();
        assert_eq!(
            result.value,
            serde_json::json!({"positional": "hello", "keyword": "world"})
        );
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
        let result = execute(code, session, Duration::from_secs(10))
            .await
            .unwrap();
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
        let result = execute(code, session, Duration::from_secs(10))
            .await
            .unwrap();
        let val = result.value;
        assert_eq!(val["lines"], 3);
        assert_eq!(val["status"], "ok");
    }

    #[test]
    fn tool_signatures_match_positional_and_keyword_only_bindings() {
        let sigs = tool_signatures();
        assert!(
            sigs.contains("def exec(command: str, args: list[str] = [], *, cwd: str = None)"),
            "exec args must be documented as positional and cwd as keyword-only"
        );
        assert!(
            sigs.contains("def ls(path: str = None, depth: int = None, *, glob: str = None, type: str = None)"),
            "ls must document its glob/type filters"
        );
        assert!(
            sigs.contains("def read_file(path: str, *, offset: int = None, limit: int = None)"),
            "named-only binding parameters must be marked with *"
        );
        assert!(
            sigs.contains("def session_stats(*, scope: str = \"session\", days: int = None)"),
            "fully named-only bindings must be documented as such"
        );
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
        assert!(sigs.contains("discord"));
    }

    // --- normalize_string_literals ---

    #[test]
    fn normalize_no_strings_unchanged() {
        let code = "x = 1 + 2\nresult = x";
        assert_eq!(normalize_string_literals(code), code);
    }

    #[test]
    fn normalize_single_line_string_unchanged() {
        let code = r#"result = "hello world""#;
        assert_eq!(normalize_string_literals(code), code);
    }

    #[test]
    fn normalize_escape_newline_in_string_unchanged() {
        // \n as a two-char escape sequence, not a literal newline
        let code = r#"write_file("f.txt", "line1\nline2")"#;
        assert_eq!(normalize_string_literals(code), code);
    }

    #[test]
    fn normalize_triple_quoted_string_unchanged() {
        let code = "result = \"\"\"multi\nline\ncontent\"\"\"";
        assert_eq!(normalize_string_literals(code), code);
    }

    #[test]
    fn normalize_comment_not_treated_as_string() {
        let code = "# this is a comment with \"quotes\"\nresult = 1";
        assert_eq!(normalize_string_literals(code), code);
    }

    #[test]
    fn normalize_double_quoted_multiline_becomes_triple() {
        // Literal newline inside a "..." string → """..."""
        let input = "write_file(\"f.txt\", \"line1\nline2\")";
        let output = normalize_string_literals(input);
        // Should be parseable by Starlark now
        assert!(
            output.contains("\"\"\""),
            "must use triple-quotes: {output}"
        );
        assert!(
            output.contains("line1\nline2"),
            "content preserved: {output}"
        );
        // Verify it actually parses
        AstModule::parse("test", output, &Dialect::Standard).expect("normalized code must parse");
    }

    #[test]
    fn normalize_single_quoted_multiline_becomes_triple() {
        let input = "write_file('f.txt', 'line1\nline2')";
        let output = normalize_string_literals(input);
        assert!(
            output.contains("'''"),
            "must use single triple-quotes: {output}"
        );
        assert!(
            output.contains("line1\nline2"),
            "content preserved: {output}"
        );
        AstModule::parse("test", output, &Dialect::Standard).expect("normalized code must parse");
    }

    #[test]
    fn normalize_preserves_surrounding_code() {
        let input = "x = 1\nwrite_file(\"a\", \"b\nc\")\nresult = x";
        let output = normalize_string_literals(input);
        assert!(output.starts_with("x = 1\n"), "prefix preserved: {output}");
        assert!(
            output.ends_with("\nresult = x"),
            "suffix preserved: {output}"
        );
    }

    #[test]
    fn normalize_escaped_quote_in_string_preserved() {
        // Escaped quote inside string stays escaped; no collision possible
        // because unescaped " always closes "..." strings.
        let code = "write_file(\"f\", \"has \\\"escaped\\\" quote\")";
        let output = normalize_string_literals(code);
        // No literal newline → unchanged
        assert_eq!(output, code);
    }

    #[tokio::test]
    async fn execute_multiline_string_literal_succeeds() {
        // This is the bug: "line1\nline2" with a LITERAL newline used to fail
        // with "parse error: unfinished string literal".
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let session = Arc::new(Mutex::new(session));

        // Embed a literal newline inside a double-quoted string, simulating
        // what a model produces when it writes multi-line file content.
        let code = "write_file(\"out.txt\", \"use serde::Serialize;\nuse async_trait::async_trait;\n\")\nresult = True";
        let result = execute(code, session.clone(), Duration::from_secs(10))
            .await
            .expect("execute must not fail with literal newline in string");
        assert_eq!(result.value, serde_json::json!(true));

        // Verify the file was actually written with newlines preserved.
        let content = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        assert!(
            content.contains("use serde::Serialize;\n"),
            "newline must be a real newline"
        );
        assert!(
            content.contains("use async_trait::async_trait;"),
            "second line preserved"
        );
    }

    #[tokio::test]
    async fn execute_double_colon_in_string_literal_succeeds() {
        // Previously :: in multi-line strings caused misleading parse errors.
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(dir.path().to_path_buf(), Arc::new(Config::default()));
        let session = Arc::new(Mutex::new(session));

        let code = "write_file(\"src.rs\", \"use async_trait::async_trait;\nuse serde::Serialize;\n\")\nresult = \"ok\"";
        let result = execute(code, session, Duration::from_secs(10))
            .await
            .expect(":: in string content must not cause parse error");
        assert_eq!(result.value, serde_json::json!("ok"));
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

    // --- In-script LLM sub-calls (ADR-008) ---

    use crate::providers::LlmResponse;

    /// Provider double: echoes the prompt as `echo:<prompt>`, or returns an
    /// error response when the prompt matches `fail_on`. Reports `out_tokens`
    /// output tokens so usage roll-up is observable.
    struct MockProvider {
        fail_on: Option<String>,
        out_tokens: u64,
    }

    #[async_trait::async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
            let prompt = ctx
                .messages
                .last()
                .and_then(|m| {
                    m.content.iter().find_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.clone()),
                        _ => None,
                    })
                })
                .unwrap_or_default();
            if self.fail_on.as_deref() == Some(prompt.as_str()) {
                return LlmResponse::error("boom");
            }
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::Text(format!("echo:{prompt}"))],
                stop_reason: StopReason::EndTurn,
                error_message: None,
                context_overflow: false,
                usage: Usage {
                    output: self.out_tokens,
                    ..Default::default()
                },
            }
        }
    }

    fn subcall_env(
        provider: Arc<dyn LlmProvider>,
        max_subcalls: usize,
        max_batch: usize,
    ) -> (SubcallEnv, Arc<std::sync::Mutex<Usage>>) {
        let usage = Arc::new(std::sync::Mutex::new(Usage::default()));
        let env = SubcallEnv {
            provider,
            model: "test-model".to_string(),
            max_tokens: 128,
            max_subcalls,
            max_batch,
            usage: Arc::clone(&usage),
            count: Arc::new(AtomicUsize::new(0)),
            ordinal: Arc::new(AtomicU64::new(0)),
        };
        (env, usage)
    }

    #[tokio::test]
    async fn llm_query_returns_text_and_accumulates_usage() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            fail_on: None,
            out_tokens: 5,
        });
        let (env, usage) = subcall_env(provider, 8, 4);
        let result = execute_with_op_count(
            "result = llm_query(\"hi\")",
            test_session(),
            Duration::from_secs(5),
            Arc::new(AtomicUsize::new(0)),
            Some(env),
        )
        .await
        .unwrap();
        assert_eq!(result.value, serde_json::json!("echo:hi"));
        assert_eq!(usage.lock().unwrap().output, 5);
    }

    #[tokio::test]
    async fn llm_query_unavailable_without_env() {
        let err = execute(
            "result = llm_query(\"hi\")",
            test_session(),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(err.contains("llm_query unavailable"), "got: {err}");
    }

    #[tokio::test]
    async fn llm_query_batched_reports_per_item_outcomes() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            fail_on: Some("bad".to_string()),
            out_tokens: 2,
        });
        let (env, _usage) = subcall_env(provider, 8, 4);
        let result = execute_with_op_count(
            "result = llm_query_batched([\"good\", \"bad\"])",
            test_session(),
            Duration::from_secs(5),
            Arc::new(AtomicUsize::new(0)),
            Some(env),
        )
        .await
        .unwrap();
        let arr = result.value.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["ok"], serde_json::json!(true));
        assert_eq!(arr[0]["text"], serde_json::json!("echo:good"));
        assert_eq!(arr[1]["ok"], serde_json::json!(false));
        assert!(arr[1]["error"].as_str().unwrap().contains("boom"));
    }

    #[tokio::test]
    async fn subcall_budget_is_enforced() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            fail_on: None,
            out_tokens: 1,
        });
        let (env, _usage) = subcall_env(provider, 1, 4);
        let err = execute_with_op_count(
            "llm_query(\"a\")\nresult = llm_query(\"b\")",
            test_session(),
            Duration::from_secs(5),
            Arc::new(AtomicUsize::new(0)),
            Some(env),
        )
        .await
        .unwrap_err();
        assert!(err.contains("budget exhausted"), "got: {err}");
    }

    #[tokio::test]
    async fn llm_query_batched_rejects_oversized_batch() {
        let provider: Arc<dyn LlmProvider> = Arc::new(MockProvider {
            fail_on: None,
            out_tokens: 1,
        });
        let (env, _usage) = subcall_env(provider, 8, 2);
        let err = execute_with_op_count(
            "result = llm_query_batched([\"a\", \"b\", \"c\"])",
            test_session(),
            Duration::from_secs(5),
            Arc::new(AtomicUsize::new(0)),
            Some(env),
        )
        .await
        .unwrap_err();
        assert!(err.contains("max_script_subcall_batch"), "got: {err}");
    }

    #[tokio::test]
    async fn llm_query_treats_refusal_as_error() {
        // A Refusal/Aborted stop reason must not surface as a successful
        // (possibly empty) completion.
        struct RefusingProvider;
        #[async_trait::async_trait]
        impl LlmProvider for RefusingProvider {
            async fn complete(&self, _ctx: &Context, _opts: &CompleteOpts) -> LlmResponse {
                LlmResponse {
                    retryable: false,
                    content: vec![ContentBlock::Text("partial".into())],
                    stop_reason: StopReason::Refusal,
                    error_message: None,
                    context_overflow: false,
                    usage: Usage::default(),
                }
            }
        }
        let provider: Arc<dyn LlmProvider> = Arc::new(RefusingProvider);
        let (env, _usage) = subcall_env(provider, 8, 4);
        let err = execute_with_op_count(
            "result = llm_query(\"hi\")",
            test_session(),
            Duration::from_secs(5),
            Arc::new(AtomicUsize::new(0)),
            Some(env),
        )
        .await
        .unwrap_err();
        assert!(err.contains("refused"), "got: {err}");
    }
}
