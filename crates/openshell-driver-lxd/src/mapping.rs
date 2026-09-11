// SPDX-License-Identifier: AGPL-3.0-or-later

//! Translation between the proto's `DriverSandbox`/`DriverSandboxTemplate`
//! shapes and LXD's instance config/devices/profiles shapes.

use std::collections::HashMap;

use computev1::pb::{
    DriverCondition, DriverSandbox, DriverSandboxSpec, DriverSandboxStatus, DriverSandboxTemplate,
};
use lxd_client::{resources, Instance};
use prost_types::value::Kind;
use prost_types::Struct;

use crate::error::DriverError;

pub(crate) const KEY_SANDBOX_ID: &str = "user.openshell.sandbox_id";
const KEY_NAMESPACE: &str = "user.openshell.namespace";
const KEY_WORKSPACE: &str = "user.openshell.workspace";
const LABEL_PREFIX: &str = "user.openshell.label.";
const ENV_PREFIX: &str = "environment.";
const DEFAULT_STORAGE_POOL: &str = "default";
const DEFAULT_NETWORK: &str = "lxdbr0";

/// Identifies the guest-side path where the token file is bind-mounted.
/// The supervisor finds it via `OPENSHELL_SANDBOX_TOKEN_FILE`.
pub(crate) const GUEST_SANDBOX_TOKEN_PATH: &str = "/etc/openshell/auth/sandbox.jwt";

/// Identifies the guest-side Unix socket path where the supervisor binds its
/// SSH relay listener. The supervisor reads it via `OPENSHELL_SSH_SOCKET_PATH`.
pub(crate) const GUEST_SSH_SOCKET_PATH: &str = "/run/openshell/ssh.sock";

/// Guest-side directory where the digest-keyed supervisor storage volume is mounted.
pub(crate) const GUEST_SUPERVISOR_BIN_DIR: &str = "/opt/openshell/bin";

/// Guest-side directory where the digest-keyed DHCP client storage volume is mounted.
pub(crate) const GUEST_DHCP_CLIENT_DIR: &str = "/opt/openshell/net";

/// Guest-side executable path of the supervisor binary inside the mounted volume directory.
#[allow(dead_code)]
pub(crate) const GUEST_SUPERVISOR_BIN_PATH: &str = "/opt/openshell/bin/openshell-sandbox";

/// Deterministic LXD custom storage volume name for the given supervisor binary digest.
pub(crate) fn supervisor_volume_name(digest: &str) -> String {
    let clean = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("openshell-supervisor-{clean}")
}

/// Deterministic LXD custom storage volume name for the given DHCP client digest.
pub(crate) fn dhcp_client_volume_name(digest: &str) -> String {
    let clean = digest.strip_prefix("sha256:").unwrap_or(digest);
    format!("openshell-dhcp-client-{clean}")
}

/// LXD system containers ignore OCI entrypoints and run their own init. To ensure
/// network interfaces (lo, eth0) are brought up and an IPv4 lease is obtained
/// via DHCP before the supervisor starts, `lxc.init.cmd` is pointed at the
/// injected init script (`/openshell-init.sh`) which performs one-shot network
/// initialization and then exec-replaces itself into `/opt/openshell/bin/openshell-sandbox`
/// (mounted from a custom storage volume disk device).
/// Publishing an instance to an image does *not* carry this kind of instance config
/// forward, so it has to be set on every create, not just once on the image.
const KEY_RAW_LXC: &str = "raw.lxc";
pub(crate) const RAW_LXC_INIT_CMD: &str = "lxc.init.cmd = /openshell-init.sh";

