#!/usr/bin/env bash
# Orchestrates a remote benchmark: launches two AWS instances (Ubuntu baseline
# vs. daimonos distro), provisions them, runs identical benchmark tasks, collects
# results, and tears down instances.
#
# Usage:
#   ./run-remote-benchmark.sh [--keep] [--skip-provision] [--task TASK_ID]
#
# Required environment:
#   ANTHROPIC_API_KEY   API key for the Claude CLI
#   DAIMONOS_AMI        AMI ID for the daimonos distro image
#
# Optional environment:
#   UBUNTU_AMI          AMI ID for Ubuntu (default: latest Ubuntu 24.04 in region)
#   AWS_PROFILE         AWS CLI profile (default: experimental-admin)
#   AWS_REGION          AWS region (default: us-east-1)
#   INSTANCE_TYPE       EC2 instance type (default: t3.medium)
#   KEY_NAME            EC2 key pair name (default: auto-detect)
#   SSH_KEY             Path to SSH private key (default: ~/.ssh/id_ed25519)
#   BENCH_MODEL         Model for Claude CLI (default: opus)
#   BENCH_TAG           Optional tag for run naming
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$BENCH_DIR/.." && pwd)"

PROFILE="${AWS_PROFILE:-experimental-admin}"
REGION="${AWS_REGION:-us-east-1}"
INSTANCE_TYPE="${INSTANCE_TYPE:-t3.medium}"
SSH_KEY="${SSH_KEY:-$HOME/.ssh/id_ed25519}"
MODEL="${BENCH_MODEL:-opus}"
TAG="${BENCH_TAG:-remote}"
SSH_OPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=10 -o LogLevel=ERROR"

KEEP_INSTANCES=false
SKIP_PROVISION=false
TASK_FILTER=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --keep) KEEP_INSTANCES=true; shift ;;
        --skip-provision) SKIP_PROVISION=true; shift ;;
        --task) TASK_FILTER="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Validate required env ──

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    API_KEY_FILE="$REPO_ROOT/claude_api_key.env"
    if [[ -f "$API_KEY_FILE" ]]; then
        # shellcheck disable=SC1090
        source "$API_KEY_FILE"
        export ANTHROPIC_API_KEY
    else
        echo "Error: ANTHROPIC_API_KEY not set and $API_KEY_FILE not found"
        exit 1
    fi
fi

if [[ -z "${DAIMONOS_AMI:-}" ]]; then
    echo "Error: DAIMONOS_AMI must be set to the daimonos distro AMI ID"
    echo "  Build and deploy with: cd distro && ./build-buildroot.sh && ./deploy-aws.sh"
    exit 1
fi

# Auto-detect Ubuntu 24.04 AMI if not set
if [[ -z "${UBUNTU_AMI:-}" ]]; then
    echo "Looking up latest Ubuntu 24.04 AMI..."
    UBUNTU_AMI=$(aws ec2 describe-images \
        --owners 099720109477 \
        --filters "Name=name,Values=ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*" \
                  "Name=state,Values=available" \
        --query 'sort_by(Images, &CreationDate)[-1].ImageId' \
        --output text \
        --profile "$PROFILE" \
        --region "$REGION")
    echo "  Ubuntu AMI: $UBUNTU_AMI"
fi

# Auto-detect key pair
if [[ -z "${KEY_NAME:-}" ]]; then
    KEY_NAME=$(aws ec2 describe-key-pairs \
        --query 'KeyPairs[0].KeyName' \
        --output text \
        --profile "$PROFILE" \
        --region "$REGION" 2>/dev/null || true)
    if [[ -z "$KEY_NAME" || "$KEY_NAME" == "None" ]]; then
        echo "Error: No EC2 key pair found. Set KEY_NAME env var."
        exit 1
    fi
    echo "  Using key pair: $KEY_NAME"
fi

