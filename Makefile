SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c
MAKEFLAGS += --no-builtin-rules

.PHONY: help build install auth test clean

help:
	@echo "tt-devpro — Available Commands"
	@echo ""
	@echo "  make install   Build and install tt-devpro to ~/.cargo/bin"
	@echo "  make build     Build the release binary (target/release/tt-devpro)"
	@echo "  make test      Run the test suite"
	@echo "  make auth      Log in through a browser and store the session cookie in 1Password"
	@echo "  make clean     Remove build artifacts"
	@echo ""
	@echo "After install:  tt-devpro settle --dry-run"

build:
	cargo build --release

install:
	cargo install --path .

auth:
	./auth.sh

test:
	cargo test

clean:
	cargo clean
