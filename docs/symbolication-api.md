# Symbolication API

This page describes the public web API of the [Native Symbolicator](https://www.notion.so/9ce2a7da-ad06-44d0-8563-a963aa9e390f).

Symbolicator exposes a HTTP API to allow symbolication of raw native stack traces and minidumps. All information necessary for symbolication needs to be part of the request, such as external buckets and their auth tokens or full stack traces. There are the following endpoints:

- `POST /symbolicate`: Symbolicate raw native stacktrace
- `POST /minidump`: Symbolicate a minidump and extract information
- `GET /requests/:id`: Status update on running symbolication jobs
- `GET /health`: System status and health monitoring

## Symbolication Request

    POST /symbolicate?timeout=123&scope=123
    
    {
      "signal": 11,
      "sources": [
        {
          "id": "<uuid>",
          "type": "http",
          ... // see "External Buckets"
        },
        ...
      ],
      "threads": [
        {
          "frames": [
            {
              "instruction_addr": "0xfeedbeef" // can also be a number
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

### Query Parameters

- `demangle[=true]`: Tries to demangle the mangled symbol and puts the result in the `"name"` field.
- `timeout`: If given, a response status of `pending` might be sent by the server.
- `scope`: An optional scope which will be used to isolate cached files from each other

### Request Body

A JSON payload describing the stack traces and code modules for symbolication, as well as external sources to pull symbols from:

- `sources`: A list of descriptors for internal or external symbol sources. See *External Sources*.
- `modules`: A list of code modules (aka debug images) that were loaded into the process. All attributes other than `type` are required. The Symbolicator may optimize lookups based on the `type` if present.
Valid types are `macho`, `pe`, `elf`. Invalid types are silently ignored. The Symbolicator still works if the type is invalid, but less efficiently. However, a schematically valid but *wrong* type is fatal for finding symbols.
- `threads`: A list of process threads to symbolicate.
    - `registers`: Optional register values aiding symbolication heuristics. For example, register values may be used to perform correction heuristics on the instruction address of the top frame.
    - `frames`: A list of frames with addresses. Arbitrary additional properties may be passed with frames, but are discarded.

### Response

See *Symbolication Response*

## Minidump Request

    POST /minidump?timeout=5&scope=123
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
        ... // see "External Buckets"
      },
      ...
    ]

### Query Parameters

- `demangle[=true]`: Tries to demangle the mangled symbol and puts the result in the `"name"` field.
- `timeout`: If given, a response status of `pending` might be sent by the server.
- `scope`: An optional scope which will be used to isolate cached files from each other

### Request Body

A multipart form data body containing the minidump, as well as the external sources to pull symbols from.

- `sources`: A list of descriptors for internal or external symbol sources. See *External Sources*.
- `upload_file_minidump`: The minidump file to be analyzed.

### Response

See *Symbolication Response*.

## Symbolication Response

The response to a symbolication request is a JSON object which contains different data depending on the status of the symbolication job:

- `pending`: Symbolication has not finished yet. Try again later.
- `complete`: The symbolication request has been processed and results are ready. This status is only reported once, after which the job is cleaned up.
- `error`: Something went wrong during symbolication, and details are in the payload.

### Success Response

Symbol server responds with *200 OK* and the response payload listed below if symbolication succeeds within a configured timeframe (around 20 seconds): 

    {
      "status": "complete",                   // TODO: Describe all status
    
      // Symbolicated stack traces
      "stacktraces": [
        {                                     // Only frames, no registers
          "frames": [
            {
              // symbolication meta data
              "status": "symbolicated",       // TODO: Describe all statuses
              "original_index": 0,
    
              // frame information
              "instruction_addr": "0xfeedbeef",
              "package": "/path/to/module.so",
              "function": "memset",           // demangled short version of symbol
              "lang": "cpp",                  // TODO: List all languages
              "symbol": "__1cGmemset6FpviI_0_",
              "sym_addr": "0xfeed0000",
              "filename": "../src/file.c",    // Relative to compilation dir
              "abs_path": "/path/to/src/file.c",
              "lineno": 22,
              "line_addr": "0xfeedbe00",      // This does not exist in Sentry
            },
            ...
          ],
          "registers": { ... }
        }
      ],
    
      // Modules completed with information read from object files
      "modules": [
        {
          "status": "found", 
          ...
        }
      ],
    
      // Additional information read from crash report
      "arch": "x86_64",
      "signal": 11,
      "os": {
        "name": "Windows NT",
        "version": "8.1.2700"
      }
    }

The symbolicated frames are returned in the same order as provided in the request. Additional properties passed in the request are discarded. Errors that occurred during symbolication, such as missing symbol files or unresolvable addresses within symbols are reported as values for `status` in both modules and frames.

### Backoff Response

If symbolication takes longer than the threshold `timeout`, the server instead responds with a backoff response. It will then continue to fetch symbols and symbolication. The response indicates the estimated time for symbol retrieval, after which the symbolication request can be expected to succeed:

    {
      "status": "pending",
      "request_id": "deadbeef",
      "retry_after": 300,   // 5 minutes
    }

The symbolication server must not send a backoff response if no timeout was sent by the client.

Note that the `retry_after` value is just an estimation and does not give any guarantee. The request may be repeated at any time:

    GET /requests/deadbeef?timeout=123

### Invalid Request Response

If the user provided a non-existent request ID, the server responds with *404 Not Found*.

Requests should always be treated transient as they might disappear during a deploy. Clients must expect that 404 is returned even for valid request IDs and then re-schedule symbolication

On a related note, generally state on the server is ephemeral.

## External Sources

The service loads symbol files from external buckets as specified in the symbolication request. Access to these buckets may occur via one of the following protocols.

Refer to `README.md` for the possible source types.
