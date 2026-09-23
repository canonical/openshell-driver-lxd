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
| `make test` | provisions LXD for lxd-client's integration tests, then `cargo test --workspace` |
| `make test-conformance` | runs upstream OpenShell's conformance suite against the driver (see `scripts/conformance.sh`; the environment is `scripts/openshell-env.sh`) |
| `make test-upstream-e2e` | runs upstream OpenShell's policy, Landlock and inference e2e tests against the driver (see `scripts/upstream-e2e.sh`) |
| `make fmt` / `make fmt-check` | format / check formatting |
| `make clippy` | `cargo clippy --workspace --all-targets -- -D warnings` |
| `make proto` | rebuild `computev1` (forces proto codegen) |
| `make rock` | pack the OCI rock with Rockcraft |
| `make test-rock` | run smoke tests against the packed rock |
| `make run` | run the driver binary |
| `make release` | `cargo build --release --workspace` |
| `make clean` | `cargo clean` |

## Conventions

- **Spelling**: US English spelling throughout (e.g. "organization", "color",
  "initialize").
- **Commit format**: see [COMMITS.md](COMMITS.md) for types, scopes, and
  signing requirements.
- **License**: AGPLv3 (see [LICENSE](LICENSE)).

## Rock packaging

The rock definition lives in `rockcraft.yaml` (base `ubuntu@26.04`). It bundles
`openshell-gateway` (upstream NVIDIA/OpenShell) and `openshell-driver-lxd`
under Pebble. A second rock definition lives in `rocks/supervisor/rockcraft.yaml`
(base `bare`); it ships the statically-linked `openshell-sandbox` boundary
binary that the driver extracts into every sandbox, built from the same pinned
upstream revision as the gateway. A GitHub Actions workflow
(`.github/workflows/rock.yaml`) builds and smoke-tests both rocks
(`tests/rock/smoke.sh`, `tests/rock/smoke-supervisor.sh`) on every push/PR to
`main`, and assembles/pushes multi-arch OCI images and manifests to GHCR on
push to `main` (`ghcr.io/<owner>/openshell-gateway` and
`ghcr.io/<owner>/openshell-supervisor`).
