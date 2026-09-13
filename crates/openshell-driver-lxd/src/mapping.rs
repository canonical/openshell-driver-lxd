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
            conditions: vec![ready_condition(instance)],
            deleting: false,
        }),
    }
}

/// Ready-condition reason when a sandbox's init exited on its own — an
/// ordinary application exit or a crash.
///
/// Terminal: the gateway deliberately does not relaunch these at startup, so a
/// genuine failure keeps its error signal.
pub(crate) const CONDITION_EXITED: &str = "ContainerExited";

/// Ready-condition reason when a sandbox was stopped through the API, i.e. the
/// driver was asked to stop it. The gateway treats this as recoverable.
pub(crate) const CONDITION_STOPPED: &str = "ContainerStopped";

/// Ready-condition reason while a sandbox exists but has not been started yet.
/// In the gateway's transient set, so it maps to `Provisioning`, not `Error`.
pub(crate) const CONDITION_CREATED: &str = "ContainerCreated";

/// Ready-condition reason while a sandbox is starting. Also transient.
pub(crate) const CONDITION_STARTING: &str = "ContainerStarting";

/// Ready-condition reason when a sandbox is frozen/paused.
pub(crate) const CONDITION_PAUSED: &str = "ContainerPaused";

/// Instance config key recording that the *driver* stopped this sandbox.
///
/// LXD reports a plain `Stopped` status whichever way an instance went down —
/// `volatile.last_state.power` is `STOPPED` both when the init exited by itself
/// and when the API stopped it — so intent has to be recorded when the stop is
/// issued. Without it a user-requested stop would be reported as
/// [`CONDITION_EXITED`] and surface as `Error` instead of `Stopped`.
pub(crate) const KEY_STOP_INTENT: &str = "user.openshell.stop_intent";

/// LXD sets this volatile key the first time an instance starts, so its absence
/// distinguishes "created, never started" from "ran and is now down".
const KEY_LAST_POWER: &str = "volatile.last_state.power";

