// SPDX-License-Identifier: AGPL-3.0-or-later

//! Translation between the proto's `DriverSandbox`/`DriverSandboxTemplate`
//! shapes and LXD's instance config/devices/profiles shapes.

use std::collections::HashMap;

use computev1::pb::{
    DriverCondition, DriverSandbox, DriverSandboxSpec, DriverSandboxStatus, DriverSandboxTemplate,
};
use lxd_client::{resources, Instance, InstanceState};
use prost_types::value::Kind;
use prost_types::Struct;

use crate::error::DriverError;

const KEY_SANDBOX_ID: &str = "user.openshell.sandbox_id";
const KEY_NAMESPACE: &str = "user.openshell.namespace";
const LABEL_PREFIX: &str = "user.openshell.label.";
const ENV_PREFIX: &str = "environment.";
const DEFAULT_STORAGE_POOL: &str = "default";
const DEFAULT_NETWORK: &str = "lxdbr0";

/// Guest-side path where the token file is bind-mounted, matching the
/// Docker/Podman driver convention so the supervisor finds it via
/// `OPENSHELL_SANDBOX_TOKEN_FILE`.
pub(crate) const GUEST_SANDBOX_TOKEN_PATH: &str = "/etc/openshell/auth/sandbox.jwt";

/// LXD containers built from rockcraft rocks have no traditional init
/// system (the rock's own entrypoint is Pebble), so every sandbox needs an
/// explicit `lxc.init.cmd` override pointing at the container-adapted init
/// wrapper baked into the image. Publishing an instance to an image does
/// *not* carry this kind of instance config forward, so it has to be set on
/// every create, not just once on the image.
const KEY_RAW_LXC: &str = "raw.lxc";
const RAW_LXC_INIT_CMD: &str = "lxc.init.cmd = /opt/openshell/bin/openshell-container-init.sh";

/// Maps an [`Instance`] + [`InstanceState`] pair to a [`DriverSandbox`]
/// observation. `spec` is left unset, per the proto's own doc comment:
/// "Drivers may omit this in observed snapshots returned by Get/List/Watch."
pub fn instance_to_driver_sandbox(instance: &Instance, _state: &InstanceState) -> DriverSandbox {
    DriverSandbox {
        id: instance
            .config
            .get(KEY_SANDBOX_ID)
            .cloned()
            .unwrap_or_default(),
        name: instance.name.clone(),
        namespace: instance
            .config
            .get(KEY_NAMESPACE)
            .cloned()
            .unwrap_or_default(),
        spec: None,
        status: Some(DriverSandboxStatus {
            sandbox_name: instance.name.clone(),
            instance_id: instance.name.clone(),
            agent_fd: String::new(),
            sandbox_fd: String::new(),
            conditions: vec![ready_condition(&instance.status)],
            deleting: false,
        }),
    }
}

/// `Ready` is currently keyed only on LXD's own instance status, not on
/// supervisor-connected-to-gateway acknowledgment -- that signal doesn't
/// exist yet (see SPEC.md's "Supervisor connection acknowledgment" open
/// question).
fn ready_condition(lxd_status: &str) -> DriverCondition {
    let (status, reason) = match lxd_status {
        "Running" => ("True", ""),
        "Stopped" => ("False", "Stopped"),
        // "Starting" is in the gateway's transient-reason set → Provisioning phase.
        "Starting" => ("False", "Starting"),
        "Error" => ("False", "Error"),
        // "Unknown" status (not "False") → gateway maps to Provisioning, not Error.
        _ => ("Unknown", "Unknown"),
    };
    DriverCondition {
        r#type: "Ready".to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        message: String::new(),
        last_transition_time: String::new(),
    }
}

