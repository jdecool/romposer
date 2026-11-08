# Development tasks. `make` alone lists them.

LINUX_TARGETS := x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu
MACOS_TARGETS := aarch64-apple-darwin x86_64-apple-darwin
# Linux binaries build natively on Linux and through `cross` (Docker) elsewhere.
LINUX_CARGO ?= $(if $(filter Linux,$(shell uname -s)),cargo,cross)

define BANNER
 ____
|  _ \ ___  _ __ ___  _ __   ___  ___  ___ _ __
| |_) / _ \| '_ ` _ \| '_ \ / _ \/ __|/ _ \ '__|
|  _ < (_) | | | | | | |_) | (_) \__ \  __/ |
|_| \_\___/|_| |_| |_| .__/ \___/|___/\___|_|
                     |_|
endef
export BANNER

.DEFAULT_GOAL := help
.PHONY: help compile build build-linux build-macos test test-fixtures fmt lint check bench install clean

help: ## Show this help
	@printf '%s\n\n' "$$BANNER"
	@printf 'Usage: make <target>\n\n'
	@grep -E '^[a-z-]+:.*## ' $(MAKEFILE_LIST) | awk -F ':.*## ' '{ printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2 }'

compile: ## Debug build
	cargo build

build: ## Release build for this machine (target/release/romposer)
	cargo build --release --locked

build-linux: ## Release builds for Linux x86_64 and arm64 (needs `cross` off Linux)
	@for t in $(LINUX_TARGETS); do \
		[ "$(LINUX_CARGO)" = cargo ] && rustup target add $$t; \
		$(LINUX_CARGO) build --release --locked --target $$t || exit 1; \
	done

build-macos: ## Release builds for macOS arm64 and x86_64 (run on macOS)
	@for t in $(MACOS_TARGETS); do \
		rustup target add $$t && cargo build --release --locked --target $$t || exit 1; \
	done

test: ## Full test suite (golden tests need composer 2.11.1 on PATH)
	cargo test

test-fixtures: ## Composer installer fixtures pass rate
	cargo test --test installer_fixtures -- --nocapture 2>&1 | grep "fixtures passing"

fmt: ## Format the code
	cargo fmt

lint: ## Format check and clippy, as in CI
	cargo fmt --check
	cargo clippy -- -D warnings

check: lint test ## Everything CI runs

bench: build ## Install benchmark against composer (N=3 runs)
	bench/install.sh $(or $(N),3)

install: ## Install romposer into ~/.cargo/bin
	cargo install --path . --locked

clean: ## Remove build artefacts
	cargo clean
