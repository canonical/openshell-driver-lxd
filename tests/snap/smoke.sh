#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Smoke test for the openshell-driver-lxd snap.
# Invoked by the CI build job after `snapcraft pack`.
#
# Proves: the snap installs; ships the driver and its wrapper; stages the
# image-conversion helpers the driver shells out to (skopeo and sqfstar);
# declares the lxd, network and network-bind plugs and the `compute-driver`
# content plug the OpenShell snap provides; and the driver binary runs and
# advertises its LXD options.
#
# Needs root (via sudo) to install the snap, and removes any openshell-driver-lxd
# already present so the smoke always exercises the artifact just packed.
set -euo pipefail

SNAP_DIR="${SNAP_DIR:-.}"
SNAP_NAME="openshell-driver-lxd"

SNAP_FILE=$(find "${SNAP_DIR}" -maxdepth 1 -name '*.snap' | head -1)
if [ -z "${SNAP_FILE}" ]; then
    echo "ERROR: no .snap file found in ${SNAP_DIR}" >&2
    exit 1
fi
echo "Using snap: ${SNAP_FILE}"

SUDO=""
if [ "$(id -u)" != "0" ]; then
    SUDO="sudo"
fi

echo "==> Check: snap installs"
# A stale install would turn this into a refresh against the wrong artifact.
"${SUDO}" snap remove --purge "${SNAP_NAME}" >/dev/null 2>&1 || true
"${SUDO}" snap install --dangerous "${SNAP_FILE}" >/dev/null

APP_DIR="/snap/${SNAP_NAME}/current"
META="${APP_DIR}/meta/snap.yaml"

# --- 1. The driver and its wrapper are shipped. -----------------------------
echo "==> Check: driver and wrapper are shipped"
for binary in bin/openshell-driver-lxd bin/driver-wrapper; do
    test -x "${APP_DIR}/${binary}" \
        || { echo "ERROR: ${APP_DIR}/${binary} is missing or not executable" >&2; exit 1; }
done

# --- 2. The image-conversion helpers are staged. ----------------------------
echo "==> Check: skopeo and sqfstar are staged"
# The driver pulls an OCI image with skopeo and builds the LXD rootfs squashfs
# with sqfstar. A missing one only fails on first create_sandbox, so it is
# worth asserting here.
for helper in usr/bin/skopeo usr/bin/sqfstar; do
    test -e "${APP_DIR}/${helper}" \
        || { echo "ERROR: ${helper} is not staged; image import will fail" >&2; exit 1; }
done

# --- 3. The content plug the OpenShell snap provides is declared. -----------
echo "==> Check: the compute-driver content plug is declared"
# shellcheck disable=SC2016  # the literal $SNAP_COMMON is what is declared
for needle in \
    'interface: content' \
    'content: openshell-compute-driver' \
    'default-provider: openshell' \
    'target: $SNAP_COMMON/openshell'
do
    grep -qF "${needle}" "${META}" \
        || { echo "ERROR: ${META} does not declare '${needle}'" >&2; exit 1; }
done

# --- 4. The daemon plugs the interfaces it needs. ---------------------------
echo "==> Check: the driver app declares its plugs"
# snapd auto-connects `network` and `network-bind` after install, and that can
# lag the install itself -- most noticeably on the first install after the base
# snap is fetched. Let the connections settle before asserting.
for _ in $(seq 1 30); do
    snap connections "${SNAP_NAME}" | grep -q 'openshell-driver-lxd:network-bind ' && break
    sleep 1
done
for plug in lxd network network-bind; do
    snap connections "${SNAP_NAME}" | grep -q "openshell-driver-lxd:${plug} " \
        || { echo "ERROR: the ${plug} plug is not declared" >&2; exit 1; }
done

# --- 5. The driver binary runs and advertises its LXD options. --------------
echo "==> Check: driver --help"
# shellcheck disable=SC2016  # $SNAP expands in the snap's own shell, not here
DRIVER_HELP=$("${SUDO}" snap run --shell "${SNAP_NAME}.driver" \
    -c '$SNAP/bin/openshell-driver-lxd --help')
for option in --lxd-url --socket; do
    grep -q -- "${option}" <<<"${DRIVER_HELP}" \
        || { echo "ERROR: the driver --help does not advertise ${option}" >&2; exit 1; }
done

echo "==> Check: cleanup"
"${SUDO}" snap remove --purge "${SNAP_NAME}" >/dev/null

echo "All snap smoke checks passed."
