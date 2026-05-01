# Rust Collection Patterns

Patterns to follow and anti-patterns to avoid when using collections in
long-lived Rust services.

## Use: Bounded insert wrapper

Wrap every cache `HashMap` in a method that enforces a size limit. Don't
expose the raw map for direct insertion from call sites.

```rust
// Encapsulate the bound check in a single method
pub fn cache_put(&mut self, key: K, value: V) {
    if self.cache.len() >= self.max_entries {
        let evict = self.cache.keys().next().cloned();
        if let Some(k) = evict { self.cache.remove(&k); }
    }
    self.cache.insert(key, value);
}
```

**Why**: If callers use `map.insert()` directly, every call site must
independently remember the bound check. One missed site = unbounded growth.

## Use: Remove-on-terminal-state

When a map tracks in-flight work (processes, requests, connections), the
entry must be removed in the same code path that observes termination.

```rust
match proc.try_wait() {
    Ok(Some(exit)) => {
        // Terminal state — clean up immediately
        self.processes.remove(&pid);
        cleanup_temp_files(&pid);
        Ok(exit)
    }
    Ok(None) => Ok(Running),
    Err(e) => Err(e),
}
```

**Why**: If the "check" function only reports status without cleanup, the
caller must remember to clean up — and often doesn't.

## Use: AtomicBool for dirty flags

When a synchronous callback (filesystem watcher, signal handler) needs to
communicate a boolean state change to async code, use `AtomicBool`.

```rust
let dirty = Arc::new(AtomicBool::new(false));
let dirty_clone = dirty.clone();

// Sync callback — no allocations, no threads
watcher.watch(move |_event| {
    dirty_clone.store(true, Ordering::Relaxed);
});

// Async reader — check and clear
if dirty.swap(false, Ordering::Relaxed) {
    invalidate_cache().await;
}
```

**Why**: `std::thread::spawn` in a callback creates an OS thread per event.
Channels require allocation per event. `AtomicBool` is zero-cost.

## Avoid: Conditional environment inheritance

When building a child environment from a parent, always start with the
parent's value as the baseline. Don't skip inheritance when there are no
modifications.

```rust
// WRONG: PATH missing when no extras
if !extras.is_empty() {
    env.insert("PATH", format!("{}:{}", extras, parent));
}

// RIGHT: always inherit
let path = if !extras.is_empty() {
    format!("{}:{}", extras, parent)
} else {
    parent
};
env.insert("PATH", path);
```

**Why**: Downstream code assumes PATH exists. Conditional insertion creates
a silent failure mode that only surfaces in specific environments.

## Avoid: Insert-only maps without remove

If a `HashMap::insert` exists for a key type, there must be a corresponding
`HashMap::remove` somewhere for the same key type. If there isn't, the map
grows monotonically.

Audit checklist:
- `grep` for every `.insert(` on the map
- For each insert, confirm a `.remove(` exists on a reachable code path
- If the map is meant to be permanent (like a config registry), document
  that explicitly

## Avoid: Testing only the happy path

When a function has both a "create" and "destroy" phase, test both. A test
that only verifies creation succeeded will miss bugs in cleanup.

```rust
// INCOMPLETE: only tests creation
let id = create_resource(&mut state);
assert!(state.resources.contains_key(&id));

// COMPLETE: tests full lifecycle
let id = create_resource(&mut state);
assert!(state.resources.contains_key(&id));
finish_resource(&mut state, id);
assert!(!state.resources.contains_key(&id));
assert!(!temp_path(id).exists());
```
