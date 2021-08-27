---
title: Introduction
---

<p align="center">
    <img src="https://github.com/getsentry/symbolicator/raw/master/artwork/logo.png" width="520" alt="Symbolicator">
    <br />
</p>

Symbolicator is a standalone service that resolves function names, file location
and source context in native stack traces. It can process Minidumps and Apple
Crash Reports. Additionally, Symbolicator can act as a proxy to symbol servers
supporting multiple formats, such as Microsoft's symbol server or Breakpad
symbol repositories.

## Usage

Start the server with:

```shell
$ symbolicator run -c config.yml
```

The configuration file can be omitted. Symbolicator will run with default
settings in this case.

## Configuration

Write this to a file (`config.yml`):

```yaml
cache_dir: "/tmp/symbolicator"
bind: "0.0.0.0:3021"
logging:
  level: "info"
  format: "pretty"
  enable_backtraces: true
metrics:
  statsd: "127.0.0.1:8125"
  prefix: "symbolicator"
```

- `cache_dir`: Path to a directory to cache downloaded files and symbolication
  caches. Defaults to `/data` inside Docker which is already defined as a
  persistent volume, and `null` otherwise, which disables caching. **It is
  strictly recommended to configure caches in production!**
- `bind`: Host and port for HTTP interface.
- `logging`: Command line logging behavior.
    - `level`: Log level, defaults to `info`. Can be one of `off`, `error`,
      `warn`, `info`, `debug`, or `trace`.
    - `format`: The format with which to print logs. Defaults to `auto`. Can be
      one of: `json`, `simplified`, `pretty`, or `auto` (pretty on console,
      simplified on tty).
    - `enable_backtraces`: Whether backtraces for errors should be computed. This
      causes a slight performance hit but improves debuggability. Defaults to
      `true`.
- `metrics`: Configure a statsd server to send metrics to.
    - `statsd`: The host and port to send metrics to. Defaults to STATSD_SERVER
      environment variable or in case it is not defined, then it defaults to `null`,
      which disables metric submission.
    - `prefix`: A prefix for every metric, defaults to `symbolicator`.
    - `hostname_tag`: If set, report the current hostname under the given tag name for all metrics.
    - `environment_tag`: If set, report the current environment under the given tag name for all metrics.
- `sentry_dsn`: DSN to a Sentry project for internal error reporting. Defaults
  to `null`, which disables reporting to Sentry.
- `sources`: An optional list of preconfigured sources. If these are configured
  they will be used as default sources for symbolication requests and they will
  be proxied by the symbol proxy if enabled. The format for the sources here
  matches the sources in the HTTP API.
- `symstore_proxy`: Enables or disables the symstore proxy mode. Creates an
  endpoint to download raw symbols from configured sources Symbolicator as if it
  were a `symstore` (Microsoft Symbol Server) compatible server. Defaults to
  `true`.
- `connect_to_reserved_ips`: Allow reserved IP addresses for requests to
  sources. See [Security](#security). Defaults to `false`.
- `processing_pool_size`: The number of subprocesses in Symbolicator's internal
  processing pool. Defaults to the total number of logical CPUs on the machine.
- `max_concurrent_requests`: The maximum number of requests symbolicator will process concurrently. Further requests will result in a 503 status code.
  Set it to `null` to turn off the limit. Defaults to 120.

> All time units for the following configuration settings can be either a time
expression like `1s`.  Units can be `s`, `seconds`, `m`, `minutes`, `h`,
`hours`, `d`, `days`, `w`, `weeks`, `M`, `months`, `y`, `years`.

- `max_download_timeout`: The timeout for downloading debug files.
- `connect_timeout`: The timeout for establishing a connection to a symbol
  server to download debug files.
- `streaming_timeout`: The timeout for streaming the contents of a debug file.
- `caches`: Fine-tune cache expiry.

> Time units for caches may also be `null` to disable cache expiration.

  - `downloaded`: Fine-tune caches for downloaded files.
     - `max_unused_for`: Maximum duration to keep a file since last
       use of it.
     - `retry_misses_after`: Duration to wait before re-trying to
       download a file which was not found.
     - `retry_malformed_after`: Duration to wait before re-trying to
       download a file which was malformed.
     - `max_lazy_redownloads`: Symbolicator will fall back to a compatible but out-of-date cache version if available,
       and start computing the up-to-date version in the background. This option sets the maximum number of such lazy downloads that symbolicator will do concurrently. Defaults to 50.
  - `derived`: Fine-tune caches for files which are derived from
    downloaded files.  These files are usually versions of the
    downloaded files optimised for fast lookups.
    - `max_unused_for`: Maximum duration to keep a file since last
      use of it.
    - `retry_misses_after`: Duration to wait before re-trying to
      download a file which was not found.
    - `retry_malformed_after`: Duration to wait before re-trying to
      download a file which was malformed.
    - `max_lazy_recomputations`: Symbolicator will fall back to a compatible but out-of-date cache version if available,
      and start computing the up-to-date version in the background. This option sets the maximum number of such lazy computations that symbolicator will do concurrently. Defaults to 20.
  - `diagnostics`: This configures the duration diagnostics data
    will be stored in cache.  E.g. minidumps which failed to be
    processed correctly will be stored in this cache.
    - `retention`: Duration a file will be kept in this cache.

## Security

By default, Symbolicator does not try to download debug files from [reserved IP
ranges](https://en.wikipedia.org/wiki/Reserved_IP_addresses). This ensures that
no unintended connections are made to internal systems when source configuration
is passed in from an untrusted source.

To allow internal connections, set `connect_to_reserved_ips` to `true`.

An exception from this rule is the `"sentry"` source type. Sentry is expected to
run within the same network as Symbolicator, which is why it is exempt by
default.
