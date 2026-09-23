# openshell-driver-lxd

[OpenShell](https://github.com/NVIDIA/OpenShell) Compute driver for LXD / MicroCloud.

The driver utilizes system containers provided by LXD to host sandboxes managed by
OpenShell and provides the necessary integration between both sides.

## Overview

`openshell-driver-lxd` is an out-of-tree [OpenShell](https://github.com/NVIDIA/OpenShell)
compute driver. It implements OpenShell's `compute_driver.proto` contract and serves it
over gRPC via a Unix domain socket, which the OpenShell gateway connects to at startup.

The driver is packaged together with the OpenShell gateway as an OCI
container image as `ghcr.io/canonical/openshell-gateway`. Furthermore
the supervisor binary is also packaged as
`ghcr.io/canonical/openshell-supervisor` to provide both in lock step
for solutions to use.

## License

Licensed under the [GNU Affero General Public License v3.0](LICENSE).

Also see [THIRD-PARTY-NOTICES](./THIRD-PARTY-NOTICES).
