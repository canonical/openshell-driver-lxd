#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Snap wrapper for openshell-driver-lxd. It publishes the descriptor the
# OpenShell gateway reads over the content interface, then starts the driver
# with confinement-friendly paths.

set -eu

# snapd mounts the slot's source under the plug target as a subdirectory named
# after the source, so `openshell:compute-driver` appears here, not at the
# target root itself.
shared="${SNAP_COMMON}/openshell/compute-driver"
socket="${shared}/openshell-driver.sock"

# The directory only exists as the content-interface mount once the plug is
# connected; before that this is a plain directory in the driver's own space.
mkdir -p "$shared"
# World-readable: the OpenShell gateway may run as a user, not just as root.
umask 022
printf 'driver_name=lxd\n' >"${shared}/driver.conf"

mkdir -p "${SNAP_COMMON}/cache"

set -- \
    --socket "$socket" \
    --allow-plaintext-gateway \
    --project "${OPENSHELL_DRIVER_PROJECT:-default}" \
    --supervisor-cache-dir "${SNAP_COMMON}/cache/supervisor" \
    --image-work-dir "${SNAP_COMMON}/cache/image-work"
if [ -n "${OPENSHELL_DRIVER_NETWORK:-}" ]; then
    set -- --default-network "$OPENSHELL_DRIVER_NETWORK" "$@"
fi
# Which LXD network sandboxes require, and with it whether the driver fences
# them. Left unset the driver's own default applies, `ovn`; a bridge has to be
# asked for here, so an unfenced sandbox is never selected by accident.
if [ -n "${OPENSHELL_DRIVER_NETWORK_MODE:-}" ]; then
    set -- --network-mode "$OPENSHELL_DRIVER_NETWORK_MODE" "$@"
fi
if [ -n "${OPENSHELL_DRIVER_GATEWAY_ENDPOINT:-}" ]; then
    set -- --gateway-endpoint "$OPENSHELL_DRIVER_GATEWAY_ENDPOINT" "$@"
fi

exec "${SNAP}/bin/openshell-driver-lxd" "$@"
