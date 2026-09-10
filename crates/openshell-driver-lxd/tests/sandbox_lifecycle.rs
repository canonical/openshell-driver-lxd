// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration tests against a real LXD daemon: the full sandbox lifecycle
//! through the gRPC service, driven in-process (no socket needed, mirroring
//! `tests/get_capabilities.rs`).
//!
//! Requires a running LXD with a `default` storage pool and an `lxdbr0`
//! network. The sandbox image is the upstream OpenShell supervisor image,
//! which the driver pulls and imports on demand via `skopeo`/`umoci`/
//! `mksquashfs` — no image needs to be pre-built or pre-loaded. The test
//! host must therefore have those tools installed and outbound access to
//! `ghcr.io`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use openshell_driver_lxd::image::{ImageCache, SkopeoImporter};
use tonic::{Code, Request};

/// Upstream OpenShell supervisor image, imported on demand by the driver.
const SANDBOX_IMAGE: &str = "ghcr.io/nvidia/openshell/supervisor:latest";

fn raw_lxd_client() -> LxdClient {
    LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(DEFAULT_LXD_SOCKET))).unwrap()
}

/// Imports [`SANDBOX_IMAGE`] through the same code path the driver uses and
/// returns the resulting local LXD image alias. Lets tests that create
/// instances directly via the raw client (bypassing the driver) still boot
/// from a real, present image without depending on any pre-built alias.
async fn ensure_sandbox_image_alias() -> String {
    let config = Config::parse_from(["openshell-driver-lxd"]);
    let importer = Arc::new(SkopeoImporter::new(
        raw_lxd_client(),
        None,
        None,
        None,
        Duration::from_secs(config.operation_timeout_secs),
    ));
    let cache = ImageCache::new(raw_lxd_client(), importer, config.image_cache_alias_prefix);
    cache
        .resolve_alias(SANDBOX_IMAGE)
        .await
        .expect("importing the upstream sandbox image should succeed")
}

fn service() -> ComputeDriverService {
    let cache_dir = std::env::temp_dir().join("openshell-test-supervisor-cache");
    let config = Config::parse_from([
        "openshell-driver-lxd",
        "--supervisor-cache-dir",
        cache_dir.to_str().unwrap(),
    ]);
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
        workspace: "test-workspace".to_string(),
        spec: Some(DriverSandboxSpec {
            template: Some(DriverSandboxTemplate {
                image: String::new(),
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
    assert_eq!(got.workspace, "test-workspace");
    assert_eq!(got.id, name);

    // Verify via raw LxdClient that the instance has the supervisor custom storage volume attached
    let raw_instance = raw_lxd_client()
        .get_instance(&name)
        .await
        .expect("get_instance via raw client should succeed");
    let supervisor_dev = raw_instance
        .devices
        .get("supervisor")
        .expect("supervisor disk device should be present on created sandbox");
    assert_eq!(supervisor_dev.get("type"), Some(&"disk".to_string()));
    assert_eq!(
        supervisor_dev.get("path"),
        Some(&"/opt/openshell/bin".to_string())
    );
    assert_eq!(supervisor_dev.get("readonly"), Some(&"true".to_string()));

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
async fn get_sandbox_by_id_only_resolves_name() {
    let service = service();
    let name = unique_name();

    service
        .create_sandbox(Request::new(CreateSandboxRequest {
            sandbox: Some(sandbox(&name)),
        }))
        .await
        .expect("create_sandbox should succeed");

    // The fixture sets DriverSandbox.id == name, so this exercises the
    // sandbox_id-only fallback path in resolve_name: no sandbox_name is
    // given, only sandbox_id, matching how a caller that only has the
    // gateway-assigned ID would address the sandbox.
    let got = service
        .get_sandbox(Request::new(GetSandboxRequest {
            sandbox_id: name.clone(),
            sandbox_name: String::new(),
        }))
        .await
        .expect("get_sandbox by id alone should succeed")
        .into_inner()
        .sandbox
        .expect("response should carry a sandbox");
    assert_eq!(got.name, name);

    service
        .delete_sandbox(Request::new(DeleteSandboxRequest {
            sandbox_id: String::new(),
            sandbox_name: name.clone(),
        }))
        .await
        .expect("delete_sandbox should succeed");
}

#[tokio::test]
async fn get_sandbox_unknown_id_returns_not_found() {
    let service = service();

    let status = service
        .get_sandbox(Request::new(GetSandboxRequest {
            sandbox_id: "definitely-does-not-exist".to_string(),
            sandbox_name: String::new(),
        }))
        .await
        .expect_err("get_sandbox should fail for an unknown id");

    assert_eq!(status.code(), Code::NotFound);
}

#[tokio::test]
async fn get_sandbox_empty_name_and_id_returns_invalid_argument() {
    let service = service();

    let status = service
        .get_sandbox(Request::new(GetSandboxRequest {
            sandbox_id: String::new(),
            sandbox_name: String::new(),
        }))
        .await
        .expect_err("get_sandbox should fail when both name and id are empty");

    assert_eq!(status.code(), Code::InvalidArgument);
}

#[tokio::test]
async fn unmanaged_instance_is_treated_as_not_found() {
    let service = service();
    let raw_lxd = raw_lxd_client();
    let name = unique_name();
    let image_alias = ensure_sandbox_image_alias().await;

    // Created directly via lxd-client, bypassing create_sandbox, so it never
    // gets the user.openshell.sandbox_id marker. Still needs the "default"
    // profile for a root disk device, same as the driver's own instances
    // (see mapping::build_profiles), or LXD fails the create operation and
    // rolls the instance back.
    let create_op = raw_lxd
        .create_instance(
            &name,
            &image_alias,
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
