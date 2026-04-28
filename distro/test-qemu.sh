#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SSH_PORT="${1:-2222}"

# Prefer Buildroot QEMU image, fall back to legacy
BR_IMAGE="$SCRIPT_DIR/buildroot/output/images/disk-qemu.img"
ALPINE_IMAGE="$SCRIPT_DIR/build/daimonos.raw"

if [ -f "$BR_IMAGE" ]; then
    IMAGE="$BR_IMAGE"
elif [ -f "$ALPINE_IMAGE" ]; then
    IMAGE="$ALPINE_IMAGE"
    echo "WARNING: Using legacy Alpine image. Run build-buildroot.sh for the current image."
else
    echo "ERROR: No image found."
    echo "  Buildroot: $BR_IMAGE  (run ./build-buildroot.sh)"
    echo "  Alpine:    $ALPINE_IMAGE  (run ./build.sh)"
    exit 1
fi

WORK_IMAGE="$SCRIPT_DIR/build/daimonos-test.raw"
mkdir -p "$SCRIPT_DIR/build"
cp "$IMAGE" "$WORK_IMAGE"

echo "=== daimonos QEMU test ==="
echo "Image: $IMAGE"
echo "SSH:   localhost:$SSH_PORT"
echo ""
echo "Connect with:"
echo "  ssh -p $SSH_PORT -o StrictHostKeyChecking=no -i ~/.ssh/id_ed25519 agent@localhost"
echo ""
echo "Press Ctrl-A X to quit QEMU"
echo ""

qemu-system-x86_64 \
    -m 512M \
    -cpu host \
    -enable-kvm \
    -nographic \
    -drive file="$WORK_IMAGE",format=raw,if=virtio \
    -netdev user,id=net0,hostfwd=tcp::${SSH_PORT}-:22 \
    -device virtio-net-pci,netdev=net0 \
    -serial mon:stdio
