.DEFAULT_GOAL := lint

.PHONY: format lint test

format:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy --locked --all-targets -- -D warnings

test:
	cargo test --locked
