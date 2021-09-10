---
title: Overview
---

# API

Symbolicator exposes a HTTP API to allow symbolication of raw native stack
traces and minidumps. All information necessary for symbolication needs to be
part of the request, such as external buckets and their auth tokens or full
stack traces. There are the following endpoints:

- `POST /symbolicate`: Symbolicate raw native stacktrace
- `POST /minidump`: Symbolicate a minidump and extract information
- `POST /applecrashreport`: Symbolicate an Apple Crash Report
- `GET /requests/:id`: Status update on running symbolication jobs
- `GET /healthcheck`: System status and health monitoring

## Sources

For Symbolicator to operate correctly, it needs to be pointed to at least one
source. It supports various different backends for where it looks for symbols.
There are two modes for Symbolicator with regards to how it looks for symbols.
One is the preconfigured mode where the `sources` key is configured right in the
config file. The second is one where the sources are defined with the HTTP
request to the symbolication API.

If you want to use Symbolicator as a symstore compatible proxy you need to
preconfigure the sources.

Example configuration:

```json
[
  {
    "id": "my-bucket-linux",
    "type": "s3",
    "bucket": "my-project-my-bucket",
    "region": "us-east-1",
    "prefix": "/linux",
    "access_key": "AMVSAVWEXRIRJPOMCKWN",
    "secret_key": "Lqnc45YWr9y7qftCI+vST/1ZPmmw1H6SkbIf2v/8",
    "filters": {
      "filetypes": ["elf_debug", "elf_code"]
    },
    "layout": {
      "type": "native",
      "casing": "lowercase"
    }
  },
  {
    "id": "my-bucket-windows",
    "type": "s3",
    "bucket": "my-project-my-bucket",
    "region": "us-east-1",
    "prefix": "/windows",
    "access_key": "AMVSAVWEXRIRJPOMCKWN",
    "secret_key": "Lqnc45YWr9y7qftCI+vST/1ZPmmw1H6SkbIf2v/8",
    "filters": {
      "filetypes": ["pe", "pdb"]
    },
    "layout": {
      "type": "native",
      "casing": "lowercase"
    }
  },
  {
    "id": "microsoft",
    "type": "http",
    "filters": {
      "filetypes": ["pe", "pdb"]
    },
    "url": "https://msdl.microsoft.com/download/symbols/"
  }
]
```

Sources are ordered by priority. Each source needs at least two keys:

- `id`: the ID of the source. This can be freely chosen and is used to identify
  cache files in the cache folder
- `type`: defines the type of the source (`http`, `s3`, `gcs` or `sentry`)

These are common parameters that work on most symbol sources (except `sentry`):

- `filters`: a set of filters to reduce the number of unnecessary hits on a
  symbol server. This configuration key is an object with two keys:

    - `filetypes`: a list of file types to restrict the server to. Possible
      values: `pe`, `pdb`, `mach_debug`, `mach_code`, `elf_debug`, `elf_code`,
      `breakpad`)
    - `path_patterns`: a list of glob matches that need to be matched on the image
      name. If the debug image has no name it will never match here.

- `layout`: configures the file system layout of the sources. This configuration
  key is an object with two keys:

    - `type`: defines the general layout of the directory. Possible values are
      `native`, `symstore`, `symstore_index2`, `ssqp`, and `unified`.
      `native` uses the file type's native format. `symstore` and `ssqp` both
      use the Microsoft Symbol Server format but control the case
      conventions. `symstore` uses the conventional casing rules for
      signatures and filenames, `ssqp` uses the Microsoft SSQP casing rules
      instead. Additionally `symstore_index2` works like `symstore` but uses
      the "Two tier" (index2.txt) layout where the first two characters of
      the filename are used as a toplevel extra folder. `unified` is the
      unified lookup format that symbolicator recommends.
    - `casing`: enforces a casing style. The default is not to touch the casing
      and forward it unchanged. If the backend does not support a case insensitive
      backend (eg: S3) then it's recommended to set this to `lowercase` to enforce
      changing all to lowercase. Possible values: `default`, `lowercase`,
      `uppercase`.

## HTTP source

The HTTP source lets one fetch symbols from a Microsoft Symbol Server or similar
systems. There is some flexibility to how it operates.

- `type`: `"http"`
- `url`: This defines the URL where symbolicator should be fetching from. For
  instance this can be `https://msdl.microsoft.com/download/symbols/` to point
  it to the official microsoft symbol server.
- `headers`: an optional dictionary of headers that should be sent with the HTTP
  requests. This can be used for instance to configure HTTP basic auth
  configuration.

## Amazon S3 Bucket

This source connects straight to an S3 bucket and looks for symbols there. It's
recommended for this to be configured with explicit `lowercase` casing to avoid
issues as the SSQP protocol demands case insensitive lookups.

- `type`: `"s3"`
- `bucket`: the name of the S3 bucket
- `prefix`: a path prefix to put in front of all keys (eg: `/windows`)
- `region`: the AWS region where the bucket is located. Default regions can be
  supplied as strings, i.e. "us-east-1". In order to use a custom region for an
  S3 compatible service such as Ceph or minio, specify a tuple:
  `["custom-region-name", "http://minio-address/"]`.
- `access_key`: the AWS access key to use
- `secret_key`: the AWS secret key to use

## GCS Bucket

This source connects to a GCS bucket and looks for symbols there. It behaves
similarly to `s3` but uses different credentials:

- `type`: `"gcs"`
- `bucket`: the name of the GCS bucket
- `prefix`: a path prefix to put in front of all keys (eg: `/windows`)
- `private_key`: the GCS private key (base64 encoded and with optional PEM
  envelope)
- `client_email`: the GCS client email for authentication

## Sentry

This points Symbolicator at a Sentry installation to fetch customer supplied
symbols from there. Sentry applies proper configuration automatically.
