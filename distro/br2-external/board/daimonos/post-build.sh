#!/bin/sh
set -eu

TARGET_DIR="$1"
BOARD_DIR="$(dirname "$0")"

# --- Serial console ---
if [ -e "${TARGET_DIR}/etc/inittab" ]; then
    grep -qE '^ttyS0::' "${TARGET_DIR}/etc/inittab" || \
        sed -i '/GENERIC_SERIAL/a\
ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100' "${TARGET_DIR}/etc/inittab"
fi

# --- GRUB config ---
mkdir -p "${TARGET_DIR}/boot/grub"
cp "${BOARD_DIR}/grub.cfg" "${TARGET_DIR}/boot/grub/grub.cfg"
cp "${TARGET_DIR}/lib/grub/i386-pc/boot.img" "${BINARIES_DIR}/"

# --- SSH config ---
install -d -m 755 "${TARGET_DIR}/etc/ssh"
cat > "${TARGET_DIR}/etc/ssh/sshd_config" <<'SSHD'
Port 22
AddressFamily any
ListenAddress 0.0.0.0
ListenAddress ::

HostKey /etc/ssh/ssh_host_ed25519_key
HostKey /etc/ssh/ssh_host_rsa_key

PermitRootLogin no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AuthorizedKeysFile .ssh/authorized_keys
LogLevel INFO

X11Forwarding no
PrintMotd no
AcceptEnv LANG LC_*
SSHD

# --- SSH init script (host key gen + start) ---
cat > "${TARGET_DIR}/etc/init.d/S50sshd" <<'INITSCRIPT'
#!/bin/sh

case "$1" in
    start)
        for type in ed25519 rsa; do
            keyfile="/etc/ssh/ssh_host_${type}_key"
            if [ ! -f "$keyfile" ]; then
                echo "Generating SSH ${type} host key..."
                ssh-keygen -t "$type" -f "$keyfile" -N ""
            fi
        done
        echo "Starting sshd..."
        /usr/sbin/sshd
        ;;
    stop)
        echo "Stopping sshd..."
        killall sshd 2>/dev/null
        ;;
    restart)
        "$0" stop
        "$0" start
        ;;
    *)
        echo "Usage: $0 {start|stop|restart}"
        exit 1
        ;;
esac
INITSCRIPT
chmod 755 "${TARGET_DIR}/etc/init.d/S50sshd"

# --- Register daimonos-shell as valid login shell ---
grep -q '/usr/bin/daimonos-shell' "${TARGET_DIR}/etc/shells" || \
    echo "/usr/bin/daimonos-shell" >> "${TARGET_DIR}/etc/shells"

# --- Ensure directories exist (ownership set by permissions table) ---
install -d -m 755 "${TARGET_DIR}/home/agent"
install -d -m 700 "${TARGET_DIR}/home/agent/.ssh"
install -d -m 755 "${TARGET_DIR}/home/agent/workspace"
install -d -m 755 "${TARGET_DIR}/etc/daimonos"

# --- Bench user directories (for on-instance benchmarking) ---
install -d -m 755 "${TARGET_DIR}/home/bench"
install -d -m 700 "${TARGET_DIR}/home/bench/.ssh"

# --- First-boot rootfs auto-expand (CLA-216) ---
cat > "${TARGET_DIR}/etc/init.d/S20resize" <<'INITSCRIPT'
#!/bin/sh

PHASE1="/var/lib/daimonos-resize-phase1"
PHASE2="/var/lib/daimonos-resize-done"

case "$1" in
    start)
        [ -f "$PHASE2" ] && exit 0

        # Resolve the actual root device
        ROOT_DEV=$(sed 's/.*root=\([^ ]*\).*/\1/' /proc/cmdline)
        [ -z "$ROOT_DEV" ] || [ ! -b "$ROOT_DEV" ] && exit 0

        DISK=$(echo "$ROOT_DEV" | sed 's/p\?[0-9]*$//')
        [ "$DISK" = "$ROOT_DEV" ] && exit 0

        if [ ! -f "$PHASE1" ]; then
            # Phase 1: check if resize is needed, expand partition, reboot
            DISK_SECTORS=$(blockdev --getsz "$DISK" 2>/dev/null)
            PART_SECTORS=$(blockdev --getsz "$ROOT_DEV" 2>/dev/null)
            [ -z "$DISK_SECTORS" ] || [ -z "$PART_SECTORS" ] && exit 0
            if [ "$PART_SECTORS" -ge $((DISK_SECTORS * 9 / 10)) ]; then
                mkdir -p /var/lib; touch "$PHASE2"; exit 0
            fi
            echo "Expanding root partition to fill disk..."
            echo ", +" | /sbin/sfdisk -f -N 1 "$DISK" 2>&1
            mkdir -p /var/lib
            touch "$PHASE1"
            echo "Rebooting to apply new partition table..."
            sync
            sleep 1
            echo b > /proc/sysrq-trigger
        else
            # Phase 2: expand filesystem (partition already resized, just grow fs)
            echo "Expanding root filesystem..."
            /sbin/resize2fs "$ROOT_DEV" 2>&1
            echo "Resize complete: $(df -h / | awk 'NR==2{print $2}')"
            mkdir -p /var/lib
            touch "$PHASE2"
        fi
        ;;