echo ""
echo "=== Remote Benchmark ==="
echo "Profile:       $PROFILE"
echo "Region:        $REGION"
echo "Instance type: $INSTANCE_TYPE"
echo "Ubuntu AMI:    $UBUNTU_AMI"
echo "Daimonos AMI:  $DAIMONOS_AMI"
echo "Key pair:      $KEY_NAME"
echo "Model:         $MODEL"
echo "Tag:           $TAG"
echo ""

# ── Ensure security group ──

SG_NAME="daimonos-bench-sg"
SG_ID=$(aws ec2 describe-security-groups \
    --filters "Name=group-name,Values=$SG_NAME" \
    --query 'SecurityGroups[0].GroupId' \
    --output text \
    --profile "$PROFILE" \
    --region "$REGION" 2>/dev/null || true)

if [[ -z "$SG_ID" || "$SG_ID" == "None" ]]; then
    echo "Creating security group $SG_NAME..."
    SG_ID=$(aws ec2 create-security-group \
        --group-name "$SG_NAME" \
        --description "Daimonos benchmark instances" \
        --output text \
        --query 'GroupId' \
        --profile "$PROFILE" \
        --region "$REGION")
    aws ec2 authorize-security-group-ingress \
        --group-id "$SG_ID" \
        --protocol tcp --port 22 --cidr 0.0.0.0/0 \
        --profile "$PROFILE" \
        --region "$REGION"
    echo "  Security group: $SG_ID"
fi

# ── Launch instances ──

launch_instance() {
    local ami="$1"
    local name="$2"
    aws ec2 run-instances \
        --image-id "$ami" \
        --instance-type "$INSTANCE_TYPE" \
        --key-name "$KEY_NAME" \
        --security-group-ids "$SG_ID" \
        --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$name},{Key=Project,Value=daimonos-bench}]" \
        --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":20,"VolumeType":"gp3","DeleteOnTermination":true}}]' \
        --query 'Instances[0].InstanceId' \
        --output text \
        --profile "$PROFILE" \
        --region "$REGION"
}

echo "Launching instances..."
UBUNTU_ID=$(launch_instance "$UBUNTU_AMI" "daimonos-bench-baseline")
DAIMONOS_ID=$(launch_instance "$DAIMONOS_AMI" "daimonos-bench-daimonos")
echo "  Ubuntu instance:   $UBUNTU_ID"
echo "  Daimonos instance: $DAIMONOS_ID"

cleanup() {
    if [[ "$KEEP_INSTANCES" == "true" ]]; then
        echo ""
        echo "Instances kept alive (--keep). Terminate manually:"
        echo "  aws ec2 terminate-instances --instance-ids $UBUNTU_ID $DAIMONOS_ID --profile $PROFILE --region $REGION"
        return
    fi
    echo ""
    echo "Terminating instances..."
    aws ec2 terminate-instances \
        --instance-ids "$UBUNTU_ID" "$DAIMONOS_ID" \
        --profile "$PROFILE" \
        --region "$REGION" >/dev/null 2>&1 || true
    echo "  Terminated."
}
trap cleanup EXIT

# ── Wait for instances to be running ──

echo "Waiting for instances to start..."
aws ec2 wait instance-running \
    --instance-ids "$UBUNTU_ID" "$DAIMONOS_ID" \
    --profile "$PROFILE" \
    --region "$REGION"

