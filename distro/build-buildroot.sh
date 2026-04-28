#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BR_DIR="$SCRIPT_DIR/buildroot"
BR_EXT="$SCRIPT_DIR/br2-external"
IMAGES_DIR="$BR_DIR/output/images"
BOARD_DIR="$BR_EXT/board/daimonos"

echo "=== daimonos distro builder (Buildroot) ==="

# Ensure static musl binary exists
DAIMONOS_BIN="$REPO_ROOT/target/x86_64-unknown-linux-musl/release/daimonos"
if [ ! -f "$DAIMONOS_BIN" ]; then
    echo "ERROR: daimonos binary not found at $DAIMONOS_BIN"
    echo "Run: cargo build --release --target x86_64-unknown-linux-musl"
    exit 1
fi

# Copy binary to overlay (strip to save ~5 MB)
cp "$DAIMONOS_BIN" "$BR_EXT/overlay/usr/bin/daimonos"
strip "$BR_EXT/overlay/usr/bin/daimonos"
chmod 755 "$BR_EXT/overlay/usr/bin/daimonos"

# Copy config to overlay
cp "$REPO_ROOT/daimonos.default.toml" "$BR_EXT/overlay/etc/daimonos/config.toml"

# Configure Buildroot (first time or when defconfig changes)
cd "$BR_DIR"

if [ ! -f "$BR_DIR/output/.config" ]; then
    echo "Configuring Buildroot..."
    make BR2_EXTERNAL="$BR_EXT" daimonos_defconfig
fi

echo "Building (this takes 20-40 minutes on first run)..."
# CLA-209: -j$(nproc) causes process explosion on multi-core machines
make BR2_EXTERNAL="$BR_EXT" -j4

# --- Generate dual images (CLA-215) ---
# Buildroot produces disk.img with QEMU grub.cfg (root=/dev/vda1).
# We create an AWS variant by swapping grub.cfg inside rootfs.ext4.

ROOTFS="$IMAGES_DIR/rootfs.ext4"

# QEMU image is what Buildroot just built
cp "$IMAGES_DIR/disk.img" "$IMAGES_DIR/disk-qemu.img"

# Create AWS image: swap grub.cfg in rootfs, regenerate disk
echo "Generating AWS image variant..."
debugfs -w -R "rm /boot/grub/grub.cfg" "$ROOTFS" 2>/dev/null
debugfs -w -R "write ${BOARD_DIR}/grub-aws.cfg /boot/grub/grub.cfg" "$ROOTFS" 2>/dev/null

rm -rf "$BR_DIR/output/build/genimage.tmp"
"$BR_DIR/output/host/bin/genimage" \
    --rootpath "$BR_DIR/output/target" \
    --tmppath "$BR_DIR/output/build/genimage.tmp" \
    --inputpath "$IMAGES_DIR" \
    --outputpath "$IMAGES_DIR" \
    --config "$BOARD_DIR/genimage.cfg"
mv "$IMAGES_DIR/disk.img" "$IMAGES_DIR/disk-aws.img"

# Restore QEMU grub.cfg in rootfs for subsequent builds
debugfs -w -R "rm /boot/grub/grub.cfg" "$ROOTFS" 2>/dev/null
debugfs -w -R "write ${BOARD_DIR}/grub.cfg /boot/grub/grub.cfg" "$ROOTFS" 2>/dev/null

echo ""
echo "=== Build complete ==="
ls -lh "$IMAGES_DIR/disk-qemu.img" "$IMAGES_DIR/disk-aws.img"
echo ""
echo "Test with QEMU:"
echo "  ./test-qemu.sh"
echo ""
echo "Deploy to AWS:"
echo "  ./deploy-aws.sh"
