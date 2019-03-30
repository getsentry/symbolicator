export SYMBOLICATOR_PYTHON_VERSION := python3

all: check test
.PHONY: all

check: style lint
.PHONY: check

clean:
	cargo clean
	rm -rf .venv
.PHONY: clean

# Tests

test: test-rust test-integration
.PHONY: test

test-rust:
	cargo test --all --all-features
.PHONY: test-rust

test-integration: .venv/bin/python
	.venv/bin/pip install -U pytest pytest-rerunfailures pytest-localserver requests pytest-xdist pytest-icdiff boto3
	cargo build
	@.venv/bin/pytest tests --reruns 5 -n12 -vv
.PHONY: test-integration

# Style checking

style: style-rust style-python
.PHONY: style

style-python: .venv/bin/python
	@echo TODO
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
	cargo +stable clippy --all-features --all --tests --examples -- -D clippy::all
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
	virtualenv -p $$SYMBOLICATOR_PYTHON_VERSION .venv
