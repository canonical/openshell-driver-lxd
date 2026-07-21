#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Provisions the LXD environment lxd-client's integration tests
# (crates/lxd-client/tests/integration.rs) expect: an installed and
# initialized LXD, and a `lxd-client-test` image alias. Idempotent and safe to
# run locally before development and in CI before every test run.
#
# Must run as root.
set -euo pipefail

if [ "$(id -u)" != "0" ]; then
    echo "This script must be run as root." >&2
    exit 1
fi

IMAGE_SOURCE="${LXD_CLIENT_TEST_IMAGE:-ubuntu-minimal-daily:26.04}"
IMAGE_ALIAS="lxd-client-test"

if ! snap list lxd >/dev/null 2>&1; then
    echo "Installing lxd snap..."
    snap install lxd --channel=latest/edge
fi

if ! lxc storage show default >/dev/null 2>&1; then
    echo "Initializing LXD..."
    lxd init --auto
fi

if ! lxc network show lxdbr0 >/dev/null 2>&1; then
    echo "Creating lxdbr0 network..."
    lxc network create lxdbr0
fi

if ! lxc image info "local:${IMAGE_ALIAS}" >/dev/null 2>&1; then
    echo "Staging ${IMAGE_SOURCE} as image alias ${IMAGE_ALIAS}..."
    lxc image copy "${IMAGE_SOURCE}" local: --alias "${IMAGE_ALIAS}"
fi
