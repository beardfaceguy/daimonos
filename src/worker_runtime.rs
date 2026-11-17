//! WP10 (FR-05, ADR-028): credential-free worker execution.
//!
//! Context: docs/adr/028-worker-only-execution.md,
//! docs/contexts/reviewer-runtime/CONTEXT.md and INTERFACES.md (WP10 rows).
//!
//! When `agent --worker-config PATH` is set, the model-side process holds the
//! credential and reaches the model endpoint, but executes NO tool itself.
//! Every non-final tool call is routed to a caller-supplied MCP-stdio worker
//! (the "tool side") or refused; there is no local/native/opcode/script
//! fallback. The final-answer tool (FR-01) is intercepted and validated
//! model-side and is never forwarded to a worker (PM FR-05 condition).
//!
//! A [`WorkerExecutor`] owns one connected [`mcp_bridge::WorkerClient`] per
//! worker server. Each worker child is launched credential-free (see
//! [`mcp_bridge::connect_worker`]): a cleared environment plus only the
//! caller's explicit variable map, so no model credential, provider
//! configuration, or inherited ambient state reaches it (AC-05.1). Discovery
//! runs before the catalog is frozen (ADR-028 startup ordering); the exported
//! catalog names each tool `mcp__{worker}__{tool}` exactly as the forwarded
//! bridge does, so WP06's allowlist and the audit route them uniformly.
//!
//! Worker tools are the caller's sandbox (PM FR-05): they are NOT subject to
//! confinement/`--workspace-read-only` path checks here. Their ONLY run-policy
//! gate is `run_policy::ToolPolicy::allows` at the agent-loop dispatch boundary
//! (the same FR-02 allowlist check every tool passes).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde_json::Value;

use crate::mcp_bridge::{self, WorkerCallResult, WorkerClient};
use crate::providers::ToolSchema;

/// One parsed worker server definition from the `--worker-config` control file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// The explicit, caller-controlled child environment. This is the COMPLETE
    /// environment the worker sees — nothing is inherited (ADR-028).
    pub env: HashMap<String, String>,
}

/// The validated `--worker-config` document: a set of worker server
/// definitions, sorted by name for deterministic catalog/order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    pub workers: Vec<WorkerSpec>,
}

impl WorkerConfig {
    /// Parse and validate a `--worker-config` JSON document. Fail-closed: an
    /// invalid document, an empty worker set, or a malformed entry is an error
    /// naming the problem (never a silent default). The shape is:
    ///
    /// ```json
    /// { "workers": {
    ///     "toolside": { "command": "/abs/daimonos", "args": ["--mcp", "-w", "/evidence"],
    ///                   "env": { "PATH": "/usr/bin" } },
    ///     "compute":  { "command": "/abs/compute", "env": {} }
    /// } }
    /// ```
    pub fn parse(content: &str) -> Result<Self, String> {
        let root: Value =
            serde_json::from_str(content).map_err(|error| format!("invalid JSON: {error}"))?;
        let workers_obj = root
            .get("workers")
            .and_then(Value::as_object)
            .ok_or_else(|| "missing top-level \"workers\" object".to_string())?;
        if workers_obj.is_empty() {
            return Err(
                "\"workers\" is empty; a worker-only run needs at least one worker".to_string(),
            );
        }
        let mut names: Vec<&String> = workers_obj.keys().collect();
        names.sort();
        let mut workers = Vec::with_capacity(names.len());
        for name in names {
            let entry = workers_obj[name]
                .as_object()
                .ok_or_else(|| format!("worker {name:?}: entry is not an object"))?;
            let command = entry
                .get("command")
                .and_then(Value::as_str)
                .filter(|command| !command.is_empty())
                .ok_or_else(|| format!("worker {name:?}: missing \"command\""))?
                .to_string();
            let args = entry
                .get("args")
                .map(|value| {
                    value
                        .as_array()
                        .ok_or_else(|| format!("worker {name:?}: \"args\" must be an array"))
                        .and_then(|items| {
                            items
                                .iter()
                                .map(|item| {
                                    item.as_str()
                                        .map(str::to_string)
                                        .ok_or_else(|| {
                                            format!("worker {name:?}: every \"args\" entry must be a string")
                                        })
                                })
                                .collect::<Result<Vec<String>, String>>()
                        })
                })
                .transpose()?
                .unwrap_or_default();
            let env = match entry.get("env") {
                None => HashMap::new(),
                Some(value) => {
                    let map = value
                        .as_object()
                        .ok_or_else(|| format!("worker {name:?}: \"env\" must be an object"))?;
                    let mut env = HashMap::with_capacity(map.len());
                    for (key, value) in map {
                        let value = value.as_str().ok_or_else(|| {
                            format!("worker {name:?}: env {key:?} must be a string")
                        })?;
                        env.insert(key.clone(), value.to_string());
                    }
                    env
                }
            };
            workers.push(WorkerSpec {
                name: name.clone(),
                command,
                args,
                env,
            });
        }
        Ok(WorkerConfig { workers })
    }
}

