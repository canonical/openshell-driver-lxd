# AGENTS.md — openshell-driver-lxd Agent Instructions

`openshell-driver-lxd` is an out-of-tree [OpenShell](https://github.com/NVIDIA/OpenShell)
compute driver. It implements OpenShell's `compute_driver.proto` contract
(OpenShell PR #1703) over gRPC via a Unix domain socket, using
[LXD](https://github.com/canonical/lxd) as the compute backend.

## Build and test

Requires `protoc` (`apt install protobuf-compiler`) for `computev1`'s build
script.

| Target | Description |
|---|---|
| `make build` | `cargo build --workspace` |
| `make check` | `cargo check --workspace --all-targets` |
| `make test` | `cargo test --workspace` |
| `make fmt` / `make fmt-check` | format / check formatting |
| `make clippy` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `make proto` | rebuild `computev1` (forces proto codegen) |
| `make run` | run the driver binary |
| `make release` | `cargo build --release --workspace` |
| `make clean` | `cargo clean` |

## Conventions

- **Spelling**: US English spelling throughout (e.g. "organization", "color",
  "initialize").
- **Commit format**: see [COMMITS.md](COMMITS.md) for types, scopes, and
  signing requirements.
- **License**: AGPLv3 (see [LICENSE](LICENSE)).
