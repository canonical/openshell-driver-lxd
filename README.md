# openshell-driver-lxd

OpenShell Compute driver for LXD

**Status:** Early development. The core sandbox lifecycle (create/get/list/stop/delete,
token delivery, exec) works end-to-end against a real OpenShell gateway. A
number of features are not yet implemented — see
[Known limitations](#known-limitations) below.

## Overview

`openshell-driver-lxd` is an out-of-tree [OpenShell](https://github.com/NVIDIA/OpenShell)
compute driver backed by [LXD](https://github.com/canonical/lxd). It implements
OpenShell's `compute_driver.proto` contract and serves it over gRPC via a Unix 
domain socket, which the OpenShell gateway connects to at startup.

```
OpenShell gateway
    └── Unix socket (gRPC)
            └── openshell-driver-lxd
                    └── LXD REST API
                            └── LXD VM
```

## Demo

Creating an LXD-backed sandbox end to end — the driver and gateway logs on top,
the `openshell` CLI driving them below.

![Launching an LXD-backed OpenShell sandbox](demo.gif)

## Requirements

- Rust (stable, see `rust-toolchain.toml`)
- `protoc` (`apt install protobuf-compiler libprotobuf-dev`) for `computev1`'s proto codegen
- [LXD](https://github.com/canonical/lxd), initialized with a `default` storage pool and an `lxdbr0` network
- `skopeo`, `umoci`, and `mksquashfs` (`apt install skopeo umoci squashfs-tools`) — the driver uses these to pull and import sandbox OCI images into LXD on demand

## Quickstart

This walks through building the driver and wiring it up to a real OpenShell
gateway so you can create a sandbox end-to-end.

1. **Install and initialize LXD**, if you haven't already:

   ```sh
   sudo snap install lxd
   lxd init --auto
   ```

2. **Build and run the driver:**

   ```sh
   make build
   ./target/debug/openshell-driver-lxd \
       --socket /tmp/openshell-driver.sock \
       --gateway-grpc-port 17670
   ```

   No sandbox image needs to be pre-built or pre-loaded: the driver pulls the
   default image (`--default-image`, the upstream community
   `ghcr.io/nvidia/openshell-community/sandboxes/base:latest`) from the
   registry and imports it into LXD on first use, caching it by content
   digest. Pass `--default-image <oci-ref>` to boot from a different image.

   The supervisor binary comes from a *separate* image
   (`--supervisor-image`, the upstream
   `ghcr.io/nvidia/openshell/supervisor:latest`) and is mounted into every
   sandbox from a storage volume. The two are deliberately distinct: the
   supervisor image ships the binary on a minimal BusyBox rootfs and cannot
   serve as a sandbox rootfs, because BusyBox's `ip` has no `netns`
   subcommand and the supervisor's proxy mode needs it to isolate the
   sandbox.

   Importing an image needs scratch space for the OCI copy plus the unpacked
   rootfs — several GiB for a real sandbox image. That scratch lives in
   `--image-work-dir` (default `/var/cache/openshell/lxd-image-work`), which
   deliberately defaults to a disk-backed path rather than `TMPDIR`/`/tmp`,
   a memory-backed tmpfs on most modern distributions.

   `--gateway-grpc-port` must match the port the gateway is told to listen on
   below — the driver uses it to construct each sandbox's `OPENSHELL_ENDPOINT`.

3. **Start an OpenShell gateway pointed at the driver's socket**, using the
   out-of-tree driver flags. A plaintext gateway still enforces request
   authentication by default, so for local/dev use also pass a `--config`
   file disabling it:

   ```sh
   cat > /tmp/openshell-gateway.toml <<'EOF'
   [openshell.gateway.auth]
   allow_unauthenticated_users = true
   EOF

   openshell-gateway \
       --disable-tls \
       --bind-address 0.0.0.0 \
       --port 17670 \
       --drivers lxd \
       --compute-driver-socket /tmp/openshell-driver.sock \
       --db-url "sqlite:/tmp/openshell-gateway.db?mode=rwc" \
       --config /tmp/openshell-gateway.toml
   ```

   `--bind-address 0.0.0.0` is required: the default loopback-only bind is
   unreachable from sandboxes on LXD's `lxdbr0` bridge network. `--disable-tls`
   plus `allow_unauthenticated_users` are a plaintext, unauthenticated dev
   shortcut — **not** for production use; see
   [Security limitations](#security-limitations).

4. **Register the gateway with the CLI and create a sandbox:**

   ```sh
   openshell gateway add http://127.0.0.1:17670 --local --name lxd-demo
   openshell gateway select lxd-demo

   openshell sandbox create --name demo -- id
   openshell sandbox exec demo -- id
   openshell sandbox delete demo
   ```

## Security limitations

- **No default-deny egress or sandbox-to-sandbox network isolation.**
  Sandboxes can reach each other and the network freely today. `lxd-client`
  has the Network ACL APIs needed to build this, but nothing in the driver
  calls them yet.
- **`security.nesting=true` is the container's trust boundary.** This grants
  the `userns` capability, relaxes `/proc/sys` and cgroup mount restrictions,
  and allows AppArmor-stacking access — independent of
  `security.privileged`, which is not set.
- **PID limits are enforced, other cgroup limits are not.** Every sandbox
  gets `limits.processes` (`--default-max-processes`, default 4096) so one
  sandbox cannot fork-bomb its co-tenants, but there is no I/O or PID-cgroup
  budgeting beyond that, and CPU/memory are only set when the request asks
  for them.
- **No seccomp/AppArmor allowlist audit yet.** Sandboxes rely on LXD's
  default seccomp deny list (`kexec_load`, `open_by_handle_at`,
  `init_module`, `delete_module`), not a syscall allowlist scoped to what the
  supervisor actually needs.
- **`Ready=True` reflects LXD container status, not confirmed
  supervisor-to-gateway connectivity.** A sandbox can report `Ready=True` as
  soon as the LXD container reaches `Running`, before the supervisor inside
  has finished booting and connecting to the gateway. Correctly wiring this
  needs a guest-to-driver signal that containers don't provide; the fix lands
  with a planned microVM + `lxd-agent`-over-vsock transition, not before.

  As a partial mitigation, `create_sandbox` checks the instance again a few
  seconds after start and restarts it up to `--start-retries` times (default
  1) if its init has already exited. The container's init *is* the
  supervisor, so a supervisor that gives up during start-up — losing a race
  with the gateway finishing the sandbox record, say — otherwise leaves the
  instance `Stopped` with nothing to bring it back: LXD's `boot.autorestart`
  is VM-only and `boot.autostart` only covers daemon restarts.

## Images and Caching

The driver supports per-sandbox OCI images specified via `template.image` in the
gateway request (e.g. `docker://registry.example.com/org/sandbox:latest` or
`ghcr.io/org/custom-sandbox:v1`).

- **Contract:** `template.image` must be a purpose-built OpenShell sandbox OCI image
  bundling the OpenShell supervisor as init and conforming to the supervisor contract.
- **Digest-pinned resolution and caching:** On `create_sandbox`, the driver validates
  the OCI reference and resolves the manifest digest for the host architecture
  by reading the raw image index and selecting the matching `os`/`architecture`
  entry, so two architectures of the same tag never share a cache entry. It maps
  the digest to a local LXD image alias (e.g. `openshell-oci-<64-hex-sha256>`).
  If the alias is already present in LXD, it is reused immediately.
  If not cached, the driver pulls the image by digest using `skopeo`, unpacks it with
  `umoci`, packs it into squashfs and metadata archives, and imports it via LXD's
  split image REST API.
- **Tag mutation:** Because the cache is keyed on content digest rather than tag,
  if a tag points to a new digest, the driver will automatically pull and import the new
  image on first use.
- **Fallback:** If `template.image` is omitted or empty, the sandbox falls back to
  `--default-image` (default: the upstream community `ghcr.io/nvidia/openshell-community/sandboxes/base:latest`),
  which is resolved and imported through the same on-demand path.
- **Configuration flags:**
  - `--supervisor-image`: OCI image reference to extract the OpenShell supervisor binary from (default: `ghcr.io/nvidia/openshell/supervisor:latest`).
  - `--supervisor-bin`: optional path to a pre-extracted supervisor binary on the host (bypasses extraction).
  - `--supervisor-cache-dir`: host directory for caching extracted supervisor binaries by content digest (default: `/var/cache/openshell/lxd-supervisor`).
  - `--supervisor-storage-pool`: LXD storage pool for the supervisor and DHCP-client volumes. When unset, each sandbox's own pool (`driver_config.storage_pool`, itself defaulting to `default`) is used, so the auxiliary volumes always land beside the rootfs they attach to. Set it to pin every auxiliary volume to one pool.
  - `--image-work-dir`: host scratch directory for image conversion (default: `/var/cache/openshell/lxd-image-work`). Must not be a small tmpfs such as `/tmp`.
  - `--default-max-processes`: `limits.processes` applied to every sandbox, bounding its PID count (default: 4096; `0` leaves it unlimited). Overridable per sandbox via `driver_config.max_processes`.
  - `--start-retries`: how many times to restart a sandbox whose init exits immediately after the first start (default: 1; `0` disables).
  - `--image-pull-timeout-secs`: timeout for image inspection and pulling (default: 300s).
  - `--image-cache-alias-prefix`: prefix for cached LXD aliases (default: `openshell-oci-`).
  - `--skopeo-path`, `--umoci-path`, `--mksquashfs-path`: optional binary path overrides.

### Supervisor Binary Delivery via Custom Storage Volume

Upstream Docker and Podman drivers extract the supervisor binary (`/openshell-sandbox`) to a host cache and bind-mount it into sandboxes. For LXD, a host-path bind mount fails in clustered or distributed storage environments (e.g. Ceph) where containers may run on cluster nodes different from the driver host.

`openshell-driver-lxd` instead packages the supervisor binary into a digest-keyed LXD custom storage volume (`openshell-supervisor-<digest>`) on the sandbox's own storage pool (or `--supervisor-storage-pool` when pinned), following the pattern established in `canonical/workshop` (`lxd_backend_sdk.go`):
- **Volume layout & Mount distinction:** A `content-type: filesystem` storage volume is a filesystem tree. The volume contains `openshell-sandbox` at its root and is attached to each sandbox container as a read-only `disk` device mounted at directory `/opt/openshell/bin`. The injected guest init script (`/openshell-init.sh`) execs the binary at `/opt/openshell/bin/openshell-sandbox`.
- **Clustered LXD safety:** Because the disk device refers to a named storage-pool volume rather than a local host path, LXD manages replication and cluster-wide attachment automatically.
- **Idempotency & Race safety:** Volume creation is serialized in-process per pool and digest, and handles existing volume conflicts idempotently. Subsequent sandboxes reusing the same supervisor binary digest share the volume.

### Bundled Fallback DHCP Client Delivery

Guest networking on LXD containers relies on DHCP over `eth0`. Some base sandbox images (such as `ghcr.io/nvidia/openshell-community/sandboxes/base:latest`) do not bundle any DHCP client.

To guarantee sandboxes obtain an IP lease regardless of what packages are installed in the guest image:
- The driver vendors a static DHCP client (`udhcpc`) and an event script (`udhcpc.script`) embedded into the driver binary.
- On sandbox creation, a digest-keyed custom storage volume (`openshell-dhcp-client-<digest>`) is provisioned on the storage pool and mounted read-only at `/opt/openshell/net`.
- In `/openshell-init.sh`, the init script probes for existing image-provided DHCP clients (`udhcpc`, `dhclient`, `dhcpcd`). If none are present on `PATH`, it runs the bundled fallback client (`/opt/openshell/net/udhcpc`) with `/opt/openshell/net/udhcpc.script` in the background to acquire and apply the network lease.

## Sandbox State Reporting

The `Ready` condition's `reason` uses the cross-driver vocabulary upstream
defines in `openshell-core::driver_utils`, because the gateway keys real
behaviour off these exact strings — which reasons are transient (mapping to
`Provisioning` rather than `Error`) and which are eligible for recovery when
the gateway restarts:

| LXD state | reason | gateway phase |
| --- | --- | --- |
| `Running` / `Ready` | — (`Ready=True`) | `Ready` |
| `Stopped`, init exited by itself | `ContainerExited` | `Error` (terminal) |
| `Stopped`, stop requested | `ContainerStopped` | `Stopped` |
| `Stopped`, never started | `ContainerCreated` | `Provisioning` |
| `Starting` | `ContainerStarting` | `Provisioning` |
| `Frozen` | `ContainerPaused` | `Error` |

LXD reports the same `Stopped` status however an instance went down, so the
driver records `user.openshell.stop_intent` on the instance when it is asked
to stop one. Without it a user-requested stop is indistinguishable from a
crash and surfaces as `Error` instead of `Stopped`.

## Known limitations

- GPU requests attach every host GPU; an exact requested `count` isn't honored.
- `lxd-client` opens a fresh connection per request; no connection pooling.
- No MicroCloud / multi-node cluster scheduling — single LXD daemon only.
- Not yet packaged as a snap for production distribution (see [Snap](#snap)
  for the local build path that exists today).

## Snap

The repository ships a snap package definition in `snap/snapcraft.yaml`.

### Build the snap locally

```sh
snapcraft
```

Snapcraft uses LXD as its build environment — install and initialise it first
if needed:

```sh
sudo snap install lxd
lxd init --auto
sudo snap install snapcraft --classic
```

### Install and run

```sh
sudo snap install openshell-driver-lxd_*.snap --dangerous
sudo snap connect openshell-driver-lxd:lxd lxd
sudo openshell-driver-lxd
```

## License

Licensed under the [GNU Affero General Public License v3.0](LICENSE).

## Development

See [AGENTS.md](AGENTS.md) for development and contribution conventions.
