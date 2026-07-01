#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2025-2026 Kadin Sayani
# SPDX-License-Identifier: AGPL-3.0-or-later

# Run the upstream OpenShell e2e tests against a gateway backed by
# openshell-driver-lxd. Tests are driver-agnostic and live in the OpenShell
# repository; this script wires up the LXD-specific runtime environment.
#
# Usage:
#   OPENSHELL_REPO=/path/to/OpenShell scripts/e2e-lxd.sh
#
# The script defaults OPENSHELL_REPO to a sibling directory named OpenShell,
# matching the recommended local dev layout:
#   ~/git/openshell-driver-lxd/   (this repo)
#   ~/git/OpenShell/              (NVIDIA/OpenShell clone)
#
# Tests run (in order):
#   1. smoke              — create sandbox, exec command, assert output, delete
#   2. sandbox_lifecycle  — --no-keep flag, list polling
#   3. sandbox_labels     — create with labels, selector filtering, get, delete
#
# Env vars:
#   OPENSHELL_REPO              Path to NVIDIA/OpenShell checkout (required if
#                               not a sibling directory)
#   OPENSHELL_E2E_LXD_TEST      Which test to run: smoke | lifecycle | labels | all
#                               Defaults to "all"
#   OPENSHELL_PROVISION_TIMEOUT Sandbox provision timeout in seconds (default 120)
#   GATEWAY_READY_TIMEOUT       Seconds to wait for gateway readiness (default 60)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
OPENSHELL_REPO="${OPENSHELL_REPO:-$(cd "${ROOT}/../OpenShell" && pwd)}"

source "${OPENSHELL_REPO}/e2e/support/gateway-common.sh"

E2E_TEST="${OPENSHELL_E2E_LXD_TEST:-all}"
GATEWAY_READY_TIMEOUT="${GATEWAY_READY_TIMEOUT:-60}"
SANDBOX_PROVISION_TIMEOUT="${OPENSHELL_PROVISION_TIMEOUT:-120}"

# ── Build binaries ───────────────────────────────────────────────────

echo "==> Building openshell-gateway and openshell-cli from ${OPENSHELL_REPO}"
GATEWAY_TARGET_DIR="${OPENSHELL_REPO}/target"
(
  cd "${OPENSHELL_REPO}"
  cargo build -p openshell-server --bin openshell-gateway
  cargo build -p openshell-cli
)
GATEWAY_BIN="${GATEWAY_TARGET_DIR}/debug/openshell-gateway"
CLI_BIN="${GATEWAY_TARGET_DIR}/debug/openshell"

if [ ! -x "${GATEWAY_BIN}" ]; then
  echo "ERROR: expected gateway binary at ${GATEWAY_BIN}" >&2
  exit 1
fi

echo "==> Building openshell-driver-lxd from ${ROOT}"
cargo build --manifest-path "${ROOT}/Cargo.toml" -p openshell-driver-lxd
DRIVER_BIN="${ROOT}/target/debug/openshell-driver-lxd"

# ── Per-run state ────────────────────────────────────────────────────

HOST_PORT="$(e2e_pick_port)"

# Keep the path short — AF_UNIX SUN_LEN is 108 bytes on Linux.
RUN_STATE_DIR="$(mktemp -d /tmp/e2e-lxd.XXXXXX)"
DRIVER_SOCK="${RUN_STATE_DIR}/driver.sock"
DRIVER_LOG="${RUN_STATE_DIR}/driver.log"
GATEWAY_LOG="${RUN_STATE_DIR}/gateway.log"
GATEWAY_CONFIG="${RUN_STATE_DIR}/gateway.toml"
JWT_DIR="${RUN_STATE_DIR}/jwt"
GATEWAY_NAME="openshell-e2e-lxd-${HOST_PORT}"

export XDG_CONFIG_HOME="${RUN_STATE_DIR}/config"
export XDG_DATA_HOME="${RUN_STATE_DIR}/data"

# ── Cleanup (trap) ───────────────────────────────────────────────────

