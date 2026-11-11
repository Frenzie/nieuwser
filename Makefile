# The app is much more responsive in release mode; use `make dev` only while hacking.

# Use sccache automatically when it is installed and the caller did not choose a wrapper.
ifeq ($(origin RUSTC_WRAPPER),undefined)
SCCACHE := $(shell command -v sccache 2>/dev/null)
ifneq ($(SCCACHE),)
export RUSTC_WRAPPER := $(SCCACHE)
endif
endif
.PHONY: all build run dev slint-mcp test test-core check-core fmt check clean sccache-stats

all: build

build:
	cargo build --release -p nieuwser

run:
	cargo run --release -p nieuwser

dev:
	cargo run -p nieuwser

# Slint's embedded MCP server for AI-assisted debugging (UI tree, screenshots, input).
# Connect over Streamable HTTP at http://127.0.0.1:$(SLINT_MCP_PORT)/mcp. Set
# SLINT_BACKEND=headless to run without a display.
SLINT_MCP_PORT ?= 9315
slint-mcp:
	SLINT_EMIT_DEBUG_INFO=1 SLINT_MCP_PORT=$(SLINT_MCP_PORT) \
		cargo run -p nieuwser --features slint/mcp

test:
	cargo test --workspace

test-core:
	cargo test -p nieuwser-core

check-core:
	cargo check -p nieuwser-core

sccache-stats:
	sccache --show-stats

fmt:
	cargo fmt --all

check:
	cargo fmt --all --check
	cargo test --workspace

clean:
	cargo clean
