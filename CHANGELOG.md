# Changelog

## Unreleased

### Features

- Use `rust:bullseye-slim` as base image for docker container builder. ([#578](https://github.com/getsentry/symbolicator/pull/578))
- Use `debian:bullseye-slim` as base image for docker container runner. ([#578](https://github.com/getsentry/symbolicator/pull/578))

## 0.4.0

### Features

- S3 sources: add support for custom regions. ([#448](https://github.com/getsentry/symbolicator/pull/448))
- Bump symbolic to support versioning of CFI Caches. ([#467](https://github.com/getsentry/symbolicator/pull/467))
- Update symbolic to write versioned CFI Caches, and fix SymCache conversion related to inline-parent offset. ([#470](https://github.com/getsentry/symbolicator/pull/470))
- Update symbolic to allow processing PDB files with broken inlinee records. ([#477](https://github.com/getsentry/symbolicator/pull/477))
- Update symbolic to correctly apply bcsymbolmaps to symbol and filenames coming from DWARF. ([#479](https://github.com/getsentry/symbolicator/pull/479))
- Update symbolic to correctly restore registers when using compact unwinding. ([#487](https://github.com/getsentry/symbolicator/pull/487))
- Make various timeouts related to downloading files configurable. ([#482](https://github.com/getsentry/symbolicator/pull/482), [#489](https://github.com/getsentry/symbolicator/pull/489), [#491](https://github.com/getsentry/symbolicator/pull/491))
- Introduced the `max_download_timeout` config setting for source downloads, which defaults to 315s. ([#482](https://github.com/getsentry/symbolicator/pull/482), [#489](https://github.com/getsentry/symbolicator/pull/489), [#503](https://github.com/getsentry/symbolicator/pull/503)
- Introduced the `streaming_timeout` config setting for source downloads, which defaults to 250s. ([#489](https://github.com/getsentry/symbolicator/pull/489), [#503](https://github.com/getsentry/symbolicator/pull/503)
- Introduced the `connect_timeout` config setting for source downloads, which has a default of 15s. ([#491](https://github.com/getsentry/symbolicator/pull/491), [#496](https://github.com/getsentry/symbolicator/pull/496))
- GCS, S3, HTTP, and local filesystem sources: Attempt to retry failed downloads at least once. ([#485](https://github.com/getsentry/symbolicator/pull/485))
- Refresh symcaches when a new `BcSymbolMap` becomes available. ([#493](https://github.com/getsentry/symbolicator/pull/493))
- Cache download failures and do not retry the download for a while ([#484](https://github.com/getsentry/symbolicator/pull/484), [#501](https://github.com/getsentry/symbolicator/pull/501))
- New configuration option `hostname_tag` ([#513](https://github.com/getsentry/symbolicator/pull/513))
- Introduced a new cache status to represent failures specific to the cache: Download failures that aren't related to the file being missing in download caches and conversion errors in derived caches. Download failure entries expire after 1 hour, and conversion errors expire after 1 day, or after symbolicator is restarted. ([#509](https://github.com/getsentry/symbolicator/pull/509))
- Malformed and Cache-specific Error cache entries now contain some diagnostic info. ([#510](https://github.com/getsentry/symbolicator/pull/510))
- New configuration option `environment_tag` ([#517](https://github.com/getsentry/symbolicator/pull/517))
- Source candidates which symbolicator has failed to download due to non-400 errors are now being returned in symbolication payloads. These candidates include additional diagnostic info which briefly describes the download error. ([#512](https://github.com/getsentry/symbolicator/pull/512))
- If a DIF object candidate could not be downloaded due to a lack of permissions, their respective entry in a symbolication response will now mention something about permissions instead of marking the candidate as just Missing. ([#512](https://github.com/getsentry/symbolicator/pull/512), [#518](https://github.com/getsentry/symbolicator/pull/518))
- Introduced the `max_concurrent_requests` config setting, which limits the number of requests that will be processed concurrently. It defaults to `Some(120)`. ([#521](https://github.com/getsentry/symbolicator/pull/521), [#537](https://github.com/getsentry/symbolicator/pull/537))
- Symbolication tasks are being spawned on a tokio 1 Runtime now. ([#531](https://github.com/getsentry/symbolicator/pull/531))
- Symbolicator now allows explicitly versioning its caches. It will fall back to supported outdated cache versions immediately instead of eagerly waiting for updated cache files. Recomputations of newer cache versions are being done lazily in the background, bounded by new settings `max_lazy_redownloads` and `max_lazy_recomputations` for downloaded and derived caches respectively. ([#524](https://github.com/getsentry/symbolicator/pull/524), [#533](https://github.com/getsentry/symbolicator/pull/533), [#535](https://github.com/getsentry/symbolicator/pull/535))
- Remove actix-web in favor of axum. This changes the web framework and also completely switches to the tokio 1 Runtime. ([#544](https://github.com/getsentry/symbolicator/pull/544))
- Search for all known types of debug companion files during symbolication, in case there exists for example an ELF debug companion for a PE. ([#555](https://github.com/getsentry/symbolicator/pull/555))
- Introduced the `custom_tags` config setting for metrics ([#569](https://github.com/getsentry/symbolicator/pull/569))

### Fixes

- Skip trailing garbage frames after `_start`. ([#514](https://github.com/getsentry/symbolicator/pull/514))
- Respect the build_id provided on the commandline of wasm-split. ([#554](https://github.com/getsentry/symbolicator/pull/554))
- Update symbolic to a version with more lenient ELF parsing which is able to process invalid flutter 2.5 debug files. ([#562](https://github.com/getsentry/symbolicator/pull/562))

### Tools

- `symsorter` no longer emits files with empty debug identifiers. ([#469](https://github.com/getsentry/symbolicator/pull/469))
- MacOS universal builds (x86_64 + ARM in one fat binary) of symbolicator, symsorter and wasm-split can be downloaded from GitHub releases now. ([#565](https://github.com/getsentry/symbolicator/pull/565), [#568](https://github.com/getsentry/symbolicator/pull/568))

### Internal

- Measure how long it takes to download sources from external servers. ([#483](https://github.com/getsentry/symbolicator/pull/483))
- Avoid serialization overhead when processing large minidumps out-of-process. ([#508](https://github.com/getsentry/symbolicator/pull/508))

## 0.3.4

### Features

- Add support for BCSymbolMap auxiliary files which when present on the symbol server will automatically resolve obfuscated symbol names ([#403](https://github.com/getsentry/symbolicator/pull/403))
- S3 sources: add support for using AWS container credentials provided by IAM task roles. ([#417](https://github.com/getsentry/symbolicator/pull/417))
- Implement new Symbolication Quality Metrics. ([#426](https://github.com/getsentry/symbolicator/pull/426))
- Convert stackwalking/cfi loading into a fixpoint iteration. ([#450](https://github.com/getsentry/symbolicator/pull/450))

### Bug Fixes

- Update the mtime of a cache when it is 1h out of date. ([#390](https://github.com/getsentry/symbolicator/pull/390))
- Bump symbolic to fix large public records. ([#385](https://github.com/getsentry/symbolicator/pull/385), [#387](https://github.com/getsentry/symbolicator/pull/387))
- Bump symbolic to support `debug_addr` indexes in DWARF functions. ([#389](https://github.com/getsentry/symbolicator/pull/389))
- Fix retry of DIF downloads from sentry ([#397](https://github.com/getsentry/symbolicator/pull/397))
- Remove expired cache entries in auxdifs and diagnostics ([#444](https://github.com/getsentry/symbolicator/pull/444))
- Bump symbolic to update other DWARF and WASM handling dependencies, and improve support for compact unwind information, including stackless functions in MachO, with special handling for `_sigtramp`. ([#457](https://github.com/getsentry/symbolicator/pull/457))

### Tools

- `wasm-split` can remove names with `--strip-names`. ([#412](https://github.com/getsentry/symbolicator/pull/412))
- Linux x64 builds of `symsorter` and `wasm-split` can be downloaded from [GitHub releases](https://github.com/getsentry/symbolicator/releases/latest) now. ([#422](https://github.com/getsentry/symbolicator/pull/422))

## 0.3.3

### Features

- Add NotFound status for sources which didn't provide any DIF object candidates. ([#327](https://github.com/getsentry/symbolicator/pull/327))
- Symbolication responses include download, debug and unwind information about all DIF object files which were looked up on the available object sources. ([#309](https://github.com/getsentry/symbolicator/pull/309) [#316](https://github.com/getsentry/symbolicator/pull/316) [#324](https://github.com/getsentry/symbolicator/pull/324) [#344](https://github.com/getsentry/symbolicator/pull/344))

### Bug Fixes

- Silently ignore unknown fields in minidump and apple crash report requests instead of responding with `400 Bad Request`. ([#321](https://github.com/getsentry/symbolicator/pull/321))
- Update breakpad to allow processing MIPS minidumps and support reading MIPS debug sections. ([#325](https://github.com/getsentry/symbolicator/pull/325))
- Improve amd64 stack scanning by excluding some false-positive frames. ([#325](https://github.com/getsentry/symbolicator/pull/325))
- Resolve correct line information for large inline functions.

### Internal

- Changed the internal HTTP transport layer. There is no expected difference in the way Symbolicator downloads files from symbol servers, although some internal timeouts and error messages may change. ([#335](https://github.com/getsentry/symbolicator/pull/335))
- Report more descriptive errors if symbolication fails with internal errors, timeouts or crashes. ([#365](https://github.com/getsentry/symbolicator/pull/365))
- Remove an unused threadpool. ([#370](https://github.com/getsentry/symbolicator/pull/370), [#378](https://github.com/getsentry/symbolicator/pull/378))

## 0.3.2

### Bug Fixes

- Ensure deserialisation works for objects passed through procspawn. ([#314](https://github.com/getsentry/symbolicator/pull/314))

### Tools

- `wasm-split` now retains all sections. ([#311](https://github.com/getsentry/symbolicator/pull/311))

### Features

- Symbolicator will now track Release Health Sessions for each symbolication request. ([#289](https://github.com/getsentry/symbolicator/pull/289))

## 0.3.1

### Features

- Add support for DWARF in WASM. ([#301](https://github.com/getsentry/symbolicator/pull/301))
- Support symbolication of native Dart stack traces by prioritizing DWARF function names over symbol table entries.

### Tools

- Add `wasm-split`, which splits WASM files into code and debug information. ([#303](https://github.com/getsentry/symbolicator/pull/303))

## 0.3.0

### Features

- Publish Docker containers to DockerHub at `getsentry/symbolicator`. ([#271](https://github.com/getsentry/symbolicator/pull/271))
- Add `processing_pool_size` configuration option that allows to set the size of the processing pool. ([#273](https://github.com/getsentry/symbolicator/pull/273))
- Use a dedicated `tmp` sub-directory in the cache directory to write temporary files into. ([#265](https://github.com/getsentry/symbolicator/pull/265))
- Use STATSD_SERVER environment variable to set metrics.statsd configuration option ([#182](https://github.com/getsentry/symbolicator/pull/182))
- Added WASM support. ([#301](https://github.com/getsentry/symbolicator/pull/301))

### Bug Fixes

- Fix a bug where Sentry requests were cached even though caches were disabled. ([#260](https://github.com/getsentry/symbolicator/pull/260))
- Fix CFI cache status in cases where the PDB file was found but not the PE file. ([#279](https://github.com/getsentry/symbolicator/pull/279))
- Instead of marking unused unwind info as missing, report it as unused. ([#293](https://github.com/getsentry/symbolicator/pull/293))

## 0.2.0

- This changelog is entirely incomplete, future releases will try to improve this.

### Breaking Changes

- The configuration file is stricter on which fields can be present, unknown fields will no longer be accepted and cause an error.
- Configuring cache durations is now done using (humantime)[https://docs.rs/humantime/latest/humantime/fn.parse_duration.html].

## 0.1.0

- Initial version of symbolicator
