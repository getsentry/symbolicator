# Changelog

## 24.11.0

### Dependencies

- Bump Native SDK from v0.7.10 to v0.7.12 ([#1546](https://github.com/getsentry/symbolicator/pull/1546), [#1550](https://github.com/getsentry/symbolicator/pull/1550))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#0712)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.7.10...0.7.12)

## 24.10.0

### Various fixes & improvements

- Added `dry-run` mode to the `cleanup` command that simulates the cleanup
  without deleting files. ([#1531](https://github.com/getsentry/symbolicator/pull/1531))
- Parse debug identifiers from scraped JavaScript source files
  ([#1534](https://github.com/getsentry/symbolicator/pull/1534))

### Dependencies

- Bump Native SDK from v0.7.9 to v0.7.10 ([#1527](https://github.com/getsentry/symbolicator/pull/1527))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#0710)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.7.9...0.7.10)

## 24.9.0

### Various fixes & improvements

- Transaction sample rate is now configurable through the config file ([#1517](https://github.com/getsentry/symbolicator/pull/1517)).
- In native symbolication, trying to download a source file from a
  sourcelink now creates a candidate ([#1507](https://github.com/getsentry/symbolicator/pull/1510)).

### Dependencies

- Bump Native SDK from v0.7.7 to v0.7.9 ([#1510](https://github.com/getsentry/symbolicator/pull/1510), [#1513](https://github.com/getsentry/symbolicator/pull/1513))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#079)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.7.7...0.7.9)

## 24.8.0

### Dependencies

- Bump Native SDK from v0.7.6 to v0.7.7 ([#1502](https://github.com/getsentry/symbolicator/pull/1502))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#077)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.7.6...0.7.7)

## 24.7.1

- No documented changes.

## 24.7.0

### Dependencies

- Bump Native SDK from v0.7.5 to v0.7.6 ([#1486](https://github.com/getsentry/symbolicator/pull/1486))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#076)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.7.5...0.7.6)

### Various fixes & improvements

- The `symbolicate-jvm` endpoint now additionally accepts a list
  `classes` of class names to deobfuscate. ([#1496](https://github.com/getsentry/symbolicator/pull/1496))

## 24.6.0

### Dependencies

- Bump Native SDK from v0.7.2 to v0.7.5 ([#1475](https://github.com/getsentry/symbolicator/pull/1475))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#075)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.7.2...0.7.5)

## 24.5.1

### Various fixes & improvements

- fix: Don't report max requests errors to Sentry (#1471) by @loewenheim
- config: Turn max_concurrent_requests up to 200 (#1467) by @loewenheim
- Do a `cargo update` (#1468) by @Swatinem
- ref(proguard): Don't log "not found" errors (#1469) by @loewenheim
- ref(js): Always serialize `symbolicated` flag (#1465) by @loewenheim

## 24.5.0

### Various fixes and improvements

- feat(proguard): Remap filenames and abs_paths (#1432) by @loewenheim

## 24.4.2

### Various fixes & improvements

- fix(js): Don't apply minified source context to symbolicated frame (#1449) by @loewenheim
- meta(proguard:) Update comments about remapping (#1444) by @loewenheim

## 24.4.1

### Various fixes & improvements

- fix: Always trim source context lines (#1443) by @loewenheim
- fix(symbolicli): Add C# platform (#1442) by @loewenheim
- fix(proguard): Don't set `in_app` if frame wasn't mapped (#1440) by @loewenheim
- fix(proguard): Correctly compute source file name (#1441) by @loewenheim
- Do a `cargo update` (#1439) by @Swatinem
- ref(proguard): Increase in-memory cache size to 5GiB (#1438) by @loewenheim
- feat(js): JS frames have a new field `data.sourcemap_origin` containing information about where the sourcemap used to symbolicate the frame came frmo (#1447) by @loewenheim

## 24.4.0

### Various fixes & improvements

- proguard: Added a mandatory `index` field to `JvmFrame` (#1411) by @loewenheim
- proguard: Support for source contex (#1414) by @loewenheim
- proguard: Field `lineno` on `JvmFrame` is now optional (#1417) by @loewenheim
- proguard: add support for frame signature (#1425 ) by @viglia
- proguard: add the translated and deobfuscated signature even in the case that the the whole frame could not be remapped (#1427) by @viglia

### Dependencies

- Bump Native SDK from v0.7.0 to v0.7.2 ([#1416](https://github.com/getsentry/symbolicator/pull/1416), [#1428](https://github.com/getsentry/symbolicator/pull/1428))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#072)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.7.0...0.7.2)

## 24.3.0

### Various fixes & improvements

- Allow http symbol sources to indicate that invalid SSL certificates should be accepted (#1405) by @loewenheim
- Make host deny list optional and allow exclusion of individual hosts (#1407) by @loewenheim

## 24.2.0

### Various fixes & improvements

- Do a `cargo update` (#1377) by @Swatinem
- Increase percentage of sampled transactions (#1376) by @Swatinem
- feat(proguard): Add types for JVM requests (#1373) by @loewenheim
- Make stresstest observability configurable (#1375) by @Swatinem
- chore(ci): Revert upload-artifact version (#1374) by @azaslavsky
- doc(sources): Document rationale for Sentry list_file behavior (#1372) by @loewenheim
- Add a basic `proguard` workspace crate (#1371) by @Swatinem
- build(deps): bump google-github-actions/auth from 1 to 2 (#1365) by @dependabot
- build(deps): bump actions/setup-python from 4 to 5 (#1364) by @dependabot
- build(deps): bump actions/checkout from 3 to 4 (#1368) by @dependabot
- build(deps): bump codecov/codecov-action from 3 to 4 (#1367) by @dependabot
- build(deps): bump actions/upload-artifact from 3.1.1 to 4.3.1 (#1366) by @dependabot

## 24.1.2

- Add dependabot for github actions. ([#1362](https://github.com/getsentry/symbolicator/pull/1362))

## 24.1.1

- No documented changes.

## 24.1.0

### Dependencies

- Bump Native SDK from v0.6.5 to v0.7.0 ([#1347](https://github.com/getsentry/symbolicator/pull/1347))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#070)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.6.5...0.7.0)

## 23.12.1

- No documented changes.

## 23.12.0

- No documented changes.

## 23.11.2

- No documented changes.

## 23.11.1

- No documented changes.

## 23.11.0

- No documented changes.

## 23.10.1

- No documented changes.

## 23.10.0

- No documented changes.

## 23.9.1

- No documented changes.

## 23.9.0

### Various fixes & improvements

- Docker containers were updated to debian bookworm. ([#1293](https://github.com/getsentry/symbolicator/pull/1293))
- Do not process `.o` files in Symsorter. ([#1288](https://github.com/getsentry/symbolicator/pull/1288))
- symsorter now uses the same filter as sentry-cli to ignore (very) large files and precompiled headers. ([#1273](https://github.com/getsentry/symbolicator/pull/1273))
- JS source mapping URLs are now read without query strings or fragments. ([#1294](https://github.com/getsentry/symbolicator/pull/1294))

### Dependencies
- Bump `symbolic` from 12.3.0 to 12.4.0. ([#1294](https://github.com/getsentry/symbolicator/pull/1294))

## 23.8.0

### Dependencies

- Bump `aws-sdk-rust` from 0.55.3 to 0.56.0. ([#1282](https://github.com/getsentry/symbolicator/pull/1282))

## 23.7.2

### Dependencies

- Bump Native SDK from v0.6.4 to v0.6.5 ([#1247](https://github.com/getsentry/symbolicator/pull/1247))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#065)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.6.4...0.6.5)

## 23.7.1

### Various fixes & improvements

- Add a new `BundleIndex` for SourceMap processing (#1251) by @Swatinem

## 23.7.0

- Add authentication to Source Context fetching via scraping config ([#1250](https://github.com/getsentry/symbolicator/pull/1250))

## 23.6.2

### Features

- Add `--symbols` argument to `symbolicli` ([#1241](https://github.com/getsentry/symbolicator/pull/1241))
- Add a new `BundleIndex` for SourceMap processing. ([#1251](https://github.com/getsentry/symbolicator/pull/1251))

### Fixes

- Reintroduce `--version` option to `symsorter` and `wasm-split` ([#1219](https://github.com/getsentry/symbolicator/pull/1219))
- Add special case for extracting name of rewritten async functions in Dart lang ([#1246](https://github.com/getsentry/symbolicator/pull/1246))

### Dependencies

- Bump Native SDK from v0.6.3 to v0.6.4 ([#1224](https://github.com/getsentry/symbolicator/pull/1224))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#064)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.6.3...0.6.4)

## 23.6.1

- No documented changes.

## 23.6.0

### Dependencies

- Bump Native SDK from v0.6.2 to v0.6.3 ([#1207](https://github.com/getsentry/symbolicator/pull/1207))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#063)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.6.2...0.6.3)
- Bump `aws-sdk-rust` from 0.52.0 to 0.55.3. ([#1211](https://github.com/getsentry/symbolicator/pull/1211))

## 23.5.2

- JsFrame::lineno is no longer optional ([#1203](https://github.com/getsentry/symbolicator/pull/1203))

## 23.5.1

- No documented changes.

## 23.5.0

### Features

- Add single-file ZipArchive support to `maybe_decompress_file` ([#1139](https://github.com/getsentry/symbolicator/pull/1139))
- Make source-context application optional. ([#1173](https://github.com/getsentry/symbolicator/pull/1173))
- `symbolicli` now supports JS symbolication. ([#1186](https://github.com/getsentry/symbolicator/pull/1186))
- Tighten up download related timeouts and introduce new `head_timeout`. ([#1190](https://github.com/getsentry/symbolicator/pull/1190))

### Fixes

- Allow symbolicli to connect to reserved IPs ([#1165](https://github.com/getsentry/symbolicator/pull/1165))
- JS symbolication now requires a Sentry source ([#1180](https://github.com/getsentry/symbolicator/pull/1180))
- `symbolicli` now uses the right stacktraces for symbolication. ([#1189](https://github.com/getsentry/symbolicator/pull/1189))

### Dependencies

- Bump Native SDK from v0.6.1 to v0.6.2 ([#1178](https://github.com/getsentry/symbolicator/pull/1178))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#062)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.6.1...0.6.2)

## 23.4.0

### Features

- Migrate to monthly CalVer releases. ([#1130](https://github.com/getsentry/symbolicator/pull/1130))
- Introduce a `CacheKeyBuilder` and human-readable `CacheKey` metadata. ([#1033](https://github.com/getsentry/symbolicator/pull/1033), [#1036](https://github.com/getsentry/symbolicator/pull/1036))
- Use new `CacheKey` for writes and shared-cache. ([#1038](https://github.com/getsentry/symbolicator/pull/1038))
- Consolidate `CacheVersions` and bump to refresh `CacheKey` usage. ([#1041](https://github.com/getsentry/symbolicator/pull/1041), [#1042](https://github.com/getsentry/symbolicator/pull/1042))
- Automatically block downloads from unreliable hosts. ([#1039](https://github.com/getsentry/symbolicator/pull/1039))
- Fully migrate `CacheKey` usage and remove legacy markers. ([#1043](https://github.com/getsentry/symbolicator/pull/1043))
- Add support for in-memory caching. ([#1028](https://github.com/getsentry/symbolicator/pull/1028))
- Add --log-level argument to `symbolicli`. ([#1074](https://github.com/getsentry/symbolicator/pull/1074))
- Resolve source context from embedded source links (mainly in Portable PDBs) ([#1103](https://github.com/getsentry/symbolicator/pull/1103), [#1108](https://github.com/getsentry/symbolicator/pull/1108))

### Fixes

- Mask invalid file names in minidumps. ([#1047](https://github.com/getsentry/symbolicator/pull/1047), [#1133](https://github.com/getsentry/symbolicator/pull/1133))
- Update `minidump-processor` so minidumps with a 0-sized module are being processed. ([#1131](https://github.com/getsentry/symbolicator/pull/1131))

### Dependencies

- Bump Native SDK from v0.5.4 to v0.6.1 ([#1048](https://github.com/getsentry/symbolicator/pull/1048), [#1121](https://github.com/getsentry/symbolicator/pull/1121))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#061)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.5.4...0.6.1)

## 0.7.0

### Features

- Added a field `adjust_instruction_addr: Option<bool>` to `RawFrame` to signal whether the
  frame's instruction address needs to be adjusted for symbolication. ([#948](https://github.com/getsentry/symbolicator/pull/948))
- Added offline mode and caching to `symbolicli`. ([#967](https://github.com/getsentry/symbolicator/pull/967),[#968](https://github.com/getsentry/symbolicator/pull/968))
- Support PortablePDB embedded sources. ([#996](https://github.com/getsentry/symbolicator/pull/996))
- Properly support NuGet symbols with SymbolChecksum. ([#993](https://github.com/getsentry/symbolicator/pull/993))
- Use `moka` as an in-memory `Cacher` implementation. ([#1010](https://github.com/getsentry/symbolicator/pull/1010))

### Internal

- Use a default crash DB if we have a Cache configured. ([#941](https://github.com/getsentry/symbolicator/pull/941))
- Simplified internal error and cache architecture. ([#929](https://github.com/getsentry/symbolicator/pull/929),
  [#936](https://github.com/getsentry/symbolicator/pull/936), [#937](https://github.com/getsentry/symbolicator/pull/937))
- Migrate from `rusoto` to `aws-sdk-s3`. ([#849](https://github.com/getsentry/symbolicator/pull/849), [#954](https://github.com/getsentry/symbolicator/pull/954))
- Replace the last remaining LRU caches with `moka` versions and remove `lru` dependency. ([#978](https://github.com/getsentry/symbolicator/pull/978))

### Dependencies

- Bump Native SDK from v0.5.0 to v0.5.4 ([#940](https://github.com/getsentry/symbolicator/pull/940), [#997](https://github.com/getsentry/symbolicator/pull/997))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#054)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.5.0...0.5.4)

## 0.6.0

### Features

- Use Portable PDB files for .NET event symbolication ([#883](https://github.com/getsentry/symbolicator/pull/883))
- Use Unity il2cpp line mapping files in symcache creation ([#831](https://github.com/getsentry/symbolicator/pull/831))
- Read thread names from minidumps and Apple crash reports ([#834](https://github.com/getsentry/symbolicator/pull/834))
- Add support for serving web requests using HTTPS ([#829](https://github.com/getsentry/symbolicator/pull/829))
- Bump Native SDK to v0.5.0 ([#865](https://github.com/getsentry/symbolicator/pull/865))
  - [changelog](https://github.com/getsentry/sentry-native/blob/master/CHANGELOG.md#050)
  - [diff](https://github.com/getsentry/sentry-native/compare/0.4.17-2-gbd56100...0.5.0)
- Use `rust:bullseye-slim` as base image for docker container builder. ([#885](https://github.com/getsentry/symbolicator/pull/885))
- Use `debian:bullseye-slim` as base image for docker container runner. ([#885](https://github.com/getsentry/symbolicator/pull/885))
- Use jemalloc as global allocater (for Rust and C). ([#885](https://github.com/getsentry/symbolicator/pull/885))
- Clean up empty cache directories. ([#887](https://github.com/getsentry/symbolicator/pull/887))
- Update symbolic and increase SymCache Version to `4` which now uses a LEB128-prefixed string table. ([#886](https://github.com/getsentry/symbolicator/pull/886))
- Add Configuration options for in-memory Caches. ([#911](https://github.com/getsentry/symbolicator/pull/911))
- Add the `symbolicli` utility for local symbolication. ([#919](https://github.com/getsentry/symbolicator/pull/919))

### Fixes

- Update symbolic and increase the SymCache version for the following fixes: ([#857](https://github.com/getsentry/symbolicator/pull/857))
  - fixed problems with DWARF functions that have the same line records for different inline hierarchy
  - fixed problems with PDB where functions have line records that don't belong to them
  - fixed problems with PDB/DWARF when parent functions don't have matching line records
  - using a new TypeFormatter for PDB that can pretty-print function arguments
- Update symbolic and increase CFI/SymCache versions for the following fixes: ([#861](https://github.com/getsentry/symbolicator/pull/
  861))
  - Allow underflow in Win-x64 CFI which allows loading registers from outside the stack frame.
  - Another round of SymCache fixes as followup to the above.
- Extract the correct `code_id` from iOS minidumps. ([#858](https://github.com/getsentry/symbolicator/pull/858))
- Fetch/use CFI for modules that only have a CodeId. ([#860](https://github.com/getsentry/symbolicator/pull/860))
- Properly mask Portable PDB Age for symstore/SSQP lookups. ([#888](https://github.com/getsentry/symbolicator/pull/888))
- Avoid a redundant open/read and fix shared cache refresh. ([#893](https://github.com/getsentry/symbolicator/pull/893))
- Correctly return object candidates for malformed caches. ([#917](https://github.com/getsentry/symbolicator/pull/917))

### Internal

- Fetch CFI on-demand during stackwalking ([#838](https://github.com/getsentry/symbolicator/pull/838))
- Deduplicate SymbolFile computations in SymbolicatorSymbolProvider ([#856](https://github.com/getsentry/symbolicator/pull/856))
- Separated the symbolication service from the http interface. ([#903](https://github.com/getsentry/symbolicator/pull/903))
- Move Request/Response/Poll handling into web frontend. ([#904](https://github.com/getsentry/symbolicator/pull/904))
- Remove unused `processing_pool_size` config. ([#910](https://github.com/getsentry/symbolicator/pull/910))

## 0.5.1

### Features

- Added an internal option to capture minidumps for hard crashes. This has to be enabled via the `_crash_db` config parameter. ([#795](https://github.com/getsentry/symbolicator/pull/795))

### Fixes

- Update symbolic to generate/use higher fidelity CFI instructions for Win-x64 binaries. ([#822](https://github.com/getsentry/symbolicator/pull/822))

## 0.5.0

### Features

- Support for `external_debug_info` section in wasm-split for external dwarf files. ([#619](https://github.com/getsentry/symbolicator/pull/619))
- Added windows binaries for `wasm-split` and `symsorter` to releases ([#624](https://github.com/getsentry/symbolicator/pull/624))
- Also populate the shared cache from existing items in the local cache, not only new ones. ([#648](https://github.com/getsentry/symbolicator/pull/648))
- Switch minidump processing entirely to `rust-minidump`, this is currently faster, more reliable and produces at least similar quality stacktraces. Breakpad has been removed as a dependency. ([#723](https://github.com/getsentry/symbolicator/pull/723) [#731](https://github.com/getsentry/symbolicator/pull/731) [#732](https://github.com/getsentry/symbolicator/pull/732))

### Fixes

- Avoid errors from symbol source in bcsymbolmap lookups from cancelling the entire symcache lookup. ([#643](https://github.com/getsentry/symbolicator/pull/643))
- Use wildcard matcher for symstore proxy. ([#671](https://github.com/getsentry/symbolicator/pull/671))
- Detect unwind and debug information in ELF files that contain sections with offset `0`. They were previously skipped in some cases. ([#688](https://github.com/getsentry/symbolicator/pull/688))
- Support processing unwind information in ELF files linked with `gold`. ([#688](https://github.com/getsentry/symbolicator/pull/688))
- Automatically refresh expired AWS ECS container credentials. ([#754](https://github.com/getsentry/symbolicator/pull/754))
- Don't emit ANSI escape sequences with `auto=false` and `simplified` logging formats. ([#755](https://github.com/getsentry/symbolicator/pull/755))
- Make sure "source" candidates are being recorded (ISSUE-1365). ([#765](https://github.com/getsentry/symbolicator/pull/765))

## 0.4.1

### Features

- Very basic shared-cache support, allowing multiple symbolicators to share one global cache and have faster warmup times. ([#581](https://github.com/getsentry/symbolicator/pull/581))
- Support concurrency control and backpressure on uploading to shared cache. ([#584](https://github.com/getsentry/symbolicator/pull/584))
- Use Google Application Credentials to authenticate to GCS in the shared cache. ([#591](https://github.com/getsentry/symbolicator/pull/591))

### Fixes

- Truncate Malformed and Cache-Specific entries in the cache to match the length of their contents, in case they overwrote some longer, existing entry. ([#586](https://github.com/getsentry/symbolicator/pull/586))

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
- Use the `tracing` crate for logging ([#534](https://github.com/getsentry/symbolicator/pull/534))

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
- Configuring cache durations is now done using [humantime](https://docs.rs/humantime/latest/humantime/fn.parse_duration.html).

## 0.1.0

- Initial version of symbolicator
