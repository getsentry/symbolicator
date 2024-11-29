---
title: POST /symbolicate-js
---

# Symbolication Request

```http
POST /symbolicate-js?timeout=123&scope=123 HTTP/1.1
Content-Type: application/json

{
  "platform": "node",
  "source": {
    "id": "<id>",
    "url": "https://sentry.io/api/0/projects/sentry-org/sentry-project/artifact-lookup/",
    "token": "secret"
  },
  "stacktraces": [
    {
      "frames": [
        {
          "function": "x",
          "module": "x",
          "filename": "x",
          "abs_path": "x",
          "lineno": 1,
          "colno": 1,
          "in_app": true
        }
      ]
    }
  ],
  "modules": [
    {
      "code_file": "some-code-file",
      "debug_id": "some-debug-id",
      "type": "debug_id"
    }
  ],
  "dist": "production",
  "release": "1.33.1",
  "scraping": {
    "enabled": true,
    "allowed_origins": ["*.domain.com"]
  }
}
```

## Query Parameters

- `timeout`: If given, a response status of `pending` might be sent by the
  server.
- `scope`: An optional scope which will be used to isolate cached files from
  each other

## Request Body

A JSON payload describing the stack traces and code modules for symbolication,
as well as configuration for scraping sources from external servers:

- `platform`: The event's platform.
- `source`: A descriptor for the Sentry source to be used for symbolication. See
  [Sentry](index.md) source.
- `modules`: A list of source code files with a corresponding debug id that
  were loaded during JS code execution. The list is handled by the Sentry source.
- `stacktrace`: A list of stacktraces to symbolicate.
  - `frames`: A list of frames with corresponding `abs_path`, `lineno`,
    and other optional fields like `colno` or minified `function` name.
- `release`: Name of Sentry `release` for the processed request.
- `dist`: Name of Sentry `dist` for the processed request.
- `scraping`: Configuration for scraping of JS sources and sourcemaps from the web.
  - `enabled`: Whether scraping should happen at all.
  - `allowed_origins`: A list of "allowed origin patterns" that control what
    URLs we are allowed to scrape from. Allowed origins may be defined in several ways:
    - `http://domain.com[:port]`: Exact match for base URI (must include port).
    - `*`: Allow any domain.
    - `*.domain.com`: Matches domain.com and all subdomains, on any port.
    - `domain.com`: Matches domain.com on any port.
    - `*:port`: Wildcard on hostname, but explicit match on port.
  - `headers`: A map of headers to send with every HTTP request while scraping.
- `options`: Symbolication-specific options which control the endpoint's behavior.
  - `apply_source_context`: Whether to apply source context for the stack frames.

## Response

See [Symbolication Response](response.md).
