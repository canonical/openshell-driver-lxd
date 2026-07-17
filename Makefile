.PHONY: build release check test test-lxd-client test-driver setup-lxd-test-env fmt fmt-check clippy shellcheck doc static-checks proto sync-proto run clean sandbox-image

build:
	cargo build --workspace

release:
	cargo build --release --workspace

check:
	cargo check --workspace --all-targets

test: test-lxd-client test-driver

test-lxd-client: setup-lxd-test-env
	cargo test -p lxd-client

test-driver:
	lxc image info openshell-sandbox >/dev/null 2>&1 || $(MAKE) sandbox-image
	cargo test -p openshell-driver-lxd

# Provisions LXD for lxd-client's integration tests (see
# crates/lxd-client/tests/integration.rs). Idempotent; a prerequisite of
# `test` so the same command works locally and in CI.
setup-lxd-test-env:
	sudo ./scripts/setup-lxd-test-env.sh

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

# Lints the shell scripts under scripts/.
shellcheck:
	shellcheck scripts/*.sh

# Builds the API docs, treating warnings (e.g. broken intra-doc links) as
# errors so documentation stays valid.
doc:
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

# Runs all static checks: formatting, linting, shell linting, and docs.
static-checks: fmt-check clippy shellcheck doc

# Builds computev1 (and runs proto codegen if inputs changed).
proto:
	cargo build -p computev1

# Sync proto/compute_driver.proto with upstream NVIDIA/OpenShell main.
sync-proto:
	$(eval UPSTREAM := $(shell gh api repos/NVIDIA/OpenShell/contents/proto/compute_driver.proto --jq '.content' | base64 -d > /tmp/compute_driver_upstream.proto && echo /tmp/compute_driver_upstream.proto))
	@if diff -q /tmp/compute_driver_upstream.proto proto/compute_driver.proto > /dev/null 2>&1; then \
		echo "proto is already in sync with upstream main"; \
	else \
		cp /tmp/compute_driver_upstream.proto proto/compute_driver.proto && \
		cargo build --workspace && \
		if [ -t 0 ]; then \
			read -r -p "Would you like to commit changes to proto/compute_driver.proto (Y/n)? " answer; \
			if [ "$${answer:-y}" = "y" ] || [ "$${answer:-y}" = "Y" ]; then \
				git commit -S -s -m "chore(proto): sync compute_driver.proto with upstream main" -- proto/compute_driver.proto; \
			fi; \
		else \
			echo "==> proto/compute_driver.proto has been updated; please commit the change" >&2; \
			exit 1; \
		fi; \
	fi

run:
	cargo run -p openshell-driver-lxd -- $(ARGS)

# Builds the sandbox container image and publishes it to the local LXD
# image store under the openshell-sandbox alias.
sandbox-image:
	./scripts/build-sandbox-image.sh

clean:
	cargo clean
