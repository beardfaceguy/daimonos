#![allow(dead_code)]

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::Instrument;

use crate::agent::{AgentConfig, AgentResult, TokenLogConfig};
use crate::analytics::{AgentRunRecord, AnalyticsStore};
use crate::config::Config;
use crate::observability::{PromptMetadata, PromptSpan};
use crate::providers::{
    CompleteOpts, ContentBlock, LlmProvider, Message, Role, StreamEvent, ToolSchema,
};
use crate::safety::SafetyPolicy;
use crate::session::Session;
use crate::tool_facade;

pub struct AgentCmdArgs {
    pub task: String,
    pub model: Option<String>,
    pub dry_run: bool,
    /// Safety policy (denylist/allowlist/approval mode). `None` = no restrictions.
    pub safety: Option<SafetyPolicy>,
    /// Analytics store for recording actual token usage. `None` = no recording.
    pub analytics: Option<Arc<AnalyticsStore>>,
    /// `--debug-tokens` log file path. `None` = no per-call token logging.
    pub token_log: Option<std::path::PathBuf>,
    /// Private, truncate-per-run destination for streamed thinking text.
    pub thought_log: Option<std::path::PathBuf>,
    /// Reasoning effort for the model (#1122), from `DAIMONOS_AGENT_THINKING`
    /// (default `medium`). Forwarded into `CompleteOpts.thinking`.
    pub thinking: crate::providers::ThinkingLevel,
    /// WP08 (FR-04, ADR-026): caller-requested local run audit sink, threaded
    /// into the agent loop. `None` = audit off.
    pub audit: Option<Arc<crate::run_audit::RunAudit>>,
    /// Opt-in structured final output (ADR-024, WP05). `Some` offers the
    /// final-answer tool and switches the loop to terminal-call accounting;
    /// the outcome is recorded in `final_output_sink`. `None` = unchanged.
    pub final_output: Option<Arc<crate::structured_output::FinalOutputSpec>>,
    /// Where the agent loop records the terminal final-output decision for the
    /// caller to map to stdout/stderr and the reviewed exit code.
    pub final_output_sink: Option<Arc<Mutex<Option<crate::structured_output::FinalOutputOutcome>>>>,
    /// WP09 (FR-04 reqs 14-17, ADR-027): the single same-tool capability probe.
    /// `Some` sends one unretried probe offering the frozen run tool catalog
    /// before any evidence tool runs; the verdict is recorded in
    /// `capability_probe_sink` and a failed probe stops the run before the
    /// agent loop. `None` = probe off (default).
    pub capability_probe: Option<crate::capability_probe::ProbeSpec>,
    /// Where the capability-probe verdict is recorded for the caller to map to
    /// the `model_unsupported` exit (21).
    pub capability_probe_sink: Option<Arc<Mutex<Option<crate::capability_probe::ProbeOutcome>>>>,
    /// WP10 (FR-05, ADR-028): opt-in credential-free worker execution. `Some`
    /// routes every non-final tool call to a caller-supplied worker (nothing
    /// runs in-process). `None` = legacy local/remote dispatch.
    pub worker_config: Option<Arc<crate::worker_runtime::WorkerConfig>>,
}

/// Opt-in one-shot behavior (ADR-036). The default is the behavior of a run
/// without the new options.
#[derive(Default)]
pub struct RunOptions {
    /// Provider name. Used only in the JSON result document.
    pub provider: String,
    pub output: crate::cli::AgentOutputFormat,
    /// A claimed named session (`--session-id` or `--resume`).
    pub session: Option<crate::oneshot_session::OpenSession>,
    /// The provider was built with prompt caching on
    /// (`DAIMONOS_AGENT_PROMPT_CACHE`).
    pub prompt_cache: bool,
    /// [`crate::oneshot_session::endpoint_digest`] of the run. Saved in the
    /// prefix record of a new session.
    pub endpoint: String,
}

/// The `--output json` result document of one invocation (ADR-036,
/// Decision 13). Field order is the output order. `schema_version` changes
/// when a field is removed or changes its meaning.
#[derive(Debug, serde::Serialize)]
pub struct RunReport {
    pub schema_version: u32,
    pub session_id: Option<String>,
    pub resumed: bool,
    pub session_saved: Option<bool>,
    pub session_error: Option<String>,
    pub provider: String,
    pub model: String,
    pub stop_reason: String,
    pub error_message: Option<String>,
    pub context_overflow: bool,
    pub final_text: String,
    pub turns: u32,
    pub usage: ReportUsage,
    pub evidence: ReportEvidence,
    pub duration_ms: u64,
}

/// Token usage of this invocation only, with the `agent_runs` column names.
#[derive(Debug, serde::Serialize)]
pub struct ReportUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_output_tokens: Option<u64>,
    pub thinking_bytes: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub cost_usd: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct ReportEvidence {
    pub status: &'static str,
}

impl RunReport {
    fn new(
        options: &RunOptions,
        model: &str,
        result: &AgentResult,
        session_save: Option<&std::result::Result<(), String>>,
        initial_len: usize,
        turns: u32,
        elapsed: std::time::Duration,
    ) -> Self {
        // The text blocks of the last assistant message that this invocation
        // added. Earlier messages of a resumed session are not included.
        let final_text = result
            .messages
            .iter()
            .skip(initial_len)
            .rev()
            .find(|message| matches!(message.role, Role::Assistant))
            .map(|message| {
                message
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let status = match result.evidence.status() {
            crate::evidence::EvidenceStatus::Unknown => "unknown",
            crate::evidence::EvidenceStatus::NoMutations => "no_mutations",
            crate::evidence::EvidenceStatus::Verified => "verified",
            crate::evidence::EvidenceStatus::Unverified => "unverified",
        };
        RunReport {
            schema_version: 1,
            session_id: options.session.as_ref().map(|s| s.id().to_string()),
            resumed: options.session.as_ref().is_some_and(|s| s.is_resume()),
            session_saved: session_save.map(|save| save.is_ok()),
            session_error: session_save.and_then(|save| save.as_ref().err().cloned()),
            provider: options.provider.clone(),
            model: model.to_string(),
            stop_reason: result.stop_reason.as_str().to_string(),
            error_message: result.error_message.clone(),
            context_overflow: result.context_overflow,
            final_text,
            turns,
            usage: ReportUsage {
                input_tokens: result.usage.input,
                output_tokens: result.usage.output,
                reasoning_output_tokens: result.usage.reasoning_output,
                thinking_bytes: result.usage.thinking_bytes,
                cache_read_tokens: result.usage.cache_read,
                cache_write_tokens: result.usage.cache_write,
                cost_usd: result.usage.cost.total_usd,
            },
            evidence: ReportEvidence { status },
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        }
    }
}

enum ThoughtCaptureState {
    Active(File),
    Failed(String),
}

struct ThoughtCapture {
    path: std::path::PathBuf,
    state: Mutex<ThoughtCaptureState>,
}

impl ThoughtCapture {
    fn open(path: std::path::PathBuf) -> std::io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            path,
            state: Mutex::new(ThoughtCaptureState::Active(file)),
        })
    }

    fn write_delta(&self, text: &str) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let ThoughtCaptureState::Active(file) = &mut *state else {
            return;
        };
        if let Err(error) = file.write_all(text.as_bytes()) {
            *state = ThoughtCaptureState::Failed(error.to_string());
        }
    }

    fn finish(&self) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match &mut *state {
            ThoughtCaptureState::Active(file) => file.flush().map_err(|error| {
                anyhow::anyhow!(
                    "failed to flush thought log {}: {error}",
                    self.path.display()
                )
            }),
            ThoughtCaptureState::Failed(error) => Err(anyhow::anyhow!(
                "failed to write thought log {}: {error}",
                self.path.display()
            )),
        }
    }
}

// NOTE: no compaction policy here — ADR-002 compaction operates BETWEEN
// turns of a multi-turn AgentSession, and a one-shot `daimonos agent` run is
// a single turn (there is never an older turn to evict). Intra-run
// compaction for a single long tool loop is explicit ADR-002 future work.

/// Run the agent subcommand.
///
/// Takes `args` by value so the optional `SafetyPolicy` can be consumed into
/// the `before_tool_call` hook without cloning. `cfg` is the already-loaded
/// CLI/project config (vikunja #958) — the tool `Session` is built from it
/// instead of `Config::default()`, so user-configured settings (verbosity,
/// process/extra_path, MCP settings, etc.) apply to agent tool calls too.
/// Only assistant text blocks are written to `out`; thinking blocks are suppressed.
pub async fn run_agent(
    provider: &dyn LlmProvider,
    workspace: &Path,
    cfg: Arc<Config>,
    args: AgentCmdArgs,
    out: &mut dyn Write,
) -> Result<AgentResult> {
    run_agent_with(provider, workspace, cfg, args, RunOptions::default(), out).await
}

