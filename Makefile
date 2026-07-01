.PHONY: build release check test fmt fmt-check clippy proto run clean sandbox-image e2e

build:
	cargo build --workspace

release:
	cargo build --release --workspace

check:
	cargo check --workspace --all-targets

test:
	cargo test --workspace

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

# Builds the sandbox container image and publishes it to the local LXD
# image store under the openshell-sandbox alias.
sandbox-image:
	./scripts/build-sandbox-image.sh

clean:
	cargo clean

e2e: ## Run e2e tests against a real LXD daemon (requires OPENSHELL_REPO)
	scripts/e2e-lxd.sh
