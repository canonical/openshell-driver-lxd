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
# 2. Bring up eth0 via DHCP.
#
#    The image ships no systemd/netplan/NetworkManager, so nothing else in
#    the container will ever request a lease: eth0 otherwise keeps only its
#    kernel-assigned IPv6 link-local address, and the supervisor can never
#    reach the gateway over IPv4 (Network is unreachable). Run dhclient as
#    a background daemon (not -1/one-shot) so the lease renews for
#    longer-lived sandboxes, then briefly poll for the address before
#    moving on.
# ---------------------------------------------------------------------------
# Ubuntu 24.04 ships /etc/resolv.conf as a symlink to systemd-resolved's stub,
# which doesn't exist when systemd is not PID 1. Replace with a static file
# before any DNS is needed. Use the lxdbr0 host IP (from OPENSHELL_ENDPOINT,
# which LXD injects as an env var before PID 1 starts) as the nameserver since
# LXD's dnsmasq on the bridge provides DNS for the container network.
if [ -L /etc/resolv.conf ] || ! [ -s /etc/resolv.conf ]; then
    rm -f /etc/resolv.conf
    _ns=""
    if [ -n "${OPENSHELL_ENDPOINT:-}" ]; then
        _ep="${OPENSHELL_ENDPOINT#http://}"
        _ep="${_ep#https://}"
        _ns="${_ep%%/*}"
        _ns="${_ns%%:*}"
    fi
    if [ -n "$_ns" ]; then
        printf 'nameserver %s\n' "$_ns" > /etc/resolv.conf
    else
        # Prefer the LXD bridge host (default gateway) for DNS if available.
        _gw=$(ip route show default 2>/dev/null | awk '/^default/ { print $3; exit }') || true
        if [ -n "${_gw:-}" ]; then
            printf 'nameserver %s\n' "$_gw" > /etc/resolv.conf
        else
            printf 'nameserver 8.8.8.8\n' > /etc/resolv.conf
        fi
    fi
fi

if command -v dhclient >/dev/null 2>&1; then
    dhclient eth0 2>/dev/null &
elif command -v dhcpcd >/dev/null 2>&1; then
    dhcpcd eth0 2>/dev/null &
fi
for _ in 1 2 3 4 5 6 7 8 9 10; do
    if ip -4 -o addr show eth0 2>/dev/null | grep -q 'inet '; then
        break
    fi
    sleep 0.5
done
_eth0_addr=$(ip -4 -o addr show eth0 2>/dev/null | awk '{print $4}') || true
if [ -n "$_eth0_addr" ]; then
    ts "eth0 acquired IPv4: ${_eth0_addr}"
else
    ts "WARN: eth0 did not acquire an IPv4 address within 5s"
fi

# ---------------------------------------------------------------------------
# 3. Set container hostname from OPENSHELL_SANDBOX_ID (or fall back to the
#    kernel-assigned name, which LXD sets from the instance name).
# ---------------------------------------------------------------------------
if [ -n "${OPENSHELL_SANDBOX_ID:-}" ]; then
    hostname "${OPENSHELL_SANDBOX_ID}" 2>/dev/null || true
fi

# ---------------------------------------------------------------------------
# 4. Source any injected environment file.
# ---------------------------------------------------------------------------
if [ -f /srv/openshell-env.sh ]; then
    # shellcheck source=/dev/null
    source /srv/openshell-env.sh
fi

# ---------------------------------------------------------------------------
# 5. Seed /etc/hosts with host.openshell.internal → the lxdbr0 host-side IP.
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
# 6. Probe OPENSHELL_ENDPOINT reachability before handing off to the
#    supervisor, matching upstream's diagnostic pattern.
# ---------------------------------------------------------------------------
if [ -n "${OPENSHELL_ENDPOINT:-}" ]; then
    _probe_result="unreachable"
    if curl --silent --max-time 5 --output /dev/null \
            --write-out "%{http_code}" "${OPENSHELL_ENDPOINT}" 2>/dev/null \
            | grep -qE '^[1-9][0-9]{2}$'; then
        _probe_result="reachable"
    elif [ -n "$OPENSHELL_HOST_IP" ] && \
         curl --silent --max-time 5 --output /dev/null \
              "http://${OPENSHELL_HOST_IP}/" 2>/dev/null; then
        _probe_result="reachable (fallback)"
    fi
    ts "OPENSHELL_ENDPOINT probe: ${_probe_result} (${OPENSHELL_ENDPOINT})"
fi

# ---------------------------------------------------------------------------
# 7. Log token file status before handing off to the supervisor.
#
#    The driver pushes the per-sandbox JWT into the container at
#    OPENSHELL_SANDBOX_TOKEN_FILE via the LXD files API
#    (push_file_into_instance in lxd-client) before starting the instance.
#    Log whether it arrived so a failed push is immediately visible in the
#    console log without needing to exec in.
# ---------------------------------------------------------------------------
if [ -n "${OPENSHELL_SANDBOX_TOKEN_FILE:-}" ]; then
    if [ -f "${OPENSHELL_SANDBOX_TOKEN_FILE}" ] && [ -s "${OPENSHELL_SANDBOX_TOKEN_FILE}" ]; then
        _size=$(wc -c < "${OPENSHELL_SANDBOX_TOKEN_FILE}" 2>/dev/null || echo "?")
        ts "token file present: ${OPENSHELL_SANDBOX_TOKEN_FILE} (${_size} bytes)"
    elif [ -f "${OPENSHELL_SANDBOX_TOKEN_FILE}" ]; then
        ts "WARN: token file is empty: ${OPENSHELL_SANDBOX_TOKEN_FILE} — file push may have failed"
    else
        ts "WARN: token file missing: ${OPENSHELL_SANDBOX_TOKEN_FILE} — supervisor will fail to authenticate"
    fi
fi

# ---------------------------------------------------------------------------
# 8. Exec-replace this wrapper with the supervisor using an explicit dynamic
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
    ts "exec: ${LOADER} --library-path ${LIB_PATH} ${SUPERVISOR} --workdir /sandbox --ssh-socket-path /tmp/openshell-relay.sock"
    exec "$LOADER" --library-path "$LIB_PATH" "$SUPERVISOR" \
        --workdir /sandbox \
        --ssh-socket-path /tmp/openshell-relay.sock
else
    ts "WARN: no explicit loader found; falling back to direct exec"
    exec "$SUPERVISOR" --workdir /sandbox \
        --ssh-socket-path /tmp/openshell-relay.sock
fi
