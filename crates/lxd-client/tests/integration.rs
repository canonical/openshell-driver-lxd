// SPDX-License-Identifier: AGPL-3.0-or-later

//! Integration tests against a real LXD daemon: instance lifecycle,
//! operation waiting (including its headers-then-delayed-body timing),
//! and real error shapes.
//!
//! Requires a running LXD with a `default` storage pool, an `lxdbr0`
//! network, and a locally-published `lxd-client-test` image alias.
//! `make test` provisions all three before running (see
//! `scripts/setup-lxd-test-env.sh`); CI runs the same target.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lxd_client::{LxdClient, LxdEndpoint, LxdError, LxdNetworkAclRule};

const TEST_IMAGE_ALIAS: &str = "lxd-client-test";
const LXD_SOCKET: &str = "/var/snap/lxd/common/lxd/unix.socket";

static NAME_COUNTER: AtomicU64 = AtomicU64::new(0);

fn client() -> LxdClient {
    LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(LXD_SOCKET))).unwrap()
}

/// Short, unique-enough LXD instance name for one test run.
fn unique_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_nanos();
    let n = NAME_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("lxdc-{nanos:x}-{n}")
}

fn sandbox_devices() -> HashMap<String, HashMap<String, String>> {
    let mut devices = HashMap::new();

    let mut root = HashMap::new();
    root.insert("type".to_string(), "disk".to_string());
    root.insert("pool".to_string(), "default".to_string());
    root.insert("path".to_string(), "/".to_string());
    devices.insert("root".to_string(), root);

    let mut eth0 = HashMap::new();
    eth0.insert("type".to_string(), "nic".to_string());
    eth0.insert("network".to_string(), "lxdbr0".to_string());
    devices.insert("eth0".to_string(), eth0);

    devices
}

#[tokio::test]
async fn create_get_list_start_stop_delete_lifecycle() {
    let client = client();
    let name = unique_name();

    let mut config = HashMap::new();
    config.insert(
        "user.openshell.sandbox_id".to_string(),
        "test-sandbox".to_string(),
    );

    // Created stopped, so start_instance (not just create's own `start`
    // flag) gets exercised below.
    let create_op = client
        .create_instance(
            &name,
            TEST_IMAGE_ALIAS,
            config,
            sandbox_devices(),
            vec![],
            false,
        )
        .await
        .expect("create_instance should succeed");
    tokio::time::timeout(
        Duration::from_secs(60),
        client.wait_operation(&create_op.id),
    )
    .await
    .expect("create operation should not time out")
    .expect("create operation should complete successfully");

    let instance = client
        .get_instance(&name)
        .await
        .expect("get_instance should succeed");
    assert_eq!(instance.name, name);
    assert_eq!(instance.status, "Stopped");
    assert_eq!(
        instance
            .config
            .get("user.openshell.sandbox_id")
            .map(String::as_str),
        Some("test-sandbox")
    );

    let names: Vec<String> = client
        .list_instances()
        .await
        .expect("list_instances should succeed")
        .into_iter()
        .map(|instance| instance.name)
        .collect();
    assert!(names.contains(&name), "expected {name} in {names:?}");

    let start_op = client
        .start_instance(&name)
        .await
        .expect("start_instance should succeed");
    tokio::time::timeout(Duration::from_secs(60), client.wait_operation(&start_op.id))
        .await
        .expect("start operation should not time out")
        .expect("start operation should complete successfully");

    let running = client
        .get_instance(&name)
        .await
        .expect("get_instance should succeed after start");
    assert_eq!(running.status, "Running");

    let state = client
        .get_instance_state(&name)
        .await
        .expect("get_instance_state should succeed");
    assert_eq!(state.status, "Running");

    let stop_op = client
        .stop_instance(&name, true)
        .await
        .expect("stop_instance should succeed");
    tokio::time::timeout(Duration::from_secs(30), client.wait_operation(&stop_op.id))
        .await
        .expect("stop operation should not time out")
        .expect("stop operation should complete successfully");

    let stopped = client
        .get_instance(&name)
        .await
        .expect("get_instance should succeed after stop");
    assert_eq!(stopped.status, "Stopped");

    let delete_op = client
        .delete_instance(&name)
        .await
        .expect("delete_instance should succeed");
    tokio::time::timeout(
        Duration::from_secs(30),
        client.wait_operation(&delete_op.id),
    )
    .await
    .expect("delete operation should not time out")
    .expect("delete operation should complete successfully");

    let err = client
        .get_instance(&name)
        .await
        .expect_err("instance should be gone after delete");
    match err {
        LxdError::Api { status_code, .. } => assert_eq!(status_code, 404),
        other => panic!("expected LxdError::Api(404), got {other:?}"),
    }
}

