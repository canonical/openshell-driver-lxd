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

shopt -s nullglob
_rocks=(openshell-container-image_*.rock)
if [ "${#_rocks[@]}" -ne 1 ]; then
    echo "ERROR: expected exactly one openshell-container-image_*.rock, found ${#_rocks[@]}" >&2
    exit 1
fi
ROCK="${_rocks[0]}"

echo "==> extracting rock '$ROCK' to an OCI image layout"
mkdir oci-image
tar -xf "$ROCK" -C oci-image
TAG=$(jq -r '.manifests[0].annotations["org.opencontainers.image.ref.name"]' oci-image/index.json)

echo "==> umoci unpack (tag: $TAG)"
sudo umoci unpack --image "oci-image:$TAG" bundle

echo "==> lxd-convert"
LXD_CONVERT="$(command -v lxd-convert || true)"
if [ -z "$LXD_CONVERT" ]; then
    echo "ERROR: lxd-convert not found in PATH (required to convert the OCI bundle to an LXD image)" >&2
    exit 1
fi
sudo "$LXD_CONVERT" --type container \
    --source bundle/rootfs --name "$BUILD_INSTANCE" \
    --non-interactive

echo "==> lxc publish --alias $ALIAS"
lxc publish "$BUILD_INSTANCE" --alias "$ALIAS" \
    description="OpenShell sandbox container image"
lxc delete "$BUILD_INSTANCE"

rm -f "$ROCK"

echo "==> done"
lxc image list "$ALIAS"
