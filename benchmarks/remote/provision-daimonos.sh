#!/bin/sh
# Provisions a daimonos distro instance for benchmarking.
# Runs ON the instance as the bench user (SCP'd and executed by the orchestrator).
# Node.js + npm are already in the distro image (Buildroot).
set -eu

echo "=== Provisioning daimonos benchmark instance ==="

# Set npm global prefix to user-writable location (no root on daimonos)
mkdir -p "$HOME/.local"
npm config set prefix "$HOME/.local"
export PATH="$HOME/.local/bin:$PATH"

# Claude Code CLI (Node.js + npm already in the distro)
if ! command -v claude >/dev/null 2>&1; then
    echo "Installing Claude Code CLI..."
    npm install -g @anthropic-ai/claude-code
fi

# Rust via rustup (must use musl-static binary on this distro)
if ! command -v cargo >/dev/null 2>&1; then
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf \
        https://static.rust-lang.org/rustup/dist/x86_64-unknown-linux-musl/rustup-init \
        -o /tmp/rustup-init
    chmod +x /tmp/rustup-init
    /tmp/rustup-init -y --default-host x86_64-unknown-linux-musl
    rm -f /tmp/rustup-init
    . "$HOME/.cargo/env"
fi

# Use rust-lld as linker (no system cc on this minimal distro)
mkdir -p "$HOME/.cargo"
if ! grep -q 'rust-lld' "$HOME/.cargo/config.toml" 2>/dev/null; then
    printf '[target.x86_64-unknown-linux-musl]\nlinker = "rust-lld"\n' >> "$HOME/.cargo/config.toml"
fi

echo ""
echo "Versions:"
echo "  node:  $(node --version)"
echo "  npm:   $(npm --version)"
echo "  cargo: $(cargo --version 2>/dev/null || echo 'not found')"
echo "  claude: $(claude --version 2>/dev/null || echo 'installed')"
echo "  git:   $(git --version)"
echo "  daimonos: $(/usr/bin/daimonos --version 2>/dev/null || echo 'pre-installed')"
echo ""
echo "=== Daimonos provisioning complete ==="
