#!/bin/sh
# SPDX-FileCopyrightText: 2026 Canonical Ltd.
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# DO NOT EDIT: This file is automatically injected into the guest rootfs
# by openshell-driver-lxd and should not be modified manually.

set -e

# Bring up loopback and eth0
ip link set lo up 2>/dev/null || true
ip link set eth0 up 2>/dev/null || true

has_ipv4() {
    ip -4 addr show dev eth0 2>/dev/null | grep -q "inet "
}

# Detect and run DHCP client one-shot with up to 30s timeout
if command -v udhcpc >/dev/null 2>&1; then
    # udhcpc: -i eth0, -f (foreground), -q (quit after lease), -n (exit on lease fail),
    # -t 10 -T 3 (10 attempts every 3 seconds = 30s bounded timeout)
    udhcpc -i eth0 -f -q -n -t 10 -T 3 || true
elif command -v dhclient >/dev/null 2>&1; then
    if command -v timeout >/dev/null 2>&1; then
        timeout 30 dhclient -1 eth0 || true
    else
        dhclient -1 eth0 || true
    fi
elif command -v dhcpcd >/dev/null 2>&1; then
    dhcpcd -1 -t 30 eth0 || true
elif [ -x /opt/openshell/net/udhcpc ]; then
    /opt/openshell/net/udhcpc -i eth0 -f -q -n -t 10 -T 3 -s /opt/openshell/net/udhcpc.script || true
else
    echo "openshell-init: no supported DHCP client found (udhcpc, dhclient, dhcpcd)" >&2
    exit 1
fi

# Ensure IPv4 address is acquired (wait up to remaining time if needed)
waited=0
while ! has_ipv4 && [ "$waited" -lt 10 ]; do
    sleep 1
    waited=$((waited + 1))
done

if ! has_ipv4; then
    echo "openshell-init: timed out waiting for IPv4 address on eth0" >&2
    exit 1
fi

# Ensure /sandbox directory exists
mkdir -p /sandbox

# Exec-replace PID 1 with the supervisor
exec /opt/openshell/bin/openshell-sandbox --workdir /sandbox "$@"
