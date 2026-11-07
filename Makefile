# The app is much more responsive in release mode; use `make dev` only while hacking.
.PHONY: all build run dev test test-core check-core fmt check clean

all: build

build:
	cargo build --release -p nieuwser

run:
	cargo run --release -p nieuwser

dev:
	cargo run -p nieuwser

test:
	cargo test --workspace

test-core:
	cargo test -p nieuwser-core

check-core:
	cargo check -p nieuwser-core

fmt:
	cargo fmt --all

check:
	cargo fmt --all --check
	cargo test --workspace

clean:
	cargo clean
