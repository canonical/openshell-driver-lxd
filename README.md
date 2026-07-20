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

## Requirements

- Rust (stable, see `rust-toolchain.toml`)
- `protoc` (`apt install protobuf-compiler libprotobuf-dev`) for `computev1`'s proto codegen
- [LXD](https://github.com/canonical/lxd), initialized with a `default` storage pool and an `lxdbr0` network

## Quickstart

This walks through building the driver, publishing the sandbox image, and
wiring both up to a real OpenShell gateway so you can create a sandbox
end-to-end.

1. **Install and initialize LXD**, if you haven't already:

   ```sh
   sudo snap install lxd
   lxd init --auto
   ```

2. **Build and publish the sandbox container image** under the
   `openshell-sandbox` alias (this drives `scripts/build-sandbox-image.sh`):

   ```sh
   make sandbox-image
   ```

3. **Build and run the driver:**

   ```sh
   make build
   ./target/debug/openshell-driver-lxd \
       --socket /tmp/openshell-driver.sock \
       --default-image openshell-sandbox \
       --gateway-grpc-port 17670
   ```

   `--gateway-grpc-port` must match the port the gateway is told to listen on
   below — the driver uses it to construct each sandbox's `OPENSHELL_ENDPOINT`.

4. **Start an OpenShell gateway pointed at the driver's socket**, using the
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

5. **Register the gateway with the CLI and create a sandbox:**

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
- **No seccomp/AppArmor allowlist audit yet.** Sandboxes rely on LXD's
  default seccomp deny list (`kexec_load`, `open_by_handle_at`,
  `init_module`, `delete_module`), not a syscall allowlist scoped to what the
  supervisor actually needs.
- **Every sandbox runs the same fixed base image**, regardless of
  `template.image` in the request — per-template image selection isn't
  consulted yet.
- **`Ready=True` reflects LXD container status, not confirmed
  supervisor-to-gateway connectivity.** A sandbox can report `Ready=True` as
  soon as the LXD container reaches `Running`, before the supervisor inside
  has finished booting and connecting to the gateway. Correctly wiring this
  needs a guest-to-driver signal that containers don't provide; the fix lands
  with a planned microVM + `lxd-agent`-over-vsock transition, not before.

## Known limitations

- GPU requests attach every host GPU; an exact requested `count` isn't honored.
- No image auto-import — `make sandbox-image` (or an equivalent manual
  import) is a prerequisite; the driver only fails fast if the alias is
  missing at startup, it doesn't build or fetch one.
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