/// Which worker serves an exported tool name, and the worker's original tool
/// name to send on the wire.
#[derive(Debug, Clone)]
pub struct WorkerRoute {
    /// Index into [`WorkerExecutor::clients`].
    worker: usize,
    /// The tool's un-namespaced name as the worker exposes it.
    original: String,
}

/// The disposition of one model tool call under worker-only execution.
pub enum WorkerDispatch {
    /// The worker served the call; `is_error` is a tool-reported error result.
    Served { content: String, is_error: bool },
    /// No worker in the frozen catalog serves this name. Refused (not fatal):
    /// nothing runs locally. The agent loop answers it with `tool_not_allowed`.
    NoRoute,
    /// The worker route was lost (exit, closed stdio, or stall). Terminal:
    /// ADR-028 aborts the whole run with a distinct failure and no fallback.
    Aborted(String),
}

/// A machine token recorded in the audit `reason` and surfaced in the run's
/// terminal error_message when a worker route is lost (AC-05.4). Distinct and
/// stable so dbunk can recognize the failure class (ADR-028, FR-05 req 6).
pub const WORKER_UNAVAILABLE: &str = "worker_unavailable";

/// The audit `route` value for a call served by (or refused for) a worker, so
/// AC-05.3 can confirm no tool ran in the model-side process.
pub const WORKER_ROUTE: &str = "worker";

/// Owns the connected workers, the frozen exported-name route table, and the
/// model-visible tool catalog. Reuses `mcp_bridge` MCP stdio clients and their
/// lifecycle ownership (INTERFACES: WP10 B).
pub struct WorkerExecutor {
    clients: Vec<WorkerClient>,
    routes: HashMap<String, WorkerRoute>,
    tools: Vec<ToolSchema>,
    call_timeout: Duration,
    shutdown_timeout: Duration,
}

