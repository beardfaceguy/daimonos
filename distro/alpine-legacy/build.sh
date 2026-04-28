#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BUILD_DIR="$SCRIPT_DIR/build"
OUTPUT="$BUILD_DIR/daimonos.raw"
IMAGE_SIZE=1G
ALPINE_BRANCH=v3.21

echo "=== daimonos distro builder ==="
echo "Output: $OUTPUT"
echo "Alpine: $ALPINE_BRANCH"
echo ""

if [ "$(id -u)" -ne 0 ]; then
    echo "Re-running with sudo..."
    exec sudo "$0" "$@"
fi

mkdir -p "$BUILD_DIR"

DAIMONOS_BIN="$REPO_ROOT/target/x86_64-unknown-linux-musl/release/daimonos"
if [ ! -f "$DAIMONOS_BIN" ]; then
    echo "ERROR: daimonos binary not found at $DAIMONOS_BIN"
    echo "Run: cargo build --release --target x86_64-unknown-linux-musl"
    exit 1
fi

cp "$DAIMONOS_BIN" "$SCRIPT_DIR/rootfs-overlay/usr/bin/daimonos"
strip "$SCRIPT_DIR/rootfs-overlay/usr/bin/daimonos"
chmod 755 "$SCRIPT_DIR/rootfs-overlay/usr/bin/daimonos"

cp "$REPO_ROOT/daimonos.default.toml" "$SCRIPT_DIR/rootfs-overlay/etc/daimonos.toml"

if [ ! -f "$BUILD_DIR/alpine-make-vm-image" ]; then
    echo "Downloading alpine-make-vm-image..."
    curl -fsSL -o "$BUILD_DIR/alpine-make-vm-image" \
        "https://raw.githubusercontent.com/alpinelinux/alpine-make-vm-image/master/alpine-make-vm-image"
    chmod +x "$BUILD_DIR/alpine-make-vm-image"
fi

echo "Building image..."
"$BUILD_DIR/alpine-make-vm-image" \
    --image-format raw \
    --image-size "$IMAGE_SIZE" \
    --branch "$ALPINE_BRANCH" \
    --packages "openssh-server git" \
    --fs-skel-dir "$SCRIPT_DIR/rootfs-overlay" \
    --script-chroot \
    --partition \
    "$OUTPUT" -- "$SCRIPT_DIR/setup.sh"

chown "$(logname):$(logname)" "$OUTPUT" 2>/dev/null || true

echo ""
echo "=== Build complete ==="
ls -lh "$OUTPUT"
echo ""
echo "Test with QEMU:"
echo "  $SCRIPT_DIR/test-qemu.sh"
echo ""
echo "Deploy to AWS:"
echo "  $SCRIPT_DIR/deploy-aws.sh"
