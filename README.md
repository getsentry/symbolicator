# symbolicator

> The symbolication service to end them all.

## Setup

### Config

Write this to a file:

```yaml
cache_dir: null
bind: 127.0.0.1:42069
metrics: null
```

* `cache_dir`: Path to a directory, to cache symbols. If it is `null`, there is
  no caching. You really want caching in production. See
  [Caching](#ref-caching).
* `bind`: Host and port for HTTP interface.
* `metrics`: If set, symbolicator will send metrics to a statsd service. See [Metrics](#ref-metrics)

### Starting

Start the server with: `symbolicator -c ./foo.yml run`

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

## Resource usage

`symbolicator` spawns at least two threads per core: One for downloading from
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
