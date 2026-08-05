//! Deterministic tool retry-storm detection for the agent loop (vikunja #1197).
//!
//! Adapted from the deep review of Octomind (`Muvon/octomind`,
//! `src/supervisor/detect.rs`): every model round's tool calls and results are
//! fingerprinted; rounds that produce no new information accumulate a
//! no-progress window. Once a threshold is crossed the detector emits ONE
//! bounded corrective steer for the next model request. If the model changes
//! its call-set that counts as attempted recovery and de-escalates; if it
//! repeats the exact call-set, reminders back off exponentially while a hard
//! circuit breaker stays armed. No LLM call ever happens in this path.
//!
//! The detector lives entirely inside one `agent::run` invocation, so a new
//! user task naturally starts with fresh windows. When intra-turn
//! microcompaction prunes results the caller must invoke
//! [`LoopDetector::on_context_pruned`] so novelty windows never reference
//! content the model can no longer see.

use crate::config::LoopDetectorConfig;
use serde_json::Value;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// One executed tool call inside a model round, reduced to fingerprints.
/// The tool-call id is deliberately excluded: providers mint a fresh id for
/// every call, so including it would make every round look novel.
#[derive(Debug, Clone)]
pub struct CallObservation {
    /// Tool name, kept for steer text only.
    pub name: String,
    /// Fingerprint of `(name, normalized args)`.
    pub call_fp: u64,
    /// Fingerprint of `(is_error, result content)`.
    pub result_fp: u64,
    /// Whether the result was an error (repeated failures are a loop signal).
    pub is_error: bool,
}

impl CallObservation {
    pub fn new(name: &str, input: &Value, is_error: bool, result_content: &str) -> Self {
        Self {
            name: name.to_string(),
            call_fp: fingerprint_call(name, input),
            result_fp: fingerprint_result(is_error, result_content),
            is_error,
        }
    }
}

/// Hash of one tool call: the name plus its serialized arguments. serde_json
/// serialization is deterministic for a given `Value`, which is sufficient —
/// both occurrences being compared come from the same provider parse path.
pub fn fingerprint_call(name: &str, input: &Value) -> u64 {
    let mut h = DefaultHasher::new();
    name.hash(&mut h);
    input.to_string().hash(&mut h);
    h.finish()
}

/// Hash of one tool result as the model sees it (post output-bounding).
pub fn fingerprint_result(is_error: bool, content: &str) -> u64 {
    let mut h = DefaultHasher::new();
    is_error.hash(&mut h);
    content.hash(&mut h);
    h.finish()
}

/// Outcome of observing one model round.
#[derive(Debug, PartialEq, Eq)]
pub enum RoundVerdict {
    /// Nothing to do.
    Proceed,
    /// Inject this bounded corrective hint into the next model request.
    Steer(String),
    /// Hard stop: the configured circuit-breaker window was exhausted.
    Break(String),
}

/// Aggregate counters for analytics/logging.
#[derive(Debug, Default, Clone, Copy)]
pub struct DetectorStats {
    pub rounds_observed: u64,
    pub steers_emitted: u64,
    pub steers_suppressed: u64,
    pub max_pair_repeats: u32,
}

pub struct LoopDetector {
    cfg: LoopDetectorConfig,
    /// Steer template sections (split on `---` lines), rotated per emission.
    steer_sections: Vec<String>,
    /// `(call_fp, result_fp, is_error)` → rounds in which the exact pair was
    /// seen. Incremented at most once per round so a parallel batch of
    /// identical calls counts as one observation (one detector round).
    pair_rounds: HashMap<(u64, u64, bool), u32>,
    /// Rounds with zero novel `(call, result)` pairs, consecutively.
    consecutive_no_novelty: u32,
    /// Call-set fingerprint of the previous round.
    last_call_set: Option<u64>,
    /// Call-set fingerprint at the moment the last steer was emitted.
    steered_call_set: Option<u64>,
    /// Rounds the model repeated the steered call-set after a steer.
    ignored_steers: u32,
    rotation: usize,
    stats: DetectorStats,
}

impl LoopDetector {
    /// `steer_template` is the resolved `loop_steer` prompt (embedded default
    /// or `[prompts].loop_steer` override). Sections are separated by lines
    /// containing only `---`; the detector rotates through them.
    pub fn new(cfg: LoopDetectorConfig, steer_template: &str) -> Self {
        let steer_sections: Vec<String> = steer_template
            .split("\n---\n")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        Self {
            cfg,
            steer_sections,
            pair_rounds: HashMap::new(),
            consecutive_no_novelty: 0,
            last_call_set: None,
            steered_call_set: None,
            ignored_steers: 0,
            rotation: 0,
            stats: DetectorStats::default(),
        }
    }

