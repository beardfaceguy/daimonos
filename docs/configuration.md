# Configuration Reference

Daimonos uses a TOML config file for tuning indexing, search, and process
behavior. All settings have sensible defaults — configuration is optional.

## Config File Location

Daimonos searches for config in this order:

1. Path passed via `--config` / `-c` flag
2. `daimonos.toml` in the workspace root
3. `~/.config/daimonos/config.toml`
4. Built-in defaults (equivalent to `daimonos.default.toml` in the repo)

To start with a custom config, copy the reference file:

```bash
cp daimonos.default.toml ~/.config/daimonos/config.toml
```

Or place it in your project:

```bash
cp daimonos.default.toml /path/to/project/daimonos.toml
```

## Settings

### `[index]` — Workspace Indexing

Daimonos builds a trigram index of your workspace in the background for fast
search. These settings control what gets indexed.

| Setting | Default | Description |
|---------|---------|-------------|
| `max_depth` | `20` | Maximum directory traversal depth |
| `max_file_size` | `1000000` (1 MB) | Skip files larger than this (bytes) |
| `binary_sniff_bytes` | `512` | Bytes to check for null bytes when detecting binary files |
| `skip_extensions` | *(see below)* | File extensions to skip (known binary formats) |

The default `skip_extensions` list covers images, audio, video, archives,
compiled objects, fonts, databases, and office documents. Directories named
`.git`, `node_modules`, and `target` are always skipped.

```toml
[index]
max_depth = 20
max_file_size = 1_000_000
binary_sniff_bytes = 512
skip_extensions = [
  "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "svg",
  "mp3", "mp4", "avi", "mov", "mkv", "flac", "wav", "ogg", "webm",
  "zip", "tar", "gz", "bz2", "xz", "7z", "rar", "zst",
  "exe", "dll", "so", "dylib", "o", "a", "lib",
  "wasm", "pyc", "pyo", "class",
  "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx",
  "sqlite", "db", "mdb",
  "ttf", "otf", "woff", "woff2", "eot",
]
```

### `[search]` — Search Limits

| Setting | Default | Description |
|---------|---------|-------------|
| `default_grep_max` | `100` | Max results for content search (grep) when not specified by caller |
| `default_find_max` | `20` | Max results for file search (trigram) when not specified |

```toml
[search]
default_grep_max = 100
default_find_max = 20
```

### `[process]` — Command Execution

| Setting | Default | Description |
|---------|---------|-------------|
| `poll_tail_lines` | `20` | Number of trailing output lines returned by `poll` for background processes |
| `exec_output_max_chars` | `100000` (100 KB) | Max characters of exec stdout/stderr before auto-truncation |
| `extra_path` | *(none)* | Additional directories to prepend to `PATH` for exec/bg commands |

Auto-truncation keeps the first and last lines of output with a
`[N lines, M chars truncated]` notice in the middle. This prevents large build
outputs from consuming excessive tokens.

Common tool directories (`~/.cargo/bin`, `~/.local/bin`, etc.) are
auto-detected and added to `PATH`. Use `extra_path` for non-standard
locations:

```toml
[process]
poll_tail_lines = 20
exec_output_max_chars = 100_000
extra_path = ["/opt/custom/bin", "/usr/local/go/bin"]
```

### `[tools.<id>]` — Tool Plugins (Advanced)

Register external tools for the tool runner system. Most users don't need
this — the built-in plugins for git, cargo, gh, and docker are auto-registered
when the binaries are found on `PATH`.

```toml
[tools.x07]
bin = "/path/to/x07"
source_pattern = "**/*.x07.json"
manifest = "x07.json"
```

## Environment Variables

These are not part of the config file but affect daimonos behavior:

| Variable | Description |
|----------|-------------|
| `DAIMONOS_LOG` | Set to `debug` or `trace` for verbose logging to stderr |
| `PATH` | Daimonos inherits the launching process's `PATH` for exec/bg commands |

## Performance Tuning Tips

**Large monorepos**: increase `max_depth` and `max_file_size` if important
files are deeply nested or large. Consider adding project-specific binary
extensions to `skip_extensions`.

**Slow indexing**: the index builds incrementally after the first full scan.
If the initial scan is slow, check that `skip_extensions` covers your binary
artifacts.

**Truncated exec output**: if you're losing important build output, increase
`exec_output_max_chars`. The default of 100 KB covers most cases.
