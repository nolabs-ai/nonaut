.DEFAULT_GOAL := check

.PHONY: ci check fmt fmt-check lint script-check test integration build smoke run clean

ci: check integration smoke

check: fmt-check lint script-check test

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets -- -D warnings
	cargo clippy --all-targets --all-features -- -D warnings

script-check:
	sh -n install.sh

test:
	cargo test --all-targets
	cargo test --all-targets --all-features

integration: build
	cargo test --test harness -- --ignored --test-threads=1

build:
	cargo build --release

smoke: build
	./target/release/nonaut exec -- "printf nonaut-smoke" | grep -q 'nonaut-smoke'

run:
	cargo run --release

clean:
	cargo clean
