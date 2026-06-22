.PHONY: build release check test setup-lxd-test-env fmt fmt-check clippy proto run clean

build:
	cargo build --workspace

release:
	cargo build --release --workspace

check:
	cargo check --workspace --all-targets

test: setup-lxd-test-env
	cargo test --workspace

# Provisions LXD for lxd-client's integration tests (see
# crates/lxd-client/tests/integration.rs). Idempotent; a prerequisite of
# `test` so the same command works locally and in CI.
setup-lxd-test-env:
	./scripts/setup-lxd-test-env.sh

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Builds computev1 (and runs proto codegen if inputs changed).
proto:
	cargo build -p computev1

run:
	cargo run -p openshell-driver-lxd -- $(ARGS)

clean:
	cargo clean