    pub fn stats(&self) -> DetectorStats {
        self.stats
    }

    /// Reset novelty windows after compaction/microcompaction removed results
    /// the fingerprints refer to. Steer/backoff escalation state survives so a
    /// storm cannot launder itself through a compaction pass.
    pub fn on_context_pruned(&mut self) {
        self.pair_rounds.clear();
        self.consecutive_no_novelty = 0;
    }

    /// Observe one complete model round (a whole parallel batch of tool calls
    /// with their results) and decide whether to steer or break.
    pub fn observe_round(&mut self, round: &[CallObservation]) -> RoundVerdict {
        if round.is_empty() {
            return RoundVerdict::Proceed;
        }
        self.stats.rounds_observed += 1;

        // A parallel batch aggregates into ONE detector round: each distinct
        // pair increments its round-count once, however many duplicates the
        // batch carried.
        let mut round_pairs: Vec<(u64, u64, bool)> = round
            .iter()
            .map(|o| (o.call_fp, o.result_fp, o.is_error))
            .collect();
        round_pairs.sort_unstable();
        round_pairs.dedup();

        let novelty = round_pairs
            .iter()
            .any(|pair| !self.pair_rounds.contains_key(pair));
        if novelty {
            // A novel (call, result) pair proves progress: restart the repeat
            // window so interleaved legitimate work (polls, staged reads)
            // never accumulates into a false storm. Only uninterrupted
            // repetition can steer. Also keeps the map bounded.
            self.pair_rounds.clear();
        }
        let mut max_repeats: u32 = 0;
        for pair in &round_pairs {
            let count = self.pair_rounds.entry(*pair).or_insert(0);
            *count += 1;
            max_repeats = max_repeats.max(*count);
        }
        self.stats.max_pair_repeats = self.stats.max_pair_repeats.max(max_repeats);

        // Call-set fingerprint (argument-level, order-insensitive).
        let call_set = {
            let mut fps: Vec<u64> = round.iter().map(|o| o.call_fp).collect();
            fps.sort_unstable();
            fps.dedup();
            let mut h = DefaultHasher::new();
            fps.hash(&mut h);
            h.finish()
        };

        if novelty {
            self.consecutive_no_novelty = 0;
        } else {
            self.consecutive_no_novelty += 1;
        }

        // Recovery: after a steer, ANY changed call-set counts as an attempt
        // to break out — de-escalate instead of nagging a model that is
        // already trying something else.
        if let Some(steered) = self.steered_call_set {
            if steered != call_set {
                self.steered_call_set = None;
                self.ignored_steers = 0;
            }
        }

        self.last_call_set = Some(call_set);

        // Hard stop before another provider call is paid for.
        let breaker = self.cfg.circuit_breaker_rounds;
        if breaker > 0 && self.consecutive_no_novelty >= breaker {
            return RoundVerdict::Break(format!(
                "loop detector circuit breaker: {} consecutive tool rounds produced no new \
                 information (repeated identical calls/results; tools: {}). Stopping this turn; \
                 re-prompt with a different approach or raise \
                 [loop_detector].circuit_breaker_rounds.",
                self.consecutive_no_novelty,
                summarize_tools(round),
            ));
        }

        let repeat_hit = self.cfg.repeat_threshold > 0 && max_repeats >= self.cfg.repeat_threshold;
        let window_hit = self.cfg.no_novelty_rounds > 0
            && self.consecutive_no_novelty >= self.cfg.no_novelty_rounds;
        if !repeat_hit && !window_hit {
            return RoundVerdict::Proceed;
        }

        // Exponential de-spam: while the model keeps repeating the exact
        // steered call-set, re-emit only on ignore counts that are powers of
        // two (1, 2, 4, 8, ...). A changed call-set was already handled above.
        if self.steered_call_set == Some(call_set) {
            self.ignored_steers += 1;
            if !self.ignored_steers.is_power_of_two() {
                self.stats.steers_suppressed += 1;
                return RoundVerdict::Proceed;
            }
        } else {
            self.steered_call_set = Some(call_set);
            self.ignored_steers = 0;
        }

        RoundVerdict::Steer(self.render_steer(round, max_repeats))
    }

    fn render_steer(&mut self, round: &[CallObservation], repeats: u32) -> String {
        self.stats.steers_emitted += 1;
        let section = if self.steer_sections.is_empty() {
            // Defensive fallback; the embedded default always has sections.
            "You are repeating identical tool calls that return identical results. Change your approach.".to_string()
        } else {
            let s = self.steer_sections[self.rotation % self.steer_sections.len()].clone();
            self.rotation += 1;
            s
        };
        section
            .replace("{tools}", &summarize_tools(round))
            .replace("{repeats}", &repeats.to_string())
            .replace("{rounds}", &self.consecutive_no_novelty.to_string())
    }
}

