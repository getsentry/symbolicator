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

test: test-rust test-integration
.PHONY: test

test-rust:
	cargo test --workspace --all-features --locked
.PHONY: test-rust

test-integration: .venv/bin/python
	.venv/bin/pip install -U pytest pytest-localserver requests pytest-xdist pytest-icdiff boto3
	cargo build --locked
	@.venv/bin/pytest tests/integration -n12 -vv
.PHONY: test-integration

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

travis-upload-docs: docs
	cd site && zip -r gh-pages .
	zeus upload -t "application/zip+docs" site/gh-pages.zip \
		|| [[ ! "$(TRAVIS_BRANCH)" =~ ^release/ ]]
.PHONY: travis-upload-docs

# Style checking

style: style-rust style-python
.PHONY: style

style-python: .venv/bin/python
	.venv/bin/pip install -U black
	.venv/bin/black --check tests
.PHONY: style-python

style-rust:
	@rustup component add rustfmt --toolchain stable 2> /dev/null
	cargo +stable fmt -- --check
.PHONY: style-rust

# Linting

lint: lint-rust lint-python
.PHONY: lint

lint-python: .venv/bin/python
	.venv/bin/pip install -U flake8
	.venv/bin/flake8 tests
.PHONY: lint-python

lint-rust:
	@rustup component add clippy --toolchain stable 2> /dev/null
	cargo +stable clippy --all-features --workspace --tests --examples
.PHONY: lint-rust

# Formatting

format: format-rust format-python
.PHONY: format

format-rust:
	@rustup component add rustfmt --toolchain stable 2> /dev/null
	cargo +stable fmt
.PHONY: format-rust

format-python: .venv/bin/python
	.venv/bin/pip install -U black
	.venv/bin/black tests
.PHONY: format-python

# Dependencies

.venv/bin/python: Makefile
	rm -rf .venv
	$$SYMBOLICATOR_PYTHON_VERSION -m venv .venv
