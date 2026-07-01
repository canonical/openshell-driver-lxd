.PHONY: build release check test fmt fmt-check clippy proto sync-proto run clean

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

clean:
	cargo clean