/// [`run_agent`] with the opt-in one-shot options of ADR-036.
///
/// Context: docs/contexts/session-durability/CONTEXT.md, ADR-036.
pub async fn run_agent_with(
    provider: &dyn LlmProvider,
    workspace: &Path,
    cfg: Arc<Config>,
    args: AgentCmdArgs,
    options: RunOptions,
    out: &mut dyn Write,
) -> Result<AgentResult> {
    let started = std::time::Instant::now();
    // FR-02 (WP06, ADR-023) R1: the explicit allowlist filters the offered
    // catalog at its single source (`active_schemas_for_policy`), so the
    // per-frontend `retain` below is gone. With no explicit list this is a
    // no-op (legacy behavior).
    let policy = crate::run_policy::ToolPolicy::from_config(cfg.tool_allowlist.as_ref());
    let schemas = tool_facade::active_schemas_for_policy(
        workspace,
        &cfg.prompts.resolved_tool_descriptions,
        cfg.filesystem_authority.is_some(),
        &policy,
    );

    if args.dry_run {
        writeln!(out, "[dry-run] task: {}", args.task)?;
        writeln!(out, "[dry-run] {} tool(s) available", schemas.len())?;
        return Ok(AgentResult {
            messages: vec![Message::user(&args.task)],
            usage: Default::default(),
            stop_reason: crate::providers::StopReason::Aborted,
            error_message: Some("dry-run".to_string()),
            last_call_usage: Default::default(),
            context_overflow: false,
            evidence: Default::default(),
            worker_lost: false,
            config_invalid: None,
        });
    }

    let native_names: std::collections::HashSet<String> =
        schemas.iter().map(|tool| tool.name.clone()).collect();
    // WP10 (FR-05, ADR-028): worker-only execution. When a `--worker-config` is
    // supplied, discover the caller's workers BEFORE freezing the catalog
    // (ADR-028 startup ordering), and build the model-visible catalog from the
    // worker-discovered tools ONLY — no native in-process tools are offered,
    // because nothing runs in this credential-bearing process. A discovery or
    // launch failure is fail-closed: the run aborts with the distinct
    // `worker_unavailable` error and never falls back to local tools.
    let worker_executor = if let Some(worker_config) = &args.worker_config {
        let init_timeout = std::time::Duration::from_secs(cfg.acp.mcp.call_timeout_secs);
        let call_timeout = std::time::Duration::from_secs(cfg.acp.mcp.call_timeout_secs);
        let shutdown_timeout = std::time::Duration::from_secs(cfg.acp.mcp.shutdown_timeout_secs);
        match crate::worker_runtime::WorkerExecutor::connect(
            worker_config,
            &native_names,
            init_timeout,
            call_timeout,
            shutdown_timeout,
        )
        .await
        {
            Ok(executor) => Some(Arc::new(executor)),
            Err(error) => {
                // WP10 (R1, ADR-028): carry the worker-init failure as a TYPED
                // error so `agent_runtime::worker_failure` downcasts it (the
                // `Err`-path analogue of the `Ok`-path `worker_lost` flag),
                // never classifying it by a substring of the message.
                return Err(anyhow::Error::new(crate::worker_runtime::WorkerInitError {
                    message: error.to_string(),
                }));
            }
        }
    } else {
        None
    };
    // WP10 r4 (L1, ADR-028): the FULL worker-discovered catalog, before the
    // allowlist policy narrows it. The allowlist is validated against this
    // frozen set (below), and the model-facing catalog is this set filtered by
    // the policy at a single point, so no frontend offers a tool the allowlist
    // does not name (the WP06 MAJOR defect class).
    let worker_catalog: Vec<ToolSchema> = if let Some(executor) = &worker_executor {
        executor.tools()
    } else {
        schemas
            .into_iter()
            .map(|s| ToolSchema {
                name: s.name,
                description: s.description,
                input_schema: s.input_schema,
            })
            .collect()
    };
    // WP10 r4 (L2, integrator decision D38): after worker discovery and BEFORE
    // the catalog freeze and the first model request, EVERY allowlist entry
    // must name a tool of the frozen catalog of this run — the worker-discovered
    // tools, plus the final-answer tool when `--final-output` is set. In worker
    // mode a native name such as `read_file` names no tool of the run and must
    // end the run at startup (exit 2, naming the entry), not be silently
    // dropped from the offered catalog and left to look empty. (Before r4 only
    // `mcp__…` entries were checked here; native names passed at parse because
    // they are in the native catalog, but they dispatch to nothing in worker
    // mode.)
    if let Some(executor) = &worker_executor {
        if let Some(allowlist) = &cfg.tool_allowlist {
            let mut frozen: std::collections::HashSet<&str> = worker_catalog
                .iter()
                .map(|tool| tool.name.as_str())
                .collect();
            if let Some(spec) = &args.final_output {
                frozen.insert(spec.tool_name());
            }
            if let Some(unknown) = allowlist
                .iter()
                .find(|name| !frozen.contains(name.as_str()))
            {
                // Reap the already-connected workers before failing closed.
                executor.shutdown().await;
                // D33/K1 (ADR-026): this refusal is decided only AFTER a side
                // effect (the worker was started). Do NOT process::exit here —
                // that would leave a lone `run_start` in the audit. Carry a
                // TYPED verdict so the single terminal source
                // (`agent_runtime::run_terminal_status`) records one `run_end`
                // (`run_configuration_invalid`, exit 2) before the process exits
                // with the same code (`exit_on_config_invalid`).
                let message =
                    format!("--tool-allowlist entry '{unknown}' names no tool of the worker run");
                return Ok(AgentResult {
                    messages: Vec::new(),
                    usage: Default::default(),
                    stop_reason: crate::providers::StopReason::Error,
                    error_message: Some(message.clone()),
                    last_call_usage: Default::default(),
                    context_overflow: false,
                    evidence: Default::default(),
                    worker_lost: false,
                    config_invalid: Some(crate::agent::ConfigInvalid {
                        reason: "tool_allowlist_unknown_entry",
                        message,
                    }),
                });
            }
        }
    }
    // WP10 r4 (L1): the model-facing worker catalog is the discovered set
    // filtered by the allowlist policy at this single point. With no explicit
    // allowlist `policy.allows` is always true (the full worker catalog, the
    // unchanged default). In non-worker mode `schemas` was already filtered at
    // its single source (`active_schemas_for_policy`), so pass it through.
    let mut tools: Vec<ToolSchema> = if worker_executor.is_some() {
        worker_catalog
            .into_iter()
            .filter(|t| policy.allows(&t.name))
            .collect()
    } else {
        worker_catalog
    };
    // Outbound MCP servers (#1289): one-shot agent mode reads the Claude-style
    // file named by `[agent.mcp]`. Disabled under worker-only mode, where tool
    // servers come exclusively from the explicit `--worker-config`.
    let agent_mcp = if worker_executor.is_some() {
        None
    } else {
        crate::agent_mcp::connect(&cfg, &native_names, args.analytics.clone()).await
    };
    if let Some(mcp) = &agent_mcp {
        // FR-02 R1: outbound MCP tools are subject to the same allowlist so the
        // listing equals the dispatchable set; a no-op with no explicit list.
        tools.extend(mcp.tools().into_iter().filter(|t| policy.allows(&t.name)));
    }

    // WP05 (ADR-024): offer the caller's final-answer tool with its schema
    // unchanged and switch the loop to terminal-call accounting. The tool is
    // intercepted model-side and never dispatched or routed to a worker.
    let final_output_active = args.final_output.is_some();
    if let Some(spec) = &args.final_output {
        tools.push(spec.tool_schema());
    }
    // The offered catalog was already filtered at its single source
    // (`active_schemas_for_policy`) and at the MCP extend above; the
    // final-answer tool is required on the allowlist at startup (R1), so no
    // per-frontend `retain` is needed here. The agent loop still refuses any
    // unlisted name at every dispatch boundary.
    // WP09 (FR-04 reqs 14-17, ADR-027): the single same-tool capability probe
    // runs here, offering the EXACT frozen `tools` vector the run will offer
    // (byte-equivalent; no dummy substitute), before any evidence tool. A
    // failed probe stops the run immediately — the agent loop never starts, so
    // no evidence tool executes — and the recorded verdict is mapped to the
    // `model_unsupported` exit (21) by the frontend.
    if let Some(probe) = &args.capability_probe {
        let final_spec = args
            .final_output
            .as_ref()
            .expect("capability probe requires final-output (enforced at startup)");
        let outcome =
            crate::capability_probe::run_probe(probe, &tools, final_spec, args.audit.as_deref())
                .await;
        let passed = outcome.passed();
        if let Some(sink) = &args.capability_probe_sink {
            *sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
        }
        if !passed {
            if let Some(mcp) = agent_mcp {
                mcp.shutdown().await;
            }
            // WP10 merge fix (#1575, invariant 4 / R6): the failed-probe early
            // return must reap owned worker children through the managed MCP
            // lifecycle, not merely drop the `Arc`. Dropping relies on
            // `kill_on_drop`, which does NOT reap a stubborn worker's
            // descendant process group; only `shutdown()` force-reaps it. The
            // normal end and the allowlist refusal already call this.
            if let Some(executor) = &worker_executor {
                executor.shutdown().await;
            }
            return Ok(AgentResult {
                messages: vec![Message::user(&args.task)],
                usage: Default::default(),
                stop_reason: crate::providers::StopReason::Aborted,
                error_message: Some("model_unsupported".to_string()),
                last_call_usage: Default::default(),
                context_overflow: false,
                evidence: Default::default(),
                // WP10 merge: a failed probe is a model verdict (exit 21 via the
                // probe sink), not a worker loss or a config refusal.
                worker_lost: false,
                config_invalid: None,
            });
        }
    }
    // ADR-036 Decision 5: a resumed session keeps its request prefix. It runs
    // on the saved model and thinking level with the saved system prompt, and
    // a different tool list is refused before any provider call.
    if let Some(session) = &options.session {
        session.check_tools(&tools)?;
    }
    let saved = options.session.as_ref().and_then(|session| session.saved());
    let model = match saved {
        Some(saved) => saved.model.clone(),
        None => args.model.unwrap_or_else(|| "claude-opus-4-8".to_string()),
    };
    let thinking = match saved {
        Some(saved) => saved.thinking.clone(),
        None => args.thinking,
    };
    // The saved text, not a rebuilt one: the agent usually changed the
    // workspace, and a rebuilt prompt can differ and miss the prompt cache.
    let system = match saved {
        Some(saved) => saved.prefix.system.clone(),
        None => crate::prompts::agent_system_for_workspace(&cfg, Some(workspace)).await,
    };
    let before_tool_call = args.safety.map(|p| p.into_before_hook());
    // #1575 (invariant 4 / R6): a thought-log open failure here is a post-probe
    // early error; reap the owned worker through its managed lifecycle before
    // returning so no worker (or its descendant group) outlives the run.
    // (`agent_mcp` is provably `None` whenever `worker_executor` is `Some` — the
    // outbound-MCP connect above is skipped in worker-only mode — so reaping the
    // executor alone suffices for the worker invariant on this path.)
    let thought_capture = match args.thought_log {
        Some(path) => match ThoughtCapture::open(path.clone()) {
            Ok(capture) => Some(Arc::new(capture)),
            Err(error) => {
                if let Some(executor) = &worker_executor {
                    executor.shutdown().await;
                }
                return Err(anyhow::anyhow!(
                    "failed to open thought log {}: {error}",
                    path.display()
                ));
            }
        },
        None => None,
    };
    let on_stream_event = thought_capture.as_ref().map(|capture| {
        let capture = Arc::clone(capture);
        Box::new(move |event| {
            if let StreamEvent::ThinkingDelta(text) = event {
                capture.write_delta(&text);
            }
        }) as crate::agent::StreamHook
    });
    let config = AgentConfig {
        system: Some(system),
        tools,
        opts: CompleteOpts {
            model,
            thinking,
            ..CompleteOpts::default()
        },
        before_tool_call,
        on_stream_event,
        token_log: args.token_log.map(|path| TokenLogConfig {
            path,
            label: "agent".to_string(),
        }),
        remote_tool_dispatch: agent_mcp.as_ref().map(|mcp| mcp.dispatch_hook()),
        // ADR-036 Decision 7. Only with prompt caching on: the Anthropic
        // adapter sets a cache marker for any non-zero prefix length, and the
        // mark must have no effect while prompt caching is off.
        stable_prefix: options.session.is_some() && options.prompt_cache,
        audit: args.audit.clone(),
        // WP05 (ADR-024): final-output mode makes each generation exactly one
        // HTTP attempt so the inclusive model-request ceiling is exact. Disable
        // in-generation retries, failover, turn-level error-resume, and
        // max-tokens auto-continue; none add a request outside that ceiling.
        provider_retry: if final_output_active {
            crate::agent::ProviderRetryConfig {
                max_attempts: 0,
                base_delay: std::time::Duration::ZERO,
                failover_models: Vec::new(),
            }
        } else {
            crate::agent::ProviderRetryConfig::default()
        },
        error_resume_budget: if final_output_active { Some(0) } else { None },
        auto_continue_budget: if final_output_active { Some(0) } else { None },
        final_output: args.final_output.clone(),
        final_output_sink: args.final_output_sink.clone(),
        // WP09 (R5, ADR-027): a probe reaches this point only after it passed,
        // having already spent exactly one model request; seed the loop's
        // ceiling so the probe counts against `max_model_requests`.
        prior_model_requests: if args.capability_probe.is_some() {
            1
        } else {
            0
        },
        worker_executor: worker_executor.clone(),
        ..AgentConfig::default()
    };

    let services = crate::provisioning::build_tool_services(
        workspace,
        &cfg,
        true,
        false,
        args.analytics.clone(),
    )
    .await;
    let skill_workspace = workspace.to_path_buf();
    let skill_task = args.task.clone();
    let skill_config = cfg.agent.skills.clone();
    // #1575 (invariant 4 / R6): a skill-activation failure (join error or the
    // expansion itself) is a post-probe early error; reap the owned worker
    // before returning so no worker (or its descendant group) outlives the run.
    let task = match tokio::task::spawn_blocking(move || {
        crate::skills::expand_manual_invocation(&skill_workspace, &skill_task, &skill_config, &[])
    })
    .await
    {
        Ok(Ok(task)) => task,
        Ok(Err(error)) => {
            if let Some(executor) = &worker_executor {
                executor.shutdown().await;
            }
            return Err(anyhow::Error::msg(error));
        }
        Err(error) => {
            if let Some(executor) = &worker_executor {
                executor.shutdown().await;
            }
            return Err(anyhow::anyhow!("Agent skill activation failed: {error}"));
        }
    };
    let mut tool_session = Session::new(workspace.to_path_buf(), cfg);
    crate::provisioning::provision_session(&mut tool_session, &services);
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(tool_session));
    // A resume continues the saved messages with the new input (Decision 6).
    let mut initial = saved
        .map(|saved| saved.messages.clone())
        .unwrap_or_default();
    initial.push(Message::user(task));
    let initial_len = initial.len();
    let external_session_id = crate::analytics::read_agent_session_id_env();
    let prompt_span = PromptSpan::new(PromptMetadata {
        mode: "agent",
        session_id: external_session_id.as_deref(),
        model: &config.opts.model,
        workspace,
        turn_index: 0,
        tools_exposed: config.tools.len(),
    });
    let result = crate::agent::run(provider, session, initial, &config)
        .instrument(prompt_span.span().clone())
        .await;
    // Graceful bridge teardown before reporting; bounded by [acp.mcp] budgets.
    if let Some(mcp) = agent_mcp {
        mcp.shutdown().await;
    }
    // WP10 (ADR-028): reap owned worker children through the managed MCP
    // lifecycle (cleanup/reaping checkpoint). Runs on every ending, including
    // the worker-abort path (the lost worker is already gone; the rest reap).
    if let Some(executor) = &worker_executor {
        executor.shutdown().await;
    }
    let thought_capture_result = match thought_capture {
        Some(capture) => capture.finish(),
        None => Ok(()),
    };
    let error_type = match result.stop_reason {
        crate::providers::StopReason::Error => Some("provider_error"),
        crate::providers::StopReason::Refusal => Some("refusal"),
        _ => None,
    };
    prompt_span.finish(result.stop_reason.as_str(), error_type);

    // ADR-036 Decision 8: a named session is saved after the loop returns, for
    // each stop reason, so a failed attempt can be continued. The caller
    // depends on the save, so a failure is reported and fails the run.
    let session_save: Option<std::result::Result<(), String>> =
        options.session.as_ref().map(|session| {
            // A resume keeps the prefix record that it loaded and checked.
            let prefix = match session.saved() {
                Some(saved) => saved.prefix.clone(),
                None => crate::oneshot_session::PrefixRecord {
                    provider: options.provider.clone(),
                    endpoint: options.endpoint.clone(),
                    system: config.system.clone().unwrap_or_default(),
                    tools: crate::oneshot_session::tool_digests(&config.tools),
                },
            };
            // Not each loop result is a history that a provider accepts with
            // a new user message after it. See `resumable_history`.
            let history = crate::oneshot_session::resumable_history(&result.messages);
            session.save(
                &config.opts.model,
                config.opts.thinking.as_str(),
                &history,
                workspace,
                &prefix,
            )
        });
    let session_failure = match (&options.session, &session_save) {
        (Some(session), Some(Err(error))) => {
            Some(format!("session {} was not saved: {error}", session.id()))
        }
        _ => None,
    };
    // Messages that this invocation added after its own input. Without a
    // session, the input is one message, so this is the value of before.
    let turns = result.messages.len().saturating_sub(initial_len) as u32;

    if let Some(store) = args.analytics {
        let task_prefix: String = args.task.chars().take(200).collect();
        store.record_agent_run(&AgentRunRecord {
            external_session_id,
            oneshot_session_id: options.session.as_ref().map(|s| s.id().to_string()),
            task_prefix,
            input_tokens: result.usage.input,
            output_tokens: result.usage.output,
            reasoning_output_tokens: result.usage.reasoning_output,
            thinking_bytes: result.usage.thinking_bytes,
            cache_read_tokens: result.usage.cache_read,
            cache_write_tokens: result.usage.cache_write,
            cost_usd: result.usage.cost.total_usd,
            stop_reason: result.stop_reason.as_str().to_string(),
            turns,
        });
    }

    if options.output == crate::cli::AgentOutputFormat::Json {
        // The document is the only stdout write of the run (ADR-036). It is
        // written for each stop reason. Order: the steps between the loop
        // result and this write cannot return early. The MCP shutdown, the
        // span, the session save, and the analytics insert return no error to
        // this function, and the results of the thought log and of the save
        // are held until after the write.
        let report = RunReport::new(
            &options,
            &config.opts.model,
            &result,
            session_save.as_ref(),
            initial_len,
            turns,
            started.elapsed(),
        );
        // Serialize first, then one write: a failure of the serializer or of
        // stdout must not leave a part of a document on stdout.
        let mut document = serde_json::to_vec(&report)?;
        document.push(b'\n');
        out.write_all(&document)?;
        out.flush()?;
        thought_capture_result?;
        if let Some(failure) = session_failure {
            anyhow::bail!(failure);
        }
        return Ok(result);
    }

    // A local debug-artifact failure must be reported, but only after the
    // completed provider run has closed its span and reached analytics.
    thought_capture_result?;

    // Decision 15: a resumed run prints only the assistant text that this
    // invocation added. Every other run prints as before.
    let already_printed = if options.session.as_ref().is_some_and(|s| s.is_resume()) {
        initial_len
    } else {
        0
    };
    // WP05 (ADR-024): final-output mode owns stdout — only the validated
    // argument JSON (emitted by the caller) may appear there. Suppress the
    // normal assistant-text echo so nothing else reaches stdout (AC-01.7).
    if !final_output_active {
        for msg in result.messages.iter().skip(already_printed) {
            if matches!(msg.role, Role::Assistant) {
                for block in &msg.content {
                    if let ContentBlock::Text(t) = block {
                        writeln!(out, "{t}")?;
                    }
                }
            }
        }
    }

    if let Some(failure) = session_failure {
        anyhow::bail!(failure);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::AnalyticsStore;
    use crate::providers::{CompleteOpts, Context, LlmResponse, Usage};
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // --- MockProvider ---

    struct MockProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
        calls: Mutex<Vec<CompleteOpts>>,
    }

    impl MockProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            MockProvider {
                responses: Mutex::new(VecDeque::from(responses)),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn call_opts(&self) -> Vec<CompleteOpts> {
            self.calls.lock().unwrap().drain(..).collect()
        }
    }

    #[async_trait]
    impl LlmProvider for MockProvider {
        async fn complete(&self, _ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.calls.lock().unwrap().push(CompleteOpts {
                model: opts.model.clone(),
                max_tokens: opts.max_tokens,
                thinking: opts.thinking.clone(),
                temperature: opts.temperature,
            });
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("MockProvider exhausted"))
        }
    }

    struct PanicProvider;

    #[async_trait]
    impl LlmProvider for PanicProvider {
        async fn complete(&self, _: &Context, _: &CompleteOpts) -> LlmResponse {
            panic!("PanicProvider should not be called in dry-run mode")
        }
    }

    fn end_turn_with_text(text: &str) -> LlmResponse {
        LlmResponse {
            retryable: false,
            content: vec![ContentBlock::Text(text.to_string())],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }
    }

    fn args(task: &str) -> AgentCmdArgs {
        AgentCmdArgs {
            task: task.to_string(),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            model: None,
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
            audit: None,
        }
    }

    #[tokio::test]
    async fn thought_capture_writes_only_thinking_in_order_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private").join("thoughts.log");
        let provider = MockProvider::new(vec![LlmResponse {
            retryable: false,
            content: vec![
                ContentBlock::Thinking("first ".to_string()),
                ContentBlock::Text("answer".to_string()),
                ContentBlock::Thinking("second".to_string()),
            ],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let mut capture_args = args("go");
        capture_args.thought_log = Some(path.clone());
        let mut out = Vec::new();

        run_agent(&provider, dir.path(), default_cfg(), capture_args, &mut out)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first second");
        assert_eq!(String::from_utf8(out).unwrap().trim(), "answer");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn thought_capture_surfaces_open_failures_before_provider_call() {
        let dir = tempfile::tempdir().unwrap();
        let parent_file = dir.path().join("not-a-directory");
        std::fs::write(&parent_file, "x").unwrap();
        let path = parent_file.join("thoughts.log");
        let provider = MockProvider::new(vec![end_turn_with_text("unused")]);
        let mut capture_args = args("go");
        capture_args.thought_log = Some(path.clone());
        let mut out = Vec::new();

        let error =
            match run_agent(&provider, dir.path(), default_cfg(), capture_args, &mut out).await {
                Err(error) => error,
                Ok(_) => panic!("invalid thought path must fail"),
            };

        assert!(error.to_string().contains(&path.display().to_string()));
        assert!(provider.call_opts().is_empty());
    }

    #[tokio::test]
    async fn thought_capture_creates_an_empty_file_when_provider_emits_no_thinking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("thoughts.log");
        let provider = MockProvider::new(vec![end_turn_with_text("answer")]);
        let mut capture_args = args("go");
        capture_args.thought_log = Some(path.clone());
        let mut out = Vec::new();

        run_agent(&provider, dir.path(), default_cfg(), capture_args, &mut out)
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(path).unwrap(), "");
    }

    #[test]
    fn thought_capture_failure_is_checked_after_run_observability() {
        let source = include_str!("agent_cmd.rs");
        let span = source
            .find("prompt_span.finish(result.stop_reason.as_str(), error_type);")
            .unwrap();
        let analytics = source
            .find("store.record_agent_run(&AgentRunRecord")
            .unwrap();
        let capture = source.find("thought_capture_result?;").unwrap();
        assert!(span < capture);
        assert!(analytics < capture);
    }

    fn default_cfg() -> Arc<Config> {
        Arc::new(Config::default())
    }

    // --- dry-run ---

    #[tokio::test]
    async fn dry_run_does_not_call_provider() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs {
            task: "do it".into(),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            model: None,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
            audit: None,
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn dry_run_prints_task_in_output() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs {
            task: "my task".into(),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            model: None,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
            audit: None,
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("my task"), "output should mention the task: {s}");
    }

    #[tokio::test]
    async fn dry_run_prints_tool_count() {
        let dir = tempfile::tempdir().unwrap();
        let a = AgentCmdArgs {
            task: "go".into(),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            model: None,
            dry_run: true,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
            audit: None,
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("tool"), "output should mention tools: {s}");
    }

    // --- run_agent output ---

    #[tokio::test]
    async fn run_prints_assistant_text_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("task complete")]);
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("do a thing"),
            &mut out,
        )
        .await
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            s.contains("task complete"),
            "text block should appear in output: {s}"
        );
    }

    #[tokio::test]
    async fn run_does_not_print_user_message_text() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("assistant reply")]);
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("user task prompt"),
            &mut out,
        )
        .await
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            !s.contains("user task prompt"),
            "user message should not appear in output: {s}"
        );
        assert!(s.contains("assistant reply"));
    }

    #[tokio::test]
    async fn run_does_not_print_thinking_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse {
            retryable: false,
            content: vec![
                ContentBlock::Thinking("internal thoughts".into()),
                ContentBlock::Text("visible answer".into()),
            ],
            stop_reason: crate::providers::StopReason::EndTurn,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }]);
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("think"),
            &mut out,
        )
        .await
        .unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(
            !s.contains("internal thoughts"),
            "thinking should be hidden: {s}"
        );
        assert!(s.contains("visible answer"));
    }

    // --- model selection ---

    #[tokio::test]
    async fn default_model_is_opus_48() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), args("go"), &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].model, "claude-opus-4-8");
    }

    #[tokio::test]
    async fn model_override_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let a = AgentCmdArgs {
            task: "go".into(),
            model: Some("claude-haiku-4-5".into()),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::Medium,
            audit: None,
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].model, "claude-haiku-4-5");
    }

    #[tokio::test]
    async fn thinking_defaults_to_medium_in_complete_opts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), args("go"), &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].thinking, crate::providers::ThinkingLevel::Medium);
    }

    #[tokio::test]
    async fn thinking_level_is_threaded_into_complete_opts() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("ok")]);
        let a = AgentCmdArgs {
            task: "go".into(),
            model: None,
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::High,
            audit: None,
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let calls = provider.call_opts();
        assert_eq!(calls[0].thinking, crate::providers::ThinkingLevel::High);
    }

    // --- tool schemas ---

    #[tokio::test]
    async fn tools_are_included_in_config() {
        struct SchemaCapture(Mutex<Vec<usize>>);

        #[async_trait]
        impl LlmProvider for SchemaCapture {
            async fn complete(&self, ctx: &Context, _: &CompleteOpts) -> LlmResponse {
                self.0.lock().unwrap().push(ctx.tools.len());
                end_turn_with_text("done")
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let provider = SchemaCapture(Mutex::new(Vec::new()));
        let mut out = Vec::new();
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("check tools"),
            &mut out,
        )
        .await
        .unwrap();
        let counts = provider.0.lock().unwrap();
        assert!(
            *counts.first().unwrap() > 0,
            "agent loop should include tools in Context"
        );
    }

    // --- token log wiring (--debug-tokens) ---

    #[tokio::test]
    async fn token_log_path_is_wired_and_written() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let provider = MockProvider::new(vec![end_turn_with_text("done")]);
        let a = AgentCmdArgs {
            task: "go".into(),
            model: None,
            dry_run: false,
            safety: None,
            analytics: None,
            token_log: Some(log_path.clone()),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
            audit: None,
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();
        let content = std::fs::read_to_string(&log_path).unwrap();
        assert_eq!(content.lines().count(), 1);
        assert!(content.contains("\"cmd\":\"agent\""));
    }

    #[tokio::test]
    async fn no_token_log_file_written_when_not_requested() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("tokens.log");
        let provider = MockProvider::new(vec![end_turn_with_text("done")]);
        run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("go"),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert!(!log_path.exists());
    }

    // --- error propagation ---

    #[tokio::test]
    async fn provider_error_still_returns_ok() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::error("api down")]);
        let mut out = Vec::new();
        let result = run_agent(&provider, dir.path(), default_cfg(), args("go"), &mut out)
            .await
            .unwrap();
        assert_eq!(result.stop_reason, crate::providers::StopReason::Error);
        assert_eq!(result.error_message.as_deref(), Some("api down"));
    }

    // --- analytics wiring ---

    #[tokio::test]
    async fn analytics_record_agent_run_called_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_analytics.db");
        let store = Arc::new(AnalyticsStore::new(&db_path, 90).unwrap());

        let provider = MockProvider::new(vec![end_turn_with_text("done")]);
        let a = AgentCmdArgs {
            task: "test analytics wiring".into(),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            model: None,
            dry_run: false,
            safety: None,
            analytics: Some(Arc::clone(&store)),
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
            audit: None,
        };
        let mut out = Vec::new();
        run_agent(&provider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();

        let summary = store.agent_runs_summary(1).unwrap();
        assert_eq!(
            summary.total_runs, 1,
            "one agent run must be recorded in analytics"
        );
    }

    #[tokio::test]
    async fn analytics_not_called_on_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test_analytics.db");
        let store = Arc::new(AnalyticsStore::new(&db_path, 90).unwrap());

        let a = AgentCmdArgs {
            task: "dry run task".into(),
            final_output: None,
            final_output_sink: None,
            capability_probe: None,
            capability_probe_sink: None,
            worker_config: None,
            model: None,
            dry_run: true,
            safety: None,
            analytics: Some(Arc::clone(&store)),
            token_log: None,
            thought_log: None,
            thinking: crate::providers::ThinkingLevel::default(),
            audit: None,
        };
        let mut out = Vec::new();
        run_agent(&PanicProvider, dir.path(), default_cfg(), a, &mut out)
            .await
            .unwrap();

        let summary = store.agent_runs_summary(1).unwrap();
        assert_eq!(
            summary.total_runs, 0,
            "dry-run must not record an agent run"
        );
    }

    // --- safety policy wiring ---

    #[tokio::test]
    async fn safety_policy_blocks_denied_tool_before_provider_sees_it() {
        use crate::agent::BeforeHookResult;
        use crate::safety::SafetyPolicy;

        // Verify that a denied tool gets a Block result from the hook
        let policy = SafetyPolicy {
            denied_commands: vec!["exec".into()],
            ..SafetyPolicy::default()
        };
        let hook = policy.into_before_hook();
        let info = crate::agent::ToolCallInfo {
            id: "t1".into(),
            name: "exec".into(),
            input: serde_json::json!({}),
        };
        assert!(matches!(hook(&info).await, BeforeHookResult::Block(_)));
    }

    #[tokio::test]
    async fn run_agent_execute_script_can_call_builtin_git_plugin() {
        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(init.success());

        let provider = MockProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "execute_script".into(),
                    input: serde_json::json!({
                        "code": "result = git(command=\"status\")"
                    }),
                }],
                stop_reason: crate::providers::StopReason::ToolUse,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_with_text("done"),
        ]);
        let mut out = Vec::new();
        let result = run_agent(
            &provider,
            dir.path(),
            default_cfg(),
            args("check git status"),
            &mut out,
        )
        .await
        .unwrap();

        let tool_result_text = result
            .messages
            .iter()
            .flat_map(|message| message.content.iter())
            .find_map(|block| match block {
                ContentBlock::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .expect("expected execute_script ToolResult");
        assert!(
            !tool_result_text.contains("plugin not available"),
            "agent execute_script must use the canonical built-in registry: {tool_result_text}"
        );
        let tool_result: serde_json::Value = serde_json::from_str(tool_result_text)
            .expect("execute_script ToolResult should be JSON");
        assert_eq!(
            tool_result["result"]["clean"], true,
            "git status should execute through the plugin: {tool_result_text}"
        );
    }

    // --- cfg wiring (vikunja #958) ---

    // Unix-only: relies on a shebang script + executable mode bit
    // (`PermissionsExt`) to prove PATH resolution through `cfg.process.extra_path`.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_agent_uses_provided_cfg_for_tool_session() {
        // The tool `Session` must be built from the `cfg` passed into
        // `run_agent`, not a hardcoded `Config::default()`. Prove it by
        // putting a probe executable only on `cfg.process.extra_path` and
        // having the agent's `exec` tool call resolve it via PATH.
        let dir = tempfile::tempdir().unwrap();
        let bin_dir = tempfile::tempdir().unwrap();
        let probe = bin_dir.path().join("cfg_probe_958");
        std::fs::write(&probe, "#!/bin/sh\necho cfg-wired\n").unwrap();
        std::fs::set_permissions(&probe, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();

        let mut cfg = Config::default();
        cfg.process.extra_path = vec![bin_dir.path().to_string_lossy().to_string()];

        let provider = MockProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "exec".into(),
                    input: serde_json::json!({"command": "cfg_probe_958"}),
                }],
                stop_reason: crate::providers::StopReason::ToolUse,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_with_text("done"),
        ]);
        let mut out = Vec::new();
        let result = run_agent(
            &provider,
            dir.path(),
            Arc::new(cfg),
            args("run probe"),
            &mut out,
        )
        .await
        .unwrap();

        let tool_result_text = result
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .find_map(|b| match b {
                ContentBlock::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
            .expect("expected a ToolResult block");
        assert!(
            tool_result_text.contains("cfg-wired"),
            "exec should resolve cfg_probe_958 via cfg.process.extra_path: {tool_result_text}"
        );
    }

    // --- ADR-036: --output json ---

    /// The default configuration without outbound MCP servers. The default
    /// reads the MCP servers of the host user's editor settings, so the tool
    /// list would depend on the host and on which servers connect.
    fn hermetic_cfg() -> Arc<Config> {
        let mut cfg = Config::default();
        cfg.agent.mcp.enabled = false;
        Arc::new(cfg)
    }

    fn json_options() -> RunOptions {
        RunOptions {
            provider: "mock".to_string(),
            output: crate::cli::AgentOutputFormat::Json,
            ..RunOptions::default()
        }
    }

    /// Parse stdout that must be exactly one JSON document and one newline.
    fn single_json_document(out: &[u8]) -> serde_json::Value {
        let text = std::str::from_utf8(out).unwrap();
        assert!(text.ends_with('\n'), "stdout must end with one newline");
        let body = &text[..text.len() - 1];
        assert!(!body.contains('\n'), "stdout must hold one line: {text:?}");
        serde_json::from_str(body).expect("stdout must be one JSON document")
    }

    #[tokio::test]
    async fn output_json_prints_one_document_for_a_successful_run() {
        let dir = tempfile::tempdir().unwrap();
        let mut response = end_turn_with_text("all good");
        response.usage = Usage {
            input: 11,
            output: 7,
            reasoning_output: Some(3),
            // The loop replaces this value with the bytes that the provider
            // streamed as thinking. This mock streams none.
            thinking_bytes: 5,
            cache_read: 13,
            cache_write: 17,
            cost: crate::providers::Cost {
                total_usd: 0.25,
                ..Default::default()
            },
        };
        let provider = MockProvider::new(vec![response]);
        let mut a = args("go");
        a.model = Some("mock-model".to_string());
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            hermetic_cfg(),
            a,
            json_options(),
            &mut out,
        )
        .await
        .unwrap();

        let doc = single_json_document(&out);
        assert_eq!(doc["schema_version"], 1);
        assert_eq!(doc["session_id"], serde_json::Value::Null);
        assert_eq!(doc["resumed"], false);
        assert_eq!(doc["session_saved"], serde_json::Value::Null);
        assert_eq!(doc["session_error"], serde_json::Value::Null);
        assert_eq!(doc["provider"], "mock");
        assert_eq!(doc["model"], "mock-model");
        assert_eq!(doc["stop_reason"], "end_turn");
        assert_eq!(doc["error_message"], serde_json::Value::Null);
        assert_eq!(doc["context_overflow"], false);
        assert_eq!(doc["final_text"], "all good");
        assert_eq!(doc["turns"], 1);
        assert_eq!(
            doc["usage"],
            serde_json::json!({
                "input_tokens": 11,
                "output_tokens": 7,
                "reasoning_output_tokens": 3,
                "thinking_bytes": 0,
                "cache_read_tokens": 13,
                "cache_write_tokens": 17,
                "cost_usd": 0.25,
            })
        );
        assert!(doc["evidence"]["status"].is_string());
        assert!(doc["duration_ms"].is_u64());
    }

    #[tokio::test]
    async fn output_json_reports_a_provider_error() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::error("api down")]);
        let mut out = Vec::new();
        let result = run_agent_with(
            &provider,
            dir.path(),
            hermetic_cfg(),
            args("go"),
            json_options(),
            &mut out,
        )
        .await
        .unwrap();
        // The exit code stays with `check_agent_result`: the result is `Error`.
        assert_eq!(result.stop_reason, crate::providers::StopReason::Error);

        let doc = single_json_document(&out);
        assert_eq!(doc["stop_reason"], "error");
        assert_eq!(doc["error_message"], "api down");
        assert_eq!(doc["context_overflow"], false);
        assert_eq!(doc["final_text"], "");
    }

    #[tokio::test]
    async fn output_json_reports_a_context_overflow() {
        let dir = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::context_overflow_error(
            "prompt is too long",
        )]);
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            hermetic_cfg(),
            args("go"),
            json_options(),
            &mut out,
        )
        .await
        .unwrap();

        let doc = single_json_document(&out);
        assert_eq!(doc["stop_reason"], "error");
        assert_eq!(doc["context_overflow"], true);
        assert_eq!(doc["error_message"], "prompt is too long");
    }

    #[tokio::test]
    async fn output_json_usage_matches_the_agent_runs_row() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("analytics.db");
        let store = Arc::new(AnalyticsStore::new(&db_path, 90).unwrap());
        let mut response = end_turn_with_text("done");
        response.usage = Usage {
            input: 101,
            output: 23,
            reasoning_output: Some(4),
            thinking_bytes: 0,
            cache_read: 3000,
            cache_write: 200,
            cost: crate::providers::Cost {
                total_usd: 0.5,
                ..Default::default()
            },
        };
        let provider = MockProvider::new(vec![response]);
        let mut a = args("go");
        a.analytics = Some(Arc::clone(&store));
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            hermetic_cfg(),
            a,
            json_options(),
            &mut out,
        )
        .await
        .unwrap();
        let doc = single_json_document(&out);

        // Read the row through a second connection, not through the store.
        let db = rusqlite::Connection::open(&db_path).unwrap();
        let row: (i64, i64, Option<i64>, i64, i64, i64, f64, String, i64) = db
            .query_row(
                "SELECT input_tokens, output_tokens, reasoning_output_tokens,
                        thinking_bytes, cache_read_tokens, cache_write_tokens,
                        cost_usd, stop_reason, turns
                 FROM agent_runs",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            row,
            (101, 23, Some(4), 0, 3000, 200, 0.5, "end_turn".into(), 1)
        );
        assert_eq!(
            doc["usage"],
            serde_json::json!({
                "input_tokens": row.0,
                "output_tokens": row.1,
                "reasoning_output_tokens": row.2,
                "thinking_bytes": row.3,
                "cache_read_tokens": row.4,
                "cache_write_tokens": row.5,
                "cost_usd": row.6,
            })
        );
        assert_eq!(doc["stop_reason"], row.7.as_str());
        assert_eq!(doc["turns"], row.8);
    }

    #[tokio::test]
    async fn output_json_final_text_is_the_last_assistant_message_only() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("note.txt"), "hello\n").unwrap();
        let provider = MockProvider::new(vec![
            LlmResponse {
                retryable: false,
                content: vec![
                    ContentBlock::Text("reading the note".to_string()),
                    ContentBlock::ToolCall {
                        id: "call-1".to_string(),
                        name: "read_file".to_string(),
                        input: serde_json::json!({"path": "note.txt"}),
                    },
                ],
                stop_reason: crate::providers::StopReason::ToolUse,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_with_text("the note says hello"),
        ]);
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            dir.path(),
            hermetic_cfg(),
            args("go"),
            json_options(),
            &mut out,
        )
        .await
        .unwrap();

        let doc = single_json_document(&out);
        assert_eq!(doc["final_text"], "the note says hello");
        // assistant (tool call), tool result, assistant (final).
        assert_eq!(doc["turns"], 3);
    }

    // --- ADR-036: --session-id ---

    use crate::oneshot_session::{self, SessionRequest};
    use crate::session_store::SessionStore;

    fn oneshot_store(dir: &Path) -> SessionStore {
        SessionStore::new(dir.to_path_buf())
    }

    fn session_options(
        state: &Path,
        request: SessionRequest,
        output: crate::cli::AgentOutputFormat,
    ) -> RunOptions {
        RunOptions {
            provider: "mock".to_string(),
            output,
            session: Some(oneshot_session::open_when_free(state, request).unwrap()),
            ..RunOptions::default()
        }
    }

    fn start(id: &str) -> SessionRequest {
        SessionRequest::Start(id.to_string())
    }

    #[tokio::test]
    async fn session_run_saves_the_conversation_and_the_prefix() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![end_turn_with_text("done")]);
        let mut a = args("first task");
        a.model = Some("mock-model".to_string());
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            a,
            session_options(state.path(), start("s1"), Default::default()),
            &mut out,
        )
        .await
        .unwrap();

        assert_eq!(out, b"done\n");
        let record = oneshot_store(state.path()).load_result("s1").unwrap();
        assert_eq!(record.model, "mock-model");
        assert_eq!(record.thinking.as_deref(), Some("medium"));
        assert_eq!(record.cwd.as_deref(), Some(workspace.path()));
        assert_eq!(record.messages.len(), 2);
        let prefix = record
            .prefix
            .expect("a one-shot record has a prefix record");
        assert_eq!(prefix.provider, "mock");
        assert!(!prefix.system.is_empty());
        assert!(prefix.tools.iter().any(|tool| tool.name == "read_file"));
    }

    #[tokio::test]
    async fn session_run_saves_after_a_provider_error() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let provider = MockProvider::new(vec![LlmResponse::error("api down")]);
        let mut out = Vec::new();
        let result = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("first task"),
            session_options(state.path(), start("s1"), Default::default()),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(result.stop_reason, crate::providers::StopReason::Error);
        let record = oneshot_store(state.path()).load_result("s1").unwrap();
        assert!(record.prefix.is_some());
        // The loop ends a failed call with an empty assistant message.
        // Providers reject empty assistant content, so a resume must not
        // send it: the saved history ends with the user message.
        assert!(result.messages.last().unwrap().content.is_empty());
        assert_eq!(record.messages.len(), 1);
        assert_eq!(
            message_json(&record.messages[0]),
            message_json(&Message::user("first task"))
        );
    }

    /// A provider that removes the session store during the model call, so
    /// the save after the loop cannot find its writer.
    struct StoreRemovingProvider {
        store: std::path::PathBuf,
    }

    #[async_trait]
    impl LlmProvider for StoreRemovingProvider {
        async fn complete(&self, _: &Context, _: &CompleteOpts) -> LlmResponse {
            std::fs::remove_dir_all(&self.store).unwrap();
            end_turn_with_text("done")
        }
    }

    #[tokio::test]
    async fn a_failed_session_save_is_an_error_after_the_json_document() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let store_dir = state.path().join("store");
        let provider = StoreRemovingProvider {
            store: store_dir.clone(),
        };
        let mut out = Vec::new();
        let run = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("first task"),
            session_options(&store_dir, start("s1"), crate::cli::AgentOutputFormat::Json),
            &mut out,
        )
        .await;
        let Err(error) = run else {
            panic!("a failed save must fail the run");
        };
        assert!(
            error.to_string().contains("session s1 was not saved"),
            "{error}"
        );

        let doc = single_json_document(&out);
        assert_eq!(doc["session_id"], "s1");
        assert_eq!(doc["resumed"], false);
        assert_eq!(doc["session_saved"], false);
        assert!(doc["session_error"].is_string());
        assert_eq!(doc["stop_reason"], "end_turn");
    }

    #[tokio::test]
    async fn session_run_reports_the_session_in_json_and_in_analytics() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let db_path = state.path().join("analytics.db");
        let analytics = Arc::new(AnalyticsStore::new(&db_path, 90).unwrap());
        let provider = MockProvider::new(vec![
            end_turn_with_text("with session"),
            end_turn_with_text("without session"),
        ]);
        let mut a = args("first task");
        a.analytics = Some(Arc::clone(&analytics));
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            a,
            session_options(
                &state.path().join("store"),
                start("s1"),
                crate::cli::AgentOutputFormat::Json,
            ),
            &mut out,
        )
        .await
        .unwrap();
        let doc = single_json_document(&out);
        assert_eq!(doc["session_id"], "s1");
        assert_eq!(doc["session_saved"], true);
        assert_eq!(doc["session_error"], serde_json::Value::Null);

        let mut b = args("second task");
        b.analytics = Some(Arc::clone(&analytics));
        run_agent(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            b,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        let db = rusqlite::Connection::open(&db_path).unwrap();
        let ids: Vec<Option<String>> = db
            .prepare("SELECT oneshot_session_id FROM agent_runs ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(ids, vec![Some("s1".to_string()), None]);
    }

    // --- ADR-036: --resume ---

    fn resume(id: &str) -> SessionRequest {
        SessionRequest::Resume(id.to_string())
    }

    /// What one provider call received, in a comparable form.
    #[derive(Debug, Clone, PartialEq)]
    struct SeenRequest {
        system: Option<String>,
        tools: Vec<String>,
        messages: Vec<String>,
        stable_prefix_len: usize,
        model: String,
        thinking: &'static str,
    }

    struct RecordingProvider {
        responses: Mutex<VecDeque<LlmResponse>>,
        seen: Mutex<Vec<SeenRequest>>,
    }

    impl RecordingProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<SeenRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    fn message_json(message: &Message) -> String {
        serde_json::to_string(message).unwrap()
    }

    #[async_trait]
    impl LlmProvider for RecordingProvider {
        async fn complete(&self, ctx: &Context, opts: &CompleteOpts) -> LlmResponse {
            self.seen.lock().unwrap().push(SeenRequest {
                system: ctx.system.clone(),
                tools: ctx
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.input_schema,
                        })
                        .to_string()
                    })
                    .collect(),
                messages: ctx.messages.iter().map(message_json).collect(),
                stable_prefix_len: ctx.stable_prefix_len,
                model: opts.model.clone(),
                thinking: opts.thinking.as_str(),
            });
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| LlmResponse::error("RecordingProvider exhausted"))
        }
    }

    #[tokio::test]
    async fn resume_sends_the_saved_prefix_and_one_new_user_message() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let provider = RecordingProvider::new(vec![
            end_turn_with_text("first answer"),
            end_turn_with_text("second answer"),
        ]);
        let first = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("first task"),
            session_options(state.path(), start("s1"), Default::default()),
            &mut Vec::new(),
        )
        .await
        .unwrap();

        // The agent usually edits the workspace. A new workspace skill changes
        // a rebuilt system prompt, so it shows that the resume does not rebuild.
        let skill = workspace.path().join(".agents/skills/probe");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: probe\ndescription: resume prefix probe\n---\nbody\n",
        )
        .unwrap();
        let rebuilt =
            crate::prompts::agent_system_for_workspace(&hermetic_cfg(), Some(workspace.path()))
                .await;

        let mut out = Vec::new();
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("second task"),
            session_options(state.path(), resume("s1"), Default::default()),
            &mut out,
        )
        .await
        .unwrap();

        let seen = provider.seen();
        assert_eq!(seen.len(), 2);
        assert_ne!(
            seen[0].system.as_deref(),
            Some(rebuilt.as_str()),
            "control: the workspace change must alter a rebuilt system prompt"
        );
        assert_eq!(seen[1].system, seen[0].system);
        assert_eq!(seen[1].tools, seen[0].tools);
        let earlier: Vec<String> = first.messages.iter().map(message_json).collect();
        assert_eq!(seen[1].messages.len(), earlier.len() + 1);
        assert_eq!(seen[1].messages[..earlier.len()], earlier[..]);
        assert_eq!(
            seen[1].messages[earlier.len()],
            message_json(&Message::user("second task"))
        );
        // Decision 15: only the assistant text of this invocation.
        assert_eq!(out, b"second answer\n");

        let record = oneshot_store(state.path()).load_result("s1").unwrap();
        assert_eq!(record.generation, 2);
        assert_eq!(record.messages.len(), earlier.len() + 2);
    }

    #[tokio::test]
    async fn resume_runs_on_the_saved_model_and_thinking_level() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let provider = RecordingProvider::new(vec![
            end_turn_with_text("first answer"),
            end_turn_with_text("second answer"),
        ]);
        let mut a = args("first task");
        a.model = Some("model-one".to_string());
        a.thinking = crate::providers::ThinkingLevel::High;
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            a,
            session_options(state.path(), start("s1"), Default::default()),
            &mut Vec::new(),
        )
        .await
        .unwrap();

        // The values of the agent env file can differ at resume time.
        let mut b = args("second task");
        b.model = Some("model-two".to_string());
        b.thinking = crate::providers::ThinkingLevel::Low;
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            b,
            session_options(state.path(), resume("s1"), Default::default()),
            &mut Vec::new(),
        )
        .await
        .unwrap();

        let seen = provider.seen();
        assert_eq!(
            (seen[1].model.as_str(), seen[1].thinking),
            ("model-one", "high")
        );
        let record = oneshot_store(state.path()).load_result("s1").unwrap();
        assert_eq!(record.model, "model-one");
        assert_eq!(record.thinking.as_deref(), Some("high"));
    }

    #[tokio::test]
    async fn resume_json_counts_only_this_invocation() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let provider = RecordingProvider::new(vec![
            end_turn_with_text("first answer"),
            end_turn_with_text("second answer"),
        ]);
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("first task"),
            session_options(state.path(), start("s1"), Default::default()),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("second task"),
            session_options(
                state.path(),
                resume("s1"),
                crate::cli::AgentOutputFormat::Json,
            ),
            &mut out,
        )
        .await
        .unwrap();

        let doc = single_json_document(&out);
        assert_eq!(doc["session_id"], "s1");
        assert_eq!(doc["resumed"], true);
        assert_eq!(doc["session_saved"], true);
        assert_eq!(doc["final_text"], "second answer");
        assert_eq!(doc["turns"], 1);
    }

    #[tokio::test]
    async fn resume_context_overflow_ends_the_run_and_saves_the_session() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let provider = RecordingProvider::new(vec![
            end_turn_with_text("first answer"),
            LlmResponse::context_overflow_error("prompt is too long"),
        ]);
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("first task"),
            session_options(state.path(), start("s1"), Default::default()),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        let mut out = Vec::new();
        let result = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("second task"),
            session_options(
                state.path(),
                resume("s1"),
                crate::cli::AgentOutputFormat::Json,
            ),
            &mut out,
        )
        .await
        .unwrap();

        // Decision 10: no compaction and no retry. One call for each run.
        assert_eq!(provider.seen().len(), 2);
        assert!(result.context_overflow);
        let doc = single_json_document(&out);
        assert_eq!(doc["stop_reason"], "error");
        assert_eq!(doc["context_overflow"], true);
        assert_eq!(doc["session_saved"], true);
        assert_eq!(
            oneshot_store(state.path())
                .load_result("s1")
                .unwrap()
                .generation,
            2
        );
    }

    #[tokio::test]
    async fn resume_with_a_different_tool_list_is_refused_before_the_provider() {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        // A session whose saved tool list is not the tool list of this build.
        {
            let session = oneshot_session::open_when_free(state.path(), start("s1")).unwrap();
            let other_tools = [ToolSchema {
                name: "not_a_real_tool".to_string(),
                description: "saved by another build".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            }];
            session
                .save(
                    "mock-model",
                    "medium",
                    &[
                        Message::user("first task"),
                        Message::assistant("first answer"),
                    ],
                    workspace.path(),
                    &oneshot_session::PrefixRecord {
                        provider: "mock".to_string(),
                        endpoint: String::new(),
                        system: "saved system".to_string(),
                        tools: oneshot_session::tool_digests(&other_tools),
                    },
                )
                .unwrap();
        }
        let provider = RecordingProvider::new(vec![end_turn_with_text("never sent")]);
        let mut out = Vec::new();
        let run = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("second task"),
            session_options(state.path(), resume("s1"), Default::default()),
            &mut out,
        )
        .await;
        let Err(error) = run else {
            panic!("a different tool list must refuse the resume");
        };
        let refusal = error
            .downcast_ref::<oneshot_session::Refusal>()
            .expect("the error must be a session refusal");
        assert_eq!(refusal.code(), "session_tools_changed");
        assert!(
            provider.seen().is_empty(),
            "no provider call after a refusal"
        );
        assert!(out.is_empty());
        let record = oneshot_store(state.path()).load_result("s1").unwrap();
        assert_eq!(record.generation, 1, "a refused resume must not save");
    }

    // --- ADR-036 Decision 7: the stable-prefix mark ---

    fn read_note_then_answer() -> Vec<LlmResponse> {
        vec![
            LlmResponse {
                retryable: false,
                content: vec![ContentBlock::ToolCall {
                    id: "call-1".to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": "note.txt"}),
                }],
                stop_reason: crate::providers::StopReason::ToolUse,
                error_message: None,
                context_overflow: false,
                usage: Usage::default(),
            },
            end_turn_with_text("done"),
        ]
    }

    /// For each provider call: (stable_prefix_len, message count).
    async fn prefix_marks(session: bool, prompt_cache: bool) -> Vec<(usize, usize)> {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "hello\n").unwrap();
        let provider = RecordingProvider::new(read_note_then_answer());
        let options = RunOptions {
            prompt_cache,
            ..if session {
                session_options(state.path(), start("s1"), Default::default())
            } else {
                RunOptions::default()
            }
        };
        run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("read the note"),
            options,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        provider
            .seen()
            .iter()
            .map(|request| (request.stable_prefix_len, request.messages.len()))
            .collect()
    }

    #[tokio::test]
    async fn a_session_run_marks_the_whole_message_list_as_stable_on_each_request() {
        // Two requests: the task, then the task with the tool call and result.
        assert_eq!(prefix_marks(true, true).await, vec![(1, 1), (3, 3)]);
    }

    #[tokio::test]
    async fn the_stable_prefix_mark_needs_a_session_and_prompt_caching() {
        // Without prompt caching the mark must not reach the adapter: the
        // Anthropic adapter sets a cache marker for any non-zero value.
        assert_eq!(prefix_marks(true, false).await, vec![(0, 1), (0, 3)]);
        // A run without a session option keeps the value 0.
        assert_eq!(prefix_marks(false, true).await, vec![(0, 1), (0, 3)]);
        assert_eq!(prefix_marks(false, false).await, vec![(0, 1), (0, 3)]);
    }

    // --- review round 1: terminal history shapes (ADR-036 Decision 8) ---

    fn response(content: Vec<ContentBlock>, stop: crate::providers::StopReason) -> LlmResponse {
        LlmResponse {
            retryable: false,
            content,
            stop_reason: stop,
            error_message: None,
            context_overflow: false,
            usage: Usage::default(),
        }
    }

    fn read_note_call(id: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: id.to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "note.txt"}),
        }
    }

    /// Each tool call of a request has a result, and no assistant message is
    /// without content for the provider.
    fn assert_request_is_well_formed(messages: &[String]) {
        let parsed: Vec<Message> = messages
            .iter()
            .map(|json| serde_json::from_str(json).unwrap())
            .collect();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for message in &parsed {
            let mut provider_content = false;
            for block in &message.content {
                match block {
                    ContentBlock::ToolCall { id, .. } => {
                        calls.push(id.clone());
                        provider_content = true;
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        results.push(tool_use_id.clone());
                        provider_content = true;
                    }
                    ContentBlock::Text(text) => provider_content |= !text.trim().is_empty(),
                    ContentBlock::Image { .. } => provider_content = true,
                    ContentBlock::Thinking(_) | ContentBlock::ProviderState { .. } => {}
                }
            }
            assert!(
                provider_content,
                "a message without provider content: {message:?}"
            );
        }
        calls.sort();
        results.sort();
        assert_eq!(calls, results, "each tool call needs one result");
    }

    /// Run a first session run with `first` responses, then resume it. Return
    /// the first result, the saved messages, and the resumed request.
    async fn first_run_then_resume(
        first: Vec<LlmResponse>,
    ) -> (AgentResult, Vec<Message>, SeenRequest) {
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("note.txt"), "hello\n").unwrap();
        let first_calls = first.len();
        let mut responses = first;
        responses.push(end_turn_with_text("recovered"));
        let provider = RecordingProvider::new(responses);
        let result = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("first task"),
            session_options(state.path(), start("s1"), Default::default()),
            &mut Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(provider.seen().len(), first_calls, "calls of the first run");
        let saved = oneshot_store(state.path())
            .load_result("s1")
            .unwrap()
            .messages;

        let mut out = Vec::new();
        let resumed = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("try again"),
            session_options(state.path(), resume("s1"), Default::default()),
            &mut out,
        )
        .await
        .unwrap();
        assert_eq!(resumed.stop_reason, crate::providers::StopReason::EndTurn);
        assert_eq!(out, b"recovered\n");
        let request = provider.seen().last().unwrap().clone();
        // The resume sends the saved messages unchanged, then the new input.
        let saved_json: Vec<String> = saved.iter().map(message_json).collect();
        assert_eq!(request.messages[..saved_json.len()], saved_json[..]);
        assert_eq!(
            request.messages[saved_json.len()..],
            [message_json(&Message::user("try again"))]
        );
        assert_request_is_well_formed(&request.messages);
        (result, saved, request)
    }

    #[tokio::test]
    async fn a_session_is_resumable_after_max_tokens_with_partial_text() {
        use crate::providers::StopReason;
        let (result, saved, _) = first_run_then_resume(vec![response(
            vec![ContentBlock::Text("partial ans".into())],
            StopReason::MaxTokens,
        )])
        .await;
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
        // Truncated text is content: it is saved.
        assert_eq!(saved.len(), 2);
        assert_eq!(
            message_json(&saved[1]),
            message_json(&Message::assistant("partial ans"))
        );
    }

    #[tokio::test]
    async fn a_session_is_resumable_after_max_tokens_with_only_thinking() {
        use crate::providers::StopReason;
        let (result, saved, _) = first_run_then_resume(vec![response(
            vec![
                ContentBlock::Thinking("thoughts, then the token limit".into()),
                ContentBlock::ProviderState {
                    provider: "p".into(),
                    data: serde_json::json!({"signature": "x"}),
                },
            ],
            StopReason::MaxTokens,
        )])
        .await;
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
        // The loop result ends with the thinking-only assistant message.
        assert_eq!(result.messages.len(), 2);
        // It has no content for a provider, so it is not saved.
        assert_eq!(saved.len(), 1);
    }

    #[tokio::test]
    async fn a_session_is_resumable_after_max_tokens_inside_a_tool_call() {
        use crate::providers::StopReason;
        let (result, saved, _) = first_run_then_resume(vec![response(
            vec![read_note_call("call-1")],
            StopReason::MaxTokens,
        )])
        .await;
        assert_eq!(result.stop_reason, StopReason::MaxTokens);
        // The open tool call is saved with a result.
        assert_eq!(saved.len(), 3);
        assert!(matches!(
            &saved[2].content[..],
            [ContentBlock::ToolResult { tool_use_id, is_error: true, .. }] if tool_use_id == "call-1"
        ));
    }

    #[tokio::test]
    async fn a_session_is_resumable_after_an_aborted_call() {
        use crate::providers::StopReason;
        let (result, saved, _) =
            first_run_then_resume(vec![response(vec![], StopReason::Aborted)]).await;
        assert_eq!(result.stop_reason, StopReason::Aborted);
        assert_eq!(saved.len(), 1);
    }

    #[tokio::test]
    async fn a_session_is_resumable_after_an_error_that_follows_a_tool_round() {
        use crate::providers::StopReason;
        let (result, saved, _) = first_run_then_resume(vec![
            response(vec![read_note_call("call-1")], StopReason::ToolUse),
            LlmResponse::error("api down"),
        ])
        .await;
        assert_eq!(result.stop_reason, StopReason::Error);
        // user, assistant (tool call), tool result. The empty assistant
        // message of the failed call is not saved.
        assert_eq!(saved.len(), 3);
        assert!(matches!(
            &saved[2].content[..],
            [ContentBlock::ToolResult { tool_use_id, is_error: false, .. }] if tool_use_id == "call-1"
        ));
    }

    #[tokio::test]
    async fn a_session_is_resumable_after_a_refusal() {
        use crate::providers::StopReason;
        let (result, saved, _) = first_run_then_resume(vec![response(
            vec![ContentBlock::Text("I cannot do that.".into())],
            StopReason::Refusal,
        )])
        .await;
        assert_eq!(result.stop_reason, StopReason::Refusal);
        assert_eq!(saved.len(), 2);
    }

    // --- review round 1: the JSON document and post-loop failures ---

    /// A provider that makes the thought log fail during the model call. The
    /// log is a FIFO. The provider closes its only reader, so the write of
    /// the streamed thinking fails with EPIPE after the open succeeded.
    struct ThoughtLogBreaker {
        reader: Mutex<Option<std::fs::File>>,
    }

    #[async_trait]
    impl LlmProvider for ThoughtLogBreaker {
        async fn complete(&self, _: &Context, _: &CompleteOpts) -> LlmResponse {
            drop(self.reader.lock().unwrap().take());
            let mut response = end_turn_with_text("answer");
            response
                .content
                .insert(0, ContentBlock::Thinking("some thoughts".into()));
            response
        }
    }

    #[tokio::test]
    async fn output_json_prints_the_document_before_a_thought_log_failure() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        let workspace = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("thoughts.fifo");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) }, 0);
        // A reader must exist, or the open for writing blocks.
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&fifo)
            .unwrap();
        let provider = ThoughtLogBreaker {
            reader: Mutex::new(Some(reader)),
        };
        let mut a = args("go");
        a.thought_log = Some(fifo);
        let mut out = Vec::new();
        let run = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            a,
            json_options(),
            &mut out,
        )
        .await;
        // The run has a result, so the document is printed. The failure of
        // the debug artifact is reported after it.
        let doc = single_json_document(&out);
        assert_eq!(doc["stop_reason"], "end_turn");
        assert_eq!(doc["final_text"], "answer");
        let Err(error) = run else {
            panic!("a failed thought log must fail the run");
        };
        assert!(error.to_string().contains("thought"), "{error}");
    }

    /// A writer that accepts `limit` bytes and then fails.
    struct FailingWriter {
        written: Vec<u8>,
        limit: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.written.len() + buf.len() > self.limit {
                return Err(std::io::Error::other("stdout is closed"));
            }
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn output_json_writes_the_document_in_one_write_or_not_at_all() {
        let workspace = tempfile::tempdir().unwrap();
        // The document does not fit: nothing may reach stdout, so a caller
        // never reads a truncated document.
        let provider = MockProvider::new(vec![end_turn_with_text("all good")]);
        let mut out = FailingWriter {
            written: Vec::new(),
            limit: 64,
        };
        let run = run_agent_with(
            &provider,
            workspace.path(),
            hermetic_cfg(),
            args("go"),
            json_options(),
            &mut out,
        )
        .await;
        assert!(run.is_err(), "a failed stdout write must fail the run");
        assert!(
            out.written.is_empty(),
            "partial document: {:?}",
            out.written
        );
    }

    // --- review round 1: result pruning in a session run (Decision 6, 7) ---

    #[tokio::test]
    async fn a_session_run_that_prunes_results_marks_and_saves_the_pruned_list() {
        use crate::providers::StopReason;
        let workspace = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(workspace.path().join(name), format!("{name} ").repeat(400)).unwrap();
        }
        // A small budget, so the third request prunes the first result.
        let mut cfg = Config::default();
        cfg.agent.mcp.enabled = false;
        cfg.tool_output.directory = Some(logs.path().to_string_lossy().into_owned());
        cfg.tool_output.intra_turn_result_budget_tokens = 1;
        cfg.tool_output.intra_turn_keep_recent_results = 1;
        let cfg = Arc::new(cfg);
        let read = |id: &str, path: &str| {
            response(
                vec![ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: "read_file".to_string(),
                    input: serde_json::json!({"path": path}),
                }],
                StopReason::ToolUse,
            )
        };
        let provider = RecordingProvider::new(vec![
            read("call-1", "a.txt"),
            read("call-2", "b.txt"),
            read("call-3", "c.txt"),
            end_turn_with_text("done"),
            end_turn_with_text("resumed"),
        ]);
        let options = RunOptions {
            prompt_cache: true,
            ..session_options(state.path(), start("s1"), Default::default())
        };
        run_agent_with(
            &provider,
            workspace.path(),
            Arc::clone(&cfg),
            args("read three files"),
            options,
            &mut Vec::new(),
        )
        .await
        .unwrap();

        let seen = provider.seen();
        assert_eq!(seen.len(), 4);
        // The mark is the length of the list that the request carries: the
        // loop prunes before it builds the request, never after.
        for request in &seen {
            assert_eq!(request.stable_prefix_len, request.messages.len());
        }
        // Control: pruning happened. The first result is intact in the second
        // request and replaced by the pruned marker in a later request.
        let first_result = |request: &SeenRequest| request.messages[2].clone();
        assert!(first_result(&seen[1]).contains("a.txt a.txt"));
        let pruned = first_result(&seen[3]);
        assert!(pruned.contains("old tool result pruned"), "{pruned}");
        assert!(!pruned.contains("a.txt a.txt"));

        // The saved history is the pruned history of the last request plus
        // the final answer, and a resume sends exactly that.
        let saved: Vec<String> = oneshot_store(state.path())
            .load_result("s1")
            .unwrap()
            .messages
            .iter()
            .map(message_json)
            .collect();
        assert_eq!(saved[..seen[3].messages.len()], seen[3].messages[..]);
        assert_eq!(saved.len(), seen[3].messages.len() + 1);

        let options = RunOptions {
            prompt_cache: true,
            ..session_options(state.path(), resume("s1"), Default::default())
        };
        run_agent_with(
            &provider,
            workspace.path(),
            cfg,
            args("more"),
            options,
            &mut Vec::new(),
        )
        .await
        .unwrap();
        let resumed = provider.seen().last().unwrap().clone();
        assert_eq!(resumed.messages[..saved.len()], saved[..]);
        assert_eq!(resumed.stable_prefix_len, resumed.messages.len());
        assert_request_is_well_formed(&resumed.messages);
    }
}
