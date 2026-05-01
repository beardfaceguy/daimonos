#!/usr/bin/env bash
# Standalone result collector for when the orchestrator was interrupted.
# Pulls benchmark results from running instances by IP.
#
# Usage:
#   ./collect-results.sh <ubuntu-ip> <daimonos-ip>
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"

UBUNTU_IP="${1:-}"
DAIMONOS_IP="${2:-}"

if [[ -z "$UBUNTU_IP" || -z "$DAIMONOS_IP" ]]; then
    echo "Usage: $0 <ubuntu-ip> <daimonos-ip>"
    exit 1
fi

LOCAL_RESULTS="$BENCH_DIR/results"
mkdir -p "$LOCAL_RESULTS"

collect() {
    local host="$1"
    local user="$2"
    local mode="$3"

    LATEST_RUN=$(ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" \
        "ls -1d ~/benchmark/results/*-${mode}* 2>/dev/null | sort | tail -1 | xargs basename" 2>/dev/null || true)

    if [[ -z "$LATEST_RUN" ]]; then
        echo "No $mode results found on $host"
        return 1
    fi

    LOCAL_RUN_DIR="$LOCAL_RESULTS/remote-$LATEST_RUN"
    mkdir -p "$LOCAL_RUN_DIR"
    ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" \
        "tar -cf - -C ~/benchmark/results/$LATEST_RUN ." | \
        tar -xf - -C "$LOCAL_RUN_DIR/"
    echo "Collected $mode -> $LOCAL_RUN_DIR"
}

collect "$UBUNTU_IP" "ubuntu" "baseline"
collect "$DAIMONOS_IP" "bench" "daimonos"

BASELINE_DIR=$(ls -1d "$LOCAL_RESULTS"/remote-*-baseline* 2>/dev/null | sort | tail -1 | xargs basename 2>/dev/null || true)
DAIMONOS_DIR=$(ls -1d "$LOCAL_RESULTS"/remote-*-daimonos* 2>/dev/null | sort | tail -1 | xargs basename 2>/dev/null || true)

if [[ -n "$BASELINE_DIR" && -n "$DAIMONOS_DIR" ]]; then
    echo ""
    python3 "$BENCH_DIR/analyze-results.py" "$LOCAL_RESULTS" "$BASELINE_DIR" "$DAIMONOS_DIR"
fi