/// Maps an instance's observed state to the `Ready` condition.
///
/// The reason strings mirror the cross-driver vocabulary in upstream's
/// `openshell-core::driver_utils` (`ContainerExited`, `ContainerStopped`,
/// `ContainerStarting`, `ContainerCreated`, `ContainerPaused`). This driver is
/// out-of-tree and cannot import that crate, but the gateway keys real
/// behaviour off these exact strings — which of them are transient (→
/// `Provisioning` rather than `Error`) and which are eligible for recovery at
/// gateway startup — so they must match upstream verbatim.
fn ready_condition(instance: &Instance) -> DriverCondition {
    let (status, reason) = match instance.status.as_str() {
        // A guest that signalled readiness over devlxd reports `Ready`; treat
        // it as running rather than falling through to `Unknown`.
        "Running" | "Ready" => ("True", ""),
        "Stopped" => {
            if !instance.config.contains_key(KEY_LAST_POWER) {
                // Created but never started: still provisioning, not a failure.
                ("False", CONDITION_CREATED)
            } else if instance.config.contains_key(KEY_STOP_INTENT) {
                ("False", CONDITION_STOPPED)
            } else {
                ("False", CONDITION_EXITED)
            }
        }
        "Starting" => ("False", CONDITION_STARTING),
        "Frozen" => ("False", CONDITION_PAUSED),
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
    default_max_processes: u32,
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

    // Bound the sandbox's PID count. Sandboxes run untrusted agent workloads
    // on a shared host, so an unlimited `pids.max` lets one sandbox fork-bomb
    // its co-tenants; the supervisor warns about this on every boot when it
    // finds the cgroup unlimited.
    let max_processes = max_processes(template).unwrap_or(default_max_processes);
    if max_processes > 0 {
        config.insert("limits.processes".to_string(), max_processes.to_string());
    }

    Ok(config)
}

/// Per-sandbox `limits.processes` override from `driver_config.max_processes`.
fn max_processes(template: &DriverSandboxTemplate) -> Option<u32> {
    let value = template
        .driver_config
        .as_ref()?
        .fields
        .get("max_processes")?
        .kind
        .as_ref()?;
    match value {
        Kind::NumberValue(n) if *n >= 0.0 => Some(*n as u32),
        Kind::StringValue(s) => s.parse::<u32>().ok(),
        _ => None,
    }
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

/// Returns the LXD storage pool a sandbox's root disk lives on:
/// `driver_config.storage_pool`, defaulting to `default`.
///
/// Also used to place the supervisor and DHCP-client volumes, so those
/// auxiliary volumes land on the same pool as the rootfs they attach to
/// unless the operator pins them with `--supervisor-storage-pool`.
pub(crate) fn storage_pool(template: &DriverSandboxTemplate) -> &str {
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

        let config = build_create_config(&sandbox, &spec, &template, "", false, 0)
            .expect("build_create_config should succeed");

        assert_eq!(
            config.get("environment.OPENSHELL_SSH_SOCKET_PATH"),
            Some(&GUEST_SSH_SOCKET_PATH.to_string())
        );
    }

    fn instance_with(status: &str, config: &[(&str, &str)]) -> Instance {
        Instance {
            name: "sb".to_string(),
            description: String::new(),
            status: status.to_string(),
            status_code: 0,
            architecture: String::new(),
            ephemeral: false,
            profiles: Vec::new(),
            config: config
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            devices: HashMap::new(),
            type_: "container".to_string(),
            project: "default".to_string(),
        }
    }

    /// The reason strings are a contract with the gateway, not cosmetic: it
    /// keys "is this transient?" and "may this be recovered at startup?" off
    /// these exact values.
    #[test]
    fn stopped_reason_distinguishes_death_from_requested_stop() {
        // Ran, then its init exited on its own → terminal ContainerExited.
        let died = instance_with("Stopped", &[("volatile.last_state.power", "STOPPED")]);
        let cond = ready_condition(&died);
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, CONDITION_EXITED);

        // The driver was asked to stop it → recoverable ContainerStopped.
        let stopped = instance_with(
            "Stopped",
            &[
                ("volatile.last_state.power", "STOPPED"),
                (KEY_STOP_INTENT, CONDITION_STOPPED),
            ],
        );
        assert_eq!(ready_condition(&stopped).reason, CONDITION_STOPPED);
    }

    /// A created-but-never-started instance is also `Stopped` in LXD. Reporting
    /// it as `ContainerExited` would put a sandbox that is merely mid-create
    /// into a terminal, sticky `Error` at the gateway.
    #[test]
    fn never_started_instance_is_transient_not_terminal() {
        let fresh = instance_with("Stopped", &[]);
        let cond = ready_condition(&fresh);
        assert_eq!(cond.status, "False");
        assert_eq!(cond.reason, CONDITION_CREATED);
    }

    #[test]
    fn running_and_guest_signalled_ready_are_both_ready() {
        assert_eq!(
            ready_condition(&instance_with("Running", &[])).status,
            "True"
        );
        // LXD reports `Ready` once a guest signals over devlxd; without this
        // arm it fell through to `Unknown`.
        assert_eq!(ready_condition(&instance_with("Ready", &[])).status, "True");
    }

    #[test]
    fn transient_states_are_reported_with_transient_reasons() {
        assert_eq!(
            ready_condition(&instance_with("Starting", &[])).reason,
            CONDITION_STARTING
        );
        assert_eq!(
            ready_condition(&instance_with("Frozen", &[])).reason,
            CONDITION_PAUSED
        );
        // An unrecognised status stays Unknown, which the gateway maps to
        // Provisioning rather than Error.
        assert_eq!(
            ready_condition(&instance_with("Weird", &[])).status,
            "Unknown"
        );
    }

    #[test]
    fn build_create_config_sets_default_pid_limit() {
        let sandbox = DriverSandbox::default();
        let spec = DriverSandboxSpec::default();
        let template = DriverSandboxTemplate::default();

        let config = build_create_config(&sandbox, &spec, &template, "", false, 4096)
            .expect("build_create_config should succeed");
        assert_eq!(config.get("limits.processes"), Some(&"4096".to_string()));

        // 0 means "leave pids.max alone".
        let unlimited = build_create_config(&sandbox, &spec, &template, "", false, 0)
            .expect("build_create_config should succeed");
        assert!(!unlimited.contains_key("limits.processes"));
    }

    #[test]
    fn driver_config_overrides_pid_limit() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "max_processes".to_string(),
            prost_types::Value {
                kind: Some(Kind::NumberValue(256.0)),
            },
        );
        let template = DriverSandboxTemplate {
            driver_config: Some(Struct { fields }),
            ..Default::default()
        };

        let config = build_create_config(
            &DriverSandbox::default(),
            &DriverSandboxSpec::default(),
            &template,
            "",
            false,
            4096,
        )
        .expect("build_create_config should succeed");

        assert_eq!(config.get("limits.processes"), Some(&"256".to_string()));
    }
}
