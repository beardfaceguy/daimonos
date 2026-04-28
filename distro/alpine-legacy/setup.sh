#!/bin/sh
set -eu

# ── daimonos distro setup ──
# Runs inside chroot during image build.
# Goal: minimal Alpine with daimonos as the only agent interface via SSH.

echo "[daimonos] Configuring system..."

# ── Networking ──
cat > /etc/network/interfaces <<'NETCFG'
auto lo
iface lo inet loopback

auto eth0
iface eth0 inet dhcp
NETCFG

echo "daimonos" > /etc/hostname
echo "127.0.0.1 daimonos localhost" > /etc/hosts

# ── SSH: key-only auth, no passwords, no interactive shell ──
cat > /etc/ssh/sshd_config <<'SSHD'
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

# Subsystem: none (no sftp -- agents use daimonos file ops)
SSHD

ssh-keygen -A

# ── Agent user: daimonos as login shell ──
# Register daimonos shell wrapper as a valid shell
echo "/usr/bin/daimonos-shell" >> /etc/shells

# Create the agent user with daimonos-shell as its login shell
adduser -D -s /usr/bin/daimonos-shell -h /home/agent agent
# Unlock account — OpenSSH 9.9+ rejects pubkey auth for locked accounts
passwd -u agent 2>/dev/null || true
chown root:root /home
chmod 755 /home
mkdir -p /home/agent/.ssh /home/agent/workspace
chmod 755 /home/agent
chmod 700 /home/agent/.ssh
chown -R agent:agent /home/agent

# Fix ownership on authorized_keys (may come from rootfs overlay)
if [ -f /home/agent/.ssh/authorized_keys ]; then
    chmod 600 /home/agent/.ssh/authorized_keys
    chown agent:agent /home/agent/.ssh/authorized_keys
else
    touch /home/agent/.ssh/authorized_keys
    chmod 600 /home/agent/.ssh/authorized_keys
    chown agent:agent /home/agent/.ssh/authorized_keys
fi

# ── Git identity (required for git commit via MCP tool) ──
cat > /home/agent/.gitconfig <<'GITCFG'
[user]
	email = agent@daimonos.dev
	name = Daimonos Agent
GITCFG
chown agent:agent /home/agent/.gitconfig

# ── daimonos config ──
mkdir -p /etc/daimonos
if [ -f /etc/daimonos.toml ]; then
    mv /etc/daimonos.toml /etc/daimonos/config.toml
fi

# ── Serial console for QEMU/EC2 debugging ──
sed -i 's|^default_kernel_opts=".*"|default_kernel_opts="console=ttyS0,115200n8 console=tty0"|' /etc/update-extlinux.conf 2>/dev/null || true
if [ -f /etc/inittab ]; then
    grep -q ttyS0 /etc/inittab || echo "ttyS0::respawn:/sbin/getty -L ttyS0 115200 vt100" >> /etc/inittab
fi

# ── Boot services ──
rc-update add networking boot
rc-update add sshd default

# ── Harden: lock root ──
passwd -l root

# ── Kernel console params for serial output ──
if [ -f /boot/extlinux.conf ]; then
    sed -i 's|APPEND |APPEND console=ttyS0,115200n8 |' /boot/extlinux.conf
fi

# ── Cleanup ──
rm -rf /var/cache/apk/*

echo "[daimonos] Setup complete."
echo "  - SSH on port 22, key-only auth"
echo "  - User 'agent' with daimonos shell"
echo "  - Workspace at /home/agent/workspace"