cleanup() {
  local exit_code=$?

  if [ -n "${GATEWAY_PID:-}" ] && kill -0 "${GATEWAY_PID}" 2>/dev/null; then
    echo "Stopping openshell-gateway (pid ${GATEWAY_PID})..."
    kill -TERM "${GATEWAY_PID}" 2>/dev/null || true
    for _ in 1 2 3 4 5; do
      kill -0 "${GATEWAY_PID}" 2>/dev/null || break
      sleep 0.5
    done
    kill -KILL "${GATEWAY_PID}" 2>/dev/null || true
    wait "${GATEWAY_PID}" 2>/dev/null || true
  fi

  if [ -n "${DRIVER_PID:-}" ] && kill -0 "${DRIVER_PID}" 2>/dev/null; then
    echo "Stopping openshell-driver-lxd (pid ${DRIVER_PID})..."
    kill -TERM "${DRIVER_PID}" 2>/dev/null || true
    wait "${DRIVER_PID}" 2>/dev/null || true
  fi

  if [ "${exit_code}" -ne 0 ]; then
    echo "=== gateway log ==="
    cat "${GATEWAY_LOG}" 2>/dev/null || true
    echo "=== end gateway log ==="
    echo "=== driver log ==="
    cat "${DRIVER_LOG}" 2>/dev/null || true
    echo "=== end driver log ==="
    echo "NOTE: preserving ${RUN_STATE_DIR} for debugging"
  else
    rm -rf "${RUN_STATE_DIR}" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# ── Generate JWT signing key ─────────────────────────────────────────

e2e_generate_gateway_jwt "${JWT_DIR}"

# ── Write gateway config ─────────────────────────────────────────────

cat >"${GATEWAY_CONFIG}" <<EOF
[openshell]
version = 1

[openshell.gateway.gateway_jwt]
signing_key_path = "${JWT_DIR}/signing.pem"
public_key_path = "${JWT_DIR}/public.pem"
kid_path = "${JWT_DIR}/kid"
gateway_id = "${GATEWAY_NAME}"
# Local e2e: sandbox JWTs do not expire.
ttl_secs = 0

[openshell.gateway.auth]
# Skip user-facing auth so the CLI and e2e harness don't need bearer tokens.
allow_unauthenticated_users = true
EOF

# ── Start driver ─────────────────────────────────────────────────────

echo "==> Starting openshell-driver-lxd (socket: ${DRIVER_SOCK})"
"${DRIVER_BIN}" \
  --socket "${DRIVER_SOCK}" \
  --gateway-grpc-port "${HOST_PORT}" \
  >"${DRIVER_LOG}" 2>&1 &
DRIVER_PID=$!

echo "==> Waiting for driver socket"
elapsed=0
while [ ! -S "${DRIVER_SOCK}" ]; do
  if ! kill -0 "${DRIVER_PID}" 2>/dev/null; then
    echo "ERROR: openshell-driver-lxd exited before socket appeared"
    exit 1
  fi
  if [ "${elapsed}" -ge 10 ]; then
    echo "ERROR: driver socket did not appear after 10s"
    exit 1
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done
echo "==> Driver ready (${elapsed}s)"

# ── Start gateway ─────────────────────────────────────────────────────
#
# Bind on 0.0.0.0 so LXD containers can reach the gateway via the bridge
# host IP injected into each sandbox as OPENSHELL_ENDPOINT.

echo "==> Starting openshell-gateway on 0.0.0.0:${HOST_PORT}"
"${GATEWAY_BIN}" \
  --bind-address 0.0.0.0 \
  --port "${HOST_PORT}" \
  --drivers lxd \
  --compute-driver-socket "${DRIVER_SOCK}" \
  --disable-tls \
  --config "${GATEWAY_CONFIG}" \
  --db-url "sqlite://${RUN_STATE_DIR}/gateway.db" \
  >"${GATEWAY_LOG}" 2>&1 &
GATEWAY_PID=$!

echo "==> Waiting for gateway readiness (timeout ${GATEWAY_READY_TIMEOUT}s)"
elapsed=0
while ! grep -q 'Server listening' "${GATEWAY_LOG}" 2>/dev/null; do
  if ! kill -0 "${GATEWAY_PID}" 2>/dev/null; then
    echo "ERROR: openshell-gateway exited before becoming ready"
    exit 1
  fi
  if [ "${elapsed}" -ge "${GATEWAY_READY_TIMEOUT}" ]; then
    echo "ERROR: openshell-gateway did not become ready after ${GATEWAY_READY_TIMEOUT}s"
    exit 1
  fi
  sleep 1
  elapsed=$((elapsed + 1))
done
echo "==> Gateway ready (${elapsed}s)"

# ── Register CLI gateway config ──────────────────────────────────────

CLI_GATEWAY_ENDPOINT="http://127.0.0.1:${HOST_PORT}"
e2e_register_plaintext_gateway \
  "${XDG_CONFIG_HOME}" \
  "${GATEWAY_NAME}" \
  "${CLI_GATEWAY_ENDPOINT}" \
  "${HOST_PORT}"

export OPENSHELL_GATEWAY_ENDPOINT="${CLI_GATEWAY_ENDPOINT}"
export OPENSHELL_E2E_DRIVER="lxd"
export OPENSHELL_PROVISION_TIMEOUT="${SANDBOX_PROVISION_TIMEOUT}"
export PATH="${GATEWAY_TARGET_DIR}/debug:${PATH}"

echo "==> Config: endpoint=${OPENSHELL_GATEWAY_ENDPOINT} provision_timeout=${SANDBOX_PROVISION_TIMEOUT}s"

# ── Run tests ─────────────────────────────────────────────────────────

run_test() {
  local test_name="$1"
  echo "==> Running e2e test: ${test_name}"
  cargo test \
    --manifest-path "${OPENSHELL_REPO}/e2e/rust/Cargo.toml" \
    --features e2e \
    --test "${test_name}" \
    -- --nocapture
  echo "==> ${test_name} passed."
}

case "${E2E_TEST}" in
  smoke)     run_test smoke ;;
  lifecycle) run_test sandbox_lifecycle ;;
  labels)    run_test sandbox_labels ;;
  all)
    run_test smoke
    run_test sandbox_lifecycle
    run_test sandbox_labels
    ;;
  *)
    echo "ERROR: unknown OPENSHELL_E2E_LXD_TEST value '${E2E_TEST}'" \
         "(expected: smoke | lifecycle | labels | all)" >&2
    exit 1
    ;;
esac
