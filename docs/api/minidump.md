---
title: POST /minidump
---

# Minidump Request

```http
POST /minidump?timeout=5&scope=123 HTTP/1.1
Content-Type: multipart/form-data; boundary=xxx

--xxx
Content-Disposition: form-data; name="upload_file_minidump"
[binary blob]

--xxx
Content-Disposition: form-data; name="sources"
[
  {
    "id": "<uuid>",
    "type": "http",
    ... // see "Sources"
  },
  ...
]

--xxx
Content-Disposition: form-data; name="platform"
"native"

--xxx--
```

## Query Parameters

- `timeout`: If given, a response status of `pending` might be sent by the
  server.
- `scope`: An optional scope which will be used to isolate cached files from
  each other

## Request Body

A multipart form data body containing the minidump, as well as the external
sources to pull symbols from.

- `platform`: The event' platform.
- `sources`: A list of descriptors for internal or external symbol sources. See
  [Sources](index.md).
- `upload_file_minidump`: The minidump file to be analyzed.

## Response

See [Symbolication Response](response.md).
