//! Cooperative, operation-scoped tool budgets (WP18, FR-19, ADR-031).
//!
//! A budget carries an absolute deadline and two independent optional logical
//! counters — bytes of file/git content read or scanned, and directory entries
//! walked. It is created once per top-level tool call and shared by every child
//! of a composite (`batch`, `execute_script`, `tool_pipeline`) so delegation
//! cannot multiply the allowance: a child inherits the remaining allowance and
//! cannot extend the deadline (ADR-031). Limits are caller-set and disabled by
//! default; when every option is absent no budget is installed and behavior is
//! unchanged.
//!
//! Counting is logical, not physical: charge the file/git object content read
//! or scanned (including cached-index content) and directory entries walked —
//! never device I/O or displayed/truncated output bytes, which remain a
//! separate mechanism (`src/tool_output.rs`).
//!
//! Context: docs/contexts/reviewer-runtime/CONTEXT.md,
//! docs/adr/031-cooperative-tool-budgets.md.

use std::io::{self, Read};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::path_policy::ToolFailure;

/// Tools that start external work (a subprocess) and read no logical file/git
/// content we can meter. An input or entry budget cannot be enforced for them,
/// so when such a budget is enabled the call is REFUSED rather than silently
/// exempted (ADR-031; INTERFACES.md row WP18). The deadline, which CAN stop and
/// reap the process group, is still enforced (see `managed_process::run`).
/// `git_read` is deliberately absent: it returns the git object content it read,
/// which the PM counts as logical input, so it is metered, not refused.
pub fn tool_input_unmeterable(name: &str) -> bool {
    matches!(
        name,
        "exec"
            | "bg"
            | "git"
            | "gh"
            | "cargo"
            | "npm"
            | "pytest"
            | "docker"
            | "shellcheck"
            | "curl"
            | "discord"
            | "lint_repair"
            | "tool_run"
    )
}

/// Caller-configured budget options. All-absent means no budget is installed
/// (legacy behavior). Caller-owned; never deserialized from workspace config.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToolBudgetConfig {
    /// `--tool-timeout-ms N`: absolute per-call deadline in milliseconds.
    pub timeout_ms: Option<u64>,
    /// `--tool-input-bytes N`: logical input byte ceiling for the call.
    pub input_bytes: Option<u64>,
    /// `--tool-max-entries N`: directory-entry ceiling for the call (D56,
    /// `(proposed)` spelling; a hit reports the reviewed `tool_input_limit`).
    pub max_entries: Option<u64>,
}

impl ToolBudgetConfig {
    pub fn any_enabled(&self) -> bool {
        self.timeout_ms.is_some() || self.input_bytes.is_some() || self.max_entries.is_some()
    }

    /// Install a fresh operation budget for a top-level call, or `None` when no
    /// option is set (preserving legacy behavior).
    pub fn start(&self) -> Option<Arc<ToolBudget>> {
        if !self.any_enabled() {
            return None;
        }
        Some(Arc::new(ToolBudget::new(*self)))
    }
}

/// Which input ceiling a failure names. Both use the reviewed
/// `tool_input_limit` code; the message names which was hit (integrator D56).
#[derive(Debug, Clone, Copy)]
pub enum Limit {
    Bytes,
    Entries,
}

impl Limit {
    fn label(self) -> &'static str {
        match self {
            Limit::Bytes => "input bytes",
            Limit::Entries => "directory entries",
        }
    }
}

/// An operation-scoped cooperative budget: an absolute deadline plus two
/// independent atomic counters. Shared across the children of one top-level
/// call via `Arc`, so concurrent delegation cannot multiply the allowance.
pub struct ToolBudget {
    deadline: Option<Instant>,
    timeout_ms: Option<u64>,
    input_limit: Option<u64>,
    input_consumed: AtomicU64,
    entry_limit: Option<u64>,
    entry_consumed: AtomicU64,
}

impl ToolBudget {
    fn new(cfg: ToolBudgetConfig) -> Self {
        let deadline = cfg
            .timeout_ms
            .map(|ms| Instant::now() + Duration::from_millis(ms));
        Self {
            deadline,
            timeout_ms: cfg.timeout_ms,
            input_limit: cfg.input_bytes,
            input_consumed: AtomicU64::new(0),
            entry_limit: cfg.max_entries,
            entry_consumed: AtomicU64::new(0),
        }
    }

