// SPDX-License-Identifier: AGPL-3.0-or-later

//! Generated tonic + prost code for the OpenShell ComputeDriver service.
//!
//! The proto schema is vendored from NVIDIA/OpenShell at
//! `proto/compute_driver.proto` (Apache-2.0). All types under `pb::` are
//! emitted by `tonic-prost-build` at compile time and never committed to
//! source control.

#![allow(clippy::all, clippy::pedantic, clippy::nursery, clippy::restriction)]

pub mod pb {
    tonic::include_proto!("openshell.compute.v1");
}

pub use pb::compute_driver_client;
pub use pb::compute_driver_server;
