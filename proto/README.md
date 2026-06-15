# proto/

`compute_driver.proto` is vendored from
[NVIDIA/OpenShell](https://github.com/NVIDIA/OpenShell) (Apache-2.0) — it
defines the `openshell.compute.v1.ComputeDriver` gRPC service that out-of-tree
compute drivers implement (see OpenShell PR #1703).

`crates/computev1` generates tonic/prost bindings from this file at build
time via `tonic-prost-build`.

When updating, keep this file in sync with the OpenShell release this driver
targets, and preserve its original `SPDX-FileCopyrightText` /
`SPDX-License-Identifier: Apache-2.0` header. See `THIRD-PARTY-NOTICES` for
attribution details.
