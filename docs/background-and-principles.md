# Background and Principles

This page describes reasoning behind the design of Symbolicator.

## Issues Before Symbolicator

There were a couple of general issues with native event processing:

- Matching of system symbol paths sometimes yields false positives
- SymbolServer only handles iOS system symbols and cannot resolve line numbers
- Handling of DIFs in Sentry causes a lot of overhead in the blob storage and is prone to race conditions
- Customers are starting to hit the maximum file size limit of 2GB
- DIFs always need to be uploaded before events, whether they are used or not
- Minidump processing requires multiple debug files at once, which required hacks in Sentry.

Additionally, there were issues around the upload of debug information files.

1. SymCaches and CFI caches are computed ahead of time and persisted. This creates inherent race conditions and also requires computation, even if the symbols are never used.
2. Debug file models only store the debug identifier. This works for Breakpad minidumps and custom SDKs, but will never be compatible to native debuggers or other systems.
3. The current logic for choosing a file based on debug ids is based on the upload time. This was done to reduce the amount of fetching from the file storage onto workers. However, this might choose a suboptimal file.
4. During symbol upload, older "redundant" files are deleted. A file is considered redundant, if it only provides a subset of the features of all newer files combined. Since this does not consider the file type, this might throw away better debug information in favor of a newer, worse file (e.g. when a breakpad file is uploaded after the original debug file). 

## Symbolicator Principles

Symbolizer is a service that unifies symbolication for all kinds of native frames and allows us to add more features or improve performance more easily than in the former setup. Goals of the Symbolizer are:

- **Symbolize entire stack traces at once**, referencing both system- and customer images. Thanks to this, heuristics for detecting system images are no longer necessary.
- **Fetch DIFs from external sources.** These could be third-party servers like the online Microsoft Symbol Server or buckets of DIFs hosted by our customers. This removes the need to proactively upload files to Sentry that will never be used.
- **Treat SymCaches and Debug Files as transient.** All files are put in a local file system cache for faster access, but there is no persistent storage for symcaches or files retrieved from an external resources. This also simplifies upgrades (simply wipe the cache).
- **Avoid one-hit wonders.** SymCaches only need to be generated for debug files that are hit multiple times within a time frame. For all other symbols, using the original debug file is sufficiently fast — especially since it only needs to be loaded once per symbolication request.
- **Improve processing of minidumps.** Potentially use multiple debug files or cache files at once to run stackwalking and symbolication in a single run. This reduces resource allocation and gets rid of our hacks in the breakpad processor.

## Terminology

### Sentry Symbol Server

The Sentry Symbol Server was created to provide symbolicated stack frames iOS system libraries without forcing our users to upload them to each project after every iOS release. Instead, Sentry detects these frames and initially skip searching for debug files in its own database. 

Symbol Server is an internal service. It can only handle dSYM files, thus constraining it to macOS and iOS. Every time Apple releases a new iOS version, the SDK team pulls new debug files from an updated device and re-uploads them to an S3 bucked. This bucked is scanned by the Symbol Server in regular intervals.

Internally, Symbol Server uses a binary format for symbolication. It only contains file names, but not line numbers.

Since sometimes images are wrongfully classified as system symbols, there is always a fallback to search for uploaded debug information files in Sentry's database.

**We deprecated this service with [PR #72](https://github.com/getsentry/symbolicator/pull/72) and instead use a simple GCS bucket that we point Symbolicator to**


### Debug Information Files

**DIFs** contain "debug information" to resolve function names and file locations from raw memory addresses in native crash reports. These files can get quite large, and we have encountered first customers with files over 2GB.

Additionally, DIFs come in various different formats depending on the operating system and build process. In some scenarios, a combination of multiple files might be needed to successfully symbolicate a native crash. This has first been encountered in minidumps, which require the executable to extract stack traces, but a separate debug file to symbolicate its addresses.

To mitigate issues with file size and platform differences, we created **SymCaches**. These are custom binary files that store an essential subset of the original debug information in a uniform format. The file format is optimized for direct mapping into memory and fast lookups of addresses. SymCaches are created for every DIF uploaded by customers that contains valid debug information.

### Reprocessing

Fundamentally, Sentry cannot reprocess Events. Put simply, the "Reprocessing" feature defers processing of events until all required DIFs have been uploaded by the customer. 

Whether a DIF is required mostly depends on our classification as system image. Unless an image is a system image, it is always required. Since this metric is only reliable for iOS, reprocessing is effectively broken for all other platforms.

### Symbol Storage in Sentry

Symbols in Sentry are stored in the main file store ("Blob Storage", on GCS in production but generally configurable) split into deduplicated 1MB chunks. This is governed by a table in the main Postgres database (`sentry_fileblobowner`), which also controls access authorization per organization. To retrieve a file, one has to query all blobs for a file with a database query first, and then manually assemble them into one continuous file on the disk. This logic is implemented in the [`File` model](https://github.com/getsentry/sentry/blob/2b4a785b4ebd9d874a339c3ba9741a4cd307faf5/src/sentry/models/file.py#L366) in the main Sentry codebase.

Debug files abstract this in the `ProjectDebugFile` model. It contains a reference to a file in the blob store, as well as meta data on the debugging file such as its architecture or name. Debug files are always **associated to projects** and keyed by their **debug id**. There might be multiple files files for each debug id with complementary **features** (such as unwind information or symbol tables).

There are several high-level abstractions that expose this to other parts of the Sentry code base and even to HTTP endpoints. These are:

- `[find_by_checksum](https://github.com/getsentry/sentry/blob/2b4a785b4ebd9d874a339c3ba9741a4cd307faf5/src/sentry/models/debugfile.py#L111)`: Resolves debug file models by their content checksum. This is used to deduplicate entire files, for example to skip redundant symbol uploads.
- `[find_by_debug_ids](https://github.com/getsentry/sentry/blob/2b4a785b4ebd9d874a339c3ba9741a4cd307faf5/src/sentry/models/debugfile.py#L117)`: Resolves debug file models for a list of debug_ids in a project. For each debug id, the *latest* file is returned if there are multiple. Optionally, a set of required *features* can be declared, by which the files will be filtered. This is used to resolve the best match for symbolication or stackwalking.
- `[fetch_difs](https://github.com/getsentry/sentry/blob/2b4a785b4ebd9d874a339c3ba9741a4cd307faf5/src/sentry/models/debugfile.py#L611)`: Downloads a list of debug files specified by their debug ids and optional features to a local file system cache. This uses `find_by_debug_ids` internally.
- `[generate_caches](https://github.com/getsentry/sentry/blob/2b4a785b4ebd9d874a339c3ba9741a4cd307faf5/src/sentry/models/debugfile.py#L596)`: Generates SymCaches and CFI caches for a debug file. Either, the file is already on the file system, or otherwise it will be downloaded. This is performed immediately after uploading; or on demand in case symcaches are missing.
- `[DebugFilesEndpoint](https://github.com/getsentry/sentry/blob/2b4a785b4ebd9d874a339c3ba9741a4cd307faf5/src/sentry/api/endpoints/debug_files.py#L71-L76)`: Lists or searches debug files by a set of meta data, including the debug id. Also offers a download, which assembles the file from its chunks and then streams it to the client.
