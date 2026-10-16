// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration tests of the driver against a real LXD daemon, with no gateway.
//!
//! Each test starts the actual `openshell-driver-lxd` binary and drives it
//! over its Unix socket with a gRPC client, so status codes, streams and
//! process startup are exactly what the gateway sees. Sandboxes run a
//! stand-in supervisor (`examples/standin_supervisor.rs`) instead of the real
//! one, which exits within seconds when there is no gateway to reach; the
//! stand-in holds a sandbox in whatever state a test asks for.
//!
//! Requires a running LXD whose `default` profile places instances on an
//! **OVN** network — that is the only kind LXD applies a per-NIC ACL to, and
//! so the only kind a sandbox can be *fenced* on; a bridge is a deliberate
//! unfenced fallback the driver warns about — with a root disk on a pool the
//! profile names (the tests read both from it rather than assuming `lxdbr0` and
//! `default`). Also the `lxc` CLI (used for out-of-band changes, as an
//! operator would make them), `skopeo` and `sqfstar` on `PATH`,
//! and outbound registry access: the sandbox image is imported on first use.
//!
//! Tests for behaviour the driver does not have yet are `#[ignore]`d with the
//! gap named; `cargo test -- --ignored` runs them.

mod harness;
mod images;
mod lifecycle;
mod process;
mod projects;
mod watch;
