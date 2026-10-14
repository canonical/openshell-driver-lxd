#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Runs upstream OpenShell's conformance suite against an OpenShell gateway
# backed by this driver, in the environment scripts/openshell-env.sh provides.
#
# From OpenShell v0.1.4 the suite is a Cargo test tree at
# `tests/suites/conformance` in the pinned source, not the
# `openshell-conformance` runner binary the earlier releases shipped. Each
# test target drives the candidate CLI named by `OPENSHELL_BIN` against an
# already-registered gateway, so the suite is pointed at the pinned CLI and
# the environment's gateway:
#
#   smoke              create, inspect, exec in, delete a base sandbox
#   lifecycle          sandbox stop, start and deletion behaviour
#   policy_advisor     mechanistic/hostname proposal and local policy flows
#   file_transfer      upload/download round-trip, git filtering, path safety
#
# `cargo test -- --list` over the suite's manifest is the authority on what
# exists. The runner is built from the same pinned revision as everything else
# it drives.
#
# Usage: scripts/conformance.sh
#
# Environment (in addition to the OPENSHELL_TEST_* variables):
#   CONFORMANCE_TESTS  space-separated Cargo test targets (default: "smoke lifecycle")

set -euo pipefail

# shellcheck source=scripts/openshell-env.sh
. "$(dirname "${BASH_SOURCE[0]}")/openshell-env.sh"

# The conformance suite comes from the same revision as everything else it
# drives. It used to be pinned separately: `openshell-conformance` appeared
# after v0.0.116, so it had to be newer than the release under test, and then
# no newer than the last revision the v0.0.116 CLI could still drive. Building
# the gateway, CLI and suite from source removes that squeeze — there is one
# revision now, and it is the one the driver targets.
CONFORMANCE_SRC="${CACHE_DIR}/openshell-src-${OPENSHELL_SOURCE_REV}"
CONFORMANCE_MANIFEST="${CONFORMANCE_SRC}/tests/suites/conformance/Cargo.toml"
CONFORMANCE_TARGET_DIR="${CACHE_DIR}/conformance-${OPENSHELL_SOURCE_REV}"
SUITE_ARTIFACTS_DIR="${ARTIFACTS_DIR}/conformance"

TESTS="${CONFORMANCE_TESTS:-smoke lifecycle}"

[ "$#" -eq 0 ] || die "usage: $0 (takes no arguments; see scripts/openshell-env.sh to manage the environment)"

require_tools
fetch_openshell

[ -f "$CONFORMANCE_MANIFEST" ] ||
    die "conformance suite not found at ${CONFORMANCE_MANIFEST}; is ${OPENSHELL_VERSION} the pinned release?"

env_up_for_suite
mkdir -p "$SUITE_ARTIFACTS_DIR"
echo "conformance ${OPENSHELL_SOURCE_REV}" >"${SUITE_ARTIFACTS_DIR}/versions.txt"

# One test target per invocation, so a failure names itself and one target's
# leftovers cannot confuse the next.
failed=()
for test in $TESTS; do
    log "running ${test}"
    if cli_env \
        OPENSHELL_BIN="$CLI_BIN" \
        RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}" \
        CARGO_TARGET_DIR="$CONFORMANCE_TARGET_DIR" \
        cargo test --locked \
        --manifest-path "$CONFORMANCE_MANIFEST" \
        --package openshell-test-conformance-cli \
        --test "$test" \
        -- --test-threads=1 --nocapture \
        </dev/null >"${SUITE_ARTIFACTS_DIR}/${test}.log" 2>&1; then
        log "${test}: passed"
    else
        log "${test}: FAILED (see ${SUITE_ARTIFACTS_DIR}/${test}.log)"
        tail -n 100 "${SUITE_ARTIFACTS_DIR}/${test}.log" >&2 || true
        failed+=("$test")
    fi
done

if [ "${#failed[@]}" -gt 0 ]; then
    die "conformance failed: ${failed[*]}; artifacts in ${ARTIFACTS_DIR}"
fi
log "all conformance tests passed"
