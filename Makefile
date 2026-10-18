# The app is much more responsive in release mode; use `make dev` only while hacking.
.PHONY: all build run dev test fmt check clean

all: build

build:
	cargo build --release -p nieuwser

run:
	cargo run --release -p nieuwser

dev:
	cargo run -p nieuwser

test:
	cargo test -p nieuwser-core

fmt:
	cargo fmt --all

check:
	cargo fmt --all --check
	cargo test -p nieuwser-core

clean:
	cargo clean
