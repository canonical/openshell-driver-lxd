# openshell-driver-lxd v1

| | |
|---|---|
| **Index** | LX176 |
| **Type** | Implementation |
| **Author(s)** | Kadin Sayani |
| **Status** | Drafting |
| **Created** | 15 Jun 2026 |

| **Reviewer(s)** | **Status** | **Date** |
|---|---|---|
| - | Pending Review | - |

---

## Abstract

OpenShell orchestrates agents inside sandboxes but is backend-neutral: it adds a `--compute-driver-socket=<path>` flag to the gateway (landed in [NVIDIA/OpenShell#1703](https://github.com/NVIDIA/OpenShell/pull/1703)) and delegates actual sandbox provisioning to a compute driver over that socket. `openshell-driver-lxd` is that driver for LXD. v1 provisions every sandbox as an LXD container from a single fixed base image, with default-deny egress enforced at L7 by the supervisor proxy.

---

## Rationale

Enterprises are deploying autonomous agents (assistants, CRM and security operations, research, financial analysis) that read corporate data, call external services, and execute code. Running such self-directed agents safely requires strong sandbox isolation so they cannot exfiltrate data past a trust boundary or escape into the host. `openshell-driver-lxd` is required to deliver an enterprise-grade agentic solution built on top of Canonical products (LXD, MicroCloud, and Ubuntu).

Virtual-machine isolation (rather than container isolation) is a fundamental design choice: each sandbox runs a full kernel, so a compromised agent cannot leverage kernel exploits to escape into the host or adjacent sandboxes. `openshell-driver-lxd` v1 will leverage LXD system containers for running agent sandboxes, with plans to switch to microVMs once LXD gains support for them.

---

## Specification

### Implementation

`openshell-driver-lxd` implements OpenShell's `openshell.compute.v1.ComputeDriver` gRPC contract (`proto/compute_driver.proto`, Apache-2.0) over a Unix domain socket, using [LXD](https://github.com/canonical/lxd) as the compute backend. Each sandbox is backed by an LXD container (with microVM support planned for when upstream LXD exposes that primitive).

```
OpenShell gateway
    └── Unix socket (gRPC)          ← --compute-driver-socket
            └── openshell-driver-lxd
                    └── LXD REST API (Unix socket)
                            └── LXD instance  (one per sandbox)
```

### Architecture and components

The driver is implemented in Rust as a single binary (`openshell-driver-lxd`) composed of three crates:

| Crate | Role |
|---|---|
| `computev1` | Build-time proto codegen via `tonic-prost-build`; exposes `pb::*` types and server/client stubs. The `.proto` source is vendored from NVIDIA/OpenShell (Apache-2.0). |
| `lxd-client` | Async HTTP client over the LXD REST API Unix socket. |
| `openshell-driver-lxd` | Driver binary + library. Contains `Config` (CLI), `LxdComputeDriver` (core logic), `ComputeDriverService` (tonic trait impl), and `DriverError`. |

At startup the binary:

1. Parses CLI flags into `Config` (socket path, LXD socket path, log level, default image alias, gateway gRPC port).
2. Creates parent directories for the gRPC socket if absent.
3. Removes a stale socket file if present; refuses to remove any non-socket path at that location.
4. Binds a `UnixListener`, sets permissions to `0600`.
5. Constructs `LxdComputeDriver` and wraps it in `ComputeDriverService`.
6. Serves the tonic `ComputeDriverServer` over the Unix listener, draining on SIGINT.

### Proto contract

The driver vendors `proto/compute_driver.proto` from NVIDIA/OpenShell (Apache-2.0). Key fields in the current contract:

- **`GetCapabilitiesResponse`**: returns `driver_name`, `driver_version`, `default_image`. Fields 4 and 5 are reserved.
- **`DriverSandboxSpec.resource_requirements` (field 9)**: contains a `GpuResourceRequirements` sub-message with an optional `count`; presence indicates a GPU request. The driver passes this through to `build_create_devices` to attach a physical GPU device to the LXD instance.
- **`DriverSandboxSpec.sandbox_token` (field 11)**: a gateway-minted JWT the driver must deliver into the sandbox container. The supervisor presents this token as a bearer credential when fetching its policy from the gateway. Delivery is via the LXD files API (`POST /1.0/instances/<name>/files`), which writes the JWT directly into the container's overlay filesystem before the instance starts (see [Sandbox environment variables](#sandbox-environment-variables)). The raw token must never be echoed back in `GetSandbox`/`ListSandboxes` responses.
- **`DriverSandboxTemplate.platform_config` (field 11)**: opaque platform-specific config forwarded verbatim by the gateway. Used by in-tree drivers (Kubernetes, Docker, Podman) for platform knobs. The LXD driver does not read this field.
- **`DriverSandboxTemplate.driver_config` (field 12)**: caller-provided, LXD-specific configuration. The gateway selects the `lxd`-named sub-block from the public `SandboxTemplate.driver_config` struct and forwards only that block here. Supported fields:

  | Field | Type | Default | Description |
  |---|---|---|---|
  | `network` | string | `"lxdbr0"` | LXD network bridge the sandbox NIC attaches to |
  | `storage_pool` | string | `"default"` | LXD storage pool for the sandbox root disk |
  | `profiles` | string[] | `[]` | Additional LXD profiles applied after `"default"` |

  Example (in the public `CreateSandbox` request body):
  ```json
  {"driver_config": {"lxd": {"network": "openshell-br0", "storage_pool": "nvme"}}}
  ```

> **Note:** The vendored proto must be kept in sync with the upstream contract. A drift in the contract (e.g. a missing or mistyped field) can cause silent failures in the supervisor; authentication errors are the most likely symptom.

### RPC-to-LXD mapping

| RPC | Description | LXD REST endpoint(s) |
|---|---|---|
| `GetCapabilities` | Returns driver name (`"lxd"`), version (`CARGO_PKG_VERSION`), and default image alias (`"openshell-sandbox"`). | None (pure in-memory response) |
| `ValidateSandboxCreate` | Pre-flight validation of a proposed sandbox spec. Currently a no-op (always valid); future versions will validate image alias existence, label key format, resource limits. | None (no-op) |
| `GetSandbox` | Fetch live platform-observed state for a single sandbox by name or ID. | `GET /1.0/instances/<name>`, `GET /1.0/instances/<name>/state` |
| `ListSandboxes` | List platform-observed state for all driver-managed sandboxes. | `GET /1.0/instances?recursion=1`, `GET /1.0/instances/<name>/state` (per instance) |
| `CreateSandbox` | Provision a stopped LXD container from the sandbox image, then start it. See [CreateSandbox behavior](#createsandbox-behavior). | `POST /1.0/instances`, `PUT /1.0/instances/<name>/state` (action: start) |
| `StopSandbox` | Stop the LXD container without deleting it (preserves storage). | `PUT /1.0/instances/<name>/state` (action: stop, force: true) |
| `DeleteSandbox` | Stop and delete the LXD container; returns `deleted: true` when a resource was actually removed, `deleted: false` when the container did not exist (404). | `PUT /1.0/instances/<name>/state` (action: stop, force: true), `DELETE /1.0/instances/<name>` |
| `WatchSandboxes` | Server-streaming RPC; emits a `WatchSandboxesDeletedEvent` for every successful `DeleteSandbox`. The gateway's `watch_loop` receives the event and immediately removes the sandbox from its store (rather than waiting for the 60-second reconcile). State-change events (Running/Stopped transitions) are not yet implemented; those still propagate via the reconcile poll. | None (in-process broadcast channel; no LXD subscription required for Deleted events) |

For Get/Stop/Delete, the driver resolves the LXD instance name as: use `sandbox_name` if non-empty, else fall back to `sandbox_id`. There is no `sandbox_id → name` index; this fallback is best-effort.

The driver communicates with LXD exclusively over the LXD REST API Unix socket (default: `/var/snap/lxd/common/lxd/unix.socket`; overridable via `--lxd-socket`). No TLS client certificates are required when using the Unix socket.

**Sandbox identity:** the driver stores the gateway-assigned `sandbox_id` and `namespace` as LXD instance config keys (`user.openshell.sandbox_id`, `user.openshell.namespace`) at create time, so Get/List/Watch can round-trip them without a separate datastore.

---

### lxd-client crate

> The below sections are derived from qwen3.6-experiment-local by Simon Fels.

The `lxd-client` crate provides an async HTTP client over the LXD REST API Unix socket.

#### Transport

Uses hyper 1.x with `tokio::net::UnixStream` + `hyper_util::rt::TokioIo` to open a fresh HTTP/1.1 connection per request over the Unix socket. No TLS is needed for local Unix socket access.

LXD wraps every response in an envelope:

```json
{ "type": "sync"|"async"|"error", "status_code": N, "metadata": <T>, "operation": "...", "error": "..." }
```

A generic `LxdResponse<T>` struct deserializes this envelope. Non-2xx status codes surface as `LxdError::Api { status_code, message }`.

#### Module layout

```
crates/lxd-client/src/
├── lib.rs          - LxdClient, LxdError, public re-exports
├── client.rs       - HTTP transport (hyper + UnixStream)
├── types.rs        - LXD API types (Instance, InstanceState, Operation, Network, ...)
├── instances.rs    - instance lifecycle methods
├── networks.rs     - network inspection (get_network for lxdbr0 host IP resolution)
├── acls.rs         - network ACL management (ensure_network_acl, delete_network_acl; not used by driver — see egress policy note)
├── operations.rs   - async operation waiting
└── events.rs       - event stream subscription (planned; WatchSandboxes not yet implemented)
```

#### Key types

| Type | Fields |
|---|---|
| `Instance` | `name`, `status`, `config: HashMap<String,String>`, `devices`, `type_`, `project`, `description` |
| `InstanceState` | `status`, `network: HashMap<String, NetworkState>` (null-tolerant; see note), `cpu`, `memory`, `disk` (null-tolerant) |
| `InstanceStatus` | `Running \| Stopped \| Starting \| Stopping \| Frozen \| Error` (string-tagged in LXD JSON) |
| `Operation` | `id`, `status`, `status_code`, `description`, `err` |
| `Network` | `name`, `type_`, `config: HashMap<String,String>` (used to extract `ipv4.address` for lxdbr0) |
| `LxdServerInfo` | `api_extensions: Vec<String>` (for microVM detection) |

> **Null-tolerance note:** LXD returns explicit JSON `null` (not an absent key) for `InstanceState.network` and `InstanceState.disk` when the instance is stopped. A custom `null_to_default` deserializer handles this; `#[serde(default)]` alone only handles absent keys.

#### Instance lifecycle methods

| Method | LXD endpoint |
|---|---|
| `create_instance(name, image, config, devices, profiles, start: bool)` | `POST /1.0/instances` |
| `get_instance(name)` | `GET /1.0/instances/<name>` |
| `get_instance_state(name)` | `GET /1.0/instances/<name>/state` |
| `list_instances()` | `GET /1.0/instances?recursion=1` |
| `start_instance(name)` | `PUT /1.0/instances/<name>/state {action: start}` |
| `stop_instance(name, force: bool)` | `PUT /1.0/instances/<name>/state {action: stop, force: …}` |
| `delete_instance(name)` | `DELETE /1.0/instances/<name>` |
| `get_network(name)` | `GET /1.0/networks/<name>` |

All mutating calls return an `Operation`. The caller passes the operation ID to `wait_operation`.

#### Operation waiting

`GET /1.0/operations/<uuid>/wait?timeout=60`; LXD blocks server-side until the operation completes. No client-side polling loop is needed.

#### Resource requirements mapping

`DriverResourceRequirements` uses Kubernetes-style quantity strings. LXD has a single limit per resource (no separate request/limit):

- `limits.cpu` = `cpu_limit` if set, else `cpu_request`. Fractional milli-CPU (e.g. `"500m"`) rounds up to the nearest whole core (LXD cannot express fractional cores). Logged at WARN level.
- `limits.memory` = `memory_limit` if set, else `memory_request`.

---

### Sandbox image pipeline

`DriverSandboxTemplate.image` carries a fully-qualified OCI image reference, but for v1 every sandbox runs the same fixed base image. `template.image` is accepted by `ValidateSandboxCreate` but not consulted to select sandbox content yet. Live OCI-ref support is deferred past v1.

> **Note:** It may be desirable to leverage imagecraft for building images for `openshell-driver-lxd` in the future.

The sandbox container image is built by the container-image CI workflow (`.github/workflows/container-image.yaml`) using rockcraft. The declarative spec is `rockcraft.yaml` at the repository root.

The workflow performs the following steps:

1. **`rockcraft pack`**: builds an OCI archive (`.rock`) containing:
   - The `openshell-sandbox` supervisor binary (prebuilt, downloaded from NVIDIA/OpenShell GitHub releases) at `/opt/openshell/bin/openshell-sandbox`.
   - The container-adapted init wrapper (`scripts/openshell-container-init.sh`) at `/opt/openshell/bin/openshell-container-init.sh`.
   - Ubuntu Noble stage-packages: `ca-certificates`, `curl`, `iproute2`, `isc-dhcp-client`, `nftables`, `python3-minimal`, `sqlite3`.
   - A `sandbox` system user and group (UID/GID 10001) created via a rockcraft `override-overlay` step using `useradd`/`groupadd`. The supervisor requires this user to exist in the image; it will refuse to start if `"sandbox"` is not found.

2. **`umoci unpack`**: extracts the OCI rock to a rootfs directory.

3. **`lxd-convert --type container --source rootfs/`**: converts the rootfs into a temporary LXD container instance.

4. **`lxc publish --alias openshell-sandbox`**: turns the container into a reusable LXD image in the local image store.

5. **`lxc image export openshell-sandbox`**: produces `openshell-sandbox.tar.gz`, a portable LXD image tarball.

To import on a host running the driver:

```bash
lxc image import openshell-sandbox.tar.gz --alias openshell-sandbox
```

To rebuild and reimport locally:

```bash
make sandbox-image
```

### CreateSandbox behavior

`CreateSandbox` creates every sandbox from the published `openshell-sandbox` image. The driver:

1. Resolves the lxdbr0 host-side IP via `GET /1.0/networks/<bridge>` to build `OPENSHELL_ENDPOINT`.
2. Builds the LXD instance config map (see [Sandbox environment variables](#sandbox-environment-variables) and [Container security profile](#container-security-profile)).
3. **Creates the instance in stopped state** (`start: false`) via `POST /1.0/instances` and waits for the operation to complete.
4. **Pushes the token file**: if `sandbox_token` is non-empty, writes the JWT to `/etc/openshell/auth/sandbox.jwt` inside the container via `POST /1.0/instances/<name>/files` with headers `X-LXD-uid: 0`, `X-LXD-gid: 0`, `X-LXD-mode: 0400`. The file exists before PID 1 runs. This step is skipped when `sandbox_token` is empty.
5. **Starts the instance** via `PUT /1.0/instances/<name>/state` (action: start) and waits for the operation to complete.

The create-then-start split (step 4 before step 5) is required. When creating with `start: true` in a single API call, LXD starts the container before `security.nesting` fully propagates to the kernel, causing the supervisor to crash on first boot with `Permission denied`. Splitting the calls gives LXD time to fully initialize the instance security profile before PID 1 runs.

`GetCapabilities.default_image` reports the configured image alias (operator-configurable via `--default-image`; default `"openshell-sandbox"`). There is no OCI registry resolution in v1.

---

### Base image: guest init and supervisor startup

An LXD container has no separate guest kernel to boot, so most of upstream's 840-line `openshell-vm-sandbox-init.sh` doesn't apply. A minimal container-adapted init wrapper (`scripts/openshell-container-init.sh`) runs as PID 1, performs setup, then exec-replaces itself with the supervisor. Because `exec` is used rather than `fork`, the supervisor becomes the container's real PID 1 (matching the upstream VM driver pattern) and is responsible for zombie reaping by construction.

The wrapper performs the following steps:

1. Create `/sandbox` and `chown` it to UID/GID 10001 (the `sandbox` user).
2. Bring up `eth0` via DHCP using `dhclient`, polling for an IPv4 address for up to 5 seconds. Without this step, `eth0` has only a kernel-assigned IPv6 link-local address and the supervisor cannot reach the gateway over IPv4. `dhclient` runs as a background daemon (not one-shot) so the lease renews for long-lived sandboxes.
3. Set the container hostname to `OPENSHELL_SANDBOX_ID`.
4. Source `/srv/openshell-env.sh` if present (injected environment file).
5. Seed `/etc/hosts` with `host.openshell.internal → <lxdbr0 host IP>` (parsed from `OPENSHELL_ENDPOINT`, falling back to the default gateway route), so the supervisor can reach the gateway even if in-container DNS differs.
6. Probe `OPENSHELL_ENDPOINT` reachability and log the result.
7. Exec-replace with the supervisor:

```bash
exec <ld-linux.so> --library-path <lib-path> /opt/openshell/bin/openshell-sandbox \
    --workdir /sandbox \
    --ssh-socket-path /tmp/openshell-relay.sock
```

The supervisor runs in its **default mode** (network + process), which sets up a veth pair and installs nftables rules inside the container's network namespace for L7 proxy enforcement. The `nftables` package is included in the sandbox image for this purpose. The supervisor uses `nsenter --net=<path>` for its network namespace operations, which avoids the sysfs remount that would otherwise require `CAP_SYS_ADMIN` in the host user namespace.

**`--ssh-socket-path /tmp/openshell-relay.sock`** tells the supervisor where to bind its embedded SSH daemon. The gateway relays terminal traffic (`RelayStream` RPC) through this socket to reach the agent. Without this flag, the supervisor does not start an SSH listener and any attempt to open a terminal returns `"supervisor session not connected"`.

The explicit dynamic linker path (`<ld-linux.so> --library-path ...`) mirrors upstream's technique for making the supervisor binary self-contained regardless of the rootfs `ld.so`.

---

### Sandbox environment variables

The driver injects the following environment variables into every sandbox container via LXD instance `environment.*` config keys at create time. All are available to the supervisor process from startup.

| Variable | Value | Purpose |
|---|---|---|
| `OPENSHELL_SANDBOX_ID` | UUID assigned by the gateway (`DriverSandbox.id`) | Supervisor `--sandbox-id`; used to fetch the sandbox's policy from the gateway via gRPC bearer-JWT authentication. |
| `OPENSHELL_SANDBOX` | Sandbox name (`DriverSandbox.name`, e.g. `"demo1"`) | Supervisor `--sandbox`; used for the supervisor's local policy-sync discovery path (a distinct code path from the gRPC fetch). Missing this variable causes the supervisor to exit with `"Cannot sync discovered policy: sandbox not available."` |
| `OPENSHELL_ENDPOINT` | `http://<lxdbr0-host-ip>:<gateway-grpc-port>` | The gateway's gRPC address. Required for policy fetch, log push, and SSH relay. Resolved per-sandbox at create time from `GET /1.0/networks/<bridge>`. |
| `OPENSHELL_SANDBOX_TOKEN_FILE` | `/etc/openshell/auth/sandbox.jwt` (guest path) | Path to the JWT token file pushed into the container via the LXD files API. The supervisor reads the JWT from this file and presents it as a bearer credential when authenticating to the gateway. Set only when `sandbox_token` is non-empty. |

Both `OPENSHELL_SANDBOX_ID` and `OPENSHELL_SANDBOX` are required. They serve different code paths in the supervisor and cannot substitute for each other.

The token is delivered via the LXD files API (`POST /1.0/instances/<name>/files`) after the instance is created in stopped state and before it is started. The JWT is written directly into the container's overlay filesystem at `/etc/openshell/auth/sandbox.jwt` with `uid=0`, `gid=0`, `mode=0400`. No host-side file or LXD disk device is created. The file is present when PID 1 runs.

---

### Container security profile

The supervisor runs inside the sandbox container and installs its own security controls around the agent process it spawns: Landlock filesystem restrictions and a seccomp BPF filter. One LXD config key is required on every sandbox instance (set by `mapping.rs::build_create_config`):

**`security.nesting = true`**

Enables nested clone/unshare namespace operations inside the container. Required for the supervisor's namespace setup and seccomp BPF installation.

`security.syscalls.deny_default` is left at its default (`true`). LXD's default deny list covers only four syscalls (`kexec_load`, `open_by_handle_at`, `init_module`, `delete_module`), none of which block `seccomp(2)`. The rule that would block nested seccomp listener installation (`seccompNotifyDisallow`) is only injected by LXD when `intercept.*` config keys are set; we don't use them, so the supervisor's seccomp BPF installation works without any relaxation of the deny list.

The `raw.lxc` config key is also set on every sandbox to override the container's init command:

```
lxc.init.cmd = /opt/openshell/bin/openshell-container-init.sh
```

This is required because rockcraft rocks use Pebble as their default entrypoint; without this override the container starts Pebble rather than the init wrapper.

---

### Networking

#### Interface and addressing

The container attaches to the bridge named in `driver_config.network` (default `lxdbr0`); LXD's dnsmasq DHCP assigns the address. The container init wrapper brings up `eth0` via `dhclient` before the supervisor starts.

#### Gateway reachability

The driver resolves the bridge's host-side IP via `GET /1.0/networks/<bridge>` at sandbox create time and builds `OPENSHELL_ENDPOINT` from it. The supervisor uses this endpoint to reach the gateway for policy fetch, log push, and SSH relay.

#### Default egress policy

Default-deny egress is enforced by the **supervisor L7 proxy**. The supervisor runs in its default mode (not `--mode=process`), which on first boot:

1. Creates a named network namespace (`sandbox-<id>`).
2. Creates a veth pair: host side `veth-h-<id>` at `10.200.0.1/24` (in the container's main namespace), sandbox side `veth-s-<id>` at `10.200.0.2/24`.
3. Installs nftables rules inside `sandbox-<id>` that allow only connections to the proxy (`10.200.0.1:3128`) and established/related flows; all direct TCP/UDP egress is rejected (logged first).
4. Spawns agent processes inside `sandbox-<id>` with `https_proxy=http://10.200.0.1:3128`.
5. The supervisor's HTTP/HTTPS proxy (listening on `10.200.0.1:3128` in the main namespace) enforces the sandbox's active network policy at the method level before relaying allowed connections via `eth0`.

**Why not LXD Network ACLs for L3/L4?** LXD Network ACLs (via `security.acls` on the bridge or NIC) add an implicit default-reject for all unmatched traffic the moment any ACL is attached — even if no explicit `security.acls.default.egress.action=drop` is set. This implicit reject is applied at the bridge/host boundary and blocks the supervisor proxy's own outbound relay connections (the proxy establishes TCP connections from `eth0` in the container's main namespace, not from inside the agent's sandboxed namespace). An LXD ACL that allows the gateway but default-drops everything else prevents the proxy from forwarding any policy-allowed traffic. The L7 proxy is therefore the sole enforcement layer; no LXD ACL is attached to the bridge or NIC devices.

---

### Local development setup

The driver and gateway are used as local binaries (no snap packaging required for development). The gateway binary is built from upstream `NVIDIA/OpenShell` main (PR #1703 merged).

**Gateway auth in local dev:** even with `--disable-tls`, the gateway's JWT authenticator rejects all requests without a bearer token. A local-dev config file (`gateway-dev.toml`) sets `allow_unauthenticated_users = true` so the CLI and grpcurl work without credentials. This file is never used in production.

The `--disable-tls` flag is required (rather than TLS with `--tls-client-ca`) because `--tls-client-ca` forces `require_client_auth=true` on every connection, which blocks the supervisor's bearer-JWT authentication (the supervisor presents a JWT, not a client certificate).

**Starting the driver and gateway locally:**

```bash
# Build and start the driver
cargo build -p openshell-driver-lxd
rm -f /tmp/openshell-driver.sock
./target/debug/openshell-driver-lxd --socket /tmp/openshell-driver.sock &

# Start the gateway
openshell-gateway \
  --bind-address 0.0.0.0 \
  --port 17670 \
  --drivers lxd \
  --compute-driver-socket /tmp/openshell-driver.sock \
  --disable-tls \
  --config gateway-dev.toml \
  --log-level info &
```

**Verifying a sandbox is healthy:**

After `openshell sandbox create --name demo1`, check the LXD console log:

```text
[container-init] eth0 acquired IPv4: 10.x.x.x/24
[container-init] seeded /etc/hosts: host.openshell.internal → 10.x.x.1
[container-init] OPENSHELL_ENDPOINT probe: reachable (http://10.x.x.1:17670)
[container-init] exec: .../openshell-sandbox --workdir /sandbox --ssh-socket-path /tmp/openshell-relay.sock
OCSF SSH:LISTEN [INFO]
OCSF LIFECYCLE:INSTALL [INFO] OpenShell Sandbox Supervisor success
```

The gateway log then shows `GetSandboxConfig → 200`, confirming the supervisor authenticated and fetched policy. `lxc exec demo1 -- ps aux` should show PID 1 (supervisor, root) and PID ~60 (agent, `sandbox` user uid 10001).

**Driver-direct verification (no gateway required):**

```bash
PROTO=proto/compute_driver.proto
SOCK=unix:///tmp/openshell-driver.sock

grpcurl -plaintext -proto "$PROTO" -import-path proto \
  "$SOCK" openshell.compute.v1.ComputeDriver/GetCapabilities
```

---

### Snap integration

Three independently-published, strictly-confined snaps are involved:

| Snap | Publisher | Role |
|---|---|---|
| `lxd` | Canonical | Compute backend; owns the LXD REST API Unix socket. |
| `openshell-driver-lxd` | Canonical | The compute driver; bridges the gateway's gRPC socket to LXD's REST socket. |
| `openshell` | NVIDIA | Ships the gateway daemon (`openshell.gateway`), which dials `--compute-driver-socket=<path>`. |

Strict confinement means each snap is sandboxed by AppArmor; reaching outside its own directories requires an explicit interface connection. There are two cross-snap links to wire up.

#### openshell-driver-lxd ↔ lxd

snapd ships a purpose-built `lxd` interface for this case. `snap/snapcraft.yaml` already declares `plugs: [lxd]` and passes `--lxd-socket /var/snap/lxd/common/lxd/unix.socket`. The `lxd` interface doesn't auto-connect, so operators run:

```bash
sudo snap connect openshell-driver-lxd:lxd lxd:lxd
```

#### openshell-driver-lxd's own gRPC socket (packaging bug to fix)

`Config::DEFAULT_SOCKET` is `/var/run/openshell-driver.sock`, a path outside anything a strict-confinement snap can write to. `snapcraft.yaml`'s `command:` doesn't override `--socket`, so as packaged today the binary will fail to bind under `snap run`. Fix: pass `--socket $SNAP_COMMON/sockets/compute-driver.sock`. `$SNAP_COMMON` persists across snap revisions, which is required since the gateway needs a stable path to keep dialing across `openshell-driver-lxd` refreshes.

#### openshell-driver-lxd ↔ openshell

Two snaps sharing a runtime-created Unix socket requires the `content` interface. `openshell-driver-lxd` adds a content slot:

```yaml
slots:
  compute-driver-socket:
    interface: content
    content: openshell-compute-driver-socket
    write: [$SNAP_COMMON/sockets]
```

`openshell`'s `snapcraft.yaml` adds a matching plug on the gateway app and points `--compute-driver-socket` at the mounted socket. Operators then run:

```bash
sudo snap connect openshell:compute-driver-socket openshell-driver-lxd:compute-driver-socket
```

---

## Testing

### Automated (CI: `cargo test --workspace`)

Two sets of integration tests run against a real LXD daemon on every PR:

- **`crates/lxd-client/tests/integration.rs`**: 5 tests against the LXD REST API directly: instance lifecycle (create, start, stop, delete), 404 handling, and network bridge IP resolution.
- **`crates/openshell-driver-lxd/tests/sandbox_lifecycle.rs`**: 2 tests driving the full driver in-process (no Unix socket): complete create -> get -> list -> stop -> delete lifecycle, and unknown-name 404 safety.

Unit tests (no LXD required): `driver::capabilities_reports_static_fields`, `cli::help_lists_socket_flag`, and 6 resource-limit parsing tests in `lxd-client`.

CI stages a lightweight Alpine image under the `openshell-sandbox` alias so automated tests run without the full rockcraft build.

### Manual e2e (gateway required)

There is no formal driver conformance suite in upstream OpenShell. The practical conformance baseline is three cluster-agnostic e2e tests in `e2e/rust/tests/` of the OpenShell repository, which are driver-agnostic and can be run against any gateway backend:

| Test file | What it covers |
|---|---|
| `smoke.rs::gateway_smoke` | Connect → create sandbox → exec command → assert output → list → delete |
| `sandbox_lifecycle.rs` | `--no-keep` flag behavior, sandbox list polling after create |
| `sandbox_labels.rs` | Create with `--label k=v`, list with `--selector`, get by name, delete |

Run these against the LXD driver using `scripts/e2e-lxd.sh` (or `make e2e`), which is modeled on `e2e/rust/e2e-vm.sh` and sources `e2e/support/gateway-common.sh` helpers from `$OPENSHELL_REPO`. The script defaults `OPENSHELL_REPO` to a sibling `../OpenShell` directory. Pass `OPENSHELL_E2E_LXD_TEST=smoke|lifecycle|labels` to run a single test.

The e2e tests cover the things CI cannot easily validate: sandbox reaching `SANDBOX_PHASE_READY` as observed via the public API, token delivery working end-to-end (supervisor authenticates), SSH relay binding (`openshell term` opens), and labels surviving a round-trip through LXD instance config.

### Upstream examples compatibility

The upstream OpenShell repository ships a set of examples (`examples/`) that serve as the practical conformance baseline. The table below records which work with the LXD driver today and what blocks the rest.

| Example | Status | Notes |
|---|---|---|
| `gateway-deploy-connect.md` | Works | Basic sandbox create / connect / exec; no network policy required. |
| `sync-files.md` | Works | `openshell sandbox upload/download` uses SSH relay, which is bound. |
| `vscode-remote-sandbox.md` | Works | `--editor vscode` / `ssh-proxy` also uses SSH relay. |
| `sandbox-policy-quickstart` | Works | GET to `api.github.com` returns zen quote; POST returns structured `{"error":"policy_denied"}` JSON. Tested against LXD driver with lxdbr0 bridge. |
| `policy-advisor` | Works | All 7 CTF gates unlocked. Denial events flow supervisor → `SubmitPolicyAnalysis` → gateway draft chunks → CLI `openshell rule approve-all` → policy hot-reload → proxy allows. Gate 7 exercises the `allowed_ips` SSRF override: hostname must resolve to a private IP so the mechanistic mapper can include the IP in its proposal; the proxy then permits the CONNECT. Requires `/etc/hosts` entry + HTTPS server for local testing (no public `internal.corp.example.com` DNS). |
| `agent-driven-policy-management` | Needs testing | Requires network policy enforcement (now active) and the `policy.local` proposal API. |
| `local-inference` | Needs testing | Requires proxy interception of `inference.local`; supervisor now runs in network mode. |
| `multi-agent-notepad` | Needs testing | Requires scoped network policy for GitHub API access; supervisor proxy is in place. |
| `bring-your-own-container` | Blocked | Requires `--from Dockerfile` (OCI ref / custom image support), deferred past v1. |
| `private-ip-routing` | Not applicable | Requires a Kubernetes cluster with pod networking. |
| `spiffe-token-grant-demo` | Not applicable | Requires a Kubernetes cluster with SPIRE. |

---

## Documentation

See `README.md` for build and run instructions.

---

## Security

v1 sandboxes are LXD containers, which share the host kernel (a weaker isolation boundary than hardware-rooted isolation). microVMs will be used to bridge this gap once available in upstream LXD.

**`security.syscalls.deny_default`** is left at its default (`true`). LXD's default deny list (four syscalls) does not interfere with the supervisor's seccomp BPF installation; `deny_default=false` is not set on sandbox containers.

**Sandbox JWT token** is delivered via the LXD files API (`POST /1.0/instances/<name>/files`) and written directly into the container's overlay filesystem at `/etc/openshell/auth/sandbox.jwt`, not as a raw environment variable. The token content never appears in `lxc config show` output or in LXD's instance database (it is not an instance config key or a device entry).

---

## Performance

Written in Rust. No benchmarking has been done yet.

---

## Release plan

v1 is a minimal implementation for local testing and validation. Planned work before a production release:

- `WatchSandboxes` state-change events (Running/Stopped transitions via `GET /1.0/events`; Deleted events are already implemented)
- Snap packaging fixes (socket path, content interface connection)
- CI coverage for the sandbox image build

---

## Further information

- Proto contract: `proto/compute_driver.proto`, vendored from NVIDIA/OpenShell (Apache-2.0); the rest of the repository is AGPLv3.
- Upstream OpenShell PR that added compute driver socket support (merged): https://github.com/NVIDIA/OpenShell/pull/1703
- `openshell-driver-vm` (libkrun microvm compute driver): https://github.com/NVIDIA/OpenShell/tree/main/crates/openshell-driver-vm
- LXD REST API specification: https://canonical.com/lxd/docs/latest/api/
- LXD instance configuration keys: https://documentation.ubuntu.com/lxd/en/latest/reference/instance_options/
- Snap interface documentation (lxd): https://snapcraft.io/docs/lxd-interface
- Repository: https://github.com/canonical/openshell-driver-lxd
- lxd-client crate plan: qwen3.6-experiment-local by Simon Fels
- https://github.com/markatcanonical/OpenShell/blob/snapify/rockcraft-vm-rootfs.yaml

---

## microVMs

LXD currently supports full QEMU-backed KVM VMs (`--vm`). Once LXD gains support for microVMs (lighter-weight QEMU VMs using the `microvm` machine type; faster boot, smaller footprint), `openshell-driver-lxd` will detect support via the `api_extensions` list returned by `GET /1.0`, and prefer microVMs over containers for new sandboxes. The instance type selection is abstracted behind an `InstanceShape` enum in `lxd-client` so the switch is a localized change (a config default, not a driver redesign).

---

## Spec history and changelog

| Author(s) | Status | Date | Comment |
|---|---|---|---|
| Kadin Sayani | Braindump | 15 Jun 2026 | Brain dump. |
| Kadin Sayani, Simon Fels | Drafting | 17 Jun 2026 | Met to align on goals for the initial implementation. Will use LXD containers initially and pivot to microVMs when LXD gains support. |
| Kadin Sayani | Drafting | 25 Jun 2026 | End-to-end bringup completed. Full stack verified: gateway → driver → LXD container → supervisor → agent. Key findings incorporated: container security profile requirements, supervisor invocation flags, all required env vars, DHCP client requirement, sandbox user requirement, create-then-start split, gateway auth for local dev. |
| Kadin Sayani | Drafting | 29 Jun 2026 | Switched gateway source to upstream `NVIDIA/OpenShell` main (PR #1703 merged). Re-synced vendored proto. All tests pass. |
| Kadin Sayani | Drafting | 30 Jun 2026 | Implemented `WatchSandboxes` Deleted events via in-process broadcast channel. Switched token delivery from host-side bind-mount (`--token-dir`, disk device) to LXD files API (`POST /1.0/instances/<name>/files`). All three upstream e2e tests pass (`smoke`, `sandbox_lifecycle`, `sandbox_labels`). |
| Kadin Sayani | Drafting | 1 Jul 2026 | Removed `security.syscalls.deny_default=false` (confirmed unnecessary: default deny list does not block `seccomp(2)`; `seccompNotifyDisallow` only injected when `intercept.*` keys are set, which we don't use). Removed `--mode=process` from supervisor invocation; added `nftables` to sandbox image for supervisor network mode. |
| Kadin Sayani | Drafting | 1 Jul 2026 | Removed LXD Network ACL egress enforcement. Root cause: attaching any ACL to a bridge network or NIC device adds an implicit default-reject for all unmatched traffic, including the supervisor proxy's own outbound relay connections. The proxy re-establishes TCP connections from `eth0` in the container's main namespace; the LXD ACL can't distinguish those from direct agent connections. L7 proxy is the sole enforcement layer. Verified `sandbox-policy-quickstart`: GET returns zen quote, POST returns structured `{"error":"policy_denied"}`. Updated SPEC.md networking section and examples table. |
| Kadin Sayani | Drafting | 1 Jul 2026 | Verified `policy-advisor` CTF: all 7 gates unlocked. Fixed sandbox image: ubuntu 24.04 `/etc/resolv.conf` symlink replaced with static file in init script; `isc-dhcp-client` → `dhcpcd`; `python3-minimal` → `python3` (full stdlib). Gate 7 SSRF override path: added `internal.corp.example.com` → `10.120.210.1` to container `/etc/hosts` so the mechanistic mapper can detect the private IP and include `allowed_ips` in the draft; the proxy then allows the CONNECT. |
