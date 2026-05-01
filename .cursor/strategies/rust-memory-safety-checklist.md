# Rust Memory Safety Checklist

Rust prevents memory **unsafety** (use-after-free, dangling pointers) but
does not prevent memory **leaks**. These are the patterns that cause
logical leaks in safe Rust.

## Pre-commit checklist for new code

### 1. HashMap/Vec in long-lived structs

- [ ] Is there a max size defined?
- [ ] Is there eviction logic before every insert?
- [ ] Is the max configurable (in `Config`) rather than hardcoded?
- [ ] Is there a test that inserts 2x the max and asserts `len() <= max`?

### 2. Temp files and child processes

- [ ] Every `File::create` of a temp file has a `remove_file` on completion
- [ ] Every `bg_processes.insert` has a `bg_processes.remove` when done
- [ ] Tests assert files are deleted and map entries are removed

### 3. Callbacks and closures

- [ ] No `std::thread::spawn` in high-frequency sync callbacks
- [ ] `Arc` clones in closures don't create reference cycles
- [ ] Closures capturing large data don't outlive their usefulness

### 4. Shared state (`Arc<Mutex<T>>` / `Arc<RwLock<T>>`)

- [ ] Inner collections have bounded growth
- [ ] Lock hold times are minimal (no I/O while holding)
- [ ] No nested locks that could deadlock

### 5. Environment and configuration

- [ ] Parent env vars (especially PATH) are always inherited as baseline
- [ ] Config defaults are set in `Default::default()` AND in `.toml`
- [ ] No conditional-only paths that skip essential state initialization

## Quick audit commands

```bash
# Find all HashMap/Vec fields in long-lived structs
rg 'pub \w+: (HashMap|Vec|BTreeMap)' src/ --type rust

# Find inserts without corresponding removes
rg '\.insert\(' src/ --type rust -l
rg '\.remove\(' src/ --type rust -l
# Compare the two lists — any file with insert but no remove is suspect

# Find thread::spawn in callbacks
rg 'thread::spawn' src/ --type rust

# Find temp file creation without cleanup
rg 'File::create.*temp\|tempdir\|tmp' src/ --type rust
rg 'remove_file' src/ --type rust
```
