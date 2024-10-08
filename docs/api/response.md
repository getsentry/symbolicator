---
title: Symbolication Response
---

# Symbolication Response

## Native Response

The response to a native symbolication request is a JSON object which contains
different data depending on the status of the symbolication job:

- `pending`: Symbolication has not finished yet; try again later. This is only
  returned after the timeout has expired, if one was specified.
- `complete`: The symbolication request has been processed and results are
  ready. This status is only reported once, after which the job is cleaned up.
- `error`: Something went wrong during symbolication, and details are in the
  payload.

### Success Response

Symbol server responds with _200 OK_ and the response payload listed below if
symbolication succeeds within a configured timeframe (around 20 seconds):

```javascript
{
  "status": "complete",

  // Symbolicated stack traces
  "stacktraces": [
    {
      "frames": [
        {
          // Symbolication meta data
          "status": "symbolicated",
          "original_index": 0,

          // Frame information
          "instruction_addr": "0xfeedbeef",  // actual address of the frame
          "addr_mode": "abs",                // address mode
          "sym_addr": "0xfeed0000",          // start address of the function
          "package": "/path/to/module.so",   // path to the module's code file
          "symbol": "__1cGmemset6FpviI_0_",  // original mangled function name
          "function": "memset",              // demangled short version of symbol
          "lang": "cpp",
          "abs_path": "/path/to/src/file.c", // normalized absolute path
          "filename": "../src/file.c",       // path relative to compilation dir
          "lineno": 22,
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
```

The symbolicated frames are returned in the same order as provided in the
request. Additional properties passed in the request are discarded. Errors that
occurred during symbolication, such as missing symbol files or unresolvable
addresses within symbols are reported as values for `status` in both modules and
frames.

### Note on Addresses

Addresses (`instruction_addr` and `sym_addr`) can come in two versions. They
can be absolute or relative. Symbolicator will always try to make addresses
absolute but in some cases this cannot be done. For instance WASM modules do
not have absolute addresses in which case the addresses stay relative. This is
identified by the `addr_mode` property. When it's set to `"abs"` it means
the addresses are absolute, when `"rel:X"` it's relative to module index `X`.

### Backoff Response

If symbolication takes longer than the threshold `timeout`, the server instead
responds with a backoff response. It will then continue to fetch symbols and
symbolication. The response indicates the estimated time for symbol retrieval,
after which the symbolication request can be expected to succeed:

```javascript
{
  "status": "pending",
  "request_id": "deadbeef",
  "retry_after": 300 // 5 minutes
}
```

The symbolication server must not send a backoff response if no timeout was sent
by the client.

Note that the `retry_after` value is just an estimation and does not give any
guarantee. The request may be repeated at any time:

    GET /requests/deadbeef?timeout=123

### Invalid Request Response

If the user provided a non-existent request ID, the server responds with _404
Not Found_.

Requests should always be treated transient as they might disappear during a
deploy. Clients must expect that 404 is returned even for valid request IDs and
then re-schedule symbolication

On a related note, state on the server is generally ephemeral.


## JavaScript Response

The response to a JavaScript symbolication request is a JSON object which contains
a list of original raw stack traces, corresponding list of processed stack traces
and an optional list of errors that happened during symbolication.

```javascript
{
  "stacktraces": [
    {
      "frames": [
        {
          "function": "onFailure",
          "filename": "test.js",
          "module": "files/test",
          "abs_path": "http://localhost:<port>/files/test.js",
          "lineno": 5,
          "colno": 11,
          "pre_context": [
            "var makeAFailure = (function() {",
            "  function onSuccess(data) {}",
            "",
            "  function onFailure(data) {"
          ],
          "context_line": "    throw new Error('failed!');",
          "post_context": [
            "  }",
            "",
            "  function invoke(data) {",
            "    var cb = null;",
            "    if (data.failed) {"
          ],
          "data": {
            "sourcemap": "http://localhost:<port>/files/app.js.map"
          }
        }
      ]
    }
  ],
  "raw_stacktraces": [
    {
      "frames": [
        {
          "abs_path": "http://localhost:<port>/files/app.js",
          "lineno": 1,
          "colno": 64,
          "context_line": "var makeAFailure=function(){function n(n){}function e(n){throw new Error(\"failed!\")}function r(r){var i=null;if(r.failed){i=e}else{i=n}i(r)} {snip}",
          "post_context": [
            "",
            "",
            "//# sourceMappingURL=a.different.map",
            "",
            "//@ sourceMappingURL=another.different.map"
          ]
        }
      ]
    }
  ],
  "errors": [
    {
      "abs_path": "http://localhost:<port>/assets/missing_bar.js",
      "type": "missing_source"
    }
  ]
}
```

## JVM Response

The response to a JVM symbolication request is a JSON object which contains
a list of processed stack traces, exceptions and classes as well as an optional list of error that happen during symbolication.

```javascript
{
  "exceptions": [
    {
      "type": "RuntimeException",
      "module": "java.lang"
    }
  ],
  "stacktraces": [
    {
      "frames": [
        {
          "function": "onMenuItemClick",
          "filename": "EditActivity",
          "module": "io.sentry.samples.instrumentation.ui.EditActivity$$InternalSyntheticLambda$1$ebaa538726b99bb77e0f5e7c86443911af17d6e5be2b8771952ae0caa4ff2ac7$0",
          "abs_path": "EditActivity",
          "lineno": 0,
          "in_app": true,
          "index": 18
        },
        {
          "function": "onCreate$lambda-1",
          "module": "io.sentry.samples.instrumentation.ui.EditActivity",
          "lineno": 37,
          "pre_context": [
            "        }",
            "",
            "        findViewById<Toolbar>(R.id.toolbar).setOnMenuItemClickListener {",
            "            if (it.itemId == R.id.action_save) {",
            "                try {"
          ],
          "context_line": "                    SomeService().helloThere()",
          "post_context": [
            "                } catch (e: Exception) {",
            "                    Sentry.captureException(e)",
            "                }",
            "",
            "                val transaction = Sentry.startTransaction("
          ],
          "in_app": true,
          "index": 18
        }
      ]
    }
  ],
  "classes": {},
  "errors": [
    {
      "uuid": "8236f5cf-52c8-4e35-a7cf-01421e4c2c88",
      "type": "missing"
    }
  ]
}
```
