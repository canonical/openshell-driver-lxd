#!/bin/bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Container-adapted init wrapper for OpenShell sandboxes.
#
# Runs as PID 1 inside an LXD container. Performs the minimal setup steps
# that carry over from upstream's openshell-vm-sandbox-init.sh, then
# exec-replaces itself with the openshell-sandbox supervisor so the
# supervisor becomes PID 1 (and handles zombie reaping by construction).
#
# VM-specific steps intentionally omitted: filesystem mounts, overlayfs
# assembly, GPU driver loading, TAP/gvproxy network bring-up. LXD provides
# all of those before container PID 1 runs.

set -euo pipefail

SUPERVISOR="/opt/openshell/bin/openshell-sandbox"

ts() {
    printf "[container-init] %s\n" "$*"
}

# ---------------------------------------------------------------------------
# 1. Create /sandbox and set ownership to the sandbox user (UID/GID 10001,
#    matching upstream's convention).
# ---------------------------------------------------------------------------
mkdir -p /sandbox
if ! chown 10001:10001 /sandbox 2>/dev/null; then
    ts "WARN: chown /sandbox to 10001:10001 failed (user may not exist yet)"
fi
chmod 0755 /sandbox

# ---------------------------------------------------------------------------
# 2. Set container hostname from OPENSHELL_SANDBOX_ID (or fall back to the
#    kernel-assigned name, which LXD sets from the instance name).
# ---------------------------------------------------------------------------
if [ -n "${OPENSHELL_SANDBOX_ID:-}" ]; then
    hostname "${OPENSHELL_SANDBOX_ID}" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# 3. Source any injected environment file.
# ---------------------------------------------------------------------------
if [ -f /srv/openshell-env.sh ]; then
    # shellcheck source=/dev/null
    source /srv/openshell-env.sh
fi

# ---------------------------------------------------------------------------
# 4. Seed /etc/hosts with host.openshell.internal → the lxdbr0 host-side IP.
#
#    OPENSHELL_ENDPOINT is injected by the driver at create time as
#    http://<host-lxdbr0-ip>:<port>/. Parse the hostname/IP from it so the
#    supervisor can reach the gateway even if in-container DNS differs.
#    Fall back to the default gateway IP (always the bridge host in LXD).
# ---------------------------------------------------------------------------
OPENSHELL_HOST_IP=""
if [ -n "${OPENSHELL_ENDPOINT:-}" ]; then
    # Strip scheme, then extract host portion (up to / or :port).
    _ep="${OPENSHELL_ENDPOINT#http://}"
    _ep="${_ep#https://}"
    _host="${_ep%%/*}"
    _host="${_host%%:*}"
    if [ -n "$_host" ]; then
        OPENSHELL_HOST_IP="$_host"
    fi
fi

# Fallback: read the default gateway from the routing table.
if [ -z "$OPENSHELL_HOST_IP" ]; then
    OPENSHELL_HOST_IP=$(ip route show default 2>/dev/null \
        | awk '/^default/ { print $3; exit }') || true
fi

if [ -n "$OPENSHELL_HOST_IP" ]; then
    # Remove any stale entry first, then append.
    sed -i '/host\.openshell\.internal/d' /etc/hosts 2>/dev/null || true
    printf '%s\t%s\n' "$OPENSHELL_HOST_IP" \
        "host.openshell.internal host.containers.internal host.docker.internal" \
        >> /etc/hosts
    ts "seeded /etc/hosts: host.openshell.internal → ${OPENSHELL_HOST_IP}"
else
    ts "WARN: could not determine host IP; host.openshell.internal not seeded"
fi

# ---------------------------------------------------------------------------
# 5. Probe OPENSHELL_ENDPOINT reachability before handing off to the
#    supervisor, matching upstream's diagnostic pattern.
# ---------------------------------------------------------------------------
if [ -n "${OPENSHELL_ENDPOINT:-}" ]; then
    _probe_result="unreachable"
    if curl --silent --max-time 5 --output /dev/null \
            --write-out "%{http_code}" "${OPENSHELL_ENDPOINT}" 2>/dev/null \
            | grep -qE '^[0-9]+$'; then
        _probe_result="reachable"
    elif [ -n "$OPENSHELL_HOST_IP" ] && \
         curl --silent --max-time 5 --output /dev/null \
              "http://${OPENSHELL_HOST_IP}/" 2>/dev/null; then
        _probe_result="reachable (fallback)"
    fi
    ts "OPENSHELL_ENDPOINT probe: ${_probe_result} (${OPENSHELL_ENDPOINT})"
fi

# ---------------------------------------------------------------------------
# 6. Exec-replace this wrapper with the supervisor using an explicit dynamic
#    linker path, matching upstream's technique. This avoids relying on the
#    rootfs's own ld.so and makes openshell-sandbox PID 1.
# ---------------------------------------------------------------------------
if [ ! -x "$SUPERVISOR" ]; then
    ts "FATAL: supervisor not found at ${SUPERVISOR}"
    exit 1
fi

# Probe the loader path for the current architecture.
LOADER=""
for _loader in \
    /lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 \
    /usr/lib/x86_64-linux-gnu/ld-linux-x86-64.so.2 \
    /lib64/ld-linux-x86-64.so.2 \
    /lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
    /usr/lib/aarch64-linux-gnu/ld-linux-aarch64.so.1 \
    /lib64/ld-linux-aarch64.so.1; do
    if [ -x "$_loader" ]; then
        LOADER="$_loader"
        break
    fi
done

LIB_PATH="/lib:/lib64:/usr/lib:/usr/lib64"
LIB_PATH="${LIB_PATH}:/lib/x86_64-linux-gnu:/usr/lib/x86_64-linux-gnu"
LIB_PATH="${LIB_PATH}:/lib/aarch64-linux-gnu:/usr/lib/aarch64-linux-gnu"

if [ -n "$LOADER" ]; then
    ts "exec: ${LOADER} --library-path ${LIB_PATH} ${SUPERVISOR} --workdir /sandbox"
    exec "$LOADER" --library-path "$LIB_PATH" "$SUPERVISOR" --workdir /sandbox
else
    ts "WARN: no explicit loader found; falling back to direct exec"
    exec "$SUPERVISOR" --workdir /sandbox
fi
