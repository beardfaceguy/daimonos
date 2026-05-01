#!/usr/bin/env bash
# Provisions an Ubuntu 24.04 instance for baseline benchmarking.
# Runs ON the instance (SCP'd and executed by the orchestrator).
set -euo pipefail

echo "=== Provisioning Ubuntu baseline instance ==="

export DEBIAN_FRONTEND=noninteractive

# Node.js LTS (Claude Code CLI requires it)
if ! command -v node &>/dev/null; then
    echo "Installing Node.js..."
    curl -fsSL https://deb.nodesource.com/setup_22.x | sudo -E bash -
    sudo apt-get install -y nodejs
fi

# Build essentials (needed for native npm modules and cargo)
sudo apt-get install -y git curl build-essential python3

# Rust via rustup
if ! command -v cargo &>/dev/null; then
    echo "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
fi

# Claude Code CLI
if ! command -v claude &>/dev/null; then
    echo "Installing Claude Code CLI..."
    sudo npm install -g @anthropic-ai/claude-code
fi

echo ""
echo "Versions:"
echo "  node:  $(node --version)"
echo "  npm:   $(npm --version)"
echo "  cargo: $(cargo --version)"
echo "  claude: $(claude --version 2>/dev/null || echo 'installed')"
echo "  git:   $(git --version)"
echo ""
echo "=== Ubuntu provisioning complete ==="