/// Bounded, deduplicated tool-name list for steer/breaker text.
fn summarize_tools(round: &[CallObservation]) -> String {
    const MAX_NAMES: usize = 5;
    let mut names: Vec<&str> = round.iter().map(|o| o.name.as_str()).collect();
    names.sort_unstable();
    names.dedup();
    let overflow = names.len().saturating_sub(MAX_NAMES);
    let mut out = names[..names.len().min(MAX_NAMES)].join(", ");
    if overflow > 0 {
        out.push_str(&format!(" (+{overflow} more)"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEMPLATE: &str = "[DAIMONOS LOOP DETECTOR]\nhint-a {tools} {repeats} {rounds}\n---\n[DAIMONOS LOOP DETECTOR]\nhint-b {tools}";

    fn cfg() -> LoopDetectorConfig {
        LoopDetectorConfig {
            enabled: true,
            repeat_threshold: 3,
            no_novelty_rounds: 3,
            circuit_breaker_rounds: 8,
        }
    }

    fn detector() -> LoopDetector {
        LoopDetector::new(cfg(), TEMPLATE)
    }

    fn obs(name: &str, input: &Value, is_error: bool, result: &str) -> CallObservation {
        CallObservation::new(name, input, is_error, result)
    }

    fn same_round() -> Vec<CallObservation> {
        vec![obs("read_file", &json!({"path": "a.rs"}), false, "content")]
    }

    #[test]
    fn identical_call_and_result_steers_at_threshold() {
        let mut d = detector();
        assert_eq!(d.observe_round(&same_round()), RoundVerdict::Proceed);
        assert_eq!(d.observe_round(&same_round()), RoundVerdict::Proceed);
        match d.observe_round(&same_round()) {
            RoundVerdict::Steer(text) => {
                assert!(text.contains("read_file"), "steer names the tool: {text}");
                assert!(text.contains("LOOP DETECTOR"));
            }
            other => panic!("expected steer on 3rd identical round, got {other:?}"),
        }
        assert_eq!(d.stats().steers_emitted, 1);
    }

    #[test]
    fn changing_results_never_steer() {
        // A legitimate poll: same call, different result each time.
        let mut d = detector();
        for i in 0..20 {
            let round = vec![obs(
                "get_review",
                &json!({"job_id": "cj_1"}),
                false,
                &format!("phase {i}"),
            )];
            assert_eq!(d.observe_round(&round), RoundVerdict::Proceed, "round {i}");
        }
        assert_eq!(d.stats().steers_emitted, 0);
    }

    #[test]
    fn new_arguments_reset_the_no_novelty_window() {
        let mut d = detector();
        d.observe_round(&same_round());
        d.observe_round(&same_round());
        // Novel args: window resets before the threshold is hit.
        let novel = vec![obs("read_file", &json!({"path": "b.rs"}), false, "content")];
        assert_eq!(d.observe_round(&novel), RoundVerdict::Proceed);
        assert_eq!(d.observe_round(&same_round()), RoundVerdict::Proceed);
        assert_eq!(d.stats().steers_emitted, 0);
    }

    #[test]
    fn parallel_batch_counts_as_one_round() {
        // Three duplicate calls in ONE batch must not fast-forward the
        // repeat counter to the threshold.
        let mut d = detector();
        let batch: Vec<CallObservation> = (0..3)
            .map(|_| obs("read_file", &json!({"path": "a.rs"}), false, "content"))
            .collect();
        assert_eq!(d.observe_round(&batch), RoundVerdict::Proceed);
        assert_eq!(d.observe_round(&batch), RoundVerdict::Proceed);
        assert!(matches!(d.observe_round(&batch), RoundVerdict::Steer(_)));
    }

    #[test]
    fn repeated_identical_failures_steer() {
        let mut d = detector();
        let round = vec![obs("exec", &json!({"command": "make"}), true, "error: x")];
        assert_eq!(d.observe_round(&round), RoundVerdict::Proceed);
        assert_eq!(d.observe_round(&round), RoundVerdict::Proceed);
        assert!(matches!(d.observe_round(&round), RoundVerdict::Steer(_)));
    }

    #[test]
    fn ignored_steers_back_off_exponentially() {
        let mut d = detector();
        d.observe_round(&same_round());
        d.observe_round(&same_round());
        assert!(matches!(
            d.observe_round(&same_round()),
            RoundVerdict::Steer(_)
        ));
        // Ignores 1 and 2 re-steer, 3 is suppressed, 4 re-steers.
        assert!(matches!(
            d.observe_round(&same_round()),
            RoundVerdict::Steer(_)
        )); // ignored=1
        assert!(matches!(
            d.observe_round(&same_round()),
            RoundVerdict::Steer(_)
        )); // ignored=2
        assert_eq!(d.observe_round(&same_round()), RoundVerdict::Proceed); // ignored=3
        assert!(matches!(
            d.observe_round(&same_round()),
            RoundVerdict::Steer(_)
        )); // ignored=4
        assert!(d.stats().steers_suppressed >= 1);
    }

    #[test]
    fn changed_call_set_after_steer_resets_backoff() {
        let mut d = detector();
        d.observe_round(&same_round());
        d.observe_round(&same_round());
        assert!(matches!(
            d.observe_round(&same_round()),
            RoundVerdict::Steer(_)
        ));
        // Recovery: a different call-set de-escalates...
        let other = vec![obs("search", &json!({"pattern": "x"}), false, "hits")];
        assert_eq!(d.observe_round(&other), RoundVerdict::Proceed);
        // ...so a NEW storm steers immediately at its own threshold again
        // rather than being half-way through a backoff ladder.
        d.observe_round(&same_round());
        d.observe_round(&same_round());
        assert!(matches!(
            d.observe_round(&same_round()),
            RoundVerdict::Steer(_)
        ));
    }

    #[test]
    fn circuit_breaker_trips_after_configured_rounds() {
        let mut d = detector();
        let mut broke = None;
        for i in 1..=20 {
            if let RoundVerdict::Break(msg) = d.observe_round(&same_round()) {
                broke = Some((i, msg));
                break;
            }
        }
        let (round, msg) = broke.expect("breaker must trip");
        // First round is novel; 8 no-novelty rounds follow → trips at round 9.
        assert_eq!(round, 9);
        assert!(msg.contains("circuit breaker"));
        assert!(msg.contains("read_file"));
    }

    #[test]
    fn zero_breaker_config_disables_hard_stop() {
        let mut d = LoopDetector::new(
            LoopDetectorConfig {
                circuit_breaker_rounds: 0,
                ..cfg()
            },
            TEMPLATE,
        );
        for _ in 0..50 {
            assert!(!matches!(
                d.observe_round(&same_round()),
                RoundVerdict::Break(_)
            ));
        }
    }

    #[test]
    fn context_prune_resets_novelty_windows() {
        let mut d = detector();
        d.observe_round(&same_round());
        d.observe_round(&same_round());
        d.on_context_pruned();
        // Windows restart: two more rounds stay below both thresholds.
        assert_eq!(d.observe_round(&same_round()), RoundVerdict::Proceed);
        assert_eq!(d.observe_round(&same_round()), RoundVerdict::Proceed);
        assert_eq!(d.stats().steers_emitted, 0);
    }

    #[test]
    fn steer_sections_rotate() {
        let mut d = detector();
        d.observe_round(&same_round());
        d.observe_round(&same_round());
        let first = match d.observe_round(&same_round()) {
            RoundVerdict::Steer(t) => t,
            other => panic!("expected steer, got {other:?}"),
        };
        let second = match d.observe_round(&same_round()) {
            RoundVerdict::Steer(t) => t,
            other => panic!("expected steer, got {other:?}"),
        };
        assert!(first.contains("hint-a"));
        assert!(second.contains("hint-b"));
        assert_ne!(first, second);
    }

    #[test]
    fn steer_text_is_bounded_and_fills_placeholders() {
        let mut d = detector();
        let batch: Vec<CallObservation> = (0..10)
            .map(|i| {
                obs(
                    &format!("tool_{i}"),
                    &json!({"path": "a.rs"}),
                    false,
                    "content",
                )
            })
            .collect();
        d.observe_round(&batch);
        d.observe_round(&batch);
        let text = match d.observe_round(&batch) {
            RoundVerdict::Steer(t) => t,
            other => panic!("expected steer, got {other:?}"),
        };
        assert!(!text.contains("{tools}"));
        assert!(!text.contains("{repeats}"));
        assert!(!text.contains("{rounds}"));
        assert!(text.contains("(+5 more)"), "tool list is capped: {text}");
        assert!(text.len() < 1024, "steer stays bounded: {}", text.len());
    }

    #[test]
    fn tool_call_ids_do_not_defeat_fingerprints() {
        // Fingerprints are computed from (name, args, result) only; two calls
        // that differ solely by provider-minted id are identical.
        let a = obs("read_file", &json!({"path": "a.rs"}), false, "content");
        let b = obs("read_file", &json!({"path": "a.rs"}), false, "content");
        assert_eq!(a.call_fp, b.call_fp);
        assert_eq!(a.result_fp, b.result_fp);
    }

    #[test]
    fn empty_round_is_a_noop() {
        let mut d = detector();
        assert_eq!(d.observe_round(&[]), RoundVerdict::Proceed);
        assert_eq!(d.stats().rounds_observed, 0);
    }
}
