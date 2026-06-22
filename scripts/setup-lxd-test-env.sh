#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Provisions the LXD environment lxd-client's integration tests
# (crates/lxd-client/tests/integration.rs) expect: an installed and
# initialized LXD, and a `lxd-client-test` image alias. Idempotent and safe to
# run locally before development and in CI before every test run.
set -euo pipefail

IMAGE_SOURCE="${LXD_CLIENT_TEST_IMAGE:-ubuntu-minimal-daily:24.04}"
IMAGE_ALIAS="lxd-client-test"

if ! snap list lxd >/dev/null 2>&1; then
    echo "Installing lxd snap..."
    sudo snap install lxd
fi

if ! lxc storage list -f csv 2>/dev/null | grep -q '^default,'; then
    echo "Initializing LXD..."
    sudo lxd init --auto
fi

if ! lxc image alias list local: -f csv 2>/dev/null | grep -q "^${IMAGE_ALIAS},"; then
    echo "Staging ${IMAGE_SOURCE} as image alias ${IMAGE_ALIAS}..."
    lxc image copy "${IMAGE_SOURCE}" local: --alias "${IMAGE_ALIAS}" --vm=false
fi