esac
INITSCRIPT
chmod 755 "${TARGET_DIR}/etc/init.d/S20resize"

# --- Swap setup (runs on first boot, creates swap file on disk) ---
cat > "${TARGET_DIR}/etc/init.d/S40swap" <<'INITSCRIPT'
#!/bin/sh

SWAPFILE="/swapfile"

case "$1" in
    start)
        if [ ! -f "$SWAPFILE" ]; then
            # Use 50% of free space, capped at 4 GB
            AVAIL_KB=$(df -k / | awk 'NR==2{print $4}')
            SWAP_KB=$((AVAIL_KB / 2))
            MAX_KB=$((4 * 1024 * 1024))
            [ "$SWAP_KB" -gt "$MAX_KB" ] && SWAP_KB=$MAX_KB
            [ "$SWAP_KB" -lt 65536 ] && exit 0
            SWAP_MB=$((SWAP_KB / 1024))
            echo "Creating ${SWAP_MB}MB swap file..."
            dd if=/dev/zero of="$SWAPFILE" bs=1M count="$SWAP_MB" 2>/dev/null
            chmod 600 "$SWAPFILE"
            mkswap "$SWAPFILE"
        fi
        echo "Enabling swap..."
        swapon "$SWAPFILE"
        ;;
    stop)
        swapoff "$SWAPFILE" 2>/dev/null
        ;;
    *)
        echo "Usage: $0 {start|stop}"
        exit 1
        ;;
esac
INITSCRIPT
chmod 755 "${TARGET_DIR}/etc/init.d/S40swap"

# --- Dynamic SSH key injection via IMDSv2 (CLA-201) ---
cat > "${TARGET_DIR}/etc/init.d/S45sshkeys" <<'INITSCRIPT'
#!/bin/sh

IMDS="http://169.254.169.254"

inject_keys() {
    local user="$1"
    local ssh_dir="/home/${user}/.ssh"
    local keys_file="${ssh_dir}/authorized_keys"

    mkdir -p "$ssh_dir"

    if [ -n "$IMDS_KEYS" ]; then
        printf '%s' "$IMDS_KEYS" > "${keys_file}.imds"
        if [ -f "$keys_file" ]; then
            while IFS= read -r line; do
                case "$line" in ""|\#*) continue ;; esac
                grep -qF "$line" "${keys_file}.imds" 2>/dev/null || \
                    echo "$line" >> "${keys_file}.imds"
            done < "$keys_file"
        fi
        mv "${keys_file}.imds" "$keys_file"
        chmod 600 "$keys_file"
        chown "${user}:${user}" "$keys_file"
    fi
}

case "$1" in
    start)
        TOKEN=""
        IMDS_KEYS=""
        if grep -qi amazon /sys/class/dmi/id/sys_vendor 2>/dev/null; then
            for i in 1 2 3 5 8; do
                TOKEN=$(curl -sf -X PUT -m 2 \
                    -H "X-aws-ec2-metadata-token-ttl-seconds: 60" \
                    "${IMDS}/latest/api/token" 2>/dev/null) && break
                sleep "$i"
            done
        fi

        if [ -n "$TOKEN" ]; then
            echo "Fetching SSH keys from EC2 IMDS..."
            KEY_IDS=$(curl -sf -m 2 \
                -H "X-aws-ec2-metadata-token: ${TOKEN}" \
                "${IMDS}/latest/meta-data/public-keys/" 2>/dev/null) || true

            for entry in $KEY_IDS; do
                idx=$(echo "$entry" | cut -d= -f1)
                key=$(curl -sf -m 2 \
                    -H "X-aws-ec2-metadata-token: ${TOKEN}" \
                    "${IMDS}/latest/meta-data/public-keys/${idx}/openssh-key" 2>/dev/null) || true
                [ -n "$key" ] && IMDS_KEYS="${IMDS_KEYS}${key}
"
            done
        fi

        inject_keys "agent"
        inject_keys "bench"

        AGENT_KEYS="/home/agent/.ssh/authorized_keys"
        if [ -f "$AGENT_KEYS" ] && [ -s "$AGENT_KEYS" ]; then
            echo "SSH keys: $(wc -l < "$AGENT_KEYS") key(s) installed (IMDS + static)"
        elif [ -n "$IMDS_KEYS" ]; then
            echo "SSH keys installed from IMDS"
        else
            echo "WARNING: No SSH keys available"
        fi
        ;;
esac
INITSCRIPT
chmod 755 "${TARGET_DIR}/etc/init.d/S45sshkeys"

# --- Git identity for agent user (required for git commit via MCP tool) ---
install -d -m 755 "${TARGET_DIR}/home/agent"
cat > "${TARGET_DIR}/home/agent/.gitconfig" <<'GITCFG'
[user]
	email = agent@daimonos.dev
	name = Daimonos Agent
GITCFG

# --- Git identity for bench user ---
install -d -m 755 "${TARGET_DIR}/home/bench"
cat > "${TARGET_DIR}/home/bench/.gitconfig" <<'GITCFG'
[user]
	email = bench@daimonos.dev
	name = Daimonos Bench
GITCFG

# --- Lock root ---
sed -i 's|^root:[^:]*:|root:!:|' "${TARGET_DIR}/etc/shadow" 2>/dev/null || true

echo "[daimonos] post-build complete"
