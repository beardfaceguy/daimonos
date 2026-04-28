#!/bin/sh
set -eu

BOARD_DIR="$(dirname "$0")"

# Generate disk image using genimage
rm -rf "${BUILD_DIR}/genimage.tmp"
"${HOST_DIR}/bin/genimage" \
    --rootpath "${TARGET_DIR}" \
    --tmppath "${BUILD_DIR}/genimage.tmp" \
    --inputpath "${BINARIES_DIR}" \
    --outputpath "${BINARIES_DIR}" \
    --config "${BOARD_DIR}/genimage.cfg"

echo "[daimonos] disk.img generated at ${BINARIES_DIR}/disk.img"
