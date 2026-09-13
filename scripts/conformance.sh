#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Runs upstream OpenShell's conformance suite (`openshell-conformance`)
# against an OpenShell gateway backed by this driver, in the environment
# scripts/openshell-env.sh provides:
#
#   smoke               create, Ready, list, exec, delete
#
# The conformance runner is not released; it is built from the pinned
# OpenShell source revision.
#
# Usage: scripts/conformance.sh
#
# Environment (in addition to the OPENSHELL_TEST_* variables):
#   CONFORMANCE_SCENARIOS  space-separated scenarios (default: "smoke")

set -euo pipefail

# shellcheck source=scripts/openshell-env.sh
. "$(dirname "${BASH_SOURCE[0]}")/openshell-env.sh"

CONFORMANCE_REV="${OPENSHELL_SOURCE_REV}"

SCENARIOS="${CONFORMANCE_SCENARIOS:-smoke}"
CONFORMANCE_BIN="${CACHE_DIR}/conformance-${CONFORMANCE_REV}/bin/openshell-conformance"
SUITE_ARTIFACTS_DIR="${ARTIFACTS_DIR}/conformance"

build_conformance() {
    [ -x "$CONFORMANCE_BIN" ] && return
    fetch_openshell_source
    local src="${CACHE_DIR}/openshell-src-${OPENSHELL_SOURCE_REV}"
    log "building openshell-conformance at ${CONFORMANCE_REV}"
    # Upstream pins its own toolchain; the runner builds with ours, so a
    # rustup-managed cargo does not download a second toolchain for it.
    RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" cargo install --quiet --locked \
        --path "${src}/crates/openshell-conformance-cli" \
        --root "${CACHE_DIR}/conformance-${CONFORMANCE_REV}"
}

[ "$#" -eq 0 ] || die "usage: $0 (takes no arguments; see scripts/openshell-env.sh to manage the environment)"

require_tools
fetch_openshell
build_conformance

env_up_for_suite
mkdir -p "$SUITE_ARTIFACTS_DIR"
echo "conformance ${CONFORMANCE_REV}" >"${SUITE_ARTIFACTS_DIR}/versions.txt"

failed=()
for scenario in $SCENARIOS; do
    log "running ${scenario}"
    if cli_env "$CONFORMANCE_BIN" run \
        --openshell-bin "$CLI_BIN" \
        --output json \
        "$scenario" \
        </dev/null >"${SUITE_ARTIFACTS_DIR}/${scenario}.json" 2>"${SUITE_ARTIFACTS_DIR}/${scenario}.log"; then
        log "${scenario}: passed"
    else
        log "${scenario}: FAILED (see ${SUITE_ARTIFACTS_DIR}/${scenario}.json)"
        cat "${SUITE_ARTIFACTS_DIR}/${scenario}.json" >&2 || true
        failed+=("$scenario")
    fi
done

if [ "${#failed[@]}" -gt 0 ]; then
    die "conformance failed: ${failed[*]}; artifacts in ${ARTIFACTS_DIR}"
fi
log "all conformance scenarios passed"
