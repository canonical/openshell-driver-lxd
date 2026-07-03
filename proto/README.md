# proto/

`compute_driver.proto` is vendored from
[NVIDIA/OpenShell](https://github.com/NVIDIA/OpenShell) (Apache-2.0) — it
defines the `openshell.compute.v1.ComputeDriver` gRPC service that out-of-tree
compute drivers implement.

`crates/computev1` generates tonic/prost bindings from this file at build
time via `tonic-prost-build`.

To update: copy `proto/compute_driver.proto` from the upstream
`NVIDIA/OpenShell` main branch and run `cargo build -p computev1` to confirm
it still compiles. Preserve the original `SPDX-FileCopyrightText` /
`SPDX-License-Identifier: Apache-2.0` header. See `THIRD-PARTY-NOTICES` for
attribution details.
