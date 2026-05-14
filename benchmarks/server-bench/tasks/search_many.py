"""Run 50 grep calls with varied patterns against a synthetic source tree.

Stresses opcode 6 (grep): pattern arg in `p`, search root in `q`, max
results in `n`. The pattern list mixes hits and misses against
`FILE_TEMPLATE` — `needle`/`fn process`/`TODO`/`Result<`/etc. land in
every file, names like `Send`/`#[derive` never appear, and the trailing
25 `nonexistent_token_*` entries are guaranteed misses. We want both
sides of the hit/miss latency distribution in one task.

Per-call timing here reflects ripgrep spawn cost + result formatting,
which is the dominant factor in real-world agent search latency.
"""

from __future__ import annotations

import re
import time
from pathlib import Path

ID = "search_many"
DESCRIPTION = "Run 50 grep calls across a 100-file synthetic source tree"

FILE_COUNT = 100
SEARCH_PATTERNS = [
    "needle",
    "fn process",
    "TODO",
    "panic!",
    "Result<",
    "Option<",
    "async fn",
    "pub struct",
    "impl",
    "let mut",
    "match",
    "Vec<",
    "String",
    "&str",
    "use std",
    "unwrap()",
    "expect(",
    "Arc<",
    "Box<dyn",
    "Send",
    "Sync",
    "tokio",
    "serde",
    "#[derive",
    "thiserror",
] + [f"nonexistent_token_{i:02d}" for i in range(25)]

FILE_TEMPLATE = """// Synthetic source file {idx}
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct Widget {{
    inner: Arc<Mutex<Inner>>,
}}

impl Widget {{
    pub async fn process(&self, needle: &str) -> Result<usize, std::io::Error> {{
        let guard = self.inner.lock().await;
        guard.find(needle)
    }}
}}

struct Inner {{ data: Vec<String> }}

impl Inner {{
    fn find(&self, target: &str) -> Result<usize, std::io::Error> {{
        // TODO: optimize this scan once we have benchmarks
        for (i, s) in self.data.iter().enumerate() {{
            if s.contains(target) {{
                return Ok(i);
            }}
        }}
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "miss"))
    }}
}}
"""


def setup(workspace: Path) -> None:
    src = workspace / "src"
    src.mkdir(parents=True, exist_ok=True)
    for i in range(FILE_COUNT):
        (src / f"mod_{i:03d}.rs").write_text(FILE_TEMPLATE.format(idx=i))


def run_iteration(client, workspace: Path):
    src_str = str(workspace / "src")
    samples: list[tuple[int, int]] = []
    for pat in SEARCH_PATTERNS:
        # The grep opcode forwards the pattern straight to ripgrep, so
        # regex metacharacters in our test patterns (`(`, `)`, `[`, `+`,
        # …) would otherwise be parsed as regex syntax. Escape so each
        # entry in SEARCH_PATTERNS reads as a fixed string.
        op = {"c": 6, "p": re.escape(pat), "q": src_str, "n": 50}
        start = time.perf_counter_ns()
        resp = client.call(op)
        elapsed = time.perf_counter_ns() - start
        if not resp.get("ok", False):
            raise RuntimeError(
                f"search_many failed for pattern {pat!r}: {resp.get('m')!r}"
            )
        samples.append((6, elapsed))
    return samples
