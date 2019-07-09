# Background and Principles

This page describes reasoning behind the design of Symbolicator.

## Symbolicator Principles

Symbolizer is a service that unifies symbolication for all kinds of native
frames and allows us to add more features or improve performance more easily
than in the former setup. Goals of the Symbolizer are:

- **Symbolize entire stack traces at once**, referencing both system- and
  customer images. Thanks to this, heuristics for detecting system images are no
  longer necessary.
- **Fetch DIFs from external sources.** These could be third-party servers like
  the online Microsoft Symbol Server or buckets of DIFs hosted by our customers.
  This removes the need to proactively upload files to Sentry that will never be
  used.
- **Treat SymCaches and Debug Files as transient.** All files are put in a local
  file system cache for faster access, but there is no persistent storage for
  symcaches or files retrieved from an external resources. This also simplifies
  upgrades (simply wipe the cache).
- **Avoid one-hit wonders.** SymCaches only need to be generated for debug files
  that are hit multiple times within a time frame. For all other symbols, using
  the original debug file is sufficiently fast — especially since it only needs
  to be loaded once per symbolication request.
- **Improve processing of minidumps.** Potentially use multiple debug files or
  cache files at once to run stackwalking and symbolication in a single run.
  This reduces resource allocation and gets rid of our hacks in the breakpad
  processor.

## Issues Symbolicator Solves

Symbolicator was developed at [Sentry] to allow for faster iteration speeds and
improve symboilcation performance. There were a couple of general issues with
native event processing:

- Matching of system symbol paths sometimes yields false positives
- SymbolServer only handles iOS system symbols and cannot resolve line numbers
- Handling of DIFs in Sentry causes a lot of overhead in the blob storage and is
  prone to race conditions
- Customers are starting to hit the maximum file size limit of 2GB
- DIFs always need to be uploaded before events, whether they are used or not
- Minidump processing requires multiple debug files at once, which required
  hacks in Sentry.

Additionally, there were issues around the upload of debug information files.

1. SymCaches and CFI caches are computed ahead of time and persisted. This
   creates inherent race conditions and also requires computation, even if the
   symbols are never used.
2. Debug file models only store the debug identifier. This works for Breakpad
   minidumps and custom SDKs, but will never be compatible to native debuggers
   or other systems.
3. The old logic for choosing files based on debug ids is based on the upload
   time. This was done to reduce the amount of fetching from the file storage
   onto workers. However, this might choose a suboptimal file.
4. During symbol upload, older "redundant" files are deleted. A file is
   considered redundant, if it only provides a subset of the features of all
   newer files combined. Since this does not consider the file type, this might
   throw away better debug information in favor of a newer, worse file (e.g.
   when a breakpad file is uploaded after the original debug file).

## Terminology

### Debug Information Files

**DIFs** contain "debug information" to resolve function names and file
locations from raw memory addresses in native crash reports. These files can get
quite large, and we have encountered first customers with files over 2GB.

Additionally, DIFs come in various different formats depending on the operating
system and build process. In some scenarios, a combination of multiple files
might be needed to successfully symbolicate a native crash. This has first been
encountered in minidumps, which require the executable to extract stack traces,
but a separate debug file to symbolicate its addresses.

To mitigate issues with file size and platform differences, we created
**SymCaches**. These are custom binary files that store an essential subset of
the original debug information in a uniform format. The file format is optimized
for direct mapping into memory and fast lookups of addresses. SymCaches are
created for every DIF uploaded by customers that contains valid debug
information.
