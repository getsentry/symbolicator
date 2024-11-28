---
title: POST /symbolicate-jvm
---

# Symbolication Request


```http
POST /symbolicate-jvm?timeout=123&scope=123 HTTP/1.1
Content-Type: application/json

{
    "platform": "java",
    "source": [
        {
            "type": "s3",
            "id": "<id>",
            "url": "https://sentry.io/api/0/projects/sentry-org/sentry-project/artifact-lookup/",
            "token": "secret"
        }
    ],
    "exceptions": [
        {
            "type": "RuntimeException",
            "module": "io.sentry.samples"
        }
    ],
    "stacktraces": [
        {
            "frames": [
                {
                    "function": "otherMethod",
                    "filename": "OtherActivity.java",
                    "module": "OtherActivity",
                    "abs_path": "OtherActivity.java",
                    "lineno": 100,
                    "index": 0
                }
            ]
        }
    ],
    "modules": [
        {
            "type": "source",
            "uuid": "246fb328-fc4e-406a-87ff-fc35f6149d8f"
        },
        {
            "type": "proguard",
            "uuid": "05d96b1c-1786-477c-8615-d3cf83e027c7"
        }
    ],
    "release_package": "some_release",
    "classes": [],
    "options": {
        "apply_source_context": true
    }
}

```

## Query Parameters

- `timeout`: If given, a response status of `pending` might be sent by the
  server.
- `scope`: An optional scope which will be used to isolate cached files from
  each other


## Request Body

- `platform`: The event's platform.
- `source`: A descriptor for the Sentry source to be used for symbolication. See
  [Sentry](index.md) source. Note that only proguard files uploaded to Sentry are supported at the moment.
- `exceptions`: A list of exceptions which will have their module and type fields remapped.
- `modules`: A list of source code files with a corresponding debug id that
  were loaded during JVM code execution. The list is handled by the Sentry source.
- `stacktrace`: A list of stacktraces to symbolicate.
  - `frames`: A list of frames with corresponding `abs_path`, `lineno`,
    and other optional fields like `colno` or minified `function` name.
- `release_package`: Name of Sentry `release` for the processed request.
- `classes`: A list of classes which will have their names remapped and returned in the form of a map. Allows for deobfuscation of view hierarchies.
- `options`: Symbolication-specific options which control the endpoint's behavior.
  - `apply_source_context`: Whether to apply source context for the stack frames.

## Response

See [Symbolication Response](response.md).