UBUNTU_IP=$(aws ec2 describe-instances \
    --instance-ids "$UBUNTU_ID" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' \
    --output text \
    --profile "$PROFILE" \
    --region "$REGION")

DAIMONOS_IP=$(aws ec2 describe-instances \
    --instance-ids "$DAIMONOS_ID" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' \
    --output text \
    --profile "$PROFILE" \
    --region "$REGION")

echo "  Ubuntu IP:   $UBUNTU_IP"
echo "  Daimonos IP: $DAIMONOS_IP"

# ── Wait for SSH ──

wait_ssh() {
    local host="$1"
    local user="$2"
    local max_attempts=30
    echo "  Waiting for SSH on $user@$host..."
    for i in $(seq 1 $max_attempts); do
        if ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" "echo ok" >/dev/null 2>&1; then
            echo "  SSH ready after $i attempts"
            return 0
        fi
        sleep 10
    done
    echo "  ERROR: SSH not ready after $max_attempts attempts"
    return 1
}

wait_ssh "$UBUNTU_IP" "ubuntu" &
WAIT_UBUNTU=$!
wait_ssh "$DAIMONOS_IP" "bench" &
WAIT_DAIMONOS=$!
wait $WAIT_UBUNTU
wait $WAIT_DAIMONOS

# ── Provision instances ──

if [[ "$SKIP_PROVISION" != "true" ]]; then
    echo ""
    echo "=== Provisioning instances ==="

    # Upload provision scripts, benchmark runner, tasks, and workspace
    # Upload a local directory's contents to a remote path via ssh+tar
    # Works with BusyBox tar (no SFTP subsystem needed)
    ssh_upload_dir() {
        local host="$1" user="$2" local_dir="$3" remote_dir="$4"
        tar -cf - -C "$local_dir" . | \
            ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" "mkdir -p $remote_dir && tar -xf - -C $remote_dir"
    }

    # Upload a single file to a remote path via ssh+cat
    ssh_upload_file() {
        local host="$1" user="$2" local_file="$3" remote_path="$4"
        ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" "cat > $remote_path" < "$local_file"
    }

    provision_instance() {
        local host="$1"
        local user="$2"
        local provision_script="$3"

        echo "  Uploading files to $user@$host..."

        # Create remote directory structure
        ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" "mkdir -p ~/benchmark/tasks ~/benchmark/workspace"

        # Upload individual files via cat
        ssh_upload_file "$host" "$user" "$provision_script" "~/benchmark/provision.sh"
        ssh_upload_file "$host" "$user" "$BENCH_DIR/run-benchmark.sh" "~/benchmark/run-benchmark.sh"
        ssh_upload_file "$host" "$user" "$BENCH_DIR/analyze-results.py" "~/benchmark/analyze-results.py"

        # Upload task definitions
        ssh_upload_dir "$host" "$user" "$BENCH_DIR/tasks" "~/benchmark/tasks"

        # Upload workspace (tar to preserve git; pipe gzip for BusyBox compat)
        if [[ -d "$BENCH_DIR/workspace" ]]; then
            tar -cf - -C "$BENCH_DIR" workspace/ | gzip | \
                ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" "cd ~/benchmark && gzip -d | tar -xf -"
        fi

        echo "  Running provision script on $user@$host..."
        ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" "chmod +x ~/benchmark/provision.sh && ~/benchmark/provision.sh"
    }

    provision_instance "$UBUNTU_IP" "ubuntu" "$SCRIPT_DIR/provision-ubuntu.sh" &
    PROV_UBUNTU=$!
    provision_instance "$DAIMONOS_IP" "bench" "$SCRIPT_DIR/provision-daimonos.sh" &
    PROV_DAIMONOS=$!

    echo "  Provisioning in parallel..."
    wait $PROV_UBUNTU
    echo "  Ubuntu provisioning complete."
    wait $PROV_DAIMONOS
    echo "  Daimonos provisioning complete."

    # Set up daimonos MCP config on daimonos instance
    echo "  Configuring daimonos MCP..."
    ssh $SSH_OPTS -i "$SSH_KEY" "bench@$DAIMONOS_IP" sh -s <<'REMOTE_SETUP'
WORKSPACE="$HOME/benchmark/workspace"
mkdir -p "$WORKSPACE/.cursor"
cat > "$WORKSPACE/.cursor/mcp.json" <<MCPJSON
{
  "mcpServers": {
    "daimonos": {
      "command": "/usr/bin/daimonos",
      "args": ["--mcp", "-w", "$WORKSPACE"]
    }
  }
}
MCPJSON
REMOTE_SETUP
fi

# ── Run benchmarks ──

echo ""
echo "=== Running benchmarks ==="

TIMESTAMP="$(date +%Y%m%d-%H%M%S)"

run_on_instance() {
    local host="$1"
    local user="$2"
    local mode="$3"

    TASK_ARG=""
    if [[ -n "$TASK_FILTER" ]]; then
        TASK_ARG="$TASK_FILTER"
    fi

    echo "  Starting $mode benchmark on $user@$host..."
    ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" sh -s <<REMOTE_RUN
export PATH="\$HOME/.cargo/bin:\$HOME/.local/bin:/usr/local/bin:\$PATH"
export ANTHROPIC_API_KEY="$ANTHROPIC_API_KEY"
export BENCH_MODEL="$MODEL"
export BENCH_TAG="$TAG"
export CLAUDE_BIN="\$(command -v claude)"
$(if [[ "$mode" == "daimonos" ]]; then echo 'export DAIMONOS_BIN="/usr/bin/daimonos"'; fi)
cd ~/benchmark
chmod +x run-benchmark.sh
./run-benchmark.sh "$mode" $TASK_ARG
REMOTE_RUN
}

run_on_instance "$UBUNTU_IP" "ubuntu" "baseline" &
RUN_UBUNTU=$!
run_on_instance "$DAIMONOS_IP" "bench" "daimonos" &
RUN_DAIMONOS=$!

echo "  Benchmarks running in parallel..."

FAILED=0
wait $RUN_UBUNTU || { echo "  WARNING: Ubuntu baseline benchmark had errors"; FAILED=1; }
echo "  Baseline complete."
wait $RUN_DAIMONOS || { echo "  WARNING: Daimonos benchmark had errors"; FAILED=1; }
echo "  Daimonos complete."

# ── Collect results ──

echo ""
echo "=== Collecting results ==="

LOCAL_RESULTS="$BENCH_DIR/results"
mkdir -p "$LOCAL_RESULTS"

collect_results() {
    local host="$1"
    local user="$2"
    local mode="$3"

    LATEST_RUN=$(ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" \
        "ls -1d ~/benchmark/results/*-${mode}* 2>/dev/null | sort | tail -1 | xargs basename")

    if [[ -z "$LATEST_RUN" ]]; then
        echo "  WARNING: No results found on $host for $mode"
        return 1
    fi

    LOCAL_RUN_DIR="$LOCAL_RESULTS/remote-$LATEST_RUN"
    mkdir -p "$LOCAL_RUN_DIR"
    ssh $SSH_OPTS -i "$SSH_KEY" "$user@$host" \
        "tar -cf - -C ~/benchmark/results/$LATEST_RUN ." | \
        tar -xf - -C "$LOCAL_RUN_DIR/"

    echo "  Collected $mode results -> $LOCAL_RUN_DIR"
}

collect_results "$UBUNTU_IP" "ubuntu" "baseline"
collect_results "$DAIMONOS_IP" "bench" "daimonos"

# ── Analyze ──

echo ""
echo "=== Analysis ==="

# Find the remote-tagged result dirs
BASELINE_DIR=$(ls -1d "$LOCAL_RESULTS"/remote-*-baseline* 2>/dev/null | sort | tail -1 | xargs basename 2>/dev/null || true)
DAIMONOS_DIR=$(ls -1d "$LOCAL_RESULTS"/remote-*-daimonos* 2>/dev/null | sort | tail -1 | xargs basename 2>/dev/null || true)

if [[ -n "$BASELINE_DIR" && -n "$DAIMONOS_DIR" ]]; then
    python3 "$BENCH_DIR/analyze-results.py" "$LOCAL_RESULTS" "$BASELINE_DIR" "$DAIMONOS_DIR"
else
    echo "Could not find both result sets for analysis."
    echo "  Baseline: ${BASELINE_DIR:-not found}"
    echo "  Daimonos: ${DAIMONOS_DIR:-not found}"
fi

echo ""
echo "=== Remote benchmark complete ==="
echo "Results: $LOCAL_RESULTS/"
[[ $FAILED -ne 0 ]] && echo "WARNING: Some benchmarks had errors — check raw output."
exit $FAILED