/// Builds the LXD instance `config` map for `POST /1.0/instances`.
///
/// `gateway_endpoint` is the already-resolved `OPENSHELL_ENDPOINT` value
/// (`http://<lxdbr0-host-ip>:<gateway-grpc-port>`); resolving the bridge's
/// host IP requires an LXD call, so it's done by the caller, not here.
///
/// `has_token` indicates whether a sandbox JWT token will be pushed into the
/// container (via `POST /1.0/instances/<name>/files`). When `true`, the
/// `OPENSHELL_SANDBOX_TOKEN_FILE` env var is injected so the supervisor reads
/// the file the driver pushes at `GUEST_SANDBOX_TOKEN_PATH` before start.
pub fn build_create_config(
    sandbox: &DriverSandbox,
    spec: &DriverSandboxSpec,
    template: &DriverSandboxTemplate,
    gateway_endpoint: &str,
    has_token: bool,
) -> Result<HashMap<String, String>, DriverError> {
    let mut config = HashMap::new();

    config.insert(KEY_SANDBOX_ID.to_string(), sandbox.id.clone());
    config.insert(KEY_NAMESPACE.to_string(), sandbox.namespace.clone());
    config.insert(KEY_RAW_LXC.to_string(), RAW_LXC_INIT_CMD.to_string());
    // The supervisor installs its own seccomp BPF filter around the agent
    // process and uses clone/unshare for namespace setup. security.nesting
    // enables those paths. LXD's default deny list (kexec_load,
    // open_by_handle_at, init_module, delete_module) does not block seccomp(2),
    // and seccompNotifyDisallow is only injected when intercept.* keys are set
    // (which we don't use), so deny_default=false is not needed.
    config.insert("security.nesting".to_string(), "true".to_string());

    // template.environment takes precedence over spec.environment on key
    // collision (SPEC.md "Environment merging"), plus the two driver-
    // injected vars the supervisor needs to reach the gateway.
    for (key, value) in &spec.environment {
        config.insert(format!("{ENV_PREFIX}{key}"), value.clone());
    }
    for (key, value) in &template.environment {
        config.insert(format!("{ENV_PREFIX}{key}"), value.clone());
    }
    config.insert(
        format!("{ENV_PREFIX}OPENSHELL_SANDBOX_ID"),
        sandbox.id.clone(),
    );
    // --sandbox / OPENSHELL_SANDBOX: supervisor uses this for policy-sync
    // discovery (the named-sandbox path, distinct from --sandbox-id which
    // drives the gRPC bearer-JWT policy fetch).
    config.insert(
        format!("{ENV_PREFIX}OPENSHELL_SANDBOX"),
        sandbox.name.clone(),
    );
    config.insert(
        format!("{ENV_PREFIX}OPENSHELL_ENDPOINT"),
        gateway_endpoint.to_string(),
    );
    // Gateway-minted JWT: pushed directly into the container's overlay
    // filesystem before start (via LXD's file push API), then read by the
    // supervisor via OPENSHELL_SANDBOX_TOKEN_FILE. This avoids the UID
    // mismatch that occurs when bind-mounting a host-user-owned file into
    // an unprivileged container (where container root ≠ host file owner).
    if has_token {
        config.insert(
            format!("{ENV_PREFIX}OPENSHELL_SANDBOX_TOKEN_FILE"),
            GUEST_SANDBOX_TOKEN_PATH.to_string(),
        );
    }

    for (key, value) in &template.labels {
        if !is_valid_label_key(key) {
            return Err(DriverError::InvalidArgument(format!(
                "invalid label key {key:?}: must match [a-zA-Z0-9._-]+"
            )));
        }
        config.insert(format!("{LABEL_PREFIX}{key}"), value.clone());
    }

    if let Some(resources) = &template.resources {
        let cpu = if !resources.cpu_limit.is_empty() {
            &resources.cpu_limit
        } else {
            &resources.cpu_request
        };
        if !cpu.is_empty() {
            config.insert("limits.cpu".to_string(), resources::cpu_limit_to_lxd(cpu)?);
        }

        let memory = if !resources.memory_limit.is_empty() {
            &resources.memory_limit
        } else {
            &resources.memory_request
        };
        if !memory.is_empty() {
            config.insert(
                "limits.memory".to_string(),
                resources::memory_limit_to_lxd(memory)?,
            );
        }
    }

    Ok(config)
}

/// Builds the LXD `devices` map for `POST /1.0/instances`: a root disk on
/// the configured (or default) storage pool, a NIC on the configured (or
/// default) network, and an optional GPU device.
///
/// Egress enforcement is handled by the supervisor's L7 proxy, not by LXD
/// Network ACLs. Attaching a LXD ACL to any NIC or bridge adds an implicit
/// default-reject that blocks the proxy's own relay connections.
///
/// The sandbox JWT token is delivered via the LXD file push API
/// (`push_file_into_instance`) rather than a disk device bind-mount, which
/// avoids UID-mapping permission issues in unprivileged containers.
pub fn build_create_devices(
    template: &DriverSandboxTemplate,
    gpu: bool,
) -> HashMap<String, HashMap<String, String>> {
    let mut devices = HashMap::new();

    let mut root = HashMap::new();
    root.insert("type".to_string(), "disk".to_string());
    root.insert("pool".to_string(), storage_pool(template).to_string());
    root.insert("path".to_string(), "/".to_string());
    devices.insert("root".to_string(), root);

    let mut eth0 = HashMap::new();
    eth0.insert("type".to_string(), "nic".to_string());
    eth0.insert("network".to_string(), network(template).to_string());
    devices.insert("eth0".to_string(), eth0);

    if gpu {
        let mut gpu0 = HashMap::new();
        gpu0.insert("type".to_string(), "gpu".to_string());
        gpu0.insert("gputype".to_string(), "physical".to_string());
        devices.insert("gpu0".to_string(), gpu0);
    }

    devices
}

/// `"default"` plus any operator-configured extra profiles from
/// `driver_config.profiles`.
pub fn build_profiles(template: &DriverSandboxTemplate) -> Vec<String> {
    let mut profiles = vec!["default".to_string()];
    profiles.extend(struct_get_str_list(
        template.driver_config.as_ref(),
        "profiles",
    ));
    profiles
}

/// The LXD network a sandbox's NIC attaches to: `driver_config.network`,
/// defaulting to `lxdbr0`.
pub fn network(template: &DriverSandboxTemplate) -> &str {
    struct_get_str(template.driver_config.as_ref(), "network").unwrap_or(DEFAULT_NETWORK)
}

fn storage_pool(template: &DriverSandboxTemplate) -> &str {
    struct_get_str(template.driver_config.as_ref(), "storage_pool")
        .unwrap_or(DEFAULT_STORAGE_POOL)
}

fn is_valid_label_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

fn struct_get_str<'a>(s: Option<&'a Struct>, key: &str) -> Option<&'a str> {
    match s?.fields.get(key)?.kind.as_ref()? {
        Kind::StringValue(v) => Some(v.as_str()),
        _ => None,
    }
}

fn struct_get_str_list(s: Option<&Struct>, key: &str) -> Vec<String> {
    let Some(Kind::ListValue(list)) = s.and_then(|s| s.fields.get(key)?.kind.as_ref()) else {
        return Vec::new();
    };
    list.values
        .iter()
        .filter_map(|v| match v.kind.as_ref() {
            Some(Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
        .collect()
}
