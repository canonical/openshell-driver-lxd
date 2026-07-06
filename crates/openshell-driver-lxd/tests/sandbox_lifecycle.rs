// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration tests against a real LXD daemon: the full sandbox lifecycle
//! through the gRPC service, driven in-process (no socket needed, mirroring
//! `tests/get_capabilities.rs`).
//!
//! Requires a running LXD with a `default` storage pool, an `lxdbr0`
//! network, and a published `openshell-sandbox` image alias (locally: `make
//! sandbox-image`; CI builds and imports the same image via the
//! container-image job).

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::Parser;
use computev1::pb::compute_driver_server::ComputeDriver;
use computev1::pb::{
    CreateSandboxRequest, DeleteSandboxRequest, DriverSandbox, DriverSandboxSpec,
    DriverSandboxTemplate, GetSandboxRequest, ListSandboxesRequest, StopSandboxRequest,
};
use lxd_client::{LxdClient, LxdEndpoint};
use openshell_driver_lxd::config::{Config, DEFAULT_LXD_SOCKET};
use openshell_driver_lxd::driver::LxdComputeDriver;
use openshell_driver_lxd::grpc::ComputeDriverService;
use tonic::{Code, Request};

fn raw_lxd_client() -> LxdClient {
    LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(DEFAULT_LXD_SOCKET))).unwrap()
}

fn service() -> ComputeDriverService {
    let config = Config::parse_from(["openshell-driver-lxd"]);
    ComputeDriverService::new(LxdComputeDriver::new(config, raw_lxd_client()))
}

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
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
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
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
        }))
        .await
        .expect("stop_sandbox should succeed");

    let deleted = service
        .delete_sandbox(Request::new(DeleteSandboxRequest {
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
        }))
        .await
        .expect("delete_sandbox should succeed")
        .into_inner();
    assert!(deleted.deleted);

    let deleted_again = service
        .delete_sandbox(Request::new(DeleteSandboxRequest {
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
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
            sandbox_id: String::new(),
            sandbox_name: "odl-definitely-does-not-exist".to_string(),
        }))
        .await
        .expect_err("get_sandbox should fail for an unknown name");

    assert_eq!(status.code(), Code::NotFound);
}

#[tokio::test]
async fn unmanaged_instance_is_treated_as_not_found() {
    let service = service();
    let raw_lxd = raw_lxd_client();
    let name = unique_name();

    // Created directly via lxd-client, bypassing create_sandbox, so it never
    // gets the user.openshell.sandbox_id marker. Still needs the "default"
    // profile for a root disk device, same as the driver's own instances
    // (see mapping::build_profiles), or LXD fails the create operation and
    // rolls the instance back.
    let create_op = raw_lxd
        .create_instance(
            &name,
            "openshell-sandbox",
            HashMap::new(),
            HashMap::new(),
            vec!["default".to_string()],
            false,
        )
        .await
        .expect("raw instance creation should succeed");
    raw_lxd
        .wait_operation(&create_op.id)
        .await
        .expect("raw instance creation should complete");

    let get_status = service
        .get_sandbox(Request::new(GetSandboxRequest {
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
        }))
        .await
        .expect_err("get_sandbox should reject an unmanaged instance");
    assert_eq!(get_status.code(), Code::NotFound);

    let stop_status = service
        .stop_sandbox(Request::new(StopSandboxRequest {
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
        }))
        .await
        .expect_err("stop_sandbox should reject an unmanaged instance");
    assert_eq!(stop_status.code(), Code::NotFound);

    let deleted = service
        .delete_sandbox(Request::new(DeleteSandboxRequest {
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
        }))
        .await
        .expect("delete_sandbox should not error on an unmanaged instance")
        .into_inner();
    assert!(!deleted.deleted);

    let op = raw_lxd
        .delete_instance(&name)
        .await
        .expect("cleanup: deleting the raw instance should succeed");
    raw_lxd
        .wait_operation(&op.id)
        .await
        .expect("cleanup: waiting on raw instance deletion should succeed");
}
