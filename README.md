<p align="center">
    <img src="artwork/logo.png" width="520">
    <br />
</p>

A symbolication service for native stacktraces and
[minidumps](https://docs.sentry.io/platforms/minidump/) with [symbol
server](https://en.wikipedia.org/wiki/Microsoft_Symbol_Server)
support.  It's a flexible frontend for parts of the
[symbolic](https://github.com/getsentry/symbolic) library.

## Compiling

Symbolicator is currently not distributed as binary which means you need to
compile it yourself.  It's written in [rust](https://www.rust-lang.org/) and
thus requires a recent rust installation.  We generally track latest stable.

To compile run this:

    cargo build --release

The resulting binary ends up in `target/release/symbolicator`.

## Setup

Symbolicator can be configured to work as a standalone system or to fit into a
Sentry installation. Using Sentry and just want to get this dependency running?
Skip to [the bottom of the page](#ref-sentry).

### Config

Write this to a file (`config.yml`):

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
- `sources`: An optional list of preconfigured sources.  If these are configured
  they will be used as default sources for symbolication requests and they will
  be proxied by the symbol proxy if enabled.  The format for the sources here
  matches the sources in the HTTP request.
- `symstore_proxy`: enables or disables the symstore proxy mode.  It's on by
  default but requires the `sources` key to be configured.  The symstore proxy
  lets you download raw symbols from the symbolicator as if it was a symstore
  (Microsoft Symbol Server) compatible server.
- `connect_to_unsafe_ips`: Allow reserved IP addresses for requests to sources.
    
   By default (value of `false`) source configs are not allowed to connect to
   internal IP networks or loopback, because most types of sources are
   considered untrusted user input.
    
   An exception from this rule is the Sentry source type, which is always
   allowed to connect to any kind of IP address.

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

## Sources

For the symbolicator to operate correctly it needs to be pointed to at least one
source.  It supports various different backends for where it looks for symbols.
There are two modes for the symbolicator with regards to how it looks for symbols.
One is the preconfigured mode where the `sources` key is configured right in the
config file.  The second is one where the sources are defined with the HTTP request
to the symbolication API.

If you want to use the symbolicator as a symstore compatible proxy you need to
preconfigure the sources.

Example configuration:

```yaml
sources:
  - id: my-bucket-linux
    type: s3
    bucket: my-project-my-bucket
    region: us-east-1
    prefix: /linux
    access_key: AMVSAVWEXRIRJPOMCKWN
    secret_key: Lqnc45YWr9y7qftCI+vST/1ZPmmw1H6SkbIf2v/8
    filters:
      filetypes: [elf_debug, elf_code]
    layout:
      type: native
      casing: lowercase
  - id: my-bucket-windows
    type: s3
    bucket: my-project-my-bucket
    region: us-east-1
    prefix: /windows
    access_key: AMVSAVWEXRIRJPOMCKWN
    secret_key: Lqnc45YWr9y7qftCI+vST/1ZPmmw1H6SkbIf2v/8
    filters:
      filetypes: [pe, pdb]
    layout:
      type: native
      casing: lowercase
  - id: microsoft
    type: http
    filters:
      filetypes: [pe, pdb]
    url: https://msdl.microsoft.com/download/symbols/
```

Sources are ordered by priority.  Each source needs at least two keys:

- `id`: the ID of the source.  This can be freely chosen and is used to identify
  cache files in the cache folder
- `type`: defines the type of the source (`http`, `s3`, `gcs` or `sentry`)

These are common parameters that work on most symbol sources (except `sentry`):

- `filters`: a set of filters to reduce the number of unnecessary hits on a symbol
  server.  This configuration key is an object with two keys:
  - `filetypes`: a list of file types to restrict the server to.  Possible values:
    `pe`, `pdb`, `mach_debug`, `mach_code`, `elf_debug`, `elf_code`, `breakpad`)
  - `path_patterns`: a list of glob matches that need to be matched on the image
    name.  If the debug image has no name it will never match here.
- `layout`: configures the file system layout of the sources.  This configuration
  key is an object with two keys:
  - `type`: defines the general layout of the directory.  Possible values are
    `native`, `symstore`, `symstore_index2` and `ssqp`.  `native` uses the file
    type's native format.  `symstore` and `ssqp` both use the Microsoft Symbol Server
    format but control the case conventions.  `symstore` uses the conventional
    casing rules for signatures and filenames, `ssqp` uses the Microsoft SSQP
    casing rules instead.  Additionally `symstore_index2` works like `symstore`
    but uses the "Two tier" (index2.txt) layout where the first two characters of
    the filename are used as a toplevel extra folder.
  - `casing`: enforces a casing style.  The default is not to touch the casing and
    forward it unchanged.  If the backend does not support a case insensitive
    backend (eg: S3) then it's recommended to set this to `lowercase` to enforce
    changing all to lowercase.  Possible values: `default`, `lowercase`, `uppercase`.

### `http` source

The HTTP source lets one fetch symbols from a Microsoft Symbol Server or similar
systems.  There is some flexibility to how it operates.

- `url`: This defines the URL where symbolicator should be fetching from.  For
  instance this can be `https://msdl.microsoft.com/download/symbols/` to point it
  to the official microsoft symbol server.
- `headers`: an optional dictionary of headers that should be sent with the HTTP
  requests.  This can be used for instance to configure HTTP basic auth configuration.

### `s3` source

This source connects straight to an S3 bucket and looks for symbols there.  It's
recommended for this to be configured with explicit `lowercase` casing to avoid
issues as the SSQP protocol demands case insensitive lookups.

- `bucket`: the name of the S3 bucket
- `prefix`: a path prefix to put in front of all keys (eg: `/windows`)
- `region`: the AWS region where the bucket is located
- `access_key`: the AWS access key to use
- `secret_key`: the AWS secret key to use

### `gcs` source

This source connects to a GCS bucket and looks for symbols there.  It behaves similarly
to `s3` but uses different credentials:

- `bucket`: the name of the GCS bucket
- `prefix`: a path prefix to put in front of all keys (eg: `/windows`)
- `private_key`: the GCS private key (base64 encoded and with optional PEM envelope)
- `client_email`: the GCS client email for authentication

### `sentry` source

This points the symbolicator at a sentry installation to fetch customer supplied
symbols from there.  This source is only usable for on-prem hosted sentry installations
as a system key is required to fetch symbols.

## About SSQP and SymStore Compatibility

The symbolicator attempts to stay compatible with the Microsoft Symbol Server which is
also commonly known as "SymStore" or "SymSrv".  The directory layout (or rather the HTTP
query protocol based on it) was recently enhanced for non Windows platforms by the Microsoft
.NET team.  This protocol has been dubbed [SSQP](https://github.com/dotnet/symstore/blob/master/docs/specs/SSQP_Key_Conventions.md).

When the symbolicator operates in proxy mode incoming queries below the `/symbols` endpoint
must conform to the SSQP protocol.  There are however one extensions to the protocol we added:

When fetching ELF or MachO symbols the filename can be largely omitted (non extension can be
substituted with an underscore) when a configured backend uses the "native" directory format.
In simple terms this means that
`/symbols/_/elf-buildid-180a373d6afbabf0eb1f09be1bc45bd796a71085/_` is a valid query for an
ELF executable and `/symbols/_.debug/elf-buildid-sym-180a373d6afbabf0eb1f09be1bc45bd796a71085/_.debug`
is a valid query for an ELF debug symbol.

## Compression Support

Symbolicator supports a range of compression formats (zlib, gzip, zstd and
cab). For cab compression the `cabextract` binary needs to be installed.  If the debug info file
is already compressed it will be auto detected and extracted.  For PE/PDB files symbolicator also
supports the Microsoft convention of replacing the last character in the filename with an underscore.

## Symbol Server Proxy

If the symstore proxy is enabled the symbolicator also acts as a symbol proxy.  This means that
all configured sources are probed for symbol queries below the `/symbols` prefix.  The path
following this prefix needs to be a valid SSQP query.

Example:

```
$ curl -IL http://127.0.0.1:3021/symbols/wkernel32.pdb/ff9f9f7841db88f0cdeda9e1e9bff3b51/wkernel32.pdb
HTTP/1.1 200 OK
content-length: 846848
content-type: application/octet-stream
date: Fri, 19 Apr 2019 22:47:54 GMT
```

## Usage with Sentry

<a name=ref-sentry />

The following requires a recent git version of Sentry.

While Symbolicator aims to not be tied to Sentry's usecases, [Sentry](https://github.com/getsentry/sentry) has a hard dependency on Symbolicator to process native stacktraces. To get it running for local development:

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
