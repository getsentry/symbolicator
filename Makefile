SHELL = /bin/bash
export SYMBOLICATOR_PYTHON_VERSION := python3

all: check test
.PHONY: all

check: style lint
.PHONY: check

clean:
	cargo clean
	rm -rf .venv
.PHONY: clean

# Builds

build:
	cargo build
.PHONY: build

release:
	cargo build --release --locked
	objcopy --only-keep-debug target/release/symbolicator{,.debug}
	objcopy --strip-debug --strip-unneeded target/release/symbolicator
	objcopy --add-gnu-debuglink target/release/symbolicator{.debug,}
.PHONY: release

# Tests

test:
	cargo test --workspace --all-features --locked
.PHONY: test

# Documentation

docs: .venv/bin/python
	.venv/bin/pip install -U mkdocs mkdocs-material pygments
	.venv/bin/mkdocs build
	touch site/.nojekyll
.PHONY: docs

docserver: .venv/bin/python
	.venv/bin/pip install -U mkdocs mkdocs-material pygments
	.venv/bin/mkdocs serve
.PHONY: doc

# Style checking

style:
	@rustup component add rustfmt --toolchain stable 2> /dev/null
	cargo +stable fmt -- --check
.PHONY: style

# Linting

lint:
	@rustup component add clippy --toolchain stable 2> /dev/null
	cargo +stable clippy --all-features --workspace --tests --examples
.PHONY: lint

# Formatting

format:
	@rustup component add rustfmt --toolchain stable 2> /dev/null
	cargo +stable fmt
.PHONY: format-rust

# Dependencies (currently needed for docs)

.venv/bin/python: Makefile
	rm -rf .venv
	$$SYMBOLICATOR_PYTHON_VERSION -m venv .venv

# Build GoCD pipelines

gocd:
	@ rm -rf ./gocd/generated-pipelines
	@ mkdir -p ./gocd/generated-pipelines
	@ cd ./gocd/templates && jb install
	@ find . -type f \( -name '*.libsonnet' -o -name '*.jsonnet' \) -print0 | xargs -n 1 -0 jsonnetfmt -i
	@ find . -type f \( -name '*.libsonnet' -o -name '*.jsonnet' \) -print0 | xargs -n 1 -0 jsonnet-lint -J ./gocd/templates/vendor
	@ cd ./gocd/templates && jsonnet -J vendor -m ../generated-pipelines ./symbolicator.jsonnet
	@ cd ./gocd/generated-pipelines && find . -type f \( -name '*.yaml' \) -print0 | xargs -n 1 -0 yq -p json -o yaml -i
.PHONY: gocd
