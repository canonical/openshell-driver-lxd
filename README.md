# openshell-driver-lxd

OpenShell Compute driver for LXD

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

## Build and run

```sh
make build
make run -- --socket /var/run/openshell-driver.sock
```

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
