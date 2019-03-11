# symbolicator

> The symbolication service to end them all.

## Setup

Write this to a file:

```yaml
cache_dir: null
bind: 127.0.0.1:42069
```

All keys are optional, the values in this config are the defaults.

* `cache_dir`: Path to a directory, to cache symbols. If it is `null`, there is
  no caching. You really want caching in production.
* `bind`: Host and port for HTTP interface.

Start the server: `symbolicator -c ./foo.yml run`
