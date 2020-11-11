<p align="center">
    <img src="artwork/logo.png" width="520">
    <br />
</p>

A symbolication service for native stacktraces and [minidumps] with [symbol
server] support. It's a flexible frontend for parts of the [symbolic] library.

[Documentation]

## Compiling

Symbolicator is currently not distributed as binary, which means you need to
compile it yourself. It is written in [Rust] and requires the latest stable Rust
compiler.

To compile, run:

```sh
cargo build --release
```

The resulting binary ends up in `target/release/symbolicator` along with a debug
information file. On Linux, debug information is part of the executable and
might need to be stripped using `objcopy`.

## Usage with Sentry

The following requires a recent git version of Sentry.

While Symbolicator aims to not be tied to Sentry's usecases, [Sentry] has a hard
dependency on Symbolicator to process native stacktraces. To get it running for
local development:

- In your `~/.sentry/sentry.conf.py`:

  ```python
  # Allow Symbolicator's request IP to fetch debug files from Sentry.
  INTERNAL_SYSTEM_IPS = ["127.0.0.1"]
  ```

- In your `~/.sentry/config.yml`:

  ```yaml
  symbolicator.enabled: true
  ```

Then run `sentry devservices up` to download and start Symbolicator.

## Development

To build Symbolicator, we require the **latest stable Rust**.

We use VSCode for development. This repository contains settings files
configuring code style, linters, and useful features. When opening the project
for the first time, make sure to _install the Recommended Extensions_, as they
will allow editor assist during coding.

The root of the repository contains a `Makefile` with useful commands for
development:

- `make check`: Runs code formatting checks and linters. This is useful before
  opening a pull request.
- `make test`: Runs unit tests, integration tests and Python package tests (see
  below for more information).
- `make all`: Runs all checks and tests. This runs most of the tasks that are
  also performed in CI.
- `make clean`: Removes all build artifacts, the virtualenv and cached files.

### Building and Running

The easiest way to rebuild and run Symbolicator is using `cargo`. Depending on
the configuration, you may need to have a local instance of Sentry running.

```bash
# Rebuild and run with all features
cargo run -- run
```

For quickly verifying that Symbolicator compiles after making some changes, you
can also use `cargo check`:

```bash
cargo check
```

### Local configuration

By default, Symbolicator listens on port `3021`. To override this and other
configuration values, create a file `local.yml` in the project root folder. It
is excluded from version control by default. Then, start Symbolicator and point
to the configuration file:

```bash
cargo run -- run --config local.yml
```

### Tests

The test suite comprises unit tests, and an integration test suite. Unit tests
are implemented as part of the Rust crates and can be run via:

```bash
# Tests for default features
make test-rust
```

The integration test suite requires `python`. By default, the integration test
suite will create a virtualenv, build the Symbolicator binary, and run a set of
integration tests:

```bash
# Create a new virtualenv, build Symbolicator and run integration tests
make test-integration

# Build and run a single test manually
make build
.venv/bin/pytest tests/integration -k <test_name>
```

**Note**: On macOS, the default file descriptor limit of `256` is too low and
causes test failures. Before running tests, consider to increase is to a higher
value:

```bash
ulimit -n 4096
```

### Linting

We use `rustfmt` and `clippy` from the latest stable channel for code formatting
and linting. To make sure that these tools are set up correctly and running with
the right configuration, use the following make targets:

```bash
# Format the entire codebase
make format

# Run clippy on the entire codebase
make lint
```

[documentation]: https://getsentry.github.io/symbolicator/
[sentry]: https://github.com/getsentry/sentry
[minidumps]: https://docs.sentry.io/platforms/minidump/
[symbol server]: https://en.wikipedia.org/wiki/Microsoft_Symbol_Server
[symbolic]: https://github.com/getsentry/symbolic
[rust]: https://www.rust-lang.org/
