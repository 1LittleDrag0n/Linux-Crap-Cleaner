#!/usr/bin/env bash
# Builds the AppImage inside an Ubuntu 24.04 container (podman or docker), so
# it runs on older distributions too. The result lands in dist/.
#
#   packaging/appimage/build-in-container.sh
#
# Set CONTAINER_ENGINE=docker to force docker when both are installed.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ENGINE="${CONTAINER_ENGINE:-$(command -v podman || command -v docker || true)}"
if [[ -z "$ENGINE" ]]; then
    echo "Install podman (recommended) or docker first." >&2
    exit 1
fi
IMAGE=lcc-appimage-builder

echo "==> Preparing the build container with $(basename "$ENGINE")"
"$ENGINE" build -t "$IMAGE" -f "$ROOT/packaging/appimage/Containerfile" "$ROOT/packaging/appimage"

echo "==> Building inside the container"
# A separate target folder keeps container builds apart from your normal
# ones; the cargo registry volume avoids downloading crates every time.
"$ENGINE" run --rm \
    -v "$ROOT:/src:Z" \
    -v lcc-cargo-registry:/root/.cargo/registry \
    -e CARGO_TARGET_DIR=/src/target/container \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    "$IMAGE" \
    bash -c 'packaging/appimage/build-appimage.sh \
        && chown -R "$HOST_UID:$HOST_GID" dist target/container'

ls -lh "$ROOT"/dist/*.AppImage