#[tokio::test]
async fn create_instance_with_unknown_image_alias_fails() {
    let client = client();
    let name = unique_name();

    let create_op = client
        .create_instance(
            &name,
            "definitely-not-a-real-alias",
            HashMap::new(),
            HashMap::new(),
            vec![],
            false,
        )
        .await
        .expect("create_instance call itself should succeed (LXD accepts the request and returns an operation)");

    let err = tokio::time::timeout(
        Duration::from_secs(15),
        client.wait_operation(&create_op.id),
    )
    .await
    .expect("wait_operation should not time out")
    .expect_err("waiting on the operation should fail: the image alias doesn't exist");

    match err {
        LxdError::OperationFailed { .. } => {}
        other => panic!("expected LxdError::OperationFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn get_instance_unknown_name_returns_404() {
    let client = client();

    let err = client
        .get_instance("lxdc-definitely-does-not-exist")
        .await
        .expect_err("get_instance should fail for an unknown name");

    match err {
        LxdError::Api { status_code, .. } => assert_eq!(status_code, 404),
        other => panic!("expected LxdError::Api(404), got {other:?}"),
    }
}

#[tokio::test]
async fn wait_operation_unknown_id_returns_404() {
    let client = client();

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        client.wait_operation("00000000-0000-0000-0000-000000000000"),
    )
    .await
    .expect("wait_operation should not hang on a 404")
    .expect_err("wait_operation should fail for an unknown id");

    match err {
        LxdError::Api { status_code, .. } => assert_eq!(status_code, 404),
        other => panic!("expected LxdError::Api(404), got {other:?}"),
    }
}

#[tokio::test]
async fn ensure_network_acl_create_update_delete() {
    let client = client();
    let acl_name = unique_name();

    client
        .ensure_network_acl(
            &acl_name,
            vec![LxdNetworkAclRule::allow_egress_tcp("10.0.0.0/8", 8080)],
        )
        .await
        .expect("ensure_network_acl should create a new ACL");

    // Idempotent: update the ruleset on an existing ACL.
    client
        .ensure_network_acl(
            &acl_name,
            vec![LxdNetworkAclRule::allow_egress_tcp("192.168.0.0/16", 443)],
        )
        .await
        .expect("ensure_network_acl should update an existing ACL");

    client
        .delete_network_acl(&acl_name)
        .await
        .expect("delete_network_acl should succeed");
}

#[tokio::test]
async fn delete_network_acl_nonexistent_is_ok() {
    let client = client();

    client
        .delete_network_acl("lxdc-definitely-does-not-exist-acl")
        .await
        .expect("delete_network_acl on a nonexistent ACL should return Ok");
}

#[tokio::test]
async fn project_exists_true_for_default_project() {
    let client = client();
    assert!(
        client
            .project_exists("default")
            .await
            .expect("project_exists should succeed"),
        "default project should exist"
    );
}

#[tokio::test]
async fn project_exists_false_for_nonexistent_project() {
    let client = client();
    let name = unique_name();
    assert!(
        !client
            .project_exists(&format!("{name}-missing"))
            .await
            .expect("project_exists should succeed"),
        "random project name should not exist"
    );
}

#[tokio::test]
async fn project_exists_with_non_default_project_client() {
    let client = LxdClient::new(LxdEndpoint::UnixSocket(PathBuf::from(LXD_SOCKET)))
        .unwrap()
        .with_project("default");
    assert!(
        client
            .project_exists("default")
            .await
            .expect("project_exists should succeed"),
        "default project should exist when client targets default"
    );
}

#[tokio::test]
async fn push_file_into_stopped_instance() {
    let client = client();
    let name = unique_name();

    let create_op = client
        .create_instance(
            &name,
            TEST_IMAGE_ALIAS,
            HashMap::new(),
            sandbox_devices(),
            vec![],
            false,
        )
        .await
        .expect("create_instance should succeed");
    tokio::time::timeout(
        Duration::from_secs(60),
        client.wait_operation(&create_op.id),
    )
    .await
    .expect("create should not time out")
    .expect("create should succeed");

    client
        .push_file_into_instance(&name, "/etc/openshell-test", b"hello from test")
        .await
        .expect("push_file_into_instance should succeed on a stopped container");

    // Verify overwrite works.
    client
        .push_file_into_instance(&name, "/etc/openshell-test", b"updated content")
        .await
        .expect("push_file_into_instance should overwrite an existing file");

    let delete_op = client
        .delete_instance(&name)
        .await
        .expect("delete_instance should succeed");
    tokio::time::timeout(
        Duration::from_secs(30),
        client.wait_operation(&delete_op.id),
    )
    .await
    .expect("delete should not time out")
    .expect("delete should succeed");
}
