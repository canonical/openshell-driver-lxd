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
use std::time::{SystemTime, UNIX_EPOCH};

use lxd_client::{LxdClient, LxdEndpoint, LxdError};

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
    client
        .wait_operation(&create_op.id, Some(60))
        .await
        .expect("create operation should complete");

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
    client
        .wait_operation(&start_op.id, Some(60))
        .await
        .expect("start operation should complete");

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
    client
        .wait_operation(&stop_op.id, Some(30))
        .await
        .expect("stop operation should complete");

    let stopped = client
        .get_instance(&name)
        .await
        .expect("get_instance should succeed after stop");
    assert_eq!(stopped.status, "Stopped");

    let delete_op = client
        .delete_instance(&name)
        .await
        .expect("delete_instance should succeed");
    client
        .wait_operation(&delete_op.id, Some(30))
        .await
        .expect("delete operation should complete");

    let err = client
        .get_instance(&name)
        .await
        .expect_err("instance should be gone after delete");
    match err {
        LxdError::Api { status_code, .. } => assert_eq!(status_code, 404),
        other => panic!("expected LxdError::Api(404), got {other:?}"),
    }
}

/// Verified directly against a real daemon, not assumed: when `/wait`
/// observes an operation that already failed, LXD returns a top-level
/// `error`-typed envelope (the real failure reason in `error_code`/
/// `error`), not a `sync` response wrapping an `Operation` whose `err`
/// field is set. A real creation failure surfaces as `LxdError::Api`,
/// not `LxdError::OperationFailed`.
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

    let err = client
        .wait_operation(&create_op.id, Some(15))
        .await
        .expect_err("waiting on the operation should fail: the image alias doesn't exist");

    match err {
        LxdError::Api { .. } => {}
        other => panic!("expected LxdError::Api, got {other:?}"),
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

    let err = client
        .wait_operation("00000000-0000-0000-0000-000000000000", Some(5))
        .await
        .expect_err("wait_operation should fail for an unknown id");

    match err {
        LxdError::Api { status_code, .. } => assert_eq!(status_code, 404),
        other => panic!("expected LxdError::Api(404), got {other:?}"),
    }
}
