// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration tests against a real LXD daemon: the full sandbox lifecycle
//! through the gRPC service, driven in-process (no socket needed, mirroring
//! `tests/get_capabilities.rs`).
//!
//! Requires a running LXD with a `default` storage pool, an `lxdbr0`
//! network, and a published `openshell-sandbox` image alias (locally: `make
//! sandbox-image`; CI stages a cheap stand-in under the same alias).

use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use computev1::pb::compute_driver_server::ComputeDriver;
use computev1::pb::{
    CreateSandboxRequest, DeleteSandboxRequest, DriverSandbox, DriverSandboxSpec,
    DriverSandboxTemplate, GetSandboxRequest, ListSandboxesRequest, StopSandboxRequest,
};
use openshell_driver_lxd::config::Config;
use openshell_driver_lxd::driver::LxdComputeDriver;
use openshell_driver_lxd::grpc::ComputeDriverService;
use tonic::{Code, Request};

fn service() -> ComputeDriverService {
    let config = Config::parse_from(["openshell-driver-lxd"]);
    ComputeDriverService::new(LxdComputeDriver::new(config))
}

/// Short, unique-enough LXD instance name for one test run.
fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    format!("odl-{nanos:x}")
}

fn sandbox(name: &str) -> DriverSandbox {
    DriverSandbox {
        id: name.to_string(),
        name: name.to_string(),
        namespace: "default".to_string(),
        spec: Some(DriverSandboxSpec {
            template: Some(DriverSandboxTemplate {
                image: "ignored-in-v1".to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        status: None,
    }
}

#[tokio::test]
async fn create_get_list_stop_delete_lifecycle() {
    let service = service();
    let name = unique_name();

    service
        .create_sandbox(Request::new(CreateSandboxRequest {
            sandbox: Some(sandbox(&name)),
        }))
        .await
        .expect("create_sandbox should succeed");

    let got = service
        .get_sandbox(Request::new(GetSandboxRequest {
            sandbox_name: name.clone(),
            sandbox_id: String::new(),
        }))
        .await
        .expect("get_sandbox should succeed")
        .into_inner()
        .sandbox
        .expect("response should carry a sandbox");
    assert_eq!(got.name, name);
    assert_eq!(got.namespace, "default");
    assert_eq!(got.id, name);

    let listed = service
        .list_sandboxes(Request::new(ListSandboxesRequest {}))
        .await
        .expect("list_sandboxes should succeed")
        .into_inner()
        .sandboxes;
    assert!(
        listed.iter().any(|s| s.name == name),
        "expected {name} in {listed:?}"
    );

    service
        .stop_sandbox(Request::new(StopSandboxRequest {
            sandbox_name: name.clone(),
            sandbox_id: String::new(),
        }))
        .await
        .expect("stop_sandbox should succeed");

    let deleted = service
        .delete_sandbox(Request::new(DeleteSandboxRequest {
            sandbox_name: name.clone(),
            sandbox_id: String::new(),
        }))
        .await
        .expect("delete_sandbox should succeed")
        .into_inner();
    assert!(deleted.deleted);

    let deleted_again = service
        .delete_sandbox(Request::new(DeleteSandboxRequest {
            sandbox_name: name.clone(),
            sandbox_id: String::new(),
        }))
        .await
        .expect("deleting an already-gone sandbox should succeed, not error")
        .into_inner();
    assert!(!deleted_again.deleted);
}

#[tokio::test]
async fn get_sandbox_unknown_name_returns_not_found() {
    let service = service();

    let status = service
        .get_sandbox(Request::new(GetSandboxRequest {
            sandbox_name: "odl-definitely-does-not-exist".to_string(),
            sandbox_id: String::new(),
        }))
        .await
        .expect_err("get_sandbox should fail for an unknown name");

    assert_eq!(status.code(), Code::NotFound);
}
