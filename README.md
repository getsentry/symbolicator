# Symbolicator

A symbolication service for native stacktraces and minidumps with symbol server
support.

## Setup

### Config

Write this to a file (config.yml):

```yaml
cache_dir: /tmp/symbolicator
bind: 127.0.0.1:3021
logging:
  level: debug
  format: pretty
  enable_backtraces: true
metrics:
  statsd: 127.0.0.1:8125
  prefix: symbolicator
sentry_dsn: https://mykey@sentry.io/4711
```

- `cache_dir`: Path to a directory to cache downloaded files and symbolication
  caches. Defaults to `null`, which disables caching. **It is strictly
  recommended to configure caches in production!** See [Caching](#ref-caching)
  for more information.
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
- `metrics`: Configure a statsd server to send metrics to. See
  [Metrics](#ref-metrics) for more information.
  - `statsd`: The host and port to send metrics to. Defaults to `null`, which
    disables metric submission.
  - `prefix`: A prefix for every metric, defaults to `symbolicator`.
- `sentry_dsn`: DSN to a Sentry project for internal error reporting. Defaults
  to `null`, which disables reporting to Sentry.

### Starting

Start the server with:

```bash
$ symbolicator run -c config.yml
```

The configuration file can be omitted. Symbolicator will run with default
settings in this case.

## Caching

<a name=ref-caching />

If you provide a `cache_dir`, the server assumes to have full control over that
directory. Pointing other instances to the same directory will cause
concurrency issues as access to files is only synchronized in-memory.

When a cache file is accessed, its `mtime` is bumped. `mtime` of a file
therefore equals to "Last Used".

The cache is currently unbound, there is no expiration logic. It is recommended
to prune files yourself, starting with files having the oldest `mtime`.
Effectively implementing a LRU cache from the outside of the process. You
should be able to `unlink` files while the server is running without it causing
issues (assuming a POSIX filesystem).

## Resource Usage

`symbolicator` spawns at least two threads per core: one for downloading from
external sources and one for symbolication. Requests are both CPU and IO
intensive. It is expected that the service is still IO bound.

`symbolicator` mmaps a lot to save memory. It should not consume a lot of
memory at all.

## Metrics

<a name=ref-metrics />

The `metrics` value in your config is an object with two keys:

```yaml
metrics:
  statsd: 127.0.0.1:1234
  prefix: sentry.symbolicator
```

`statsd` points to your service, `prefix` is the prefix added in front of all
metrics. Both keys are mandatory if you set `metrics` to a non-`null` value.
