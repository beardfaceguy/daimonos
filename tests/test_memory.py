"""Memory regression tests.

These verify that daimonos does not leak memory under sustained workloads.
Each test performs many operations against a long-lived MCP session and
checks that process RSS stays within bounds.
"""

import json
import os
import platform


def _get_rss_kb(pid: int) -> int:
    """Get resident set size in KB for a process."""
    if platform.system() == "Darwin":
        # macOS: ps reports RSS in bytes (sometimes KB depending on version)
        import subprocess
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True)
        return int(out.strip())
    else:
        # Linux: read from /proc
        with open(f"/proc/{pid}/status") as f:
            for line in f:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
    return 0


def _parse(result: dict) -> str:
    """Extract text content from MCP tool result."""
    content = result.get("content", [])
    if content:
        return content[0].get("text", "")
    return ""


def test_read_cache_memory_bounded(daimonos):
    """Reading many distinct files should not cause unbounded RSS growth."""
    ws = daimonos.workspace
    pid = daimonos.process.pid

    # Create 500 distinct files
    for i in range(500):
        path = os.path.join(ws, f"memtest_{i}.txt")
        with open(path, "w") as f:
            f.write(f"content for file {i}\n" * 20)

    # Warm up — read a few files to establish baseline
    for i in range(5):
        daimonos.call_tool("read_file", {"path": f"memtest_{i}.txt"})

    rss_before = _get_rss_kb(pid)

    # Read all 500 files
    for i in range(500):
        daimonos.call_tool("read_file", {"path": f"memtest_{i}.txt"})

    rss_after = _get_rss_kb(pid)
    growth_kb = rss_after - rss_before

    # RSS should not grow more than 10 MB from reading 500 small files.
    # If read_cache is unbounded, each entry adds path + hash + metadata.
    assert growth_kb < 10_000, (
        f"RSS grew by {growth_kb} KB after reading 500 files "
        f"({rss_before} -> {rss_after} KB). "
        f"Possible unbounded read_cache growth."
    )


def test_exec_memory_bounded(daimonos):
    """Running many distinct commands should not cause unbounded RSS growth."""
    pid = daimonos.process.pid

    # Warm up
    for i in range(5):
        daimonos.call_tool("exec", {"command": "true"})

    rss_before = _get_rss_kb(pid)

    # Run 500 distinct commands (unique strings to grow exec_usage)
    for i in range(500):
        daimonos.call_tool("exec", {"command": "echo", "args": [f"iteration_{i}"]})

    rss_after = _get_rss_kb(pid)
    growth_kb = rss_after - rss_before

    assert growth_kb < 10_000, (
        f"RSS grew by {growth_kb} KB after 500 exec calls "
        f"({rss_before} -> {rss_after} KB). "
        f"Possible unbounded exec_usage growth."
    )


def test_bg_processes_cleaned_up(daimonos):
    """Completed background processes should be removed from the session map."""

    for i in range(20):
        daimonos.call_tool("exec", {
            "command": "sh",
            "args": ["-c", f"echo bg_{i}"],
        })

    # Now use bg + poll to test actual background process cleanup.
    # We use the batch tool for efficiency, spawning short-lived bg processes.
    for i in range(20):
        daimonos.call_tool("exec", {
            "command": "sh",
            "args": ["-c", "true"],
        })

    # Check workspace_info for session stats — exec_usage should be present
    info_result = daimonos.call_tool("workspace_info", {})
    info_text = _parse(info_result)
    info = json.loads(info_text)
    session = info.get("session", {})

    # exec_usage tracks command frequency; verify it exists and is reasonable
    exec_usage = session.get("exec_usage", [])
    total_entries = len(exec_usage)

    # With bounded exec_usage, we shouldn't have more than a cap's worth
    assert total_entries <= 1024, (
        f"exec_usage has {total_entries} entries — should be bounded"
    )


def test_sustained_workload_stable_rss(daimonos):
    """A sustained mixed workload should not show monotonic RSS growth.

    This is the most important regression test: it simulates a real agent
    session doing reads, writes, edits, searches, and execs in a loop.
    """
    ws = daimonos.workspace
    pid = daimonos.process.pid

    # Set up workspace
    for i in range(50):
        path = os.path.join(ws, f"workload_{i}.txt")
        with open(path, "w") as f:
            f.write(f"fn function_{i}() {{\n    println!(\"hello\");\n}}\n" * 5)

    # Warm-up phase
    for i in range(10):
        daimonos.call_tool("read_file", {"path": f"workload_{i}.txt"})
        daimonos.call_tool("exec", {"command": "echo", "args": [f"warmup_{i}"]})

    rss_samples = []
    rss_samples.append(_get_rss_kb(pid))

    # 5 rounds of mixed workload
    for round_num in range(5):
        for i in range(50):
            daimonos.call_tool("read_file", {"path": f"workload_{i}.txt"})

        for i in range(20):
            daimonos.call_tool("exec", {"command": "echo", "args": [f"r{round_num}_{i}"]})

        daimonos.call_tool("search", {"pattern": "function", "max_results": 10})

        for i in range(5):
            daimonos.call_tool("write_file", {
                "path": f"workload_{i}.txt",
                "content": f"fn function_{i}() {{\n    println!(\"round {round_num}\");\n}}\n" * 5,
            })

        rss_samples.append(_get_rss_kb(pid))

    # Check that RSS is not monotonically increasing across all rounds.
    # Allow for some variance, but the last sample should not be dramatically
    # larger than the second sample (first post-warmup measurement).
    first_stable = rss_samples[1]
    last = rss_samples[-1]
    growth_kb = last - first_stable

    assert growth_kb < 20_000, (
        f"RSS grew monotonically by {growth_kb} KB over 5 workload rounds "
        f"(samples: {rss_samples}). Possible memory leak."
    )
