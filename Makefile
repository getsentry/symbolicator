export SEMAPHORE_PYTHON_VERSION := python3

venv/bin/python: Makefile
	rm -rf venv
	virtualenv -p $$SEMAPHORE_PYTHON_VERSION venv

integration-test: venv/bin/python
	venv/bin/pip install -U pytest pytest-rerunfailures pytest-localserver requests flask
	cargo build
	@venv/bin/pytest tests --reruns 5 -vv
.PHONY: integration-test

check: lint
.PHONY: check

lint: pylint rslint
.PHONY: lint

pylint: venv/bin/python
	venv/bin/pip install -U black flake8
	venv/bin/black tests
	venv/bin/flake8 tests
.PHONY: pylint

rslint:
	cargo +nightly fmt
	cargo clippy
