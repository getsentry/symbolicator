<p align="center">
    <img src="artwork/logo.png" width="520">
    <br />
</p>

A symbolication service for native stacktraces and [minidumps] with [symbol
server] support. It's a flexible frontend for parts of the [symbolic] library.

[Documentation]

## Compiling

Symbolicator is currently not distributed as binary which means you need to
compile it yourself. It's written in [Rust] and thus requires a recent rust
installation. We generally track latest stable.

To compile run this:

    cargo build --release

The resulting binary ends up in `target/release/symbolicator`.

## Usage with Sentry

The following requires a recent git version of Sentry.

While Symbolicator aims to not be tied to Sentry's usecases, [Sentry] has a hard
dependency on Symbolicator to process native stacktraces. To get it running for
local development:

- In your `~/.sentry/sentry.conf.py`:

  ```python
  # Whitelist Symbolicator's request IP to fetch debug symbols from Sentry.
  INTERNAL_SYSTEM_IPS = ["127.0.0.1"]
  ```

- In your `~/.sentry/config.yml`:

  ```yaml
  symbolicator.enabled: true
  ```

Then run `sentry devservices up` to download and start Symbolicator.

[documentation]: https://getsentry.github.io/symbolicator/
[sentry]: https://github.com/getsentry/sentry
[minidumps]: https://docs.sentry.io/platforms/minidump/
[symbol server]: https://en.wikipedia.org/wiki/Microsoft_Symbol_Server
[symbolic]: https://github.com/getsentry/symbolic
[rust]: https://www.rust-lang.org/
