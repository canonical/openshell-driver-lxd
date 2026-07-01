#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Builds the OpenShell sandbox container image from rockcraft.yaml and
# publishes it to the local LXD image store under an alias (default:
# openshell-sandbox, matching the driver's --default-image flag).
#
# Requires: rockcraft, umoci, lxd-convert, lxc, jq.

set -euo pipefail

ALIAS="${SANDBOX_IMAGE_ALIAS:-openshell-sandbox}"
BUILD_INSTANCE="openshell-sandbox-build"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

cd "$REPO_ROOT"

cleanup() {
    sudo rm -rf oci-image bundle
}
trap cleanup EXIT

# Best-effort cleanup of artifacts left behind by a previous, possibly
# failed, run.
lxc delete --force "$BUILD_INSTANCE" >/dev/null 2>&1 || true
lxc image delete "$ALIAS" >/dev/null 2>&1 || true
sudo rm -rf oci-image bundle ./openshell-container-image_*.rock

echo "==> rockcraft pack"
rockcraft pack --verbosity=brief

ROCK=$(ls openshell-container-image_*.rock | head -1)

echo "==> extracting rock '$ROCK' to an OCI image layout"
mkdir oci-image
tar -xf "$ROCK" -C oci-image
TAG=$(jq -r '.manifests[0].annotations["org.opencontainers.image.ref.name"]' oci-image/index.json)

echo "==> umoci unpack (tag: $TAG)"
sudo umoci unpack --image "oci-image:$TAG" bundle

echo "==> lxd-convert"
sudo "$(command -v lxd-convert)" --type container \
    --source bundle/rootfs --name "$BUILD_INSTANCE" \
    --non-interactive

echo "==> lxc publish --alias $ALIAS"
lxc publish "$BUILD_INSTANCE" --alias "$ALIAS" \
    description="OpenShell sandbox container image"
lxc delete "$BUILD_INSTANCE"

rm -f "$ROCK"

echo "==> done"
lxc image list "$ALIAS"
