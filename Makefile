.PHONY: build release check test fmt fmt-check clippy proto run clean

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

clean:
	cargo clean
