---
title: POST /symbolicate
---

# Symbolication Request

```http
POST /symbolicate?timeout=123&scope=123 HTTP/1.1
Content-Type: application/json

{
  "signal": 11,
  "sources": [
    {
      "id": "<uuid>",
      "type": "http",
      ...
    },
    ...
  ],
  "threads": [
    {
      "frames": [
        {
          "instruction_addr": "0xfeedbeef",
          "addr_mode": "rel:0"
        },
        ...
      ],
      "registers": {
        "rip": "0xfeedbeef",
        "rsp": "0xfeedface"
      }
    }
  ],
  "modules": [
    {
      "type": "macho",
      "debug_id": "some-debug-id",
      "code_id": "some-debug-id",
      "debug_file": "/path/to/image.so",
      "image_addr": "0xfeedbeef",
      "image_size": "0xbeef"
    },
    ...
  ]
}
```

## Query Parameters

- `timeout`: If given, a response status of `pending` might be sent by the
  server.
- `scope`: An optional scope which will be used to isolate cached files from
  each other

## Request Body

A JSON payload describing the stack traces and code modules for symbolication,
as well as external sources to pull symbols from:

- `sources`: A list of descriptors for internal or external symbol sources. See
  [Sources](index.md).
- `modules`: A list of code modules (aka debug images) that were loaded into the
  process. All attributes other than `type`, `image_addr` and `image_size` are
  required. The Symbolicator may optimize lookups based on the `type` if present.
  Valid types are `macho`, `pe`, `elf`. Invalid types are silently ignored. The
  Symbolicator still works if the type is invalid, but less efficiently. However,
  a schematically valid but _wrong_ type is fatal for finding symbols.
- `threads`: A list of process threads to symbolicate.
  - `registers`: Optional register values aiding symbolication heuristics. For
    example, register values may be used to perform correction heuristics on the
    instruction address of the top frame.
  - `frames`: A list of frames with addresses. Arbitrary additional properties
    may be passed with frames, but are discarded. The `addr_mode` property
    defines the beahvior of `instruction_addr`.

## Response

See [Symbolication Response](response.md).
