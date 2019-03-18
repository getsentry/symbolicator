export SEMAPHORE_PYTHON_VERSION := python3

.venv/bin/python: Makefile
	rm -rf .venv
	virtualenv -p $$SEMAPHORE_PYTHON_VERSION .venv

integration-test: .venv/bin/python
	.venv/bin/pip install -U pytest pytest-rerunfailures pytest-localserver requests pytest-xdist
	cargo build
	@.venv/bin/pytest tests --reruns 5 -n12 -vv
.PHONY: integration-test

check: lint integration-test
.PHONY: check

lint: pylint rslint
.PHONY: lint

pylint: .venv/bin/python pyformat
	.venv/bin/pip install -U flake8
	.venv/bin/flake8 tests
.PHONY: pylint

rslint: rsformat
	cargo clippy

format: pyformat rsformat
.PHONY: format

rsformat:
	cargo +stable fmt
.PHONY: rsformat

pyformat: .venv/bin/python
	.venv/bin/pip install -U black
	.venv/bin/black tests
.PHONY: pyformat