impl WorkerExecutor {
    /// Connect every worker, discover its tools, and freeze the exported
    /// catalog. Fail-closed: if ANY worker fails to launch or handshake, every
    /// already-connected worker is reaped and an error is returned, so a
    /// worker-only run never starts with a missing route (ADR-028: an
    /// unavailable worker makes the opt-in route unusable by design).
    ///
    /// `native_names` are reserved so a worker tool can never shadow a native
    /// tool name in the exported catalog (the same rule the forwarded bridge
    /// applies); worker names that collide are dropped and logged.
    pub async fn connect(
        config: &WorkerConfig,
        native_names: &HashSet<String>,
        init_timeout: Duration,
        call_timeout: Duration,
        shutdown_timeout: Duration,
    ) -> Result<Self, String> {
        let mut clients: Vec<WorkerClient> = Vec::with_capacity(config.workers.len());
        let mut routes: HashMap<String, WorkerRoute> = HashMap::new();
        let mut tools: Vec<ToolSchema> = Vec::new();
        let mut used: HashSet<String> = native_names.clone();
        for spec in &config.workers {
            match mcp_bridge::connect_worker(
                &spec.name,
                &spec.command,
                &spec.args,
                &spec.env,
                init_timeout,
            )
            .await
            {
                Ok((client, defs)) => {
                    let index = clients.len();
                    for def in defs {
                        let exported = mcp_bridge::namespaced_name(&spec.name, &def.name);
                        if used.contains(&exported) {
                            tracing::warn!(
                                target: "daimonos::worker_runtime",
                                worker = %spec.name,
                                tool = %def.name,
                                "worker tool name collides with a reserved name; dropped"
                            );
                            continue;
                        }
                        used.insert(exported.clone());
                        routes.insert(
                            exported.clone(),
                            WorkerRoute {
                                worker: index,
                                original: def.name.clone(),
                            },
                        );
                        tools.push(ToolSchema {
                            name: exported,
                            description: def.description,
                            input_schema: def.input_schema,
                        });
                    }
                    clients.push(client);
                }
                Err(error) => {
                    // Reap the workers already connected before failing closed.
                    for client in &clients {
                        client.shutdown(shutdown_timeout).await;
                    }
                    return Err(format!("worker discovery failed: {error}"));
                }
            }
        }
        tracing::info!(
            target: "daimonos::worker_runtime",
            workers = clients.len(),
            tools = tools.len(),
            "credential-free workers connected"
        );
        Ok(WorkerExecutor {
            clients,
            routes,
            tools,
            call_timeout,
            shutdown_timeout,
        })
    }

    /// The exported worker tool schemas the model-side catalog offers.
    pub fn tools(&self) -> Vec<ToolSchema> {
        self.tools.clone()
    }

    /// Route one model tool call to the worker that serves `name`, or refuse it
    /// ([`WorkerDispatch::NoRoute`]). A lost worker is [`WorkerDispatch::Aborted`].
    pub async fn call(&self, name: &str, arguments: &Value) -> WorkerDispatch {
        let Some(route) = self.routes.get(name) else {
            return WorkerDispatch::NoRoute;
        };
        let client = &self.clients[route.worker];
        match client
            .call(&route.original, arguments, self.call_timeout)
            .await
        {
            WorkerCallResult::Served { content, is_error } => {
                WorkerDispatch::Served { content, is_error }
            }
            WorkerCallResult::Down(detail) => WorkerDispatch::Aborted(detail),
        }
    }

    /// Reap every owned worker child through the managed MCP lifecycle.
    pub async fn shutdown(&self) {
        for client in &self.clients {
            client.shutdown(self.shutdown_timeout).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_workers_sorted_by_name() {
        let doc = r#"{ "workers": {
            "zeta": { "command": "/bin/z" },
            "alpha": { "command": "/bin/a", "args": ["--mcp"], "env": { "PATH": "/usr/bin" } }
        } }"#;
        let config = WorkerConfig::parse(doc).unwrap();
        assert_eq!(config.workers.len(), 2);
        assert_eq!(config.workers[0].name, "alpha");
        assert_eq!(config.workers[0].command, "/bin/a");
        assert_eq!(config.workers[0].args, vec!["--mcp".to_string()]);
        assert_eq!(
            config.workers[0].env.get("PATH").map(String::as_str),
            Some("/usr/bin")
        );
        assert_eq!(config.workers[1].name, "zeta");
        assert!(config.workers[1].env.is_empty());
    }

    #[test]
    fn parse_is_fail_closed() {
        assert!(WorkerConfig::parse("not json")
            .unwrap_err()
            .contains("invalid JSON"));
        assert!(WorkerConfig::parse("{}").unwrap_err().contains("workers"));
        assert!(WorkerConfig::parse(r#"{"workers":{}}"#)
            .unwrap_err()
            .contains("empty"));
        assert!(WorkerConfig::parse(r#"{"workers":{"a":{}}}"#)
            .unwrap_err()
            .contains("command"));
        assert!(
            WorkerConfig::parse(r#"{"workers":{"a":{"command":"x","args":"nope"}}}"#)
                .unwrap_err()
                .contains("args")
        );
        assert!(
            WorkerConfig::parse(r#"{"workers":{"a":{"command":"x","env":{"K":1}}}}"#)
                .unwrap_err()
                .contains("env")
        );
    }
}
