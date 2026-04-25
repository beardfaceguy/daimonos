# Daimonos — Installation & Requirements

## Supported Platforms

| Platform | Architecture | Status |
|----------|-------------|--------|
| Linux (Ubuntu/Debian) | x86_64, aarch64 | Fully supported |
| macOS | Apple Silicon (aarch64) | Fully supported |
| macOS | Intel (x86_64) | Should work (untested) |

## Prerequisites

### Required

| Dependency | Version | How to install |
|-----------|---------|----------------|
| **Rust toolchain** | 1.75+ (stable) | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| **Git** | 2.x | Pre-installed on macOS; `apt install git` on Debian/Ubuntu |
| **C compiler** | Any | Xcode CLI tools on macOS (`xcode-select --install`); `apt install build-essential` on Debian/Ubuntu |

### Optional (for integration tests)

| Dependency | Version | How to install |
|-----------|---------|----------------|
| **Python** | 3.9+ | Pre-installed on macOS; `apt install python3` on Debian/Ubuntu |
| **pytest** | 8.x | `pip install -r tests/requirements.txt` |

## Quick Start

```bash
# 1. Clone the repository
git clone git@github.com:beardfaceguy/daimonos.git
cd daimonos

# 2. Build
cargo build --release

# 3. Run tests
cargo test

# 4. (Optional) Run integration tests
pip install -r tests/requirements.txt
pytest tests/ -v

# 5. Run the MCP server
./target/release/daimonos --mcp -w /path/to/workspace
```

## Platform-Specific Notes

### macOS

- The system Python (3.9) works for integration tests. No need to install a
  newer version.
- If your git config has `commit.gpgsign = true` globally, test repos
  explicitly disable it. No GPG installation is needed for development.
- File system notifications use FSEvents (via the `notify` crate). No extra
  configuration needed.

### Linux

- File system notifications use inotify. The default `fs.inotify.max_user_watches`
  limit (usually 65536) is sufficient. For very large workspaces, increase it:
  ```bash
  echo fs.inotify.max_user_watches=524288 | sudo tee -a /etc/sysctl.conf
  sudo sysctl -p
  ```

## MCP Configuration (Cursor IDE)

Add to your `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "daimonos": {
      "command": "/path/to/daimonos",
      "args": ["--mcp", "-w", "/path/to/workspace"]
    }
  }
}
```

## CI

GitHub Actions runs on every push and PR against `master`:
- **Rust tests** on Ubuntu and macOS
- **Python integration tests** on Ubuntu and macOS
- **Clippy** lint checks
- **rustfmt** format checks