    /// Remaining time until the absolute deadline, for handing to the managed
    /// subprocess path so external work inherits (never extends) it. `None`
    /// when no deadline is set.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|d| d.saturating_duration_since(Instant::now()))
    }

    /// The typed deadline failure for this budget, used when external work
    /// (a managed subprocess) was stopped at the deadline (FR-19, WP18).
    pub fn timeout_failure(&self, argument: Option<&str>) -> ToolFailure {
        BudgetHit::Timeout {
            configured_ms: self.timeout_ms.unwrap_or(0),
        }
        .into_failure(argument)
    }

    /// Error if the absolute deadline has passed. Checked at bounded work
    /// boundaries by native/index readers.
    pub fn check_deadline(&self) -> Result<(), BudgetHit> {
        if let Some(d) = self.deadline {
            if Instant::now() >= d {
                return Err(BudgetHit::Timeout {
                    configured_ms: self.timeout_ms.unwrap_or(0),
                });
            }
        }
        Ok(())
    }

    /// True when an input-byte or directory-entry ceiling is configured (i.e.
    /// a logical-input budget this tool would have to meter). Used to refuse a
    /// tool that starts unmeterable external work (ADR-031).
    pub fn has_input_limits(&self) -> bool {
        self.input_limit.is_some() || self.entry_limit.is_some()
    }

    /// Charge `n` logical input bytes. No-op when no byte limit is set. Returns
    /// a hit when the cumulative total exceeds the ceiling; the counter still
    /// reflects the attempt so the reported `consumed` is honest. INTERFACES.md
    /// reserves this name (`charge_input`).
    pub fn charge_input(&self, n: u64) -> Result<(), BudgetHit> {
        if let Some(limit) = self.input_limit {
            let consumed = self.input_consumed.fetch_add(n, Ordering::Relaxed) + n;
            if consumed > limit {
                return Err(BudgetHit::Input {
                    which: Limit::Bytes,
                    limit,
                    consumed,
                });
            }
        }
        Ok(())
    }

    /// Charge `n` directory entries. No-op when no entry limit is set.
    pub fn charge_entries(&self, n: u64) -> Result<(), BudgetHit> {
        if let Some(limit) = self.entry_limit {
            let consumed = self.entry_consumed.fetch_add(n, Ordering::Relaxed) + n;
            if consumed > limit {
                return Err(BudgetHit::Input {
                    which: Limit::Entries,
                    limit,
                    consumed,
                });
            }
        }
        Ok(())
    }
}

/// A budget violation, convertible to the reviewed [`ToolFailure`].
#[derive(Debug, Clone)]
pub enum BudgetHit {
    Timeout {
        configured_ms: u64,
    },
    Input {
        which: Limit,
        limit: u64,
        consumed: u64,
    },
}

impl BudgetHit {
    /// Build the caller-facing typed failure. A deadline hit carries
    /// `tool_timeout`; a byte or entry hit carries `tool_input_limit` and names
    /// which ceiling was hit, the configured value, and the consumed value.
    pub fn into_failure(self, argument: Option<&str>) -> ToolFailure {
        match self {
            BudgetHit::Timeout { configured_ms } => {
                ToolFailure::tool_timeout(argument, configured_ms)
            }
            BudgetHit::Input {
                which,
                limit,
                consumed,
            } => ToolFailure::tool_input_limit(argument, which.label(), limit, consumed),
        }
    }
}

/// A `Read` wrapper that charges scanned bytes and checks the deadline at every
/// bounded read step, so a native search worker meters the bytes it SCANS (not
/// the bytes it returns) and stops mid-file at the deadline or the byte ceiling
/// (FR-19 F1/F2, ADR-031). The worker that owns the reader inspects
/// [`BudgetedReader::hit`] after the search to surface the typed failure; the
/// read error that aborts the search is only the stop signal. Wrap only when a
/// budget is active — the unbudgeted path keeps its exact legacy behavior.
pub struct BudgetedReader<R> {
    inner: R,
    budget: Arc<ToolBudget>,
    hit: Option<BudgetHit>,
}

impl<R: Read> BudgetedReader<R> {
    pub fn new(inner: R, budget: Arc<ToolBudget>) -> Self {
        Self {
            inner,
            budget,
            hit: None,
        }
    }

    /// The budget violation that stopped the scan, if any.
    pub fn hit(&self) -> Option<&BudgetHit> {
        self.hit.as_ref()
    }
}

impl<R: Read> Read for BudgetedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // Deadline first: a bounded step boundary inside a large file.
        if let Err(hit) = self.budget.check_deadline() {
            self.hit = Some(hit);
            return Err(io::Error::other("tool budget deadline"));
        }
        let n = self.inner.read(buf)?;
        if n > 0 {
            // Charge exactly the bytes just scanned.
            if let Err(hit) = self.budget.charge_input(n as u64) {
                self.hit = Some(hit);
                return Err(io::Error::other("tool input budget"));
            }
        }
        Ok(n)
    }
}
