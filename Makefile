CARGO ?= cargo
PYTHON ?= python3
VENV ?= .venv
PIP := $(VENV)/bin/pip
PY := $(VENV)/bin/python
MATURIN := $(VENV)/bin/maturin

# The bindings must not be linked into the Rust test binaries.
RUST_PACKAGES := --workspace --exclude blablablars-python

.DEFAULT_GOAL := help

.PHONY: help build release test lint fmt python-venv python-develop python-test demo clean

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

python-test: ## Run the Python test suite (builds the extension first)
	cd python && ../$(PY) -m unittest discover -s tests -v

demo: release ## Run the CLI with the example Python plugin
	$(CARGO) build --release -p blablablars-cli
	target/release/blablablars run "hello brave world" --json \
		--plugin "$(abspath $(VENV))/bin/python -m blablablars.runtime examples/python/hello_plugin.py"

clean: ## Remove build artefacts
	$(CARGO) clean
	rm -rf python/blablablars/_native*.so python/blablablars/__pycache__ python/tests/__pycache__ build dist
