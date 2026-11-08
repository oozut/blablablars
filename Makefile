CARGO ?= cargo
PYTHON ?= python3
VENV ?= .venv
PIP := $(VENV)/bin/pip
PY := $(VENV)/bin/python
MATURIN := $(VENV)/bin/maturin

# The bindings must not be linked into the Rust test binaries.
RUST_PACKAGES := --workspace --exclude blablablars-python

# The example Rust extension is a `cdylib` and its own workspace; tests build it
# into this directory (see crates/blablablars-core/tests/dynamic_extension.rs).
EXAMPLE_DIR := target/plugin-fixture
EXAMPLE_NAME := blablablars_example_rust_plugin
SOEXT := $(shell uname -s | grep -qi darwin && echo dylib || echo so)

.DEFAULT_GOAL := help

.PHONY: help build release test lint fmt python-venv python-develop python-test example demo demo-inprocess clean

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | \
		awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'

build: ## Build the workspace (debug, no Python crate)
	$(CARGO) build $(RUST_PACKAGES)

release: ## Build the shippable `blablablars` binary
	$(CARGO) build --release -p blablablars-cli

test: ## Rust tests + formatting + clippy
	$(CARGO) fmt --all --check
	$(CARGO) clippy $(RUST_PACKAGES) --all-targets -- -D warnings
	$(CARGO) test $(RUST_PACKAGES)

fmt: ## Format Rust sources
	$(CARGO) fmt --all

lint: ## Clippy only
	$(CARGO) clippy $(RUST_PACKAGES) --all-targets -- -D warnings

python-venv: ## Create the local virtualenv
	$(PYTHON) -m venv $(VENV)
	$(PIP) install --quiet --upgrade maturin

python-develop: python-venv ## Build and install the extension into $(VENV)
	VIRTUAL_ENV=$(abspath $(VENV)) $(MATURIN) develop --release --manifest-path python/pyproject.toml

python-test: python-develop example ## Run the Python test suite
	cd python && ../$(PY) -m unittest discover -s tests -v

example: ## Build the example Rust extension (a real cdylib plugin)
	$(CARGO) build --manifest-path examples/rust-plugin/Cargo.toml --target-dir $(EXAMPLE_DIR)

demo: release ## Run the CLI with the example Python plugin (child process)
	$(CARGO) build --release -p blablablars-cli
	target/release/blablablars run "hello brave world" --json \
		--plugin "$(abspath $(VENV))/bin/python -m blablablars.runtime examples/python/hello_plugin.py"

demo-inprocess: release python-develop example ## Rust and Python extensions inside one binary
	target/release/blablablars run "hello brave world" --json \
		--plugin "rust:$(EXAMPLE_DIR)/debug/lib$(EXAMPLE_NAME).$(SOEXT)" \
		--python-path python \
		--plugin python:examples/python/hello_plugin.py

clean: ## Remove build artefacts
	$(CARGO) clean
	rm -rf python/blablablars/_native*.so python/blablablars/__pycache__ python/tests/__pycache__ build dist