/// Maps an [`Instance`] to a [`DriverSandbox`] observation. `spec` is left
/// unset, per the proto's own doc comment: "Drivers may omit this in observed
/// snapshots returned by Get/List/Watch."
pub fn instance_to_driver_sandbox(instance: &Instance) -> DriverSandbox {
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
        workspace: instance
            .config
            .get(KEY_WORKSPACE)
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
/// exist yet.
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
/// Sets `OPENSHELL_SANDBOX_ID`, `OPENSHELL_SANDBOX`, and
/// `OPENSHELL_SSH_SOCKET_PATH` (pointing to [`GUEST_SSH_SOCKET_PATH`])
/// unconditionally.
///
/// `gateway_endpoint` is the resolved `OPENSHELL_ENDPOINT` value
/// (`http://<host-ip>:<gateway-grpc-port>`). When empty the env var is not
/// set — the gateway is expected to supply it via `spec.environment` instead.
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
    config.insert(KEY_WORKSPACE.to_string(), sandbox.workspace.clone());
    config.insert(KEY_RAW_LXC.to_string(), RAW_LXC_INIT_CMD.to_string());
    // The supervisor installs its own seccomp BPF filter around the agent
    // process and uses clone/unshare for namespace setup. security.nesting
    // enables those paths.
    config.insert("security.nesting".to_string(), "true".to_string());

    // template.environment takes precedence over spec.environment on key
    // collision, plus the two driver-injected vars the supervisor needs to
    // reach the gateway.
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
    config.insert(
        format!("{ENV_PREFIX}OPENSHELL_SANDBOX"),
        sandbox.name.clone(),
    );
    config.insert(
        format!("{ENV_PREFIX}OPENSHELL_SSH_SOCKET_PATH"),
        GUEST_SSH_SOCKET_PATH.to_string(),
    );
    if !gateway_endpoint.is_empty() {
        config.insert(
            format!("{ENV_PREFIX}OPENSHELL_ENDPOINT"),
            gateway_endpoint.to_string(),
        );
    }
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
/// default) network, a read-only supervisor disk volume, a read-only DHCP client
/// disk volume, and an optional GPU device.
pub fn build_create_devices(
    template: &DriverSandboxTemplate,
    gpu: bool,
    supervisor_pool: &str,
    supervisor_volume: &str,
    dhcp_client_pool: &str,
    dhcp_client_volume: &str,
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

    let mut supervisor = HashMap::new();
    supervisor.insert("type".to_string(), "disk".to_string());
    supervisor.insert("pool".to_string(), supervisor_pool.to_string());
    supervisor.insert("source".to_string(), supervisor_volume.to_string());
    supervisor.insert("path".to_string(), GUEST_SUPERVISOR_BIN_DIR.to_string());
    supervisor.insert("readonly".to_string(), "true".to_string());
    devices.insert("supervisor".to_string(), supervisor);

    let mut dhcp_client = HashMap::new();
    dhcp_client.insert("type".to_string(), "disk".to_string());
    dhcp_client.insert("pool".to_string(), dhcp_client_pool.to_string());
    dhcp_client.insert("source".to_string(), dhcp_client_volume.to_string());
    dhcp_client.insert("path".to_string(), GUEST_DHCP_CLIENT_DIR.to_string());
    dhcp_client.insert("readonly".to_string(), "true".to_string());
    devices.insert("dhcp-client".to_string(), dhcp_client);

    if gpu {
        let mut gpu0 = HashMap::new();
        gpu0.insert("type".to_string(), "gpu".to_string());
        gpu0.insert("gputype".to_string(), "physical".to_string());
        devices.insert("gpu0".to_string(), gpu0);
    }

    devices
}

/// Returns `"default"` plus any operator-configured extra profiles from
/// `driver_config.profiles`.
pub fn build_profiles(template: &DriverSandboxTemplate) -> Vec<String> {
    let mut profiles = vec!["default".to_string()];
    profiles.extend(struct_get_str_list(
        template.driver_config.as_ref(),
        "profiles",
    ));
    profiles
}

/// Returns the LXD network a sandbox's NIC attaches to: `driver_config.network`,
/// defaulting to `lxdbr0`.
pub fn network(template: &DriverSandboxTemplate) -> &str {
    struct_get_str(template.driver_config.as_ref(), "network").unwrap_or(DEFAULT_NETWORK)
}

fn storage_pool(template: &DriverSandboxTemplate) -> &str {
    struct_get_str(template.driver_config.as_ref(), "storage_pool").unwrap_or(DEFAULT_STORAGE_POOL)
}

pub(crate) fn is_valid_label_key(key: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dhcp_client_volume_name() {
        assert_eq!(
            dhcp_client_volume_name("sha256:abc123def456"),
            "openshell-dhcp-client-abc123def456"
        );
        assert_eq!(
            dhcp_client_volume_name("abc123def456"),
            "openshell-dhcp-client-abc123def456"
        );
    }

    #[test]
    fn build_create_devices_omits_gpu_by_default() {
        let devices = build_create_devices(
            &DriverSandboxTemplate::default(),
            false,
            "default",
            "vol1",
            "default",
            "dhcp-vol1",
        );

        assert!(!devices.contains_key("gpu0"));
        assert!(devices.contains_key("root"));
        assert!(devices.contains_key("eth0"));
        assert!(devices.contains_key("supervisor"));
        assert!(devices.contains_key("dhcp-client"));
    }

    #[test]
    fn build_create_devices_attaches_gpu_when_requested() {
        let devices = build_create_devices(
            &DriverSandboxTemplate::default(),
            true,
            "default",
            "vol1",
            "default",
            "dhcp-vol1",
        );

        let gpu0 = devices.get("gpu0").expect("gpu0 device should be present");
        assert_eq!(gpu0.get("type"), Some(&"gpu".to_string()));
        assert_eq!(gpu0.get("gputype"), Some(&"physical".to_string()));
    }

    #[test]
    fn build_create_devices_attaches_supervisor_and_dhcp_client_volumes() {
        let digest = "sha256:11223344556677889900aabbccddeeff11223344556677889900aabbccddeeff";
        let sup_vol_name = supervisor_volume_name(digest);
        let dhcp_vol_name = dhcp_client_volume_name(digest);
        let devices = build_create_devices(
            &DriverSandboxTemplate::default(),
            false,
            "custom-pool",
            &sup_vol_name,
            "custom-dhcp-pool",
            &dhcp_vol_name,
        );

        let sup = devices
            .get("supervisor")
            .expect("supervisor device should be present");
        assert_eq!(sup.get("type"), Some(&"disk".to_string()));
        assert_eq!(sup.get("pool"), Some(&"custom-pool".to_string()));
        assert_eq!(sup.get("source"), Some(&sup_vol_name));
        assert_eq!(sup.get("path"), Some(&GUEST_SUPERVISOR_BIN_DIR.to_string()));
        assert_eq!(sup.get("readonly"), Some(&"true".to_string()));

        let dhcp = devices
            .get("dhcp-client")
            .expect("dhcp-client device should be present");
        assert_eq!(dhcp.get("type"), Some(&"disk".to_string()));
        assert_eq!(dhcp.get("pool"), Some(&"custom-dhcp-pool".to_string()));
        assert_eq!(dhcp.get("source"), Some(&dhcp_vol_name));
        assert_eq!(dhcp.get("path"), Some(&GUEST_DHCP_CLIENT_DIR.to_string()));
        assert_eq!(dhcp.get("readonly"), Some(&"true".to_string()));
    }

    #[test]
    fn guest_supervisor_paths_and_init_cmd_contract() {
        assert_eq!(RAW_LXC_INIT_CMD, "lxc.init.cmd = /openshell-init.sh");
        assert_eq!(GUEST_SUPERVISOR_BIN_DIR, "/opt/openshell/bin");
        assert_eq!(
            GUEST_SUPERVISOR_BIN_PATH,
            format!("{GUEST_SUPERVISOR_BIN_DIR}/openshell-sandbox")
        );
        assert_eq!(GUEST_DHCP_CLIENT_DIR, "/opt/openshell/net");
    }

    #[test]
    fn build_create_config_sets_ssh_socket_path() {
        let sandbox = DriverSandbox {
            id: "sb-123".to_string(),
            name: "test-sandbox".to_string(),
            ..Default::default()
        };
        let spec = DriverSandboxSpec::default();
        let template = DriverSandboxTemplate::default();

        let config = build_create_config(&sandbox, &spec, &template, "", false)
            .expect("build_create_config should succeed");

        assert_eq!(
            config.get("environment.OPENSHELL_SSH_SOCKET_PATH"),
            Some(&GUEST_SSH_SOCKET_PATH.to_string())
        );
    }
}
