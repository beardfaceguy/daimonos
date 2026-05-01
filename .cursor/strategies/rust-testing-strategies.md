# Rust Testing Strategies for Resource Management

Patterns for catching resource leaks, unbounded growth, and lifecycle
bugs before they reach production.

## Strategy 1: Bounded growth tests

For every cache or counter in session state, write a test that inserts
well beyond the expected max and asserts the collection stays bounded.

```rust
#[test]
fn cache_bounded_after_many_inserts() {
    let mut state = TestState::new();
    for i in 0..2000 {
        state.cache_put(format!("key_{i}"), value);
    }
    assert!(state.cache.len() <= MAX_ENTRIES);
}
```

**Key insight**: Write the test *before* implementing eviction. If it
passes without eviction logic, the test is wrong.

## Strategy 2: Full lifecycle tests

Test create → use → teardown as a single test. Assert the resource is
gone after teardown, not just that teardown "succeeded."

```rust
#[tokio::test]
async fn process_cleaned_up_after_completion() {
    let pid = spawn_bg(&mut session).await;
    assert!(session.processes.contains_key(&pid));

    wait_for_completion(&mut session, pid).await;

    assert!(!session.processes.contains_key(&pid));
    assert!(!temp_log_path(pid).exists());
}
```

## Strategy 3: Accumulation stress tests

Run N iterations of an operation and verify zero accumulation afterward.
Catches leaks that only manifest over repeated use.

```rust
#[tokio::test]
async fn no_accumulation_over_many_iterations() {
    let mut session = new_session();
    for _ in 0..100 {
        let pid = spawn_and_complete(&mut session).await;
        poll_until_done(&mut session, pid).await;
    }
    assert!(session.processes.is_empty());
}
```

## Strategy 4: RSS regression guards (pytest layer)

For coarser-grained detection, measure the process's RSS before and after
a sustained workload. Useful as a safety net even when specific unit tests
exist.

```python
def test_memory_stable_under_load(daimonos):
    pid = daimonos.pid
    rss_before = get_rss_kb(pid)
    for i in range(500):
        daimonos.call_tool("read_file", {"path": f"file_{i}.txt"})
    rss_after = get_rss_kb(pid)
    assert rss_after - rss_before < 10_000  # 10 MB growth cap
```

**Limitation**: RSS tests are coarse. They catch large leaks but may miss
small per-entry overhead. Use alongside targeted unit tests, not instead of.

## Strategy 5: Write failing tests first (TDD for bugs)

When you suspect a leak:
1. Write a test that asserts bounded/cleanup behavior
2. Run it — if it passes, your theory is wrong (good!)
3. If it fails, you've confirmed the bug and have a regression test
4. Fix the bug
5. Run the test again — it should now pass

This order prevents "tests that accidentally pass" and gives confidence
the fix is real.
