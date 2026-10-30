.DEFAULT_GOAL := check

.PHONY: ci check fmt fmt-check lint lint-all script-check test test-all integration build release-build smoke run clean

ci: check lint-all test-all integration smoke

check: fmt-check lint script-check test

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets -- -D warnings

lint-all:
	cargo clippy --all-targets --all-features -- -D warnings

script-check:
	sh -n install.sh

test:
	cargo test --all-targets

test-all:
	cargo test --all-targets --all-features

integration: build
	cargo test --test harness -- --ignored --test-threads=1

build:
	cargo build

release-build:
	cargo build --release

smoke: release-build
	@smoke_config=$$(mktemp -d); \
	trap 'rm -rf "$$smoke_config"' EXIT; \
	XDG_CONFIG_HOME="$$smoke_config" ./target/release/nonaut exec -- "printf nonaut-smoke" | grep -q 'nonaut-smoke'

run:
	cargo run --release

clean:
	cargo clean
