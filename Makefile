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

# Proto files vendored from upstream NVIDIA/OpenShell. compute_driver.proto is
# the driver contract itself; options.proto defines the custom field and method
# options it imports (e.g. the `secret` field option on sandbox_token) and must
# be resolvable on protoc's include path for codegen to succeed.
UPSTREAM_PROTOS := compute_driver.proto options.proto

# Sync proto/ with upstream NVIDIA/OpenShell main.
sync-proto:
	@changed=""; \
	for p in $(UPSTREAM_PROTOS); do \
		gh api repos/NVIDIA/OpenShell/contents/proto/$$p --jq '.content' \
			| base64 -d > /tmp/openshell_upstream_$$p; \
		if [ ! -s /tmp/openshell_upstream_$$p ]; then \
			echo "ERROR: failed to fetch proto/$$p from upstream" >&2; \
			exit 1; \
		fi; \
		if ! diff -q /tmp/openshell_upstream_$$p proto/$$p > /dev/null 2>&1; then \
			cp /tmp/openshell_upstream_$$p proto/$$p; \
			changed="$$changed proto/$$p"; \
		fi; \
	done; \
	if [ -z "$$changed" ]; then \
		echo "protos are already in sync with upstream main"; \
	else \
		echo "==> updated:$$changed"; \
		cargo build --workspace && \
		if [ -t 0 ]; then \
			read -r -p "Would you like to commit changes to$$changed (Y/n)? " answer; \
			if [ "$${answer:-y}" = "y" ] || [ "$${answer:-y}" = "Y" ]; then \
				git commit -S -s -m "chore(proto): sync protos with upstream main" --$$changed; \
			fi; \
		else \
			echo "==>$$changed updated; please commit the change" >&2; \
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
